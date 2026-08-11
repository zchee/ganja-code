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
//!   configured instead: [`PROVIDER_ENV`] when it names one, otherwise the
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

use std::time::Duration;

use async_trait::async_trait;
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

/// Seconds in a day, for [`current_year`].
const DAY: u64 = 86_400;

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
        let read = |name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };

        Self {
            exa: read(EXA_KEY_ENV),
            parallel: read(PARALLEL_KEY_ENV),
        }
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
                .replace(YEAR_TOKEN, &current_year().to_string()),
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
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

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
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header(
            reqwest::header::USER_AGENT,
            concat!("ganja/", env!("CARGO_PKG_VERSION")),
        )
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

    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .find_map(payload)
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
/// Hand-rolled over the unreserved set of RFC 3986 rather than reached for:
/// this is the only place in the crate that builds a query, and the alternative
/// is a dependency for one line. Anything outside `A-Za-z0-9-._~` is escaped,
/// which is stricter than a URL needs and cannot be wrong.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }

    out
}

/// The year this process is running in, UTC.
///
/// The same **D24** trade the environment block makes: there is no date
/// library in this workspace, and the year is wanted for one substitution in
/// one prompt. The arithmetic is the civil-from-days walk — the engine's
/// `instruction` module does it too, and cannot be borrowed from here because
/// this crate sits beneath it and must stay there.
///
/// A clock before the epoch yields 1970, which is the year of a machine whose
/// clock is wrong rather than a panic in a tool description.
fn current_year() -> u32 {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let mut days = seconds / DAY;
    let mut year = 1970;

    loop {
        let length = if leap(year) { 366 } else { 365 };
        if days < length {
            return year;
        }
        days -= length;
        year += 1;
    }
}

