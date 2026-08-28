//! The `webfetch` tool: reads a page and hands the model its text.
//!
//! Spec: upstream `packages/opencode/src/tool/webfetch.ts` and `webfetch.txt`.
//!
//! Unlike the provider client, this one follows redirects. That client refuses
//! them because every provider request carries an API key in a header, and a
//! 3xx is an instruction to hand that header to a host of the server's
//! choosing. Nothing here carries a credential — the request is a bare `GET`
//! with a user agent and an `Accept` — so a redirect costs nothing to follow,
//! and refusing them would break the large fraction of the web that answers
//! `http` with a 301 to `https`.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs as _};
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use markup5ever_rcdom::{Node, NodeData};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolCtx, ToolError, ToolOutput, truncate};

/// Most bytes a response may carry before it is refused. Upstream's 5 MB.
const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024;

/// How long a fetch runs when the call names no timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest timeout a call may ask for; longer requests are clamped to it.
const MAX_TIMEOUT: Duration = Duration::from_secs(120);

/// What the endpoint is told the client is.
///
/// A browser string, as upstream sends: a great many sites answer an obviously
/// automated agent with a challenge page, and the point of the tool is to
/// return what a person looking at the URL would have seen.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

/// Elements whose text belongs to the machine rather than the reader.
const SKIPPED: [&str; 6] = ["script", "style", "noscript", "iframe", "object", "embed"];

/// Elements a plain-text reading puts a blank line around.
///
/// Not the CSS block set, which is a question about rendering boxes: these are
/// the elements that separate *prose*, so a heading does not run into the
/// paragraph under it and one list item does not run into the next. Anything
/// unlisted is treated as inline, which is the right default — a tag a text
/// rendering has never heard of is far more likely to be a `<span>` than a
/// `<section>`. `td`/`th` are absent on purpose: rows separate through `tr`,
/// and cells gluing within a row beats a table exploding into one paragraph
/// per cell.
const BLOCK: [&str; 38] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "button",
    "caption",
    "dd",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "label",
    "legend",
    "li",
    "main",
    "nav",
    "ol",
    "optgroup",
    "option",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "title",
    "tr",
    "ul",
];

/// Most redirects one fetch will follow, which is reqwest's own default. Spelled
/// out because guarding each hop means policing the chain here rather than
/// leaving it to the client.
const MAX_REDIRECTS: usize = 10;

/// How the fetched page should be handed back.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Format {
    /// Tags stripped, text kept.
    Text,
    /// Prose, with the markup rendered as markdown.
    #[default]
    Markdown,
    /// The response body exactly as it arrived.
    Html,
}

/// What the model passes to `webfetch`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The URL to fetch content from
    url: String,
    /// The format to return the content in (text, markdown, or html). Defaults to markdown.
    #[serde(default)]
    format: Format,
    /// Optional timeout in seconds (max 120)
    #[serde(default)]
    timeout: Option<u64>,
}

/// Fetches a URL.
pub struct WebfetchTool {
    /// Whether a URL resolving onto this machine or a private network is
    /// fetched rather than refused. See [`WebfetchTool::allowing_private`].
    allow_private: bool,
}

impl WebfetchTool {
    /// The tool as it ships: an address on this machine or a private network is
    /// refused.
    ///
    /// A deliberate divergence — upstream fetches whatever it is given. The
    /// URL here is one a *model* chose, and a model chooses it after reading
    /// files and pages that a stranger may have written, so "fetch this and
    /// tell me what it says" is a working read of a metadata service, a
    /// database admin port, or a router's console. The provider client already
    /// refuses to speak plainly to anything but loopback for the same class of
    /// reason; this is that judgement applied where the address, and not the
    /// credential, is what matters (deviation:
    /// `webfetch-refuses-private-addresses`).
    #[must_use]
    pub fn new() -> Self {
        Self { allow_private: false }
    }

    /// The tool with that refusal lifted, for a session whose config asked for
    /// it.
    ///
    /// A real need — an intranet wiki, a service on the developer's own
    /// machine — and one nobody can serve from here, because which private
    /// addresses are legitimate is a question only the person running the
    /// session can answer.
    #[must_use]
    pub fn allowing_private() -> Self {
        Self { allow_private: true }
    }
}

impl Default for WebfetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebfetchTool {
    fn id(&self) -> &str {
        "webfetch"
    }

    fn description(&self) -> &str {
        include_str!("webfetch.txt")
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let url = args.get("url").and_then(serde_json::Value::as_str).unwrap_or_default();

        format!("fetch {url}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        // Only the two schemes the tool is for. Anything else — `file`, `data`,
        // a bare host with no scheme at all — is either a way to read the disk
        // through a tool that is gated as a network call, or a typo.
        if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
            return Err(ToolError::InvalidArgs(
                "URL must start with http:// or https://".to_owned(),
            ));
        }

        let timeout = args
            .timeout
            .map_or(DEFAULT_TIMEOUT, |seconds| Duration::from_secs(seconds).min(MAX_TIMEOUT));

        tokio::select! {
            fetched = fetch(&args, timeout, self.allow_private) => fetched,
            () = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
        }
    }
}

