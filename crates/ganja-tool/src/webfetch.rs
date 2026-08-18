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

use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs as _},
    rc::Rc,
    time::Duration,
};

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
/// `<section>`.
const BLOCK: [&str; 32] = [
    "address",
    "article",
    "aside",
    "blockquote",
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
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "td",
    "th",
    "tr",
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
        Self {
            allow_private: false,
        }
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
        Self {
            allow_private: true,
        }
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
        let url = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

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

        let timeout = args.timeout.map_or(DEFAULT_TIMEOUT, |seconds| {
            Duration::from_secs(seconds).min(MAX_TIMEOUT)
        });

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
    let host = url
        .host_str()
        .ok_or_else(|| ToolError::Failed("the URL names no host".to_owned()))?;

    Ok(host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .to_owned())
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

    // One deadline over the whole exchange — connect, headers and body — so a
    // server that answers instantly and then dribbles the body forever is
    // still bounded.
    let response = tokio::time::timeout(timeout, async {
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

        Ok::<_, ToolError>((content_type, body))
    })
    .await
    .map_err(|_elapsed| ToolError::Failed("Request timed out".to_owned()))??;

    let (content_type, body) = response;
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let title = format!("{} ({content_type})", args.url);

    // The protocol carries no attachments yet, so an image is reported rather
    // than returned; upstream hands the bytes back as a data URL.
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

    // Lossy, as upstream's `TextDecoder` is: a page that declares one encoding
    // and serves another is common, and mangling a few characters beats
    // failing the call.
    let content = String::from_utf8_lossy(&body);
    let html = mime.contains("text/html");
    let rendered = match args.format {
        // Upstream converts HTML to markdown with turndown; this is `htmd`,
        // which is the same conversion by a different implementation — the
        // headings, links and lists survive as markdown instead of being
        // flattened into the text a stripper leaves behind. A text rendering
        // is still a text rendering: `Format::Text` keeps the stripper.
        Format::Markdown if html => to_markdown(&content),
        Format::Text if html => strip_tags(&content),
        Format::Markdown | Format::Text | Format::Html => content.into_owned(),
    };
    let clamped = truncate::clamp(&rendered);

    Ok(ToolOutput {
        title,
        output: clamped.text,
        metadata: serde_json::json!({}),
    })
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
        tokio::task::spawn_blocking(move || resolved_and_allowed(&parsed))
            .await
            .map_err(|error| {
                ToolError::Failed(format!("the address check did not run: {error}"))
            })??
    };

    // Only a name can be pinned; an address written into the URL is already
    // the address that was checked.
    let host = host_of(&parsed)?;
    let builder = if host.parse::<IpAddr>().is_ok() {
        builder
    } else {
        builder.resolve_to_addrs(&host, &checked)
    };

    builder
        .build()
        .map_err(|error| ToolError::Failed(format!("no HTTP client: {error}")))
}

