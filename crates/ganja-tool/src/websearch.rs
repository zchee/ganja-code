//! The `websearch` tool: asks a search service and hands the model its answer.
//!
//! Spec: upstream `packages/opencode/src/tool/websearch.ts`, its wire half
//! `packages/opencode/src/tool/mcp-websearch.ts`, and `websearch.txt`.
//!
//! Two services, one shape. Both Exa and Parallel expose their search as an
//! MCP endpoint reached over plain HTTP, so upstream sends a single
//! `tools/call` JSON-RPC body and reads the answer out of whatever comes back
//! — a JSON object, or a `text/event-stream` whose `data:` lines each hold
//! one. This ports **that request**, not an MCP client: a session's MCP
//! servers are configured, dialled and listed
//! (`ganja-core`'s `mcp` module), and none of that applies to one fixed call
//! to one fixed endpoint. `mcp-websearch.ts` is upstream making the same
//! judgement — it is a hand-rolled POST living beside the tool, not a use of
//! its own MCP client.
//!
//! # Divergences
//!
//! - **`websearch-picks-the-service-without-a-session-id`** — upstream picks
//!   between the two services by hashing the session id
//!   (`websearch.ts:30-37`), so a conversation keeps one service for its whole
//!   life and the population splits evenly. A tool call here is handed
//!   [`ToolCtx`], which carries no session id — deliberately, since it is a bag
//!   of values rather than a handle back to a session — so the coin cannot be
//!   flipped the way upstream flips it. The choice is made from what is
//!   configured instead: `PROVIDER_ENV` when it names one, otherwise the
//!   service whose key this machine holds. Which of the two should win when
//!   both keys are present is a question about accounts and billing that only
//!   the person running the session can answer; until it is answered, `exa`
//!   wins — it is the arm upstream's own default falls through to first, and
//!   the one whose arguments carry the knobs this tool's schema exposes.
//! - **`websearch-refuses-without-a-key`** — upstream calls Exa's endpoint with
//!   no key at all when `EXA_API_KEY` is unset (`mcp-websearch.ts:4-6`), which
//!   is an unauthenticated request to a third party made on the model's say-so.
//!   Here a missing key is a refusal that names the variable to set, which the
//!   model reads and can repeat to the user.
//! - **`websearch-sends-no-session-id`** — Parallel's arguments carry
//!   `session_id` and `model_name` upstream (`websearch.ts:70-77`), both of
//!   which identify a conversation to the service. Neither is reachable from
//!   here for the reason above, and both are optional in upstream's own schema
//!   (`mcp-websearch.ts:51-56`), so they are omitted rather than invented.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolCtx, ToolError, ToolOutput, truncate};

/// Exa's endpoint. The key travels in the query string because that is where
/// this service takes it (`mcp-websearch.ts:4-6`) — which is also why nothing
/// here ever logs a request URL.
const EXA_URL: &str = "https://mcp.exa.ai/mcp";

/// Parallel's endpoint, which takes its key in an `Authorization` header
/// (`mcp-websearch.ts:7`, `websearch.ts:54-58`).
const PARALLEL_URL: &str = "https://search.parallel.ai/mcp";

/// Where Exa's key is read from, spelled as upstream spells it so a machine
/// already set up for opencode needs no second variable.
const EXA_KEY_ENV: &str = "EXA_API_KEY";

/// The same for Parallel.
const PARALLEL_KEY_ENV: &str = "PARALLEL_API_KEY";

/// Names the service a session should use, overriding what the keys imply.
/// Upstream's is `OPENCODE_WEBSEARCH_PROVIDER` (`websearch.ts:31`).
const PROVIDER_ENV: &str = "GANJA_WEBSEARCH_PROVIDER";

/// How long one search may take, upstream's 25 seconds
/// (`websearch.ts:78`, `:95`).
const TIMEOUT: Duration = Duration::from_secs(25);

/// What the tool sends when the model names no count (`websearch.ts:91`).
const DEFAULT_RESULTS: u32 = 8;

/// What the tool sends when the model names no crawl mode (`websearch.ts:92`).
const DEFAULT_LIVECRAWL: &str = "fallback";

/// What the tool sends when the model names no search type (`websearch.ts:90`).
const DEFAULT_TYPE: &str = "auto";

/// What the model is told when the service answered with nothing usable
/// (`websearch.ts:136`).
const NOTHING_FOUND: &str = "No search results found. Please try a different query.";

/// The token the description carries for the year the session is running in.
const YEAR_TOKEN: &str = "{{year}}";