/// Whether `address` is one this tool refuses by default.
///
/// The set is the one a request from this process can reach and a request from
/// the internet cannot: loopback (`127.0.0.0/8`, `::1`), the RFC 1918 ranges
/// (`10/8`, `172.16/12`, `192.168/16`) with their IPv6 counterpart
/// (`fc00::/7`), and link-local (`169.254/16`, `fe80::/10`) — which is where
/// every cloud's instance metadata service lives.
///
/// The unspecified addresses are in the set too, though nothing named them:
/// `0.0.0.0` and `::` route to this machine on every stack that matters, so
/// leaving them out would make the loopback line above decorative.
fn blocked(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
        }
        // An address written as `::ffff:127.0.0.1` is the v4 address it wraps,
        // and refusing it as one is the whole point of unwrapping here.
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(mapped) => blocked(IpAddr::V4(mapped)),
            // `is_unique_local` and `is_unicast_link_local` are still unstable,
            // so the two prefixes are matched here rather than paid for with a
            // crate-wide feature gate.
            None => {
                address.is_loopback()
                    || address.is_unspecified()
                    || (address.segments()[0] & 0xfe00) == 0xfc00
                    || (address.segments()[0] & 0xffc0) == 0xfe80
            }
        },
    }
}

/// Resolves `url`'s host and refuses it if any address it answers to is one
/// [`blocked`] names.
///
/// **Every** address is checked rather than the first: a name answering with
/// one public address and one private one is the oldest way around a check
/// that stops at the head of the list.
///
/// Returns what it resolved, so the caller can connect to exactly the
/// addresses it just checked instead of asking again and trusting the second
/// answer.
fn resolved_and_allowed(url: &reqwest::Url) -> Result<Vec<SocketAddr>, ToolError> {
    let host = host_of(url)?;
    // Both the literal-address and the name case: `ToSocketAddrs` parses an
    // address before it consults a resolver, which is why a host written as
    // `10.0.0.1` needs no separate arm.
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ToolError::Failed(format!("{host} names no port to connect to")))?;

    let addresses: Vec<SocketAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|_error| ToolError::Failed(format!("{host} did not resolve")))?
        .collect();

    if addresses.is_empty() {
        return Err(ToolError::Failed(format!("{host} did not resolve")));
    }
    if addresses.iter().any(|address| blocked(address.ip())) {
        return Err(refusal(&host));
    }

    Ok(addresses)
}

/// What the model is told when an address is refused.
///
/// The host and nothing else. A URL a model was handed can carry a token in
/// its path or its query — that is what the provider client's own redaction
/// exists for — and a refusal is not a reason to put one in a transcript.
fn refusal(host: &str) -> ToolError {
    ToolError::Failed(format!(
        "{host} resolves to an address on this machine or a private network, which webfetch \
         does not reach. Set webfetch.allow_private in the config to allow it."
    ))
}

/// `url`'s host, without the brackets a URL writes an IPv6 literal in.
fn host_of(url: &reqwest::Url) -> Result<String, ToolError> {
    let host =
        url.host_str().ok_or_else(|| ToolError::Failed("the URL names no host".to_owned()))?;

    Ok(host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host).to_owned())
}

