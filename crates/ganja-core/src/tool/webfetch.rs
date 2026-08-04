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

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::{Tool, ToolCtx, ToolError, ToolOutput, truncate};

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
pub struct WebfetchTool;

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
            fetched = fetch(&args, timeout) => fetched,
            () = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
        }
    }
}

/// Gets the URL and renders the body in the format the call asked for.
async fn fetch(args: &Args, timeout: Duration) -> Result<ToolOutput, ToolError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|error| ToolError::Failed(format!("no HTTP client: {error}")))?;
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
        // Upstream converts HTML to markdown with turndown. Nothing here does
        // yet, so an HTML body comes back as its readable text rather than as
        // markup no reader would want.
        Format::Markdown | Format::Text if html => strip_tags(&content),
        Format::Markdown | Format::Text | Format::Html => content.into_owned(),
    };
    let clamped = truncate::clamp(&rendered);

    Ok(ToolOutput {
        title,
        output: clamped.text,
        metadata: serde_json::json!({}),
    })
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

/// The text a reader would see in `html`.
///
/// Markup is dropped and the elements in [`SKIPPED`] take their contents with
/// them, so a page's scripts and stylesheets do not reach the model as if they
/// were prose. Text is joined exactly as upstream's parser joins it, without
/// separators of its own, so the page's own whitespace is what separates
/// words.
fn strip_tags(html: &str) -> String {
    let mut text = String::with_capacity(html.len() / 2);
    let mut rest = html;

    while let Some(open) = rest.find('<') {
        text.push_str(&rest[..open]);
        rest = &rest[open + 1..];

        let Some(close) = rest.find('>') else {
            // An unterminated tag is the end of the document as far as any
            // parser is concerned; there is no text left to recover.
            return decode_entities(text.trim());
        };
        let tag = &rest[..close];
        rest = &rest[close + 1..];

        if tag.starts_with('/') || tag.ends_with('/') {
            continue;
        }
        if SKIPPED.contains(&element_name(tag).as_str()) {
            rest = skip_element(rest, &element_name(tag));
        }
    }
    text.push_str(rest);

    decode_entities(text.trim())
}

/// The lowercased element name an open or close tag carries.
fn element_name(tag: &str) -> String {
    tag.trim_start_matches('/')
        .split(|character: char| character.is_whitespace() || character == '/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// What follows the end tag for `name`.
///
/// Inside a script or a stylesheet the markup rules do not apply — `if (1 < 2)`
/// is arithmetic, not a tag — so nothing but the element's own end tag can end
/// it. Parsing that content as markup is how a stray `<` swallows the rest of
/// the document.
fn skip_element<'a>(html: &'a str, name: &str) -> &'a str {
    let mut rest = html;

    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];

        let Some(tail) = rest.strip_prefix('/') else {
            continue;
        };
        let ends_it = tail
            .get(..name.len())
            .is_some_and(|found| found.eq_ignore_ascii_case(name))
            && tail[name.len()..]
                .starts_with(|character: char| character.is_whitespace() || character == '>');
        if !ends_it {
            continue;
        }

        // Past the end tag, or nowhere: an unterminated one takes the rest of
        // the document with it, exactly as a browser's parser would.
        return tail.find('>').map_or("", |close| &tail[close + 1..]);
    }

    ""
}

/// The characters `text`'s entity references stand for.
///
/// The named references are the handful that appear in ordinary prose; the
/// full HTML table is thousands of entries and belongs to a parser rather than
/// to this. A reference this does not know is left as it was written, which is
/// what a reader would see in a plain-text rendering anyway.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];

        // A reference is short; anything longer is an ampersand in prose.
        let end = rest[1..]
            .char_indices()
            .take(10)
            .find(|(_, character)| *character == ';')
            .map(|(offset, _)| offset + 1);

        // Anything that does not decode is an ampersand somebody wrote as an
        // ampersand. Only that character is consumed, so the scan resumes
        // inside what looked like a reference rather than past it — `a & b
        // &amp; c` still gets its real reference decoded.
        match end.and_then(|end| decode_entity(&rest[1..end]).map(|decoded| (decoded, end))) {
            Some((decoded, end)) => {
                out.push(decoded);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);

    out
}

/// The character `reference` names, without its `&` and `;`.
fn decode_entity(reference: &str) -> Option<char> {
    match reference {
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" | "#39" => return Some('\''),
        "nbsp" => return Some('\u{a0}'),
        _ => {}
    }

    let digits = reference.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };

    char::from_u32(code)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };
    use tokio_util::sync::CancellationToken;

    use super::{MAX_RESPONSE_SIZE, WebfetchTool};
    use crate::tool::{FileTimes, Tool, ToolCtx, ToolError};

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

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: PathBuf::from("."),
            cancel: CancellationToken::new(),
            call_id: "call-1".to_owned(),
            files: Arc::new(FileTimes::default()),
            spawn: None,
        }
    }

    const PAGE: &str = "<html><head><title>t</title><style>body{color:red}</style>\
                        <script>var x = 1 < 2;</script></head>\
                        <body><h1>Ganja </h1><p>ports &amp; tests</p></body></html>";

    #[tokio::test]
    async fn an_html_page_asked_for_as_text_comes_back_without_its_markup() {
        let endpoint = serve(Some(response("text/html; charset=utf-8", PAGE))).await;

        let out = WebfetchTool
            .run(
                serde_json::json!({ "url": endpoint.url, "format": "text" }),
                &ctx(),
            )
            .await
            .expect("the endpoint answers");

        assert_eq!(out.output, "tGanja ports & tests");
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

        WebfetchTool
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

    #[tokio::test]
    async fn a_body_that_is_not_html_is_handed_over_as_it_arrived() {
        let endpoint = serve(Some(response("text/plain", "plain <b>text</b>"))).await;

        let out = WebfetchTool
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

        let out = WebfetchTool
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

        let refused = WebfetchTool
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
        let refused = WebfetchTool
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

        let refused = WebfetchTool
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
            let refused = match WebfetchTool
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
        let refused = WebfetchTool
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
            WebfetchTool.describe(&serde_json::json!({ "url": "https://example.com/a" })),
            "fetch https://example.com/a"
        );
    }

    #[test]
    fn the_prompt_and_schema_are_what_the_model_is_given() {
        let schema = serde_json::to_value(WebfetchTool.schema()).expect("a schema is JSON");

        assert_eq!(WebfetchTool.id(), "webfetch");
        assert!(
            WebfetchTool
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
            "beforeafter",
            "`i < n` inside a script is arithmetic, not the start of a tag"
        );
        assert_eq!(
            super::strip_tags("<p>kept</p><script>never closed"),
            "kept",
            "an unterminated script takes the rest of the document, as a browser's parser does"
        );
    }
}
