//! The HTTP providers against a real socket.
//!
//! Everything here serves canned bytes over loopback rather than mocking the
//! client, so a test exercises the request that is actually built, the retry
//! that is actually scheduled, and the body that is actually split into frames.
//! The response is written in small pieces so that a frame boundary falls
//! wherever the kernel happens to put it, which is the condition the splitter
//! exists to survive.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::StreamExt as _;
use ganja_core::{
    Engine,
    protocol::{Command, Event, FinishReason},
    provider::{
        AnthropicProvider, ChatRequest, OpenAiProvider, Provider, ProviderError, ProviderEvent,
    },
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

/// The credential every test authenticates with. A key that never leaves the
/// machine still must never appear in an error or a log, and grepping the test
/// output for this string is how that is checked.
const CANARY: &str = "sk-test-canary-XYZ";

/// Bytes written to the socket at a time. Small enough that every fixture
/// frame is split across at least one write.
const PIECE: usize = 48;

/// A loopback endpoint that answers each connection with the next canned
/// response and then closes it.
struct Endpoint {
    /// Base URL a provider should be pointed at.
    url: String,
    /// Kept so the server outlives the test that is talking to it.
    _server: tokio::task::JoinHandle<()>,
}

/// Serves `responses`, one per connection, pausing `pace` between writes.
async fn serve(responses: Vec<Vec<u8>>, pace: Duration) -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );

    let server = tokio::spawn(async move {
        for response in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };

            // Draining the request first is what keeps the client from seeing a
            // reset instead of the answer.
            request(&mut socket).await;

            for piece in response.chunks(PIECE) {
                if socket.write_all(piece).await.is_err() {
                    break;
                }
                let _ = socket.flush().await;
                if !pace.is_zero() {
                    tokio::time::sleep(pace).await;
                }
            }

            // Dropping the socket ends the body, which is how a close-delimited
            // response says it is complete — and how a truncated one says
            // nothing at all.
            drop(socket);
        }
    });

    Endpoint {
        url,
        _server: server,
    }
}

/// A loopback endpoint that records everything it is sent.
///
/// The point of recording is to be able to prove a negative: that a host the
/// client was told to go to was never talked to at all.
struct Recorder {
    /// Base URL a redirect can point at.
    url: String,
    /// Every request the endpoint received, in order.
    seen: Arc<Mutex<Vec<String>>>,
    _server: tokio::task::JoinHandle<()>,
}

impl Recorder {
    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("the log is never poisoned").clone()
    }

    /// Forgets what it has been sent, so that a later assertion about an empty
    /// log is about the requests that came after this point.
    fn clear(&self) {
        self.seen.lock().expect("the log is never poisoned").clear();
    }
}

/// The value `request` carries for the header `name`.
///
/// Field names are case-insensitive, and which case a client sends is its own
/// business, so the comparison has to be too.
fn header(request: &str, name: &str) -> Option<String> {
    request.lines().find_map(|line| {
        let (field, value) = line.split_once(':')?;

        field
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// Serves `responses`, one per connection, keeping what it was asked.
async fn record(responses: Vec<Vec<u8>>) -> Recorder {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);

    let server = tokio::spawn(async move {
        for response in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };

            let received = request(&mut socket).await;
            log.lock()
                .expect("the log is never poisoned")
                .push(String::from_utf8_lossy(&received).into_owned());

            let _ = socket.write_all(&response).await;
            let _ = socket.flush().await;
        }
    });

    Recorder {
        url,
        seen,
        _server: server,
    }
}

/// Reads one whole HTTP request: head, then as many body bytes as it declared.
async fn request(socket: &mut TcpStream) -> Vec<u8> {
    let mut seen = Vec::new();
    let mut chunk = [0_u8; 1024];

    loop {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return seen,
            Ok(read) => seen.extend_from_slice(&chunk[..read]),
        }

        let Some(head_end) = seen.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&seen[..head_end]).to_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_default();

        if seen.len() >= head_end + 4 + length {
            return seen;
        }
    }
}

/// Builds a close-delimited HTTP response.
fn response(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nconnection: close\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);

    out.into_bytes()
}

