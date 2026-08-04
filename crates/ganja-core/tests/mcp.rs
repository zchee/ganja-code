//! MCP servers end to end: a real server, a real transport, a real turn.
//!
//! # Why the server is somebody else's
//!
//! Every test here that speaks the protocol speaks it to a server built on
//! `@modelcontextprotocol/sdk` 1.29.0 — the SDK upstream opencode itself uses,
//! taken out of the reference checkout the golden differential already
//! requires. A fixture server built on `rmcp`, the crate the client under test
//! is built on, would agree with the client whether or not either of them read
//! the specification correctly. This one cannot: it is a different
//! implementation in a different language, and the two only agree by being
//! right.
//!
//! `bun` and that checkout are therefore hard prerequisites, and a run without
//! them **fails** rather than skipping — the golden suite's rule, for the same
//! reason: a green run that talked to nothing would be worth less than no run.
//!
//! # What each leg covers
//!
//! - The stdio transport, the naming, the permission gate, the error paths and
//!   the death of a server mid-session are all driven against that reference
//!   server through a scripted [`Engine`] turn.
//! - The remote transport gets a loopback HTTP server of its own, hand-rolled
//!   over a real socket in the shape `tests/http.rs` uses, so the request that
//!   is checked is the one the transport actually builds.
//! - The golden fixtures are checked to still contain no MCP anything, which
//!   is what keeps the differential comparing what it was written to compare.
//!
//! Ordering and collision rules, sanitization, schema forcing, timeout
//! resolution and result rendering are unit-tested beside the module; this file
//! is about the parts that need another process.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::stream::BoxStream;
use ganja_core::{
    Command, Config, Engine, Event, FinishReason, McpServers, McpStatus, PartBody, PermissionReply,
    Permissions, Registry, ToolState, provider::Provider,
};
use ganja_testkit::{ScriptedProvider, drain_answering, says, tool_call};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

/// Environment variable naming the upstream checkout, shared with the golden
/// suite so one vendored copy serves both.
const CHECKOUT_ENV: &str = "GANJA_OPENCODE_DIR";

/// How long a connect and its listing may take before the test gives up on
/// the fixture. Generous: a cold `bun` start is seconds.
const READY: Duration = Duration::from_secs(30);

/// The final state of the tool part named `tool`, whatever it finally was.
fn tool_part(seen: &[Event], tool: &str) -> ToolState {
    seen.iter()
        .rev()
        .find_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    tool: named, state, ..
                } if named == tool => Some(state.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|| panic!("the turn produced no part for {tool}"))
}

/// The output text of a completed call.
fn completed(state: &ToolState) -> String {
    match state {
        ToolState::Completed { output, .. } => output.clone(),
        other => panic!("expected a completed call, got {other:?}"),
    }
}

/// The error text of a failed call.
fn errored(state: &ToolState) -> String {
    match state {
        ToolState::Error { error, .. } => error.clone(),
        other => panic!("expected a failed call, got {other:?}"),
    }
}

/// The upstream checkout, and the SDK inside it the fixture server is built on.
///
/// Both are hard prerequisites. A missing one is a failure and never a skip,
/// for the reason `tests/golden.rs` gives at the same guard.
fn reference_sdk() -> PathBuf {
    let checkout = std::env::var_os(CHECKOUT_ENV).map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.omc/reference/opencode-v1.18.11")
                .to_owned()
        },
        PathBuf::from,
    );

    assert!(
        checkout.join("packages/opencode/src/index.ts").is_file(),
        "the upstream checkout is not where this expects it: {}. \
         Vendor opencode v1.18.11 there, or point {CHECKOUT_ENV} at it.",
        checkout.display()
    );

    // Where `bun install` puts a workspace package's own dependency, with the
    // hoisted copy as the fallback.
    let candidates = [
        checkout.join("packages/opencode/node_modules/@modelcontextprotocol/sdk"),
        checkout.join("node_modules/@modelcontextprotocol/sdk"),
    ];
    candidates
        .into_iter()
        .find(|candidate| candidate.join("dist/esm/server/index.js").is_file())
        .unwrap_or_else(|| {
            panic!(
                "the upstream checkout has no @modelcontextprotocol/sdk installed. \
                 Run `bun install` in {}.",
                checkout.display()
            )
        })
}

