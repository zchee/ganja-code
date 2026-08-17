//! The engine's rate-limit visibility, end to end over a real socket
//! (**D484**, `rate-limit-visibility`).
//!
//! `crates/ganja-provider/src/provider/rate.rs` owns the parser's own table
//! tests. What this file pins is everything downstream of it: that a *real*
//! response over a *real* socket lands on `Engine::rate_windows`, that a
//! backend which sends no such headers leaves it empty rather than inventing
//! a set, and that a fresh session on the same wire keeps the buckets — which
//! is the whole claim of holding them per credential rather than per
//! conversation.
//!
//! Since P17 (**D485**) the same three claims are pinned for the *plan*
//! buckets that ride the same seam: `Engine::plan_windows` over a real socket,
//! the used/remaining normalization landing in one direction, and a snapshot
//! carrying no reset staying undated rather than borrowing one.
//!
//! P22 (`53v`) gave the *rate* buckets that last property too, so it is pinned
//! here as well: grok's `x-ratelimit-*` arrive with the two counts and no
//! reset at all, and a set this build once dropped whole now reaches the
//! accessor undated.
//!
//! Its own binary, `http.rs`'s posture: the response bytes are canned but the
//! socket is not, so a header only counts once it has survived being written,
//! read and parsed by the same client a real turn uses.

use std::{sync::Arc, time::SystemTime};

use ganja_core::{
    Engine,
    protocol::Command,
    provider::{AnthropicProvider, Provider as _},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};

/// The credential every test authenticates with, which nothing may render.
const CANARY: &str = "sk-test-canary-XYZ";

/// The shortest Messages stream that completes a turn, so the engine has a
/// finished turn to have polled state after.
const HAPPY: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"usage":{"input_tokens":4,"output_tokens":0}}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

/// A loopback endpoint answering every connection with the same headers and
/// the same stream.
struct Endpoint {
    url: String,
    _server: tokio::task::JoinHandle<()>,
}

/// Serves `turns` responses, each carrying `headers` beside the happy stream.
async fn serve(headers: Vec<Vec<(String, String)>>) -> Endpoint {
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
        for set in headers {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            drain(&mut socket).await;

            let mut out = "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: \
                           text/event-stream\r\n"
                .to_owned();
            for (name, value) in &set {
                out.push_str(&format!("{name}: {value}\r\n"));
            }
            out.push_str("\r\n");
            out.push_str(HAPPY);

            let _ = socket.write_all(out.as_bytes()).await;
            let _ = socket.flush().await;
            drop(socket);
        }
    });

    Endpoint {
        url,
        _server: server,
    }
}

/// Reads one whole HTTP request, so the client sees an answer rather than a
/// reset.
async fn drain(socket: &mut TcpStream) {
    let mut seen = Vec::new();
    let mut chunk = [0_u8; 1024];

    loop {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
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
            return;
        }
    }
}

/// An engine on a wire pointed at `endpoint`.
fn engine(endpoint: &Endpoint) -> Engine {
    Engine::new(
        Arc::new(
            AnthropicProvider::new(CANARY)
                .expect("a client builds")
                .with_base_url(&endpoint.url),
        ),
        "test-model",
        Arc::new(ganja_core::tool::Registry::new(Vec::new())),
        ganja_core::permission::Permissions::default(),
    )
}