/// A 200 carrying an event stream.
fn streamed(body: &str) -> Vec<u8> {
    response("200 OK", &[("content-type", "text/event-stream")], body)
}

/// A 200 that promises more body than it delivers.
///
/// A close-delimited response ending early is indistinguishable from one that
/// simply finished; declaring a length and then dying is what makes the client
/// itself raise the error, which is the path this exercises.
fn cut_short(body: &str) -> Vec<u8> {
    let promised = (body.len() + 4_096).to_string();

    response(
        "200 OK",
        &[
            ("content-type", "text/event-stream"),
            ("content-length", &promised),
        ],
        body,
    )
}

/// The request every test sends.
fn prompt() -> ChatRequest {
    ChatRequest {
        model: "test-model".to_owned(),
        system: Some("be brief".to_owned()),
        messages: vec![ganja_core::protocol::Message::user("hello")],
        tools: Vec::new(),
    }
}

/// A request carrying both halves of tool support: the tools the model is
/// offered, and a call it already made whose result it has to be shown again.
fn tool_prompt() -> ChatRequest {
    let mut assistant = ganja_core::protocol::Message::assistant("test-model");
    assistant
        .parts
        .push(ganja_core::protocol::Part::text("Reading the file first."));
    assistant.parts.push(ganja_core::protocol::Part {
        id: ganja_core::protocol::PartId::ascending(),
        body: ganja_core::protocol::PartBody::Tool {
            call_id: "call_read".to_owned(),
            tool: "read".to_owned(),
            state: ganja_core::protocol::ToolState::Completed {
                input: serde_json::json!({"filePath": "src/main.rs"}),
                output: "fn main() {}".to_owned(),
                title: "src/main.rs".to_owned(),
                metadata: serde_json::json!({}),
                started: 1,
                completed: 2,
            },
        },
    });

    ChatRequest {
        model: "test-model".to_owned(),
        system: Some("be brief".to_owned()),
        messages: vec![
            ganja_core::protocol::Message::user("read src/main.rs"),
            assistant,
            ganja_core::protocol::Message::user("what does it do?"),
        ],
        tools: vec![ganja_core::tool::ToolDefinition {
            name: "read".to_owned(),
            description: "Reads a file from disk.".to_owned(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {"filePath": {"type": "string"}},
                "required": ["filePath"],
            }),
        }],
    }
}

/// Everything a provider streamed for [`prompt`].
async fn turn(provider: &dyn Provider) -> Result<Vec<ProviderEvent>, ProviderError> {
    ask(provider, prompt()).await
}

/// Everything a provider streamed for `request`.
async fn ask(
    provider: &dyn Provider,
    request: ChatRequest,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    Ok(provider
        .stream(request, CancellationToken::new())
        .await?
        .collect()
        .await)
}

/// The JSON body of a request an endpoint was sent.
fn sent(request: &str) -> serde_json::Value {
    let (_head, body) = request
        .split_once("\r\n\r\n")
        .expect("a request with a body has a blank line before it");

    serde_json::from_str(body).expect("a provider sends JSON")
}

/// The reply text a turn streamed.
fn text(events: &[ProviderEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn anthropic_streams_a_reply_over_a_real_socket() {
    let endpoint = serve(
        vec![streamed(include_str!("fixtures/anthropic_happy_path.sse"))],
        Duration::ZERO,
    )
    .await;
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);

    let events = turn(&provider).await.expect("the endpoint answers");

    assert_eq!(text(&events), "Hello, world!");
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed))
    );
}

#[tokio::test]
async fn openai_streams_a_reply_over_a_real_socket() {
    let endpoint = serve(
        vec![streamed(include_str!("fixtures/openai_happy_path.sse"))],
        Duration::ZERO,
    )
    .await;
    let provider = OpenAiProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);

    let events = turn(&provider).await.expect("the endpoint answers");

    assert_eq!(text(&events), "Hello, world!");
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed))
    );
}