/// What [`encode`] escapes: everything outside RFC 3986's unreserved set,
/// `A-Za-z0-9-._~`.
///
/// No stock set spells that. [`NON_ALPHANUMERIC`] is the same complement plus
/// those four marks, which is stricter than what this has ever put on the
/// wire, so the marks are taken back out — and the test
/// `the_bytes_a_query_string_value_is_escaped_into_are_exactly_these` is what
/// holds the two byte-identical.
const ESCAPED: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// Which service a search is asked of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Service {
    /// Exa, which takes the query and the tool's own search knobs.
    Exa,
    /// Parallel, which takes an objective and a list of queries.
    Parallel,
}

impl Service {
    /// The id a rule, a metadata field or [`PROVIDER_ENV`] names it by.
    const fn id(self) -> &'static str {
        match self {
            Self::Exa => "exa",
            Self::Parallel => "parallel",
        }
    }

    /// What a transcript calls it (`websearch.ts:39-43`).
    const fn label(self) -> &'static str {
        match self {
            Self::Exa => "Exa Web Search",
            Self::Parallel => "Parallel Web Search",
        }
    }

    /// The variable holding its key.
    const fn key_env(self) -> &'static str {
        match self {
            Self::Exa => EXA_KEY_ENV,
            Self::Parallel => PARALLEL_KEY_ENV,
        }
    }

    /// The tool this service registers its search under
    /// (`websearch.ts:68`, `:84`).
    const fn remote_tool(self) -> &'static str {
        match self {
            Self::Exa => "web_search_exa",
            Self::Parallel => "web_search",
        }
    }

    /// The service `id` names, or nothing.
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "exa" => Some(Self::Exa),
            "parallel" => Some(Self::Parallel),
            _ => None,
        }
    }
}

/// How live crawling is used, as upstream's schema spells the two values.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Livecrawl {
    /// Crawl only when the cached content is unavailable.
    Fallback,
    /// Crawl in preference to the cache.
    Preferred,
}

impl Livecrawl {
    /// The string the service is sent.
    const fn id(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::Preferred => "preferred",
        }
    }
}

/// How hard the service should look, as upstream's schema spells the three
/// values.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SearchType {
    /// Balanced.
    Auto,
    /// Quick.
    Fast,
    /// Comprehensive.
    Deep,
}

impl SearchType {
    /// The string the service is sent.
    const fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fast => "fast",
            Self::Deep => "deep",
        }
    }
}

/// What the model passes to `websearch`, in upstream's spelling — the golden
/// differential compares argument names, so `numResults` is the contract and
/// not a style choice.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// Websearch query
    query: String,
    /// Number of search results to return (default: 8)
    #[serde(default, rename = "numResults")]
    num_results: Option<u32>,
    /// Live crawl mode - 'fallback': use live crawling as backup if cached content unavailable, 'preferred': prioritize live crawling (default: 'fallback')
    #[serde(default)]
    livecrawl: Option<Livecrawl>,
    /// Search type - 'auto': balanced search (default), 'fast': quick results, 'deep': comprehensive search
    #[serde(default, rename = "type")]
    search_type: Option<SearchType>,
    /// Maximum characters for context string optimized for LLMs (default: 10000)
    #[serde(default, rename = "contextMaxCharacters")]
    context_max_characters: Option<u32>,
}

/// The two keys this machine holds, whichever of them it holds.
///
/// A value rather than a pair of reads at the point of use: which service a
/// call goes to is decided from *both* of them, and a decision made from two
/// separate `std::env::var` calls is a decision that can see the environment
/// change halfway through.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Keys {
    /// `EXA_API_KEY`, if it is set to anything but the empty string.
    exa: Option<String>,
    /// `PARALLEL_API_KEY`, likewise.
    parallel: Option<String>,
}

impl Keys {
    /// What this process's environment holds.
    ///
    /// An empty value is no value: a variable exported blank by a shell
    /// profile would otherwise select a service and then fail against it.
    fn from_env() -> Self {
        let read = |name| std::env::var(name).ok().filter(|value| !value.trim().is_empty());

        Self { exa: read(EXA_KEY_ENV), parallel: read(PARALLEL_KEY_ENV) }
    }

    /// The key for `service`, if it is held.
    fn get(&self, service: Service) -> Option<&str> {
        match service {
            Service::Exa => self.exa.as_deref(),
            Service::Parallel => self.parallel.as_deref(),
        }
    }
}