/// The fixture server as a config entry.
fn reference_server(name: &str) -> Config {
    fixture_server(name, "reference-server.mjs")
}

/// The fixture that ignores stdin EOF, as a config entry.
fn stubborn_server(name: &str) -> Config {
    fixture_server(name, "stubborn-server.mjs")
}

/// One of the fixture servers under `tests/fixtures/mcp/` as a config entry.
fn fixture_server(name: &str, file: &str) -> Config {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp")
        .join(file);
    assert!(script.is_file(), "{} is missing", script.display());

    let sdk = reference_sdk();
    let config = json!({
        "mcp": {
            name: {
                "type": "local",
                "command": ["bun", script.to_str().expect("the fixture path is UTF-8")],
                "environment": { "GANJA_MCP_SDK_DIR": sdk.to_str().expect("the SDK path is UTF-8") },
            }
        }
    });

    serde_json::from_value(config).expect("the fixture config is a config")
}

/// The reference fixture, run through a shell wrapper that writes its own pid
/// to `pidfile` and then sleeps for `delay` before it execs the real server.
///
/// The delay is what makes the shutdown/connect race below deterministic
/// without either side racing a sleep of its own: `shutdown` runs the
/// instant `connect_all` is spawned, with nothing of its own to wait on, so
/// it reliably finishes draining the (empty) map long before a shell `sleep`
/// measured in hundreds of milliseconds does. The pid file is the only way
/// to check the process this starts actually died — the whole point of the
/// fix under test is that its group never reaches this session's own
/// bookkeeping, which is where every other helper here would look for it.
fn delayed_reference_server(name: &str, pidfile: &Path, delay: Duration) -> Config {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp/reference-server.mjs");
    assert!(script.is_file(), "{} is missing", script.display());

    let sdk = reference_sdk();
    let command = format!(
        "echo $$ > {} && sleep {} && exec bun {}",
        pidfile.display(),
        delay.as_secs_f64(),
        script.display(),
    );
    let config = json!({
        "mcp": {
            name: {
                "type": "local",
                "command": ["sh", "-c", command],
                "environment": { "GANJA_MCP_SDK_DIR": sdk.to_str().expect("the SDK path is UTF-8") },
            }
        }
    });

    serde_json::from_value(config).expect("the fixture config is a config")
}

/// An engine over `provider` with `config`'s MCP servers connected and their
/// tools installed.
///
/// The connect runs in the background exactly as it does in a real session;
/// this waits for it, because a test that raced it would be testing the race.
async fn engine_with(provider: Arc<dyn Provider>, config: &Config) -> Engine {
    let servers = McpServers::new(config.mcp.clone(), Path::new("."));
    let mut permissions = Permissions::default();
    permissions.set_baseline(config.permission.rules());

    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::with_builtins()),
        permissions,
    )
    .with_system(Some("you are a fixture".to_owned()))
    .with_mcp(servers);
    engine.connect_mcp();

    let deadline = tokio::time::Instant::now() + READY;
    while tokio::time::Instant::now() < deadline && engine.mcp_status().is_empty() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    engine
}