/// What the model is offered and what it is shown of its own calls are decided
/// when the body is built, and everything after that point is `reqwest`'s. This
/// reads the request off the socket to prove the two survive the trip.
#[tokio::test]
async fn anthropic_puts_its_tools_and_its_call_history_on_the_wire() {
    let endpoint = record(vec![streamed(include_str!(
        "fixtures/anthropic_happy_path.sse"
    ))])
    .await;
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);

    let events = ask(&provider, tool_prompt())
        .await
        .expect("the endpoint answers");
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed))
    );

    let seen = endpoint.seen();
    let [request] = seen.as_slice() else {
        panic!("one turn is one request, got {seen:?}");
    };
    let body = sent(request);

    assert_eq!(
        body["tools"],
        serde_json::json!([{
            "name": "read",
            "description": "Reads a file from disk.",
            "input_schema": {
                "type": "object",
                "properties": {"filePath": {"type": "string"}},
                "required": ["filePath"],
            },
        }]),
        "got {body}"
    );
    assert_eq!(
        body["messages"],
        serde_json::json!([
            {"role": "user", "content": "read src/main.rs"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Reading the file first."},
                {
                    "type": "tool_use",
                    "id": "call_read",
                    "name": "read",
                    "input": {"filePath": "src/main.rs"},
                },
            ]},
            // The result belongs to the message after the one that called, and
            // the API rejects the request outright if it is anywhere else.
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_read", "content": "fn main() {}"},
            ]},
            {"role": "user", "content": "what does it do?"},
        ]),
        "got {body}"
    );
}

/// The same guarantee for the other spelling of it: calls beside the assistant
/// message, results as messages of their own.
#[tokio::test]
async fn openai_puts_its_tools_and_its_call_history_on_the_wire() {
    let endpoint = record(vec![streamed(include_str!(
        "fixtures/openai_happy_path.sse"
    ))])
    .await;
    let provider = OpenAiProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);

    let events = ask(&provider, tool_prompt())
        .await
        .expect("the endpoint answers");
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed))
    );

    let seen = endpoint.seen();
    let [request] = seen.as_slice() else {
        panic!("one turn is one request, got {seen:?}");
    };
    let body = sent(request);

    assert_eq!(
        body["tools"],
        serde_json::json!([{
            "type": "function",
            "function": {
                "name": "read",
                "description": "Reads a file from disk.",
                "parameters": {
                    "type": "object",
                    "properties": {"filePath": {"type": "string"}},
                    "required": ["filePath"],
                },
            },
        }]),
        "got {body}"
    );
    assert_eq!(
        body["messages"],
        serde_json::json!([
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "read src/main.rs"},
            {"role": "assistant", "content": "Reading the file first.", "tool_calls": [{
                "id": "call_read",
                "type": "function",
                "function": {"name": "read", "arguments": r#"{"filePath":"src/main.rs"}"#},
            }]},
            {"role": "tool", "content": "fn main() {}", "tool_call_id": "call_read"},
            {"role": "user", "content": "what does it do?"},
        ]),
        "got {body}"
    );
}

