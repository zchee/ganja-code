use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::{Keys, Service, WebsearchTool};
use crate::{Tool, ToolCtx, ToolError};

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
        self.seen.lock().expect("the request log is never poisoned").clone()
    }
}

/// Serves `response`, or nothing at all when it is [`None`].
async fn serve(response: Option<Vec<u8>>) -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    let url =
        format!("http://{}/mcp", listener.local_addr().expect("a bound socket has an address"));
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

    Endpoint { url, seen, _server: server }
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
const RESULT: &str = r#"{"result":{"content":[{"type":"text","text":"ganja is a rust port"}]}}"#;

fn ctx() -> ToolCtx {
    ToolCtx::fixture(PathBuf::from("."))
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
    assert!(lowered.contains("content-type: application/json"), "the body is JSON: {seen}");
    assert!(
        lowered.contains(&format!("user-agent: ganja-code/{}\r\n", env!("CARGO_PKG_VERSION"))),
        "the request names this build by the project's name — the one \
             spelling every wire ganja speaks in its own voice carries, and \
             never a borrowed one, since no service here involves somebody \
             else's client registration: {seen}"
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

    search_with(&endpoint, Service::Exa, "exa-key", serde_json::json!({ "query": "ganja" }))
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
    assert!(seen.starts_with("POST /mcp HTTP/1.1"), "parallel takes no key in the query: {seen}");
    assert!(
        seen.to_lowercase().contains("authorization: bearer parallel-key"),
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

    let out =
        search_with(&endpoint, Service::Exa, "exa-key", serde_json::json!({ "query": "ganja" }))
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

    assert_eq!(super::parse(&stream).as_deref(), Some("ganja is a rust port"));
    assert_eq!(super::parse(""), None);
    assert_eq!(super::parse("data: not json\n\n"), None);
    assert_eq!(super::parse(r#"{"result":{"content":[]}}"#), None);
}

/// A service that answered with nothing usable is not an error: the model
/// is told to try another query, which is what upstream tells it.
#[tokio::test]
async fn a_search_that_found_nothing_says_so_rather_than_failing() {
    let endpoint = serve(Some(response("application/json", r#"{"result":{"content":[]}}"#))).await;

    let out =
        search_with(&endpoint, Service::Exa, "exa-key", serde_json::json!({ "query": "ganja" }))
            .await
            .expect("an empty answer is still an answer");

    assert_eq!(out.output, super::NOTHING_FOUND);
}

/// A rejected credential must not read as "nothing found": one is a
/// question about the account, the other about the query.
#[tokio::test]
async fn a_service_that_refuses_the_search_is_reported_as_a_refusal() {
    let endpoint = serve(Some(status(401))).await;

    let refused =
        search_with(&endpoint, Service::Exa, "exa-key", serde_json::json!({ "query": "ganja" }))
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
    let both = Keys { exa: Some("e".to_owned()), parallel: Some("p".to_owned()) };
    let neither = Keys::default();
    let only_parallel = Keys { exa: None, parallel: Some("p".to_owned()) };

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
        tool.description().contains(&format!("The current year is {}.", super::current_utc_year())),
        "and substituted with this year: {}",
        tool.description()
    );
    assert!(super::current_utc_year() >= 2026, "the UTC clock is off");
}

/// The UTC year at calendar boundaries the hand-written walk could get
/// wrong.
#[test]
fn the_utc_year_is_pinned_at_calendar_boundaries() {
    for (seconds, expected) in [
        (0_u64, 1970_u32),
        (63_072_000, 1972),
        (946_684_800, 2000),
        (1_709_164_800, 2024),
        (1_786_924_800, 2026),
        (4_107_542_400, 2100),
    ] {
        let timestamp =
            Timestamp::from_second(i64::try_from(seconds).expect("the fixture fits i64"))
                .expect("the fixture is in range");
        assert_eq!(super::year_in_utc(timestamp), expected, "second {seconds}");
    }
}

#[test]
fn the_prompt_and_schema_are_what_the_model_is_given() {
    let tool = WebsearchTool::new();
    let schema = serde_json::to_value(tool.schema()).expect("a schema is JSON");

    assert_eq!(tool.id(), "websearch");
    assert_eq!(tool.describe(&serde_json::json!({ "query": "rust ports" })), "search rust ports");
    assert_eq!(schema["required"], serde_json::json!(["query"]));
    // Upstream's spellings, which the golden differential compares as
    // part of the contract.
    for name in ["query", "numResults", "livecrawl", "type", "contextMaxCharacters"] {
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
            .gate(WebsearchTool::new().id(), &serde_json::json!({ "query": "rust ports" }))
            .action,
        ganja_permission::permission::Decision::Ask
    );
}

#[test]
fn a_key_is_escaped_where_a_query_string_needs_it() {
    assert_eq!(super::encode("plain-key_1.0~"), "plain-key_1.0~");
    assert_eq!(super::encode("a b&c=d?e/f"), "a%20b%26c%3Dd%3Fe%2Ff");
}

/// The exact bytes a query-string value becomes, over the whole repertoire
/// one can carry: the four unreserved marks that stay literal, the
/// reserved characters that do not, and a multi-byte character escaped one
/// UTF-8 byte at a time in upper-case hex.
///
/// Written against the hand encoder before it was retired, so its
/// replacement is held to the escape set the wire already sees rather than
/// to a stock set that merely comes close: `NON_ALPHANUMERIC` alone would
/// escape all four of those marks.
#[test]
fn the_bytes_a_query_string_value_is_escaped_into_are_exactly_these() {
    assert_eq!(
        super::encode("rust-lang async_trait v1.0~rc + tokio&io=x café ✓"),
        "rust-lang%20async_trait%20v1.0~rc%20%2B%20tokio%26io%3Dx%20caf%C3%A9%20%E2%9C%93"
    );
}