/// Sends `prompt` and drains the turn, answering permissions with `reply`.
async fn turn(
    engine: &Engine,
    events: &mut BoxStream<'static, Event>,
    prompt: &str,
    reply: PermissionReply,
) -> Vec<Event> {
    engine
        .send(Command::SendPrompt {
            text: prompt.to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    drain_answering(engine, events, reply).await
}

/// The reference server round-trips a call: the argument goes out over stdio
/// and comes back in the tool result the model reads.
///
/// This is the accept for "MCP reference server round-trips a tool call". Break
/// the namespacing and `the turn produced no part for mcp__reference__echo`
/// fires, because the model's call finds no such tool.
#[tokio::test]
async fn the_reference_server_round_trips_a_call_the_model_made() {
    let (provider, requests) = ScriptedProvider::new(vec![
        tool_call("mcp__reference__echo", json!({ "text": "the argument" })),
        says("it came back"),
    ]);
    let config = reference_server("reference");
    let engine = engine_with(provider, &config).await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    assert_eq!(
        engine.mcp_status().get("reference"),
        Some(&McpStatus::Connected),
        "the fixture server did not connect"
    );

    let seen = turn(
        &engine,
        &mut events,
        "echo something",
        PermissionReply::Once,
    )
    .await;

    assert_eq!(
        completed(&tool_part(&seen, "mcp__reference__echo")),
        "echo: the argument",
        "the argument did not round-trip through the server"
    );

    // The whole surface is offered under the namespace, sanitized name and all.
    let offered: Vec<String> = requests
        .lock()
        .expect("the request log is never poisoned")
        .first()
        .expect("the turn asked the model something")
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    for name in [
        "mcp__reference__echo",
        "mcp__reference__explode",
        "mcp__reference__odd_name",
    ] {
        assert!(offered.contains(&name.to_owned()), "{name} in {offered:?}");
    }

    engine.shutdown_mcp().await;
}

/// The server's own instructions reach the system prompt, in upstream's block.
#[tokio::test]
async fn a_connected_servers_instructions_reach_the_system_prompt() {
    let (provider, requests) = ScriptedProvider::new(vec![says("nothing to do")]);
    let config = reference_server("reference");
    let engine = engine_with(provider, &config).await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    turn(&engine, &mut events, "hello", PermissionReply::Once).await;

    let system = requests
        .lock()
        .expect("the request log is never poisoned")
        .first()
        .expect("the turn asked the model something")
        .system
        .clone()
        .expect("the engine was given a system prompt");

    assert!(
        system.contains(
            "<mcp_instructions>\n  <server name=\"reference\">\n    Echo things back.\n    \
             Do not read anything into them.\n  </server>\n</mcp_instructions>"
        ),
        "{system}"
    );

    engine.shutdown_mcp().await;
}

/// Every MCP tool asks, and an answer of "always" is not asked again.
///
/// The shape of the rule an "always" leaves behind is pinned beside the
/// permission engine; what is proved here is that the shape is wide enough to
/// answer the next call, which only an end-to-end turn can show.
#[tokio::test]
async fn an_mcp_call_asks_once_and_an_always_answer_is_not_asked_again() {
    let (provider, _requests) = ScriptedProvider::new(vec![
        tool_call("mcp__reference__echo", json!({ "text": "first" })),
        says("one"),
        tool_call("mcp__reference__echo", json!({ "text": "second" })),
        says("two"),
    ]);
    let config = reference_server("reference");
    let engine = engine_with(provider, &config).await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let first = turn(&engine, &mut events, "echo first", PermissionReply::Always).await;
    assert!(
        first.iter().any(
            |event| matches!(event, Event::PermissionRequested { tool, .. }
                if tool == "mcp__reference__echo")
        ),
        "an MCP tool nothing has a rule about must ask"
    );

    let second = turn(&engine, &mut events, "echo second", PermissionReply::Reject).await;
    assert!(
        !second
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "an \"always\" answer must cover the next call to the same tool"
    );
    assert_eq!(
        completed(&tool_part(&second, "mcp__reference__echo")),
        "echo: second"
    );

    engine.shutdown_mcp().await;
}

/// A config rule written against the whole server denies its tools, and the
/// refusal is information: the turn carries on and the model answers.
#[tokio::test]
async fn a_config_wildcard_over_a_server_denies_its_tools_without_ending_the_turn() {
    let (provider, _requests) = ScriptedProvider::new(vec![
        tool_call("mcp__reference__echo", json!({ "text": "let me in" })),
        says("it said no"),
    ]);
    let mut config = reference_server("reference");
    config.permission = serde_json::from_value(json!({ "mcp__reference__*": "deny" }))
        .expect("the fixture rule parses");
    let engine = engine_with(provider, &config).await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let seen = turn(
        &engine,
        &mut events,
        "echo something",
        PermissionReply::Once,
    )
    .await;

    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "a rule already answered, so nothing may be put in front of anybody"
    );
    let refusal = errored(&tool_part(&seen, "mcp__reference__echo"));
    assert!(
        refusal.contains("prevents you from using this specific tool call"),
        "{refusal}"
    );
    // The rule that decided is quoted back at the model, wildcard and all.
    assert!(
        refusal.contains(r#""permission":"mcp__reference__*""#),
        "{refusal}"
    );
    assert!(
        matches!(
            seen.last(),
            Some(Event::MessageFinished {
                reason: FinishReason::Completed,
                ..
            })
        ),
        "a refusal is information, not the end of a turn"
    );

    engine.shutdown_mcp().await;
}

/// A tool that answers `isError` hands the model the server's own words, and
/// the turn carries on.
#[tokio::test]
async fn an_error_result_becomes_error_text_the_model_reads() {
    let (provider, _requests) = ScriptedProvider::new(vec![
        tool_call("mcp__reference__explode", json!({})),
        says("noted"),
    ]);
    let config = reference_server("reference");
    let engine = engine_with(provider, &config).await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let seen = turn(&engine, &mut events, "make it fail", PermissionReply::Once).await;

    assert_eq!(
        errored(&tool_part(&seen, "mcp__reference__explode")),
        "the fixture refused"
    );
    assert!(
        matches!(
            seen.last(),
            Some(Event::MessageFinished {
                reason: FinishReason::Completed,
                ..
            })
        ),
        "an errored tool is not a failed turn"
    );

    engine.shutdown_mcp().await;
}

/// A structured-only answer and an image answer both reach the model as text.
#[tokio::test]
async fn structured_and_binary_answers_are_rendered_for_a_model_that_reads_text() {
    let (provider, _requests) = ScriptedProvider::new(vec![
        tool_call("mcp__reference__structured", json!({})),
        tool_call("mcp__reference__picture", json!({})),
        says("seen both"),
    ]);
    let config = reference_server("reference");
    let engine = engine_with(provider, &config).await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let seen = turn(&engine, &mut events, "show me", PermissionReply::Once).await;

    let structured = completed(&tool_part(&seen, "mcp__reference__structured"));
    let parsed: Value = serde_json::from_str(&structured)
        .unwrap_or_else(|error| panic!("a structured-only answer is one JSON block: {error}"));
    assert_eq!(parsed, json!({ "answered": true, "count": 2 }));

    assert_eq!(
        completed(&tool_part(&seen, "mcp__reference__picture")),
        "here it is\n[binary MCP content omitted: image/png, 9 bytes]"
    );

    engine.shutdown_mcp().await;
}

/// A server that dies mid-session is marked failed, and its tools stop being
/// offered at the next turn — there is no reconnect, by design.
#[tokio::test]
async fn a_server_that_dies_mid_session_fails_and_loses_its_tools() {
    let (provider, requests) = ScriptedProvider::new(vec![
        tool_call("mcp__reference__vanish", json!({})),
        says("it is gone"),
        says("still here"),
    ]);
    let config = reference_server("reference");
    let engine = engine_with(provider, &config).await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let seen = turn(&engine, &mut events, "kill it", PermissionReply::Once).await;
    // The call itself fails, as information — the turn still completes.
    errored(&tool_part(&seen, "mcp__reference__vanish"));
    assert!(matches!(
        seen.last(),
        Some(Event::MessageFinished {
            reason: FinishReason::Completed,
            ..
        })
    ));

    // The next turn is where the withdrawal happens: a turn already holding a
    // snapshot keeps the tools it started with.
    turn(&engine, &mut events, "anything else", PermissionReply::Once).await;

    assert!(
        matches!(
            engine.mcp_status().get("reference"),
            Some(McpStatus::Failed { .. })
        ),
        "a closed connection is a failed server: {:?}",
        engine.mcp_status()
    );

    let offered: Vec<String> = requests
        .lock()
        .expect("the request log is never poisoned")
        .last()
        .expect("the second turn asked the model")
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert!(
        !offered
            .iter()
            .any(|name| name.starts_with("mcp__reference__")),
        "a dead server's tools must not still be offered: {offered:?}"
    );

    engine.shutdown_mcp().await;
}

/// The remote transport, over a real socket.
///
/// The server is hand-rolled rather than rmcp's, for `tests/http.rs`'s reason:
/// what is under test is the request the transport actually builds, and a
/// mocked client would skip exactly that. It answers `initialize`, `tools/list`
/// and `tools/call` as a stateless streamable-HTTP endpoint, and records the
/// headers the config asked for.
#[tokio::test]
async fn a_remote_server_is_reached_over_streamable_http() {
    let seen_headers: Arc<Mutex<Vec<String>>> = Arc::default();
    let address = streamable_http(Arc::clone(&seen_headers)).await;

    let config: Config = serde_json::from_value(json!({
        "mcp": {
            "hub": {
                "type": "remote",
                "url": format!("http://{address}/mcp"),
                "headers": { "X-Ganja-Test": "carried" },
            }
        }
    }))
    .expect("the fixture config is a config");

    let (provider, _requests) = ScriptedProvider::new(vec![
        tool_call("mcp__hub__ping", json!({ "text": "over http" })),
        says("answered"),
    ]);
    let engine = engine_with(provider, &config).await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    assert_eq!(
        engine.mcp_status().get("hub"),
        Some(&McpStatus::Connected),
        "the loopback endpoint did not connect: {:?}",
        engine.mcp_status()
    );

    let seen = turn(&engine, &mut events, "ping it", PermissionReply::Once).await;
    assert_eq!(
        completed(&tool_part(&seen, "mcp__hub__ping")),
        "pong: over http"
    );

    let headers = seen_headers
        .lock()
        .expect("the header log is never poisoned")
        .clone();
    assert!(
        headers.iter().any(|header| header == "carried"),
        "the configured header never reached the server: {headers:?}"
    );

    engine.shutdown_mcp().await;
}

/// A loopback endpoint speaking streamable HTTP, returning its address.
async fn streamable_http(headers: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the socket has an address");

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let headers = Arc::clone(&headers);
            tokio::spawn(async move {
                // Kept alive across requests: the transport posts the
                // `initialize` request and then the `initialized` notification,
                // and a server that hung up in between would look to it like a
                // connection that failed.
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];

                loop {
                    // The body length is read out of the headers rather than
                    // guessed, because a POST arrives in as many reads as it
                    // likes.
                    let (head, body) = loop {
                        if let Some(request) = whole(&buffer) {
                            break request;
                        }
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                    };
                    buffer.drain(..head.len() + 4 + body.len());

                    for line in head.lines() {
                        if let Some((name, value)) = line.split_once(':')
                            && name.eq_ignore_ascii_case("x-ganja-test")
                        {
                            headers
                                .lock()
                                .expect("the header log is never poisoned")
                                .push(value.trim().to_owned());
                        }
                    }

                    let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let response = match answer(&request) {
                        Some(answer) => {
                            let body = answer.to_string();
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                 content-length: {}\r\n\r\n{body}",
                                body.len()
                            )
                        }
                        // A notification is acknowledged and nothing more.
                        None => "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_owned(),
                    };
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = stream.flush().await;
                }
            });
        }
    });

    address
}