/// A 3xx is an instruction to send the request somewhere else, and the request
/// carries the API key in a header. `reqwest` follows up to ten of them by
/// default and strips only the headers it recognizes as credentials, which
/// `x-api-key` is not — so a redirect from an endpoint that has been hijacked,
/// or from a gateway somebody typo'd into the environment, is enough to hand
/// the key to a host of its choosing. A turn is a single `POST` that nothing
/// legitimately redirects, so the answer is to refuse.
#[tokio::test]
async fn a_redirect_is_reported_rather_than_followed_to_wherever_it_points() {
    // Answers with a stream, so that following the redirect would look like a
    // perfectly successful turn rather than an error anyone would notice.
    let bait = record(vec![
        streamed(include_str!("fixtures/anthropic_happy_path.sse")),
        streamed(include_str!("fixtures/openai_happy_path.sse")),
        streamed(include_str!("fixtures/anthropic_happy_path.sse")),
        streamed(include_str!("fixtures/openai_happy_path.sse")),
    ])
    .await;

    // Control first: reached directly, this endpoint does receive both
    // credentials. Without it, the assertions below would hold just as well
    // against a recorder that cannot read a header at all, or a provider that
    // stopped sending one — an empty log proves nothing until something has
    // shown the log can fill.
    {
        let anthropic = AnthropicProvider::new(CANARY)
            .expect("a client builds")
            .with_base_url(&bait.url);
        let openai = OpenAiProvider::new(CANARY)
            .expect("a client builds")
            .with_base_url(&bait.url);

        turn(&anthropic).await.expect("the endpoint answers");
        turn(&openai).await.expect("the endpoint answers");

        let seen = bait.seen();
        assert_eq!(
            seen.iter()
                .filter_map(|request| header(request, "x-api-key"))
                .collect::<Vec<_>>(),
            vec![CANARY],
            "the recorder should see Anthropic's key header when it is sent one: {seen:?}"
        );
        assert_eq!(
            seen.iter()
                .filter_map(|request| header(request, "authorization"))
                .collect::<Vec<_>>(),
            vec![format!("Bearer {CANARY}")],
            "the recorder should see OpenAI's bearer token when it is sent one: {seen:?}"
        );

        bait.clear();
    }

    let redirector = serve(
        vec![
            response(
                "302 Found",
                &[("location", &format!("{}/v1/messages", bait.url))],
                "",
            ),
            response(
                "302 Found",
                &[("location", &format!("{}/chat/completions", bait.url))],
                "",
            ),
        ],
        Duration::ZERO,
    )
    .await;

    let anthropic = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&redirector.url);
    let openai = OpenAiProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&redirector.url);

    for error in [
        turn(&anthropic).await.expect_err("a 302 is not an answer"),
        turn(&openai).await.expect_err("a 302 is not an answer"),
    ] {
        assert!(
            matches!(error, ProviderError::Status { status: 302, .. }),
            "a redirect must be reported as the refusal it is, got {error:?}"
        );
    }

    let seen = bait.seen();
    for name in ["x-api-key", "authorization"] {
        assert!(
            !seen.iter().any(|request| header(request, name).is_some()),
            "{name} reached the host the redirect named: {seen:?}"
        );
    }
    assert!(
        !seen.iter().any(|request| request.contains(CANARY)),
        "the credential reached the host the redirect named: {seen:?}"
    );
    assert!(
        seen.is_empty(),
        "the redirect was followed, and whatever it pointed at was sent the request: {seen:?}"
    );
}

/// A redirect that leaves the machine must not inherit the exemption that let
/// the request be plain HTTP in the first place: loopback is exempt because the
/// bytes stay on the machine, and a 3xx to a public host is exactly the thing
/// that would make that false.
///
/// Nothing is followed at all, so this holds for the same reason the test above
/// does. It is asserted separately rather than argued from two controls being
/// correct only in combination — and the assertion is specifically that the
/// redirect was *refused where it stood*, because a transport error here would
/// mean it had been followed and merely failed to resolve.
#[tokio::test]
async fn a_loopback_endpoint_cannot_redirect_the_key_off_the_machine() {
    // `.invalid` is reserved by RFC 2606 and never resolves, so a followed
    // redirect fails as a transport error rather than reaching anyone.
    let redirector = serve(
        vec![response(
            "302 Found",
            &[("location", "http://elsewhere.invalid/v1/messages")],
            "",
        )],
        Duration::ZERO,
    )
    .await;
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&redirector.url);

    let error = turn(&provider).await.expect_err("a 302 is not an answer");

    assert!(
        matches!(error, ProviderError::Status { status: 302, .. }),
        "a redirect off the machine must be refused where it stands, got {error:?}"
    );
}

/// The base URL comes out of the environment, and the key travels in a header
/// on every request, so an endpoint that is not `https` puts it on the wire in
/// the clear. Loopback is the exception the test suite itself relies on.
#[tokio::test]
async fn a_plain_http_endpoint_off_the_machine_is_refused_before_anything_is_sent() {
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url("http://ganja:sk-test-canary-XYZ@api.anthropic.example");

    let error = turn(&provider)
        .await
        .expect_err("a plain-http endpoint is refused");
    let rendered = format!("{error} / {error:?}");

    assert!(
        matches!(error, ProviderError::Transport(_)),
        "got {error:?}"
    );
    assert!(
        rendered.contains("https"),
        "the refusal should say what would have been acceptable: {rendered}"
    );
    assert!(
        !rendered.contains(CANARY) && !rendered.contains("api.anthropic.example"),
        "a base URL may carry credentials in its userinfo, so it must not be \
         echoed back: {rendered}"
    );
}