/// Reads the body, refusing one too big to be worth holding.
///
/// The declared length is checked first so an oversized response costs nothing
/// to refuse, and the body is measured as it streams so one that lies about
/// its length — or declares none at all — is refused at the same boundary
/// rather than after it has been buffered whole.
async fn collect(response: reqwest::Response) -> Result<Vec<u8>, ToolError> {
    let too_large = || ToolError::Failed("Response too large (exceeds 5MB limit)".to_owned());

    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_SIZE as u64)
    {
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
        Ok(tree) => push_text(&tree, &mut text),
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
    match &node.data {
        NodeData::Text { contents } => out.push_str(&contents.borrow()),
        NodeData::Element { name, .. } => {
            let tag = &*name.local;
            if SKIPPED.contains(&tag) {
                return;
            }
            // A line break is one line break, not the blank line a block
            // earns; markup that lays out an address or a verse with `<br>`
            // would otherwise come back double-spaced.
            if tag == "br" {
                out.push('\n');
                return;
            }

            let block = BLOCK.contains(&tag);
            if block {
                end_block(out);
            }
            for child in node.children.borrow().iter() {
                push_text(child, out);
            }
            if block {
                end_block(out);
            }
        }
        // A document, a doctype, a comment, a processing instruction: nothing
        // a reader sees, though a document's children are the page.
        _ => {
            for child in node.children.borrow().iter() {
                push_text(child, out);
            }
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
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    use super::{MAX_RESPONSE_SIZE, WebfetchTool};
    use crate::{Tool, ToolCtx, ToolError};

    /// A loopback endpoint answering one connection with canned bytes.
    ///
    /// Served over a real socket rather than through a mock, so the request
    /// that is asserted on is the one the tool actually built and sent.
    struct Endpoint {
        /// Where the tool should be pointed.
        url: String,
        /// The request the endpoint was sent, once it has had one.
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

    /// Serves `response`, or nothing at all when it is [`None`], which is how
    /// a server that accepts and then goes quiet is spelled.
    async fn serve(response: Option<Vec<u8>>) -> Endpoint {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback is bindable");
        let url = format!(
            "http://{}",
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

            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while let Ok(read) = socket.read(&mut chunk).await {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            *log.lock().expect("the request log is never poisoned") =
                String::from_utf8_lossy(&request).into_owned();

            let Some(response) = response else {
                // Held open and never answered, so the caller's own deadline is
                // the only thing that can end the exchange.
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

    /// A 302 pointing at `url`.
    fn redirect_to(url: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 302 Found\r\nconnection: close\r\nlocation: {url}\r\ncontent-length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn ctx() -> ToolCtx {
        ToolCtx::fixture(PathBuf::from("."))
    }

    const PAGE: &str = "<html><head><title>t</title><style>body{color:red}</style>\
                        <script>var x = 1 < 2;</script></head>\
                        <body><h1>Ganja </h1><p>ports &amp; tests</p></body></html>";

    /// Every test below that actually fetches something fetches it over
    /// loopback, and loopback is one of the addresses the shipped tool refuses
    /// — so each of them says `allowing_private` out loud. That is not a
    /// convenience: it is the guard being live in every one of them. The tests
    /// that ask the tool what it *is*, rather than fetching anything, use
    /// `new` because that is the tool a session gets.
    #[tokio::test]
    async fn a_url_on_this_machine_or_a_private_network_is_refused_before_anything_is_opened() {
        // Nothing here is listening, and nothing needs to be: a refusal that
        // opened a socket first would not be this refusal.
        let refused = [
            ("http://127.0.0.1/", "loopback"),
            ("http://10.1.2.3/", "an RFC 1918 ten"),
            ("http://172.16.0.1/", "an RFC 1918 172"),
            ("http://192.168.1.1/", "an RFC 1918 192"),
            ("http://169.254.169.254/latest/meta-data/", "link-local"),
            ("http://0.0.0.0/", "the unspecified address"),
            ("http://[::1]/", "loopback, written as v6"),
            ("http://[::ffff:127.0.0.1]/", "loopback, wrapped in a v6"),
            ("http://[fd00::1]/", "a unique-local v6"),
            ("http://[fe80::1]/", "a link-local v6"),
        ];

        for (url, what) in refused {
            let error = WebfetchTool::new()
                .run(serde_json::json!({ "url": url }), &ctx())
                .await
                .expect_err(&format!("{what} is refused: {url}"));

            let ToolError::Failed(message) = &error else {
                panic!("{what} should be refused as a failure: {error:?}");
            };
            assert!(
                message.contains("private network"),
                "{what} should say why: {message}"
            );
            assert!(
                !message.contains("latest/meta-data"),
                "a refusal names the host and not the whole URL, which can carry a \
                 credential in its path or query: {message}"
            );
        }
    }

    /// The same URL, refused and then fetched — because the only thing that
    /// changed is the session saying it wanted this.
    #[tokio::test]
    async fn the_same_private_url_is_fetched_once_the_session_allows_it() {
        let endpoint = serve(Some(response("text/plain", "an intranet page"))).await;

        let refused = WebfetchTool::new()
            .run(serde_json::json!({ "url": endpoint.url }), &ctx())
            .await
            .expect_err("loopback is refused by the tool as it ships");
        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("private network")),
            "got {refused:?}"
        );
        assert!(
            endpoint.seen().is_empty(),
            "and refused without connecting: {}",
            endpoint.seen()
        );

        let out = WebfetchTool::allowing_private()
            .run(serde_json::json!({ "url": endpoint.url }), &ctx())
            .await
            .expect("the endpoint answers a session that asked for this");
        assert_eq!(out.output, "an intranet page");
    }

    /// A public address is what the tool is *for*, and the check has to let it
    /// through — including the addresses that sit just outside each blocked
    /// range, which is where an off-by-one in a mask would show.
    ///
    /// Written against literals so nothing here consults a resolver or opens a
    /// connection.
    #[test]
    fn a_public_address_is_not_what_the_guard_refuses() {
        let allowed = [
            ("http://8.8.8.8/", "an ordinary public v4"),
            ("http://9.255.255.255/", "just below the ten"),
            ("http://11.0.0.0/", "just above the ten"),
            ("http://172.15.255.255/", "just below the 172 range"),
            ("http://172.32.0.1/", "just above the 172 range"),
            ("http://192.167.255.255/", "just below the 192 range"),
            ("http://192.169.0.1/", "just above the 192 range"),
            ("http://169.253.0.1/", "just below link-local"),
            ("http://[2001:4860:4860::8888]/", "a public v6"),
            ("http://[fe00::1]/", "just below the unique-local prefix"),
            ("http://[fec0::1]/", "just above the link-local prefix"),
        ];

        for (url, what) in allowed {
            let parsed = reqwest::Url::parse(url).expect("the fixture is a URL");
            super::resolved_and_allowed(&parsed)
                .unwrap_or_else(|error| panic!("{what} should be allowed: {url}: {error:?}"));
        }
    }

    /// A redirect is a URL somebody else chose, so the policy runs the same
    /// check on it that the first hop got.
    ///
    /// This pins the check the policy applies. What it does not reach is a live
    /// redirect *into* a private range while the guard is on: getting there
    /// needs a first hop on a public address, which means a listener on one,
    /// which a hermetic test cannot have. The follow path is covered by the
    /// test below.
    #[test]
    fn a_redirect_target_on_a_private_network_is_refused_by_the_check_the_policy_applies() {
        let hop =
            reqwest::Url::parse("http://169.254.169.254/latest/meta-data/").expect("a URL parses");
        let refused = super::resolved_and_allowed(&hop).expect_err("a hop into link-local");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("private network")),
            "got {refused:?}"
        );
    }

    /// And the policy that does the refusing still follows the redirects it
    /// should — the whole web answers `http` with a 301 to `https`, and a guard
    /// that broke that would be worse than no guard.
    #[tokio::test]
    async fn a_redirect_is_followed_to_the_page_it_names() {
        let target = serve(Some(response("text/plain", "arrived"))).await;
        let hop = serve(Some(redirect_to(&target.url))).await;

        let out = WebfetchTool::allowing_private()
            .run(serde_json::json!({ "url": hop.url }), &ctx())
            .await
            .expect("the hop and its target both answer");

        assert_eq!(out.output, "arrived");
        assert!(
            target.seen().starts_with("GET /"),
            "the second endpoint is the one that served the body: {}",
            target.seen()
        );
    }

    #[tokio::test]
    async fn an_html_page_asked_for_as_text_comes_back_without_its_markup() {
        let endpoint = serve(Some(response("text/html; charset=utf-8", PAGE))).await;

        let out = WebfetchTool::allowing_private()
            .run(
                serde_json::json!({ "url": endpoint.url, "format": "text" }),
                &ctx(),
            )
            .await
            .expect("the endpoint answers");

        // The title, the heading and the paragraph are three blocks, so they
        // are three blocks here; the stripper this replaced ran them into one
        // line because it had no idea which tags were which.
        assert_eq!(out.output, "t\n\nGanja\n\nports & tests");
        assert!(
            !out.output.contains("color:red") && !out.output.contains("var x"),
            "a stylesheet and a script are not prose: {:?}",
            out.output
        );
        assert!(
            out.title.contains("text/html"),
            "the title names what was served: {}",
            out.title
        );
    }

    #[tokio::test]
    async fn the_request_says_who_it_is_and_what_it_would_like_back() {
        let endpoint = serve(Some(response("text/plain", "hi"))).await;

        WebfetchTool::allowing_private()
            .run(serde_json::json!({ "url": endpoint.url }), &ctx())
            .await
            .expect("the endpoint answers");

        let seen = endpoint.seen().to_lowercase();
        assert!(
            seen.contains("user-agent: mozilla/5.0"),
            "a bare agent gets a challenge page instead of the content: {seen}"
        );
        assert!(
            seen.contains("text/markdown;q=1.0"),
            "markdown is the default, and the request should say so: {seen}"
        );
    }

    /// A page with the constructs a stripper cannot represent: a heading is a
    /// line of text to it, and a link's target is not text at all.
    const STRUCTURED: &str = "<html><body><h1>Ganja</h1><p>See \
                              <a href=\"https://example.com/docs\">the docs</a>.</p>\
                              <ul><li>one</li><li>two</li></ul>\
                              <style>body{color:red}</style>\
                              <script>var x = 1 < 2;</script></body></html>";

    /// **R17.** The markdown format is a markdown rendering, which is what
    /// upstream's turndown call produces and what the tag stripper standing in
    /// for it could not.
    #[tokio::test]
    async fn an_html_page_asked_for_as_markdown_comes_back_as_markdown() {
        let endpoint = serve(Some(response("text/html; charset=utf-8", STRUCTURED))).await;

        let out = WebfetchTool::allowing_private()
            .run(
                serde_json::json!({ "url": endpoint.url, "format": "markdown" }),
                &ctx(),
            )
            .await
            .expect("the endpoint answers");

        assert!(
            out.output.contains("# Ganja"),
            "a heading should be a heading: {:?}",
            out.output
        );
        assert!(
            out.output.contains("[the docs](https://example.com/docs)"),
            "a link should keep the target a reader would follow: {:?}",
            out.output
        );
        // Asserted by shape rather than by exact marker and spacing, which are
        // the converter's style options and not the claim being made.
        let bulleted: Vec<&str> = out
            .output
            .lines()
            .filter(|line| line.trim_start().starts_with(['-', '*', '+']))
            .collect();
        assert!(
            bulleted.len() == 2 && bulleted[0].contains("one") && bulleted[1].contains("two"),
            "a list should be a list: {:?}",
            out.output
        );

        // The other half of the claim: none of the above is something the
        // stripper this replaced could ever have emitted, so the assertions
        // are about the conversion and not about the page.
        let stripped = super::strip_tags(STRUCTURED);
        assert!(
            !stripped.contains("# Ganja")
                && !stripped.contains("example.com/docs")
                && !stripped
                    .lines()
                    .any(|line| line.trim_start().starts_with(['-', '*', '+'])),
            "the stripper has no markdown to lose: {stripped:?}"
        );
    }

    #[tokio::test]
    async fn neither_rendering_hands_the_model_a_script_or_a_stylesheet() {
        for format in ["markdown", "text"] {
            let endpoint = serve(Some(response("text/html", STRUCTURED))).await;

            let out = WebfetchTool::allowing_private()
                .run(
                    serde_json::json!({ "url": endpoint.url, "format": format }),
                    &ctx(),
                )
                .await
                .expect("the endpoint answers");

            assert!(
                !out.output.contains("color:red") && !out.output.contains("var x"),
                "{format} handed over machinery as prose: {:?}",
                out.output
            );
        }
    }

    #[tokio::test]
    async fn a_body_that_is_not_html_is_handed_over_as_it_arrived() {
        let endpoint = serve(Some(response("text/plain", "plain <b>text</b>"))).await;

        let out = WebfetchTool::allowing_private()
            .run(
                serde_json::json!({ "url": endpoint.url, "format": "markdown" }),
                &ctx(),
            )
            .await
            .expect("the endpoint answers");

        assert_eq!(out.output, "plain <b>text</b>");
    }

    #[tokio::test]
    async fn html_asked_for_as_html_keeps_its_markup() {
        let endpoint = serve(Some(response("text/html", PAGE))).await;

        let out = WebfetchTool::allowing_private()
            .run(
                serde_json::json!({ "url": endpoint.url, "format": "html" }),
                &ctx(),
            )
            .await
            .expect("the endpoint answers");

        assert_eq!(out.output, PAGE);
    }

    #[tokio::test]
    async fn a_response_over_the_size_cap_is_refused() {
        // Declares a length nobody would want to buffer, so the refusal lands
        // before the body is read at all.
        let oversized = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n",
            MAX_RESPONSE_SIZE + 1
        );
        let endpoint = serve(Some(oversized.into_bytes())).await;

        let refused = WebfetchTool::allowing_private()
            .run(serde_json::json!({ "url": endpoint.url }), &ctx())
            .await
            .expect_err("5MB is the limit");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("exceeds 5MB limit")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn an_endpoint_that_never_answers_ends_at_the_timeout() {
        let endpoint = serve(None).await;

        let started = std::time::Instant::now();
        let refused = WebfetchTool::allowing_private()
            .run(
                serde_json::json!({ "url": endpoint.url, "timeout": 1 }),
                &ctx(),
            )
            .await
            .expect_err("nothing ever came back");
        let elapsed = started.elapsed();

        assert!(
            matches!(&refused, ToolError::Failed(message) if message == "Request timed out"),
            "got {refused:?}"
        );
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(5),
            "the deadline should be the thing that ended it, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_cancel_ends_a_fetch_that_is_still_waiting() {
        let endpoint = serve(None).await;
        let context = ctx();
        let cancel = context.cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });

        let refused = WebfetchTool::allowing_private()
            .run(
                serde_json::json!({ "url": endpoint.url, "timeout": 120 }),
                &context,
            )
            .await
            .expect_err("the turn ended before the page did");

        assert!(matches!(refused, ToolError::Cancelled), "got {refused:?}");
    }

    #[tokio::test]
    async fn a_scheme_that_is_not_http_is_refused_before_anything_is_opened() {
        for url in [
            "file:///etc/passwd",
            "data:text/html,<b>hi</b>",
            "ftp://example.com/x",
            "example.com",
        ] {
            let refused = match WebfetchTool::allowing_private()
                .run(serde_json::json!({ "url": url }), &ctx())
                .await
            {
                Err(refused) => refused,
                Ok(out) => panic!("{url} was fetched rather than refused: {out:?}"),
            };

            assert!(
                matches!(&refused, ToolError::InvalidArgs(message) if message.contains("http://")),
                "{url} got {refused:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_call_without_a_url_is_refused() {
        let refused = WebfetchTool::allowing_private()
            .run(serde_json::json!({}), &ctx())
            .await
            .expect_err("there is nothing to fetch");

        assert!(
            matches!(refused, ToolError::InvalidArgs(_)),
            "got {refused:?}"
        );
    }

    #[test]
    fn the_one_line_description_names_the_url() {
        assert_eq!(
            WebfetchTool::new().describe(&serde_json::json!({ "url": "https://example.com/a" })),
            "fetch https://example.com/a"
        );
    }

    #[test]
    fn the_prompt_and_schema_are_what_the_model_is_given() {
        let schema = serde_json::to_value(WebfetchTool::new().schema()).expect("a schema is JSON");

        assert_eq!(WebfetchTool::new().id(), "webfetch");
        assert!(
            WebfetchTool::new()
                .description()
                .contains("Fetches content from a specified URL")
        );
        assert_eq!(schema["required"], serde_json::json!(["url"]));
        assert!(
            schema.to_string().contains("markdown"),
            "the schema should spell out the formats it accepts: {schema}"
        );
    }

    #[test]
    fn entity_references_survive_the_stripper() {
        assert_eq!(
            super::strip_tags("<p>a &amp; b &lt;c&gt; &#39;d&#39; &x; &#x41;</p>"),
            "a & b <c> 'd' &x; A"
        );
        assert_eq!(
            super::strip_tags("<p>Tom & Jerry &amp; friends</p>"),
            "Tom & Jerry & friends",
            "a bare ampersand must not swallow the reference that follows it"
        );
    }

    #[test]
    fn a_script_holding_markup_characters_does_not_swallow_the_page() {
        assert_eq!(
            super::strip_tags(
                "<p>before</p><script>for (i = 0; i < n; i++) { a = '</div>' }</script><p>after</p>"
            ),
            "before\n\nafter",
            "`i < n` inside a script is arithmetic, not the start of a tag"
        );
        assert_eq!(
            super::strip_tags("<p>kept</p><script>never closed"),
            "kept",
            "an unterminated script takes the rest of the document, as a browser's parser does"
        );
    }

    /// The reason the hand-rolled entity table went: it held six names, and
    /// the two here are ordinary typography that any prose page carries.
    #[test]
    fn a_named_entity_outside_the_handful_still_decodes() {
        assert_eq!(
            super::strip_tags("<p>He said &mdash; &rsquo;yes&rsquo;</p>"),
            "He said — ’yes’"
        );
    }

    /// A hand scanner that ends a tag at the first `>` ends this one inside
    /// the attribute and hands the rest of it over as prose — `b">text`.
    /// Knowing where a tag ends is the parser's job, and now it does it.
    #[test]
    fn a_greater_than_inside_an_attribute_does_not_end_its_own_tag() {
        assert_eq!(super::strip_tags("<p title=\"a > b\">text</p>"), "text");
    }

    /// Text is text. htmd's own rendering would reach the model as
    /// `src/main\_test.rs` and `\[1\]` and `\*why\*`, because it escapes
    /// every text node for a markdown syntax a plain-text answer does not
    /// have; that is why this path walks the tree instead of rendering it.
    #[test]
    fn markdown_punctuation_in_prose_is_handed_over_unescaped() {
        let text = super::strip_tags(
            "<p>Run cargo build --release, edit src/main_test.rs, \
             and see the note [1] about *why*.</p>",
        );

        assert_eq!(
            text,
            "Run cargo build --release, edit src/main_test.rs, and see the note [1] about *why*."
        );
        assert!(
            !text.contains('\\'),
            "a plain-text reading has no syntax to escape for: {text:?}"
        );
    }
}