/// One whole request out of `buffer` — its head and its body — or [`None`]
/// while the bytes are still arriving.
fn whole(buffer: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(buffer).ok()?;
    let (head, rest) = text.split_once("\r\n\r\n")?;
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    if rest.len() < length {
        return None;
    }

    Some((head.to_owned(), rest[..length].to_owned()))
}

/// What the loopback endpoint answers one JSON-RPC request with, or [`None`]
/// for a notification.
fn answer(request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request.get("method")?.as_str()?;

    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "hub", "version": "0.0.0" },
        }),
        "tools/list" => json!({
            "tools": [{
                "name": "ping",
                "description": "Answers over HTTP.",
                "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } },
            }],
        }),
        "tools/call" => {
            let text = request
                .pointer("/params/arguments/text")
                .and_then(Value::as_str)
                .unwrap_or_default();

            json!({ "content": [{ "type": "text", "text": format!("pong: {text}") }] })
        }
        _ => json!({}),
    };

    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// The golden differential compares what it was written to compare.
///
/// `snapshot`-style defaults aside, the one thing that could quietly change the
/// differential is a golden fixture gaining an MCP server: upstream would then
/// connect one and ganja would too, and the comparison would be of two
/// transcripts nobody wrote. Asserted rather than assumed.
#[test]
fn no_golden_fixture_asks_for_an_mcp_server() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden");
    let mut checked = 0;

    for entry in std::fs::read_dir(&directory).expect("the golden fixtures are readable") {
        let path = entry.expect("the directory lists").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a fixture is readable");
        assert!(
            !text.contains("mcp"),
            "{} mentions mcp; the differential no longer compares what it was written to",
            path.display()
        );
        checked += 1;
    }

    assert!(
        checked > 0,
        "no golden fixtures were found in {}",
        directory.display()
    );
}