#[tokio::test]
async fn a_body_that_stops_early_fails_the_turn_rather_than_finishing_it() {
    let endpoint = serve(
        vec![streamed(include_str!("fixtures/anthropic_truncated.sse"))],
        Duration::ZERO,
    )
    .await;
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);

    let events = turn(&provider).await.expect("the endpoint answers");

    assert_eq!(text(&events), "The connection drops right");
    assert!(
        matches!(
            events.last(),
            Some(ProviderEvent::Failed(ProviderError::Transport(_)))
        ),
        "a dropped body must never read as a completed turn, got {events:?}"
    );
}

#[tokio::test]
async fn a_body_that_dies_mid_message_reports_why_without_echoing_the_base_url() {
    // A body that dies mid-message is reported as an event rather than
    // returned, so it leaves the provider by a different path than the
    // pre-stream transport error — and that path renders a `reqwest::Error`
    // straight into the message. Today nothing has to strip the base URL's
    // userinfo out of it, because `reqwest` renders a URL with its credentials
    // already removed; this holds that guarantee still, since it is a
    // dependency's promise rather than one this crate keeps itself.
    let endpoint = serve(
        vec![cut_short(include_str!("fixtures/anthropic_truncated.sse"))],
        Duration::ZERO,
    )
    .await;
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(
            endpoint
                .url
                .replace("http://", &format!("http://ganja:{CANARY}@")),
        );

    let events = turn(&provider).await.expect("the endpoint answers");
    let rendered = format!("{events:?}");

    assert!(
        matches!(
            events.last(),
            Some(ProviderEvent::Failed(ProviderError::Transport(_)))
        ),
        "a body that died must not read as a finished turn, got {events:?}"
    );
    assert!(
        !rendered.contains(CANARY),
        "the base URL's userinfo reached a mid-stream failure: {rendered}"
    );
}

#[tokio::test]
async fn a_rate_limit_is_retried_and_the_retry_is_what_answers() {
    let endpoint = serve(
        vec![
            response(
                "429 Too Many Requests",
                // Zero seconds so the ported schedule is exercised without the
                // test waiting out a real backoff.
                &[("retry-after", "0")],
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
            ),
            streamed(include_str!("fixtures/anthropic_happy_path.sse")),
        ],
        Duration::ZERO,
    )
    .await;
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);

    let events = turn(&provider).await.expect("the retry answers");

    assert_eq!(text(&events), "Hello, world!");
}

#[tokio::test]
async fn a_rejected_credential_is_reported_without_being_echoed() {
    // A provider that quotes back the key it rejected is a real shape, and the
    // one place a credential could reach a log without anyone writing it there.
    let endpoint = serve(
        vec![response(
            "401 Unauthorized",
            &[("content-type", "application/json")],
            &format!(
                r#"{{"type":"error","error":{{"type":"authentication_error","message":"invalid x-api-key: {CANARY}"}}}}"#
            ),
        )],
        Duration::ZERO,
    )
    .await;
    let provider = OpenAiProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);

    let error = turn(&provider).await.expect_err("a 401 is not answerable");
    let rendered = format!("{error} / {error:?}");

    assert!(
        !rendered.contains(CANARY),
        "the key came back out in an error: {rendered}"
    );
    assert!(
        rendered.contains("[redacted]"),
        "the echo should be masked rather than dropped: {rendered}"
    );
    assert!(matches!(error, ProviderError::Status { status: 401, .. }));
    assert!(!error.is_retryable(), "a bad key is not worth repeating");
}

