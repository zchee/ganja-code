//! The HTTP providers against a real socket.
//!
//! Everything here serves canned bytes over loopback rather than mocking the
//! client, so a test exercises the request that is actually built, the retry
//! that is actually scheduled, and the body that is actually split into frames.
//! The response is written in small pieces so that a frame boundary falls
//! wherever the kernel happens to put it, which is the condition the splitter
//! exists to survive.

use std::{sync::Arc, time::Duration};

use futures::StreamExt as _;
use ganja_core::{
    Command, Engine, Event, FinishReason,
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

/// The request every test sends.
fn prompt() -> ChatRequest {
    ChatRequest {
        model: "test-model".to_owned(),
        system: Some("be brief".to_owned()),
        messages: vec![ganja_core::Message::user("hello")],
    }
}

/// Everything a provider streamed for [`prompt`].
async fn turn(provider: &dyn Provider) -> Result<Vec<ProviderEvent>, ProviderError> {
    Ok(provider
        .stream(prompt(), CancellationToken::new())
        .await?
        .collect()
        .await)
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
    let engine = Engine::new(Arc::new(provider), "test-model");
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
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
    let engine = Engine::new(Arc::new(provider), "test-model");
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
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
                    .filter_map(ganja_core::Part::as_text)
                    .collect::<String>(),
            ),
            Event::PartDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect()
}