/// Gets the URL and renders the body in the format the call asked for.
async fn fetch(
    args: &Args,
    timeout: Duration,
    allow_private: bool,
) -> Result<ToolOutput, ToolError> {
    let client = client(&args.url, allow_private).await?;
    let request = client
        .get(&args.url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, accept(args.format))
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9");

    // One deadline over the whole exchange — connect, headers, body and
    // rendering — so neither a dribbling server nor a pathological page can
    // hold the call forever.
    tokio::time::timeout(timeout, async {
        let response = request
            .send()
            .await
            .map_err(|error| ToolError::Failed(format!("the request did not complete: {error}")))?
            .error_for_status()
            .map_err(|error| {
                ToolError::Failed(format!("the endpoint refused the request: {error}"))
            })?;

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = collect(response).await?;
        let mime = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        let title = format!("{} ({content_type})", args.url);

        // The protocol carries no attachments yet, so an image is reported
        // rather than returned; upstream hands the bytes back as a data URL.
        if mime.starts_with("image/") {
            return Ok(ToolOutput {
                title,
                output: format!(
                    "Image fetched successfully ({mime}, {} bytes). This tool cannot hand image \
                     bytes to the model yet.",
                    body.len()
                ),
                metadata: serde_json::json!({ "mime": mime, "bytes": body.len() }),
            });
        }

        // Owned before entering the blocking task because that task may
        // outlive this async stack frame after cancellation or a timeout.
        let content = String::from_utf8_lossy(&body).into_owned();
        let html = mime.contains("text/html");
        let rendered = if html && matches!(args.format, Format::Markdown | Format::Text) {
            let format = args.format;
            tokio::task::spawn_blocking(move || match format {
                // Upstream converts HTML to markdown with turndown; this is
                // `htmd`, which preserves headings, links and lists where the
                // plain-text rendering deliberately keeps the stripper.
                Format::Markdown => to_markdown(&content),
                Format::Text => strip_tags(&content),
                Format::Html => unreachable!("HTML is not sent to the renderer"),
            })
            .await
            .map_err(|error| ToolError::Failed(format!("the page renderer did not run: {error}")))?
        } else {
            content
        };
        let clamped = truncate::clamp(&rendered);

        Ok::<_, ToolError>(ToolOutput {
            title,
            output: clamped.text,
            metadata: serde_json::json!({}),
        })
    })
    .await
    .map_err(|_elapsed| ToolError::Failed("Request timed out".to_owned()))?
}

/// The client one fetch runs through, guarded unless the session lifted it.
///
/// Guarding costs the address check, and then two things that keep the check
/// from being advice:
///
/// - the addresses just checked are pinned onto the client, so the connection
///   goes to what was inspected rather than to whatever a second lookup says
///   a moment later;
/// - every redirect is checked the same way before it is followed, because a
///   hop is a URL somebody else chose, and a *name* that resolves into a
///   private range is what an attacker redirects to — a check that read only
///   the literal in the `Location` header would be one an attacker walks past
///   with a hostname.
///
/// That per-hop check resolves synchronously, inside the policy reqwest calls
/// on its own thread. A blocking call in an async program, and here on
/// purpose: the policy is not an async fn, and the alternative is the
/// literal-only check that does not hold.
///
/// The policy is installed either way, so the guarded path and the lifted one
/// follow redirects through the same code and the same hop cap.
async fn client(url: &str, allow_private: bool) -> Result<reqwest::Client, ToolError> {
    let redirects = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.stop();
        }
        if allow_private {
            return attempt.follow();
        }

        match resolved_and_allowed(attempt.url()) {
            Ok(_) => attempt.follow(),
            Err(refused) => attempt.error(refused),
        }
    });
    let builder = reqwest::Client::builder().redirect(redirects);

    if allow_private {
        return builder
            .build()
            .map_err(|error| ToolError::Failed(format!("no HTTP client: {error}")));
    }

    let parsed = reqwest::Url::parse(url)
        .map_err(|error| ToolError::InvalidArgs(format!("the URL is not a URL: {error}")))?;
    // Off the reactor: resolution blocks, and this one happens before any
    // request is in flight, where a thread is affordable.
    let checked = {
        let parsed = parsed.clone();
        tokio::task::spawn_blocking(move || resolved_and_allowed(&parsed)).await.map_err(
            |error| ToolError::Failed(format!("the address check did not run: {error}")),
        )??
    };

    // Only a name can be pinned; an address written into the URL is already
    // the address that was checked.
    let host = host_of(&parsed)?;
    let builder = if host.parse::<IpAddr>().is_ok() {
        builder
    } else {
        builder.resolve_to_addrs(&host, &checked)
    };

    builder.build().map_err(|error| ToolError::Failed(format!("no HTTP client: {error}")))
}

/// Reads the body, refusing one too big to be worth holding.
///
/// The declared length is checked first so an oversized response costs nothing
/// to refuse, and the body is measured as it streams so one that lies about
/// its length — or declares none at all — is refused at the same boundary
/// rather than after it has been buffered whole.
async fn collect(response: reqwest::Response) -> Result<Vec<u8>, ToolError> {
    let too_large = || ToolError::Failed("Response too large (exceeds 5MB limit)".to_owned());

    if response.content_length().is_some_and(|length| length > MAX_RESPONSE_SIZE as u64) {
        return Err(too_large());
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| ToolError::Failed(format!("the response body stopped: {error}")))?;

        if body.len() + chunk.len() > MAX_RESPONSE_SIZE {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// What the request says it would like back, weighted towards `format`.
fn accept(format: Format) -> &'static str {
    match format {
        Format::Markdown => {
            "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
        }
        Format::Text => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        Format::Html => {
            "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1"
        }
    }
}

/// `html` rendered as markdown.
///
/// The elements in [`SKIPPED`] are dropped whole, as they are by
/// [`strip_tags`]: the converter treats a `<script>` as an ordinary block and
/// would hand its source to the model as prose, which is the one thing both
/// renderings agree must not happen (deviation:
/// webfetch-markdown-skips-the-same-elements-the-stripper-does).
///
/// A conversion that fails falls back to the text rendering. Returning nothing
/// because a renderer gave up would be a worse answer than the page's words in
/// plain text, and the model asked for the page rather than for markdown.
fn to_markdown(html: &str) -> String {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIPPED.to_vec())
        .build()
        .convert(html)
        .unwrap_or_else(|error| {
            tracing::warn!(
                %error,
                "the page would not convert to markdown; handing over its text instead"
            );

            strip_tags(html)
        })
}