/// Searches the web.
pub struct WebsearchTool {
    /// Upstream's description with [`YEAR_TOKEN`] already replaced.
    ///
    /// Substituted once, here, because [`Tool::description`] hands back a
    /// borrow and because upstream substitutes it in a getter it re-reads per
    /// request (`websearch.ts:106-108`) — the same answer for any session that
    /// does not outlive New Year's Eve, and one allocation instead of one per
    /// request.
    description: String,
    /// Where Exa is reached, without its key.
    exa_url: String,
    /// Where Parallel is reached.
    parallel_url: String,
}

impl WebsearchTool {
    /// The tool as it ships: the two services at their real endpoints.
    #[must_use]
    pub fn new() -> Self {
        Self::against(EXA_URL, PARALLEL_URL)
    }

    /// The same tool pointed at `exa` and `parallel` instead, which is how the
    /// request this builds is asserted against a socket rather than against a
    /// mock of the client that sends it.
    fn against(exa: &str, parallel: &str) -> Self {
        Self {
            description: include_str!("websearch.txt")
                .replace(YEAR_TOKEN, &current_utc_year().to_string()),
            exa_url: exa.to_owned(),
            parallel_url: parallel.to_owned(),
        }
    }

    /// Where `service` is reached, with Exa's key in the query string as that
    /// service takes it.
    fn endpoint(&self, service: Service, key: &str) -> String {
        match service {
            Service::Exa => format!("{}?exaApiKey={}", self.exa_url, encode(key)),
            Service::Parallel => self.parallel_url.clone(),
        }
    }

    /// One search, with the service and its key already decided.
    ///
    /// Split from [`Tool::run`] at exactly the line where the environment
    /// stops mattering: everything above it reads variables, everything below
    /// it builds and sends a request. That is what lets the request be
    /// asserted against a socket without a test mutating the environment its
    /// neighbours are reading.
    async fn execute(
        &self,
        args: &Args,
        ctx: &ToolCtx,
        service: Service,
        key: &str,
    ) -> Result<ToolOutput, ToolError> {
        tokio::select! {
            searched = search(self.endpoint(service, key), service, key, args) => searched,
            () = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
        }
    }
}

impl Default for WebsearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebsearchTool {
    fn id(&self) -> &str {
        "websearch"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let query = args.get("query").and_then(serde_json::Value::as_str).unwrap_or_default();

        format!("search {query}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let keys = Keys::from_env();
        let service = select(std::env::var(PROVIDER_ENV).ok().as_deref(), &keys)?;
        // Read before the request rather than inside it: a search that is
        // going to be refused for want of a key should be refused without
        // opening a socket to say so.
        let key = keys.get(service).ok_or_else(|| missing_key(service))?;

        self.execute(&args, ctx, service, key).await
    }
}

/// Which service this call goes to.
///
/// `named` is [`PROVIDER_ENV`]'s value, and it wins outright when it names a
/// service — a value that names something else is ignored, as upstream ignores
/// it (`websearch.ts:32`). Otherwise the keys decide; see the module's
/// `websearch-picks-the-service-without-a-session-id`.
fn select(named: Option<&str>, keys: &Keys) -> Result<Service, ToolError> {
    if let Some(service) = named.map(str::trim).and_then(Service::from_id) {
        return Ok(service);
    }
    if keys.exa.is_some() {
        return Ok(Service::Exa);
    }
    if keys.parallel.is_some() {
        return Ok(Service::Parallel);
    }

    Err(ToolError::Failed(format!(
        "websearch has no credential: set {EXA_KEY_ENV} to search with Exa, or \
         {PARALLEL_KEY_ENV} to search with Parallel."
    )))
}

/// What the model is told when the service it was pointed at has no key.
fn missing_key(service: Service) -> ToolError {
    ToolError::Failed(format!(
        "websearch is set to use {} and {} is not set.",
        service.id(),
        service.key_env()
    ))
}

/// Asks `service` and returns what it said.
async fn search(
    url: String,
    service: Service,
    key: &str,
    args: &Args,
) -> Result<ToolOutput, ToolError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|error| ToolError::Failed(format!("no HTTP client: {error}")))?;

    let mut request = client
        .post(&url)
        // Both are acceptable and which arrives is the service's choice, which
        // is why the reader below takes either (`mcp-websearch.ts:80`).
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        // Ganja's own name: neither service involves a client registration
        // somebody else owns, so neither was ever told anything but what this
        // is. Spelled the way `ganja-provider`'s `GANJA_USER_AGENT` spells it
        // — a literal rather than that constant because this crate depends on
        // `ganja-permission` and nothing else of ours, and one product name is
        // not worth an edge in that graph.
        .header(reqwest::header::USER_AGENT, concat!("ganja-code/", env!("CARGO_PKG_VERSION")))
        .json(&body(service, args));
    if service == Service::Parallel {
        request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"));
    }

    // One deadline over the whole exchange — connect, headers and body — so a
    // service that answers instantly and then dribbles forever is still
    // bounded, as upstream's `Effect.timeoutOrElse` bounds it.
    let text = tokio::time::timeout(TIMEOUT, async {
        let response = request
            .send()
            .await
            .map_err(|error| ToolError::Failed(format!("the search did not complete: {error}")))?;
        // The status first: a body read out of a 401 is an error page, and
        // handing the model "no search results" for a rejected credential
        // would be the one answer it cannot act on.
        let response = response.error_for_status().map_err(|error| {
            ToolError::Failed(format!("{} refused the search: {error}", service.label()))
        })?;

        response
            .text()
            .await
            .map_err(|error| ToolError::Failed(format!("the search response stopped: {error}")))
    })
    .await
    .map_err(|_elapsed| {
        ToolError::Failed(format!(
            "{} did not answer within {}s",
            service.label(),
            TIMEOUT.as_secs()
        ))
    })??;

    let found = parse(&text).unwrap_or_else(|| NOTHING_FOUND.to_owned());
    let clamped = truncate::clamp(&found);

    Ok(ToolOutput {
        title: format!("{}: {}", service.label(), args.query),
        output: clamped.text,
        metadata: serde_json::json!({ "provider": service.id() }),
    })
}