/// Runs one turn and waits for it to settle, so the polled state is about a
/// response that really landed.
async fn turn(engine: &Engine, prompt: &str) {
    engine
        .send(Command::SendPrompt {
            text: prompt.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    assert!(
        engine.settle(std::time::Duration::from_secs(10)).await,
        "the turn should finish against a loopback endpoint"
    );
}

fn pairs(raw: &[(&str, &str)]) -> Vec<(String, String)> {
    raw.iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// AC7's first clause: real headers on a real response reach the engine's
/// polled state.
#[tokio::test]
async fn a_response_carrying_rate_headers_lands_on_the_engines_polled_state() {
    let endpoint = serve(vec![pairs(&[
        ("anthropic-ratelimit-requests-limit", "1000"),
        ("anthropic-ratelimit-requests-remaining", "997"),
        ("anthropic-ratelimit-requests-reset", "2099-01-01T00:00:00Z"),
    ])])
    .await;
    let engine = engine(&endpoint);

    assert!(
        engine.rate_windows().is_empty(),
        "before any request there is nothing the vendor has said"
    );

    turn(&engine, "hi").await;

    let windows = engine.rate_windows();
    assert_eq!(windows.len(), 1, "got {windows:?}");
    assert_eq!(windows[0].kind, "requests");
    assert_eq!(windows[0].limit, 1_000);
    assert_eq!(windows[0].remaining, 997);
    assert!(
        !windows[0].expired(SystemTime::now()),
        "a window resetting in 2099 is not expired today"
    );
}

/// AC3 (`53v`) end to end: grok's own shape — the two counts per bucket and
/// no `-reset-` header anywhere — survives the socket and reaches the accessor
/// every rendering surface polls.
///
/// Before P22 the three-field rule dropped these on the floor, so an account
/// with real budget left metered as a silent backend. What arrives now is a
/// bucket with no clock, which nothing may call stale and nothing may date.
#[tokio::test]
async fn a_response_whose_buckets_carry_no_reset_still_reaches_the_engine() {
    let endpoint = serve(vec![pairs(&[
        ("x-ratelimit-limit-requests", "500"),
        ("x-ratelimit-remaining-requests", "499"),
        ("x-ratelimit-limit-tokens", "150000"),
        ("x-ratelimit-remaining-tokens", "149000"),
    ])])
    .await;
    let engine = engine(&endpoint);

    turn(&engine, "hi").await;

    let windows = engine.rate_windows();
    assert_eq!(windows.len(), 2, "both buckets land: got {windows:?}");
    assert_eq!(windows[0].kind, "requests");
    assert_eq!(windows[0].remaining, 499);
    assert_eq!(windows[1].kind, "tokens");
    assert_eq!(windows[1].remaining, 149_000);
    assert!(
        windows.iter().all(|window| window.reset.is_none()),
        "nothing dated these, so nothing carries a date: {windows:?}"
    );
    assert!(
        windows
            .iter()
            .all(|window| !window.expired(SystemTime::now())),
        "and a bucket nobody dated cannot have gone stale: {windows:?}"
    );
}

/// The D470 rule end to end: a backend that sends nothing meters nothing, and
/// the engine invents no denominator to fill the hole.
#[tokio::test]
async fn a_backend_that_sends_no_rate_headers_leaves_the_state_empty() {
    let endpoint = serve(vec![Vec::new()]).await;
    let engine = engine(&endpoint);

    turn(&engine, "hi").await;

    assert!(
        engine.rate_windows().is_empty(),
        "a headerless backend must render nothing, not a zero"
    );
}

/// The newest answer wins, and a later response that says nothing does not
/// erase the last one that spoke — a proxy stripping the headers mid-outage
/// is not the vendor announcing an unknown budget (P16 pre-mortem 4's other
/// half: the expiry is what dates a stale number, not a silent clear).
#[tokio::test]
async fn the_newest_answer_wins_and_a_silent_one_erases_nothing() {
    let endpoint = serve(vec![
        pairs(&[
            ("anthropic-ratelimit-requests-limit", "1000"),
            ("anthropic-ratelimit-requests-remaining", "999"),
            ("anthropic-ratelimit-requests-reset", "2099-01-01T00:00:00Z"),
        ]),
        pairs(&[
            ("anthropic-ratelimit-requests-limit", "1000"),
            ("anthropic-ratelimit-requests-remaining", "998"),
            ("anthropic-ratelimit-requests-reset", "2099-01-01T00:00:00Z"),
        ]),
        Vec::new(),
    ])
    .await;
    let engine = engine(&endpoint);

    turn(&engine, "one").await;
    assert_eq!(engine.rate_windows()[0].remaining, 999);

    turn(&engine, "two").await;
    assert_eq!(
        engine.rate_windows()[0].remaining,
        998,
        "the later response is the newer truth"
    );

    turn(&engine, "three").await;
    assert_eq!(
        engine.rate_windows()[0].remaining,
        998,
        "a response that said nothing leaves the last real answer standing"
    );
}

/// The carrier decision, pinned as behavior: the windows belong to the
/// credential, so a new session on the same wire keeps them rather than
/// starting blank.
///
/// This is the "resume clears or rebuilds?" question answered — **neither**.
/// There is nothing session-shaped to clear, because rate limits were never a
/// fact about a conversation; staleness is `RateWindow::expired`'s job, and
/// only its job.
#[tokio::test]
async fn a_new_session_on_the_same_wire_keeps_the_windows_that_credential_earned() {
    let endpoint = serve(vec![pairs(&[
        ("anthropic-ratelimit-input-tokens-limit", "80000"),
        ("anthropic-ratelimit-input-tokens-remaining", "60000"),
        (
            "anthropic-ratelimit-input-tokens-reset",
            "2099-01-01T00:00:00Z",
        ),
    ])])
    .await;
    let engine = engine(&endpoint);

    turn(&engine, "hi").await;
    let before = engine.rate_windows();
    assert_eq!(before.len(), 1, "got {before:?}");

    engine
        .send(Command::NewSession)
        .await
        .expect("an idle engine starts a new session");

    assert_eq!(
        engine.rate_windows(),
        before,
        "the account's budget did not change because the conversation did"
    );
}

/// The wire is what holds them, so a provider that never made a request
/// answers honestly through the trait's own default.
#[test]
fn a_wire_that_has_answered_nothing_reports_no_windows() {
    let provider = AnthropicProvider::new(CANARY).expect("a client builds");

    assert!(
        provider.rate_windows().is_empty(),
        "nothing is known until a response says something"
    );
}

/// The plan half, over the same socket (**D485**): a codex-shaped response's
/// headers reach `Engine::plan_windows`, in the normalized direction — the
/// vendor sends how much is *used* and this side keeps it that way.
///
/// The wire under test is the Anthropic one, as everywhere else in this file:
/// what is pinned here is the seam, and the seam is every wire's, because
/// `RateWindows::record` runs on the one response path they share. A codex
/// header on an anthropic response is not a thing that happens; a parser that
/// only worked on one wire's responses is.
#[tokio::test]
async fn a_response_carrying_plan_headers_lands_on_the_engines_polled_state() {
    let endpoint = serve(vec![pairs(&[
        ("x-codex-primary-used-percent", "37.5"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-at", "4070908800"),
        ("x-codex-secondary-used-percent", "12"),
        ("x-codex-secondary-window-minutes", "10080"),
        ("x-codex-secondary-reset-at", "4070908800"),
    ])])
    .await;
    let engine = engine(&endpoint);

    assert!(
        engine.plan_windows().is_empty(),
        "before any request there is nothing the vendor has said"
    );

    turn(&engine, "hi").await;

    let plans = engine.plan_windows();
    assert_eq!(plans.len(), 2, "got {plans:?}");
    assert_eq!(plans[0].name, "primary");
    assert!((plans[0].used_percent - 37.5).abs() < f64::EPSILON);
    assert_eq!(plans[0].window_minutes, Some(300));
    assert!(
        !plans[0].expired(SystemTime::now()),
        "a window resetting in 2099 is not expired today"
    );
    assert_eq!(plans[1].name, "secondary");

    assert!(
        engine.rate_windows().is_empty(),
        "a plan header is not a rate bucket, and neither invents the other"
    );
}

/// The copilot half, and the shape that has no clock: a quota snapshot lands
/// with its percentage flipped to used, and — carrying no `rst` — dates itself
/// not at all rather than borrowing an expiry (**D485**).
#[tokio::test]
async fn a_quota_snapshot_lands_with_its_percentage_flipped_and_no_invented_clock() {
    let endpoint = serve(vec![pairs(&[(
        "x-quota-snapshot-premium_interactions",
        "ent=300&ov=0.0&ovPerm=false&rem=88.5",
    )])])
    .await;
    let engine = engine(&endpoint);

    turn(&engine, "hi").await;

    let plans = engine.plan_windows();
    assert_eq!(plans.len(), 1, "got {plans:?}");
    assert_eq!(plans[0].name, "premium_interactions");
    assert!((plans[0].used_percent - 11.5).abs() < 1e-9);
    assert_eq!(plans[0].resets_at, None);
    assert!(
        !plans[0].expired(SystemTime::now()),
        "nothing dated it, so nothing may date it stale"
    );
}

/// The D470 rule on the new family, end to end: the credentials the W-A1 probe
/// found silent meter nothing here either.
#[tokio::test]
async fn a_backend_that_sends_no_plan_headers_leaves_the_plan_state_empty() {
    let endpoint = serve(vec![pairs(&[
        ("anthropic-ratelimit-requests-limit", "1000"),
        ("anthropic-ratelimit-requests-remaining", "997"),
        ("anthropic-ratelimit-requests-reset", "2099-01-01T00:00:00Z"),
    ])])
    .await;
    let engine = engine(&endpoint);

    turn(&engine, "hi").await;

    assert_eq!(engine.rate_windows().len(), 1, "the rate half still lands");
    assert!(
        engine.plan_windows().is_empty(),
        "a backend with no plan headers meters no plan"
    );
}