/// The text a reader would see in `html`.
///
/// html5ever's parse tree, reached through the same converter [`to_markdown`]
/// already runs on — but walked here rather than rendered. htmd's own
/// rendering cannot be used for a plain-text answer: it markdown-escapes every
/// text node on the way out, so a page saying `src/main_test.rs` would reach
/// the model as `src/main\_test.rs`, and no option it exposes turns that off.
/// Walking the tree the parser already built costs one traversal and keeps a
/// plain-text answer plain, while still getting the full entity table and the
/// tag grammar a hand scanner cannot have — an attribute holding a `>` no
/// longer ends its own tag and spills into the text.
///
/// The elements in [`SKIPPED`] take their contents with them, so a page's
/// scripts and stylesheets do not reach the model as if they were prose, and
/// those in [`BLOCK`] are separated so prose stays readable. Text itself is
/// verbatim: whatever escaping a plain-text reading did would be escaping for
/// a syntax it does not have.
fn strip_tags(html: &str) -> String {
    let mut text = String::with_capacity(html.len() / 2);

    match htmd::HtmlToMarkdown::new().html_to_tree(html) {
        Ok(tree) => {
            push_text(&tree, &mut text);
            drop_tree_iteratively(tree);
        }
        // The parser is handed the whole string at once and has nothing to
        // fail at; the `Result` is the sink API's shape rather than a case
        // that arises. Saying nothing still beats panicking inside a tool
        // call, or handing the model the page's markup as if it were prose.
        Err(error) => tracing::warn!(%error, "the page would not parse"),
    }

    text.trim().to_owned()
}

/// Appends the text under `node`, minus what [`SKIPPED`] drops.
fn push_text(node: &Rc<Node>, out: &mut String) {
    enum Work {
        Enter(Rc<Node>),
        EndBlock,
    }

    // Children go on the stack reversed, so popping visits them in order.
    fn enter_children(work: &mut Vec<Work>, node: &Rc<Node>) {
        work.extend(node.children.borrow().iter().rev().map(|child| Work::Enter(Rc::clone(child))));
    }

    let mut work = vec![Work::Enter(Rc::clone(node))];
    while let Some(item) = work.pop() {
        let Work::Enter(node) = item else {
            end_block(out);
            continue;
        };

        match &node.data {
            NodeData::Text { contents } => out.push_str(&contents.borrow()),
            NodeData::Element { name, .. } => {
                let tag = &*name.local;
                if SKIPPED.contains(&tag) {
                    continue;
                }
                // A line break is one line break, not the blank line a block
                // earns; markup that lays out an address or a verse with `<br>`
                // would otherwise come back double-spaced.
                if tag == "br" {
                    out.push('\n');
                    continue;
                }

                if BLOCK.contains(&tag) {
                    end_block(out);
                    work.push(Work::EndBlock);
                }
                enter_children(&mut work, &node);
            }
            // A document, a doctype, a comment, a processing instruction:
            // nothing a reader sees, though a document's children are the page.
            _ => enter_children(&mut work, &node),
        }
    }
}

/// Detaches every owning tree edge before each node is dropped, so teardown
/// consumes heap worklist space rather than one call stack frame per level.
///
/// This cannot cover [`to_markdown`], whose converter builds and drops its tree
/// entirely inside `convert`, beyond this module's reach.
fn drop_tree_iteratively(root: Rc<Node>) {
    let mut work = vec![root];
    while let Some(node) = work.pop() {
        work.extend(std::mem::take(&mut *node.children.borrow_mut()));
        if let NodeData::Element { template_contents, .. } = &node.data
            && let Some(contents) = template_contents.borrow_mut().take()
        {
            work.push(contents);
        }
    }
}

/// Closes the line the text is on, so what follows starts a block of its own.
///
/// Whatever whitespace is already at the end goes with it: the newline and
/// indentation between two tags belong to the markup's layout, not to the
/// reader's.
fn end_block(out: &mut String) {
    while out.ends_with([' ', '\t', '\n', '\r']) {
        out.pop();
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
}

#[cfg(test)]
#[path = "webfetch_tests.rs"]
mod tests;