/// Whether `pid` names a process that is still running.
///
/// A zombie counts as gone: it has exited and is waiting to be reaped, which is
/// the opposite of the orphan this is looking for.
#[cfg(unix)]
fn running(pid: u32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .expect("ps runs");
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();

    !state.is_empty() && !state.starts_with('Z')
}

/// A session that ends without shutting down still takes its servers with it.
///
/// `shutdown` is the orderly path and it ends the whole process *group*. This
/// pins the invariant underneath it: a session that goes away without calling it
/// must not leave stray `bun` processes behind.
///
/// It runs against `stubborn-server.mjs` rather than the reference one, because
/// a normal stdio server exits when its stdin closes and would make this pass
/// for a reason that has nothing to do with the invariant. The stubborn fixture
/// holds its event loop open forever, so only a kill can end it.
///
/// **What this does and does not discriminate.** Two mechanisms can satisfy it:
/// `rmcp`'s `ChildWithCleanup::drop`, and the `kill_on_drop` set beside the
/// spawn. Dropping the servers here happens on a healthy runtime, which is the
/// case rmcp's own `tokio::spawn`ed kill already handles — so this test passes
/// with `kill_on_drop` removed, and does *not* prove that line is carrying
/// anything. What `kill_on_drop` adds is the case this cannot reach from inside
/// a `#[tokio::test]`: a runtime being torn down, where a spawned task may never
/// be polled. Reaching that needs a second process built to die mid-flight.
/// The test is kept as a regression guard on the invariant itself — if a future
/// `rmcp` drops its cleanup *and* the `kill_on_drop` goes with it, this is what
/// notices.
#[cfg(unix)]
#[tokio::test]
async fn a_session_that_ends_without_shutting_down_leaves_no_server_running() {
    let config = stubborn_server("fixture");
    let servers = McpServers::new(config.mcp.clone(), Path::new("."));
    servers.connect_all().await;

    let groups = servers.process_groups();
    assert_eq!(groups.len(), 1, "the fixture server connected");
    let pid = groups[0];
    assert!(running(pid), "and is running before anything is dropped");

    // The abnormal exit: everything goes away, and `shutdown` is never called.
    drop(servers);

    for _ in 0..100 {
        if !running(pid) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    panic!("the MCP server outlived the session that started it (pid {pid})");
}

/// **Regression, shutdown/connect race.** A `connect` still dialing when
/// `shutdown` drains the map used to finish afterward and install a fresh
/// `Connected` client and a fresh process group that nothing would ever
/// cancel or kill again — `shutdown` had already run its once-only cleanup.
///
/// `delayed_reference_server` is what makes this deterministic rather than a
/// sleep-based race: the fixture writes its own pid and then sleeps for
/// 600ms before it execs the real server, so `connect_all` is reliably still
/// awaiting the handshake — every run, not most runs — when `shutdown`
/// follows it immediately after, with no sleep of its own to contend with.
#[cfg(unix)]
#[tokio::test]
async fn a_connect_that_finishes_after_shutdown_does_not_revive_the_session() {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let pidfile = scratch.path().join("pid");
    let config = delayed_reference_server("fixture", &pidfile, Duration::from_millis(600));
    let servers = McpServers::new(config.mcp.clone(), Path::new("."));

    let connecting = {
        let servers = Arc::clone(&servers);
        tokio::spawn(async move { servers.connect_all().await })
    };
    // No sleep here: the only clock this race depends on is the fixture's
    // own, and shutdown's whole critical path — take the lock, drain an
    // empty map, release it — is over long before that clock reads 600ms.
    servers.shutdown().await;
    connecting.await.expect("connect_all does not panic");

    assert!(
        !servers
            .status()
            .get("fixture")
            .is_some_and(|status| matches!(status, McpStatus::Connected)),
        "a connection that finishes after shutdown must never be installed"
    );
    assert!(
        servers.process_groups().is_empty(),
        "and its process group must not be reachable through this session either"
    );

    let pid: u32 = tokio::time::timeout(READY, async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the fixture always gets far enough to write its pid");

    for _ in 0..100 {
        if !running(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    panic!("the MCP server outlived the shutdown that raced its connect (pid {pid})");
}
