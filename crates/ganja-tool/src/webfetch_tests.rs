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

/// Titles and rows remain blocks while cells within one row remain inline.
#[test]
fn a_title_and_table_keep_their_intended_plain_text_boundaries() {
    let text = super::strip_tags(
        "<html><head><title>Page title</title></head><body><table>\
             <tr><td>A</td><td>B</td></tr><tr><td>C</td><td>D</td></tr>\
             </table></body></html>",
    );

    assert_eq!(text, "Page title\n\nAB\n\nCD");
}

/// A passing result proves both the text walk and the tree's teardown
/// return without consuming one call stack frame per nesting level.
#[test]
fn deeply_nested_inline_elements_do_not_overflow_the_stack() {
    let html = format!("{}text{}", "<i>".repeat(100_000), "</i>".repeat(100_000));

    assert_eq!(super::strip_tags(&html), "text");
}

/// The 100,000-level fixture completes in a real thread with a 2 MiB stack.
#[test]
fn deeply_nested_inline_elements_fit_a_two_mebibyte_thread_stack() {
    let rendered = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let html = format!("{}text{}", "<i>".repeat(100_000), "</i>".repeat(100_000));

            super::strip_tags(&html)
        })
        .expect("the bounded-stack webfetch test thread starts")
        .join()
        .expect("the bounded-stack webfetch test thread returns");

    assert_eq!(rendered, "text");
}

/// A deep template-contents chain is detached without rendering its text.
#[test]
fn deeply_nested_template_contents_do_not_overflow_the_stack() {
    const DEPTH: usize = 10_000;
    let html = format!(
        "{}text{}",
        "<template>".repeat(DEPTH),
        "</template>".repeat(DEPTH)
    );

    assert_eq!(super::strip_tags(&html), "");
}