/// Whether `year` is a leap year in the proleptic Gregorian calendar.
const fn leap(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };
    use tokio_util::sync::CancellationToken;

    use super::{Keys, Service, WebsearchTool};
    use crate::{FileTimes, Tool, ToolCtx, ToolError};

    /// A loopback endpoint answering one connection with canned bytes, and
    /// keeping what it was sent.
    ///
    /// A real socket rather than a mocked client, for `webfetch`'s reason: what
    /// is asserted has to be the request the tool actually built.
    struct Endpoint {
        /// Where the tool should be pointed.
        url: String,
        /// The request the endpoint was sent, headers and body, once it has
        /// had one.
        seen: Arc<std::sync::Mutex<String>>,
        /// Kept so the server outlives the test talking to it.
        _server: tokio::task::JoinHandle<()>,
    }

    impl Endpoint {
        fn seen(&self) -> String {
            self.seen
                .lock()
                .expect("the request log is never poisoned")
                .clone()
        }
    }

    /// Serves `response`, or nothing at all when it is [`None`].
    async fn serve(response: Option<Vec<u8>>) -> Endpoint {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback is bindable");
        let url = format!(
            "http://{}/mcp",
            listener
                .local_addr()
                .expect("a bound socket has an address")
        );
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let log = Arc::clone(&seen);

        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };

            // Headers and body both: the body is what the JSON-RPC assertions
            // read, so the read runs until the declared length has arrived.
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while let Ok(read) = socket.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if complete(&request) {
                    break;
                }
            }
            *log.lock().expect("the request log is never poisoned") =
                String::from_utf8_lossy(&request).into_owned();

            let Some(response) = response else {
                tokio::time::sleep(Duration::from_secs(60)).await;
                return;
            };
            let _ = socket.write_all(&response).await;
            let _ = socket.flush().await;
        });

        Endpoint {
            url,
            seen,
            _server: server,
        }
    }

    /// Whether `request` holds a whole request: the headers, and as many body
    /// bytes as its `content-length` declared.
    fn complete(request: &[u8]) -> bool {
        let text = String::from_utf8_lossy(request);
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            return false;
        };
        let declared = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or_default();

        body.len() >= declared
    }

    /// A 200 carrying `body` as `content_type`.
    fn response(content_type: &str, body: &str) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: {content_type}\r\n\
             content-length: {}\r\n\r\n",
            body.len()
        );
        out.push_str(body);

        out.into_bytes()
    }

    /// A status with no body, for the refusal path.
    fn status(code: u16) -> Vec<u8> {
        format!("HTTP/1.1 {code} Unauthorized\r\nconnection: close\r\ncontent-length: 0\r\n\r\n")
            .into_bytes()
    }

    /// One result, in the shape both services answer in.
    const RESULT: &str =
        r#"{"result":{"content":[{"type":"text","text":"ganja is a rust port"}]}}"#;

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: PathBuf::from("."),
            cancel: CancellationToken::new(),
            call_id: "call-1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: crate::Credentials::Unguarded,
            spawn: None,
            ask: None,
            switch: None,
        }
    }

    /// The tool pointed at `endpoint` for both services, which is what lets a
    /// test choose the service and still be answered on loopback.
    fn tool(endpoint: &Endpoint) -> WebsearchTool {
        WebsearchTool::against(&endpoint.url, &endpoint.url)
    }

    /// One search against `endpoint`, as `service`, with `key`.
    ///
    /// Every socket test goes through this rather than through
    /// [`Tool::run`]: the environment is process-wide, and a tool test that
    /// read the real `EXA_API_KEY` would pass or fail by whose machine it ran
    /// on. What `run` adds on top — reading those variables — is pinned in
    /// `tests/websearch_keys.rs`, which is a binary of its own for exactly
    /// that reason.
    async fn search_with(
        endpoint: &Endpoint,
        service: Service,
        key: &str,
        args: serde_json::Value,
    ) -> Result<crate::ToolOutput, ToolError> {
        search_cancellable(endpoint, service, key, args, &ctx()).await
    }

    /// The same, against a context a test still holds — which is how a cancel
    /// reaches a search that is already in flight.
    async fn search_cancellable(
        endpoint: &Endpoint,
        service: Service,
        key: &str,
        args: serde_json::Value,
        context: &ToolCtx,
    ) -> Result<crate::ToolOutput, ToolError> {
        let args: super::Args = serde_json::from_value(args).expect("the fixture fits the schema");

        tool(endpoint).execute(&args, context, service, key).await
    }

    /// The body half of a recorded request, as JSON.
    fn sent(endpoint: &Endpoint) -> serde_json::Value {
        let seen = endpoint.seen();
        let (_, body) = seen
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("the endpoint saw a whole request: {seen}"));

        serde_json::from_str(body).unwrap_or_else(|error| panic!("{error}: {body}"))
    }

    /// Exa takes its key in the query string and the query in the JSON-RPC
    /// body, and the whole request is asserted — an argument name that drifted
    /// would be a search the service answers differently.
    #[tokio::test]
    async fn an_exa_search_carries_its_key_in_the_query_and_its_arguments_in_the_body() {
        let endpoint = serve(Some(response("application/json", RESULT))).await;

        let out = search_with(
            &endpoint,
            Service::Exa,
            "exa/key one",
            serde_json::json!({ "query": "rust ports", "numResults": 3, "type": "deep",
                                "livecrawl": "preferred", "contextMaxCharacters": 42 }),
        )
        .await
        .expect("the endpoint answers");

        let seen = endpoint.seen();
        assert!(
            seen.starts_with("POST /mcp?exaApiKey=exa%2Fkey%20one HTTP/1.1"),
            "the key travels in the query, percent-encoded: {seen}"
        );
        let lowered = seen.to_lowercase();
        assert!(
            lowered.contains("accept: application/json, text/event-stream"),
            "either content type is acceptable and the request has to say so: {seen}"
        );
        assert!(
            lowered.contains("content-type: application/json"),
            "the body is JSON: {seen}"
        );
        assert!(
            !lowered.contains("authorization:"),
            "exa is authenticated by the query string and nothing else: {seen}"
        );

        assert_eq!(
            sent(&endpoint),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "web_search_exa",
                    "arguments": {
                        "query": "rust ports",
                        "type": "deep",
                        "numResults": 3,
                        "livecrawl": "preferred",
                        "contextMaxCharacters": 42,
                    },
                },
            })
        );
        assert_eq!(out.output, "ganja is a rust port");
        assert_eq!(out.title, "Exa Web Search: rust ports");
        assert_eq!(out.metadata, serde_json::json!({ "provider": "exa" }));
    }

    /// The defaults upstream sends when the model names none of the knobs.
    #[tokio::test]
    async fn a_search_that_names_no_options_sends_upstreams_defaults() {
        let endpoint = serve(Some(response("application/json", RESULT))).await;

        search_with(
            &endpoint,
            Service::Exa,
            "exa-key",
            serde_json::json!({ "query": "ganja" }),
        )
        .await
        .expect("the endpoint answers");

        assert_eq!(
            sent(&endpoint)["params"]["arguments"],
            serde_json::json!({
                "query": "ganja",
                "type": "auto",
                "numResults": 8,
                "livecrawl": "fallback",
            }),
            "and no context budget at all, which upstream leaves to the service"
        );
    }

    /// Parallel takes a bearer token and a different argument shape, and
    /// neither the session id nor the model name upstream sends is invented
    /// here.
    #[tokio::test]
    async fn a_parallel_search_carries_a_bearer_token_and_parallels_own_arguments() {
        let endpoint = serve(Some(response("application/json", RESULT))).await;

        let out = search_with(
            &endpoint,
            Service::Parallel,
            "parallel-key",
            serde_json::json!({ "query": "rust ports" }),
        )
        .await
        .expect("the endpoint answers");

        let seen = endpoint.seen();
        assert!(
            seen.starts_with("POST /mcp HTTP/1.1"),
            "parallel takes no key in the query: {seen}"
        );
        assert!(
            seen.to_lowercase()
                .contains("authorization: bearer parallel-key"),
            "the token travels in a header: {seen}"
        );
        assert_eq!(
            sent(&endpoint)["params"],
            serde_json::json!({
                "name": "web_search",
                "arguments": { "objective": "rust ports", "search_queries": ["rust ports"] },
            })
        );
        assert_eq!(out.title, "Parallel Web Search: rust ports");
        assert_eq!(out.metadata, serde_json::json!({ "provider": "parallel" }));
    }

    /// An event stream is the other thing either service may answer with, and
    /// the answer is in a `data:` line rather than in the body as a whole.
    #[tokio::test]
    async fn an_answer_arriving_as_an_event_stream_is_read_out_of_its_data_lines() {
        let stream = format!(
            ": a comment nothing depends on\nevent: message\ndata: {}\n\ndata: [DONE]\n\n",
            RESULT
        );
        let endpoint = serve(Some(response("text/event-stream", &stream))).await;

        let out = search_with(
            &endpoint,
            Service::Exa,
            "exa-key",
            serde_json::json!({ "query": "ganja" }),
        )
        .await
        .expect("the endpoint answers");

        assert_eq!(out.output, "ganja is a rust port");
    }

    /// The first frame carrying text wins, and a frame that carries an empty
    /// one is not it.
    #[test]
    fn the_first_result_text_that_says_anything_is_the_answer() {
        let empty = r#"{"result":{"content":[{"type":"text","text":""}]}}"#;
        let stream = format!("data: {empty}\n\ndata: {RESULT}\n\n");

        assert_eq!(
            super::parse(&stream).as_deref(),
            Some("ganja is a rust port")
        );
        assert_eq!(super::parse(""), None);
        assert_eq!(super::parse("data: not json\n\n"), None);
        assert_eq!(super::parse(r#"{"result":{"content":[]}}"#), None);
    }

    /// A service that answered with nothing usable is not an error: the model
    /// is told to try another query, which is what upstream tells it.
    #[tokio::test]
    async fn a_search_that_found_nothing_says_so_rather_than_failing() {
        let endpoint = serve(Some(response(
            "application/json",
            r#"{"result":{"content":[]}}"#,
        )))
        .await;

        let out = search_with(
            &endpoint,
            Service::Exa,
            "exa-key",
            serde_json::json!({ "query": "ganja" }),
        )
        .await
        .expect("an empty answer is still an answer");

        assert_eq!(out.output, super::NOTHING_FOUND);
    }

    /// A rejected credential must not read as "nothing found": one is a
    /// question about the account, the other about the query.
    #[tokio::test]
    async fn a_service_that_refuses_the_search_is_reported_as_a_refusal() {
        let endpoint = serve(Some(status(401))).await;

        let refused = search_with(
            &endpoint,
            Service::Exa,
            "exa-key",
            serde_json::json!({ "query": "ganja" }),
        )
        .await
        .expect_err("401 is not an answer");

        assert!(
            matches!(&refused, ToolError::Failed(message)
                if message.contains("Exa Web Search") && message.contains("401")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn a_cancel_ends_a_search_that_is_still_waiting() {
        let endpoint = serve(None).await;
        let context = ctx();
        let cancel = context.cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });

        let refused = search_cancellable(
            &endpoint,
            Service::Exa,
            "exa-key",
            serde_json::json!({ "query": "ganja" }),
            &context,
        )
        .await
        .expect_err("the turn ended before the search did");

        assert!(matches!(refused, ToolError::Cancelled), "got {refused:?}");
    }

    /// Which service a call goes to, over the whole table — including the two
    /// cases the module's divergence note is about.
    #[test]
    fn the_service_is_the_one_named_or_the_one_whose_key_is_held() {
        let both = Keys {
            exa: Some("e".to_owned()),
            parallel: Some("p".to_owned()),
        };
        let neither = Keys::default();
        let only_parallel = Keys {
            exa: None,
            parallel: Some("p".to_owned()),
        };

        let chosen = |named, keys: &Keys| {
            super::select(named, keys).unwrap_or_else(|error| panic!("{named:?}: {error:?}"))
        };

        assert_eq!(chosen(Some("parallel"), &both), Service::Parallel);
        assert_eq!(chosen(Some(" exa "), &only_parallel), Service::Exa);
        // A value that names neither service is ignored, as upstream ignores
        // it, and the keys decide.
        assert_eq!(chosen(Some("bing"), &only_parallel), Service::Parallel);
        assert_eq!(chosen(None, &both), Service::Exa);
        assert_eq!(chosen(None, &only_parallel), Service::Parallel);

        let refused = super::select(None, &neither).expect_err("nothing to search with");
        assert!(
            matches!(&refused, ToolError::Failed(message)
                if message.contains("EXA_API_KEY") && message.contains("PARALLEL_API_KEY")),
            "a refusal names both variables to set: {refused:?}"
        );
    }

    /// Pointed at a service whose key is missing, the tool says which variable
    /// to set rather than sending an unauthenticated request.
    #[test]
    fn a_service_named_without_its_key_is_refused_by_name() {
        let refused = super::missing_key(Service::Parallel);

        assert!(
            matches!(&refused, ToolError::Failed(message)
                if message.contains("parallel") && message.contains("PARALLEL_API_KEY")),
            "got {refused:?}"
        );
    }

    #[test]
    fn the_description_names_the_year_the_session_is_running_in() {
        let tool = WebsearchTool::new();

        assert!(
            !tool.description().contains("{{year}}"),
            "the token is substituted at construction: {}",
            tool.description()
        );
        assert!(
            tool.description()
                .contains(&format!("The current year is {}.", super::current_year())),
            "and substituted with this year: {}",
            tool.description()
        );
        assert!(super::current_year() >= 2026, "the epoch walk is off");
    }

    /// The civil-from-days walk, at the boundaries a leap-year rule gets wrong.
    #[test]
    fn the_year_walk_holds_at_the_leap_boundaries() {
        for (year, expected) in [
            (1970_u32, false),
            (1972, true),
            (1900, false),
            (2000, true),
            (2024, true),
            (2026, false),
            (2100, false),
        ] {
            assert_eq!(super::leap(year), expected, "{year}");
        }
    }

    #[test]
    fn the_prompt_and_schema_are_what_the_model_is_given() {
        let tool = WebsearchTool::new();
        let schema = serde_json::to_value(tool.schema()).expect("a schema is JSON");

        assert_eq!(tool.id(), "websearch");
        assert_eq!(
            tool.describe(&serde_json::json!({ "query": "rust ports" })),
            "search rust ports"
        );
        assert_eq!(schema["required"], serde_json::json!(["query"]));
        // Upstream's spellings, which the golden differential compares as
        // part of the contract.
        for name in [
            "query",
            "numResults",
            "livecrawl",
            "type",
            "contextMaxCharacters",
        ] {
            assert!(
                schema["properties"].get(name).is_some(),
                "the schema should offer {name}: {schema}"
            );
        }
    }

    /// A search is a request to a third party, made on the model's say-so,
    /// carrying a query the model wrote — so it asks, and the rules already
    /// said so before the tool existed
    /// (`ganja-permission`'s `ASK_BY_DEFAULT`). Asserted from here because a
    /// list naming a tool that is not registered proves nothing about the tool
    /// that is.
    #[test]
    fn a_search_asks_before_it_runs() {
        let permissions = ganja_permission::permission::Permissions::default();

        assert_eq!(
            permissions
                .gate(
                    WebsearchTool::new().id(),
                    &serde_json::json!({ "query": "rust ports" })
                )
                .action,
            ganja_permission::permission::Decision::Ask
        );
    }

    #[test]
    fn a_key_is_escaped_where_a_query_string_needs_it() {
        assert_eq!(super::encode("plain-key_1.0~"), "plain-key_1.0~");
        assert_eq!(super::encode("a b&c=d?e/f"), "a%20b%26c%3Dd%3Fe%2Ff");
    }
}