#[tokio::test]
async fn a_transport_failure_explains_itself_without_echoing_the_base_url() {
    // Accepted, then closed without an answer: a broken proxy, or a gateway
    // that died holding the request.
    let endpoint = serve(vec![Vec::new()], Duration::ZERO).await;
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        // A base URL is configuration, and configuration is allowed to carry
        // credentials in its userinfo, so it must not reach an error either.
        .with_base_url(
            endpoint
                .url
                .replace("http://", &format!("http://ganja:{CANARY}@")),
        );

    let error = turn(&provider)
        .await
        .expect_err("an empty response is not an answer");
    let rendered = format!("{error} / {error:?}");

    assert!(
        matches!(error, ProviderError::Transport(_)),
        "got {error:?}"
    );
    assert!(
        !rendered.contains(CANARY),
        "the base URL's userinfo reached an error: {rendered}"
    );
    assert!(
        rendered.len() > "the request did not complete: ".len(),
        "the error should say what went wrong, got {rendered}"
    );
}

#[tokio::test]
async fn a_server_error_is_retried_until_the_budget_runs_out() {
    let refusals = (0..ganja_core::provider::retry::MAX_ATTEMPTS)
        .map(|_| {
            response(
                "503 Service Unavailable",
                &[("retry-after", "0")],
                "upstream is down",
            )
        })
        .collect();
    let endpoint = serve(refusals, Duration::ZERO).await;
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);

    let error = turn(&provider)
        .await
        .expect_err("every attempt was refused");

    assert!(
        matches!(
            &error,
            ProviderError::Status { status: 503, message } if message.contains("upstream is down")
        ),
        "got {error:?}"
    );
}

#[tokio::test]
async fn a_failure_mid_stream_finishes_the_turn_as_failed_and_keeps_the_text() {
    let endpoint = serve(
        vec![streamed(include_str!(
            "fixtures/anthropic_mid_stream_error.sse"
        ))],
        Duration::ZERO,
    )
    .await;
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);
    let engine = Engine::new(
        Arc::new(provider),
        "test-model",
        Arc::new(ganja_core::tool::Registry::new(Vec::new())),
        ganja_core::permission::Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let seen = drain(&mut events).await;
    let Some(Event::MessageFinished { reason, error, .. }) = seen.last() else {
        panic!("a turn always ends with a finish, got {seen:?}");
    };

    assert_eq!(
        *reason,
        FinishReason::Failed,
        "a stream that died must not report Completed"
    );
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("Overloaded")),
        "the failure should explain itself, got {error:?}"
    );
    assert_eq!(
        replay(&seen),
        "hiLet me start by",
        "the fragments that did arrive stay in the transcript"
    );
}

#[tokio::test]
async fn a_cancel_mid_stream_finishes_the_turn_as_cancelled() {
    // Paced so that the body is still arriving when the cancel lands.
    let endpoint = serve(
        vec![streamed(include_str!("fixtures/anthropic_happy_path.sse"))],
        Duration::from_millis(5),
    )
    .await;
    let provider = AnthropicProvider::new(CANARY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);
    let engine = Engine::new(
        Arc::new(provider),
        "test-model",
        Arc::new(ganja_core::tool::Registry::new(Vec::new())),
        ganja_core::permission::Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // Wait for the reply to actually start before interrupting it.
    loop {
        match events.next().await {
            Some(Event::PartDelta { .. }) => break,
            Some(_) => {}
            None => panic!("the stream ended before the reply started"),
        }
    }
    engine
        .send(Command::CancelTurn)
        .await
        .expect("a cancel is always accepted");

    let seen = drain(&mut events).await;
    let Some(Event::MessageFinished { reason, error, .. }) = seen.last() else {
        panic!("a turn always ends with a finish, got {seen:?}");
    };

    assert_eq!(*reason, FinishReason::Cancelled);
    assert!(error.is_none(), "a cancel is not a failure, got {error:?}");
}

/// Drains events until the turn finishes.
async fn drain(events: &mut futures::stream::BoxStream<'static, Event>) -> Vec<Event> {
    let mut seen = Vec::new();

    loop {
        let Some(event) = events.next().await else {
            return seen;
        };
        let finished = matches!(event, Event::MessageFinished { .. });
        seen.push(event);

        if finished {
            return seen;
        }
    }
}

/// The text a transcript rebuilt from `events` alone would show.
fn replay(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            Event::MessageStarted { message } => Some(
                message
                    .parts
                    .iter()
                    .filter_map(ganja_core::protocol::Part::as_text)
                    .collect::<String>(),
            ),
            Event::PartDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect()
}