/// The JSON-RPC body one search sends (`mcp-websearch.ts:58-87`).
fn body(service: Service, args: &Args) -> serde_json::Value {
    let arguments = match service {
        Service::Exa => {
            let mut arguments = serde_json::json!({
                "query": args.query,
                "type": args.search_type.map_or(DEFAULT_TYPE, SearchType::id),
                "numResults": args.num_results.unwrap_or(DEFAULT_RESULTS),
                "livecrawl": args.livecrawl.map_or(DEFAULT_LIVECRAWL, Livecrawl::id),
            });
            // Optional upstream, and omitted rather than defaulted: the
            // service's own default for the context budget is not this tool's
            // to guess.
            if let Some(characters) = args.context_max_characters {
                arguments["contextMaxCharacters"] = characters.into();
            }

            arguments
        }
        Service::Parallel => serde_json::json!({
            "objective": args.query,
            "search_queries": [args.query],
        }),
    };

    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": service.remote_tool(), "arguments": arguments },
    })
}

/// The text a search result carries, out of whichever shape it arrived in.
///
/// Upstream reads the whole body as JSON first and only then scans it as an
/// event stream (`mcp-websearch.ts:30-41`), which is what makes one reader
/// serve both content types. The scan is over `data: ` lines because that is
/// all of an SSE frame this needs: the event name and the id carry nothing the
/// answer depends on.
fn parse(body: &str) -> Option<String> {
    if let Some(text) = payload(body) {
        return Some(text);
    }

    body.lines().filter_map(|line| line.strip_prefix("data: ")).find_map(payload)
}

/// The first non-empty result text in one JSON payload, or nothing when the
/// payload is not one.
fn payload(text: &str) -> Option<String> {
    let trimmed = text.trim();
    // Upstream's own guard: anything that is not an object is not an answer,
    // and asking a parser about it would be a parse error per SSE comment
    // line.
    if !trimmed.starts_with('{') {
        return None;
    }

    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;

    value
        .get("result")?
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
        .find(|text| !text.is_empty())
        .map(str::to_owned)
}

/// `value`, percent-encoded for a query string.
///
/// [`ESCAPED`] is stricter than a URL needs — every reserved character goes,
/// not only the ones that would end the value — which cannot be wrong for a
/// value nothing ever parses structure out of.
fn encode(value: &str) -> String {
    utf8_percent_encode(value, ESCAPED).to_string()
}

/// The year this process is running in, UTC.
///
/// This crate sits beneath `ganja-core`, so it cannot borrow the prompt
/// formatter above it. As with the other consumers, the dependency graph
/// admits a thin local jiff call site rather than a shared helper in the
/// protocol leaf.
///
/// A clock before the epoch still yields 1970 rather than turning a broken
/// machine clock into a misleading prompt year.
fn current_utc_year() -> u32 {
    let timestamp = Timestamp::try_from(SystemTime::now())
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .max(Timestamp::UNIX_EPOCH);

    year_in_utc(timestamp)
}

fn year_in_utc(timestamp: Timestamp) -> u32 {
    u32::try_from(timestamp.to_zoned(TimeZone::UTC).year()).unwrap_or(1970)
}

#[cfg(test)]
#[path = "websearch_tests.rs"]
mod tests;
