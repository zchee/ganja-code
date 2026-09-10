//! The only `claude` any test in this workspace runs.
//!
//! **No test spawns the user's own CLI.** A live turn spends money on a real
//! account, and eight of the D556 recording's sixteen paid runs were refused
//! by a vendor safeguard; adding to that count from a test suite is the one
//! thing that must be impossible. So this plays back a script, records what
//! it was spawned with and what crossed its stdio, and refuses the argv the
//! real CLI refuses.
//!
//! # It refuses what the real one refuses
//!
//! A double that accepted everything would let a regression in the argv
//! builder pass every suite. This one exits 1 on a missing `--verbose`, on
//! any never-list token (`--resume` included), and on a `--permission-mode`
//! value other than `manual`, each with the sentence the real CLI uses — so
//! posture C is enforced by the double as well as by `argv_tests.rs`, in two
//! independent places.
//!
//! # Two entry points
//!
//! [`main`] is for a re-exec'd test binary: `tests/claude_code_spawn.rs` is
//! `harness = false` and checks `GANJA_FAKE_CLAUDE_SCRIPT` before it runs a
//! test, so the wire's **real** spawner can spawn the test binary as the
//! CLI. [`replay`] is the in-process duplex the unit suites drive, which
//! spawns nothing at all.
//!
//! # The side file
//!
//! A **test-owned temporary artifact**: created under the test's own temp
//! directory, read by that test alone, never committed. It is not one of the
//! recording's replay fixtures and none of AC-2.1's scrub rules govern it.
//! One JSON object per process, appended — so a key that spawned twice leaves
//! two lines and a test can count them.
//!
//! Environment names are recorded by **presence only**. No value is ever read,
//! logged or written, which is what lets a test assert `ANTHROPIC_API_KEY` was
//! removed under a parent that had set one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _};

/// Names the script file a re-exec'd test binary plays.
pub const SCRIPT_ENV: &str = "GANJA_FAKE_CLAUDE_SCRIPT";

/// Names the side file it appends its record to.
pub const RECORD_ENV: &str = "GANJA_FAKE_CLAUDE_RECORD";

/// Overrides what `--version` answers, ahead of the script's own field.
///
/// A knob rather than a second script file: `from_env()` runs `--version` at
/// construction, and a test that wanted a different answer for one case would
/// otherwise have to rewrite the script the whole binary shares.
pub const VERSION_ENV: &str = "GANJA_FAKE_CLAUDE_VERSION";

/// Makes the fake write this line to **stderr** and exit 1 before
/// `system/init`, ahead of the script's own `exit_before_init`.
///
/// The arm that decides an `Auth` from a `Transport` reads what the CLI said
/// on the way out, and only a re-exec'd fake has a real stderr to say it on —
/// so this is the knob that makes that arm reachable at all.
pub const STDERR_ENV: &str = "GANJA_FAKE_CLAUDE_STDERR";

/// What the fake answers `--version` with unless a script or [`VERSION_ENV`]
/// says otherwise.
pub const DEFAULT_VERSION: &str = "2.1.263 (Claude Code)";

/// The spelling `system/init.model` carries on a fresh record.
pub const FRESH_SPELLING: &str = "claude-opus-5[1m]";

/// What the real CLI says when `-p` is given without `--verbose`.
pub const NEEDS_VERBOSE: &str =
    "Error: When using --print, --output-format=stream-json requires --verbose";

/// One turn the fake plays.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Turn {
    /// Assistant text blocks, in order.
    pub text: Vec<String>,
    /// Readable thinking blocks. An empty string is the shape a turn with no
    /// `--thinking-display` produces: a signature and no text.
    pub thinking: Vec<String>,
    /// Tools the model calls this turn.
    pub tool_calls: Vec<Call>,
    /// Whether the vendor safeguard refuses this turn.
    ///
    /// The fake then emits `system/model_refusal_no_fallback`, the CLI's own
    /// `API Error:` banner as an `assistant` frame, and a `result` carrying
    /// `is_error: true` with `subtype: "success"` — and exits **1** on EOF,
    /// which is what makes the exit code the last result's `is_error`.
    pub refused: bool,
    /// A `system/model_fallback` naming this model, before the turn's blocks.
    pub fallback: Option<String>,
    /// `system` subtypes to emit and expect to be skipped.
    pub known_system: Vec<String>,
    /// A `system` subtype this build does not know, to prove it is skipped.
    pub unknown_system: Option<String>,
    /// The `model` spelling this turn's `system/init` carries.
    pub init_model: Option<String>,
    /// What the `result` reports.
    pub usage: Usage,
    /// The `result`'s own text.
    pub result: String,
}

/// One tool call a scripted turn makes.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Call {
    /// The `toolu_…` id every part of the call shares.
    pub id: String,
    /// The registry name, bare — the fake prefixes it for the model-facing
    /// side, exactly as the CLI does.
    pub name: String,
    /// The arguments.
    pub input: serde_json::Value,
    /// Whether to send `tools/call` **before** `can_use_tool`, which is the
    /// secondary path. The recording never produced it; the arm exists so the
    /// design absorbs either order.
    pub call_first: bool,
}

/// The four counters a `result` reports.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Usage {
    /// Fresh input tokens.
    pub input_tokens: u64,
    /// Tokens produced.
    pub output_tokens: u64,
    /// Tokens read from cache.
    pub cache_read_input_tokens: u64,
    /// Tokens written to cache.
    pub cache_creation_input_tokens: u64,
}

/// What one fake process plays.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Script {
    /// What `--version` answers. A script naming `2.1.262 (Claude Code)` is
    /// how the floor refusal is driven.
    pub version: Option<String>,
    /// The turns, in order. A `user` frame past the last one is answered by
    /// the last turn again, so a test need not script a turn it does not
    /// assert on.
    pub turns: Vec<Turn>,
    /// Whether to exit before `system/init` — run 7's shape, an argv the CLI
    /// refuses after it has started.
    pub exit_before_init: Option<String>,
    /// Whether to ignore stdin EOF, so the wire's two signal bounds are
    /// reachable.
    pub ignore_eof: bool,
    /// Whether to answer a `user` frame with **nothing at all**, so the
    /// wire's silence watchdog is reachable.
    pub silent: bool,
    /// The models this seat may name, as `(value, displayName)` pairs — the
    /// `initialize` reply's own shape (**D556**, Dv-18).
    ///
    /// **Empty omits the key entirely**, which is what every script written
    /// before this field existed wants: the reply such a script gets is byte
    /// for byte the one it always got. Only a script that names models makes
    /// the CLI answer with any.
    ///
    /// The field names are the recording's own. `claude-code-replay-run1.json`
    /// redacts the value as account telemetry and records the per-entry field
    /// names beside it, of which two matter to a listing: `value` is the id a
    /// request may ask for and `displayName` is the label.
    pub models: Vec<(String, String)>,
}

/// What one fake process saw, appended to the side file as one JSON line.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Record {
    /// The argv, verbatim.
    pub argv: Vec<String>,
    /// The directory it was spawned in.
    pub cwd: String,
    /// Whether each name the wire strips or sets was **present**. Never a
    /// value.
    pub env_present: BTreeMap<String, bool>,
    /// Every signal it received, in order.
    pub signals: Vec<String>,
    /// Every `user` frame's text, in order.
    pub user_frames: Vec<String>,
    /// Every `tools/call` answer's content blocks, in order.
    pub mcp_results: Vec<Vec<String>>,
    /// Every `deny.message`, in order.
    pub deny_messages: Vec<String>,
    /// The `tools/list` result it received: the roster this process was
    /// declared, so a test can see that a fresh record carried a new one.
    pub tools_list: Vec<String>,
    /// The `initialize`'s `systemPrompt`, so a test can see which prompt a
    /// respawn carried. [`None`] is a record on the CLI's own preset.
    pub system_prompt: Option<Vec<String>>,
    /// The `--session-id` it echoed.
    pub session_id: String,
    /// How many of the script's turns this process played.
    ///
    /// A conversation outlives its processes on this wire, so a script is the
    /// **conversation's** answers and each process picks up where the last
    /// one stopped — which is what a fresh record after an eviction, a
    /// refusal or a rewind actually looks like.
    pub turns_played: usize,
    /// What it exited with.
    pub exit: i32,
}

/// Every name the wire is expected to have removed or set, recorded by
/// presence.
///
/// Kept here rather than imported so the double does not depend on the wire
/// it is a double for: a test that asserts against both is comparing two
/// independent statements rather than one restated.
const WATCHED: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "NODE_OPTIONS",
    "DEBUG",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_AGENT_SDK_CLIENT_APP",
    "DISABLE_AUTOUPDATER",
    "DISABLE_TELEMETRY",
    "CLAUDE_CODE_QUESTION_PREVIEW_FORMAT",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

/// Tokens the fake refuses outright, the way the real CLI refuses an option
/// it does not have.
///
/// `--resume` is the one that matters: the wire never builds it, and a build
/// that did would be refused here as well as by `argv_tests.rs`.
const REFUSED_TOKENS: &[&str] = &[
    "--resume",
    "--continue",
    "--bare",
    "--fork-session",
    "--dangerously-skip-permissions",
    "--include-partial-messages",
];

/// Runs as the CLI: the entry point a re-exec'd test binary calls.
///
/// Never returns — it exits the process, because that is what the thing it is
/// standing in for does.
///
/// # Panics
///
/// Panics when [`SCRIPT_ENV`] names a file that cannot be read or parsed,
/// which is a broken test rather than a behaviour under test.
pub async fn main() -> ! {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let script: Script = {
        let path = std::env::var(SCRIPT_ENV).expect("the fake is only run under a script");
        let bytes = std::fs::read(&path).expect("the script file is readable");

        serde_json::from_slice(&bytes).expect("the script file is a Script")
    };
    let side = std::env::var(RECORD_ENV).ok().map(PathBuf::from);

    let session_id = argv
        .iter()
        .position(|token| token == "--session-id")
        .and_then(|at| argv.get(at + 1))
        .cloned()
        .unwrap_or_default();

    let mut record = Record {
        argv: argv.clone(),
        session_id,
        cwd: std::env::current_dir().unwrap_or_default().display().to_string(),
        env_present: WATCHED
            .iter()
            .map(|name| ((*name).to_owned(), std::env::var_os(name).is_some()))
            .collect(),
        ..Record::default()
    };

    // `--version` first: the wire runs it at construction, and a double that
    // did not answer would fail every suite before a frame.
    if argv.iter().any(|token| token == "--version") {
        let named = std::env::var(VERSION_ENV).ok();
        let version = named.as_deref().or(script.version.as_deref()).unwrap_or(DEFAULT_VERSION);
        println!("{version}");
        finish(&side, &mut record, 0);
    }

    if let Some(code) = refuse(&argv) {
        finish(&side, &mut record, code);
    }

    // Run 7's shape, and the `Auth` arm's: an argv the CLI accepted at parse
    // and refused once it started, saying why on stderr.
    if let Some(sentence) =
        std::env::var(STDERR_ENV).ok().or_else(|| script.exit_before_init.clone())
    {
        eprintln!("{sentence}");
        finish(&side, &mut record, 1);
    }

    install_signal_recorders();

    let live = std::sync::Mutex::new(record);
    let code = replay(tokio::io::stdin(), tokio::io::stdout(), &script, &live).await;
    let mut record = live.into_inner().expect("the record is never poisoned");

    finish(&side, &mut record, code);
}

/// The exit code the argv earns, or [`None`] when it is acceptable.
///
/// Each sentence is the real CLI's, so a test reading stderr sees what a
/// person would.
#[must_use]
pub fn refuse(argv: &[String]) -> Option<i32> {
    if !argv.iter().any(|token| token == "--verbose") {
        eprintln!("{NEEDS_VERBOSE}");

        return Some(1);
    }

    if let Some(token) = argv.iter().find(|token| REFUSED_TOKENS.contains(&token.as_str())) {
        eprintln!("error: unknown option '{token}'");

        return Some(1);
    }

    let mode =
        argv.iter().position(|token| token == "--permission-mode").and_then(|at| argv.get(at + 1));
    if let Some(mode) = mode
        && mode != "manual"
    {
        eprintln!(
            "error: option '--permission-mode <mode>' argument '{mode}' is invalid. Allowed \
             choices are manual, acceptEdits, bypassPermissions, plan."
        );

        return Some(1);
    }

    None
}

/// Appends the record and exits.
fn finish(side: &Option<PathBuf>, record: &mut Record, code: i32) -> ! {
    record.exit = code;
    record.signals = signals();
    append(side.as_deref(), record);

    std::process::exit(code);
}

/// One JSON line per process, appended.
pub fn append(side: Option<&Path>, record: &Record) {
    use std::io::Write as _;

    let Some(side) = side else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(side) else {
        return;
    };
    let mut line = serde_json::to_string(record).unwrap_or_default();
    line.push('\n');
    let _ = file.write_all(line.as_bytes());
}

/// Every signal that reached this process, in order.
static SIGNALS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Records `SIGTERM` and `SIGINT` rather than dying of them.
///
/// The wire's own claim is that stdin EOF is the only orderly exit and the two
/// signals are bounds nothing should reach. A fake that died on `SIGTERM`
/// could not tell a test the difference between "no signal was sent" and "one
/// was sent and worked", which is the whole thing AC-3.9 asserts.
fn install_signal_recorders() {
    for (name, kind) in [
        ("SIGTERM", tokio::signal::unix::SignalKind::terminate()),
        ("SIGINT", tokio::signal::unix::SignalKind::interrupt()),
    ] {
        if let Ok(mut signals) = tokio::signal::unix::signal(kind) {
            tokio::spawn(async move {
                while signals.recv().await.is_some() {
                    signalled(name);
                }
            });
        }
    }
}

/// Notes that `name` arrived.
pub fn signalled(name: &str) {
    if let Ok(mut signals) = SIGNALS.lock() {
        signals.push(name.to_owned());
    }
}

/// Every signal this process has received.
#[must_use]
pub fn signals() -> Vec<String> {
    SIGNALS.lock().map(|signals| signals.clone()).unwrap_or_default()
}

// ------------------------------------------------------------ the protocol

/// Plays `script` over one pair of pipes, returning the exit code.
///
/// The in-process door: `ganja-provider`'s unit suites hand it a duplex and
/// spawn nothing at all.
pub async fn replay<R, W>(
    stdin: R,
    stdout: W,
    script: &Script,
    record: &std::sync::Mutex<Record>,
) -> i32
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut fake = Fake {
        lines: tokio::io::BufReader::new(stdin).lines(),
        out: stdout,
        script,
        record,
        next_id: 1,
        next_rpc: 0,
        turn: 0,
        refused_any: false,
        should_dial: false,
        queued: 0,
    };

    fake.run().await;

    i32::from(fake.refused_any)
}

/// One fake process, mid-conversation.
struct Fake<'a, R, W> {
    lines: tokio::io::Lines<tokio::io::BufReader<R>>,
    out: W,
    script: &'a Script,
    record: &'a std::sync::Mutex<Record>,
    next_id: u64,
    next_rpc: u64,
    turn: usize,
    refused_any: bool,
    should_dial: bool,
    queued: usize,
}

impl<R, W> Fake<'_, R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Reads until stdin ends.
    async fn run(&mut self) {
        // Run 7's shape: an argv the CLI accepted at parse and refused once
        // it started. It says its piece on stderr and exits before
        // `system/init`.
        if let Some(sentence) = &self.script.exit_before_init {
            eprintln!("{sentence}");

            return;
        }

        loop {
            let Some(frame) = self.recv().await else {
                // Stdin EOF. A script may ignore it, so the wire's two signal
                // bounds are reachable by a test that wants them.
                if self.script.ignore_eof {
                    std::future::pending::<()>().await;
                }

                return;
            };

            match frame["type"].as_str() {
                Some("control_request") => {
                    self.control(&frame).await;
                    if std::mem::take(&mut self.should_dial) {
                        self.dial().await;
                    }
                    // A prompt that arrived while the dial was in flight is a
                    // turn owed, not a line to drop.
                    while self.queued > 0 {
                        self.queued -= 1;
                        self.play().await;
                    }
                }
                Some("user") => {
                    let text = frame["message"]["content"].as_str().unwrap_or_default().to_owned();
                    self.edit(|record| record.user_frames.push(text));
                    self.play().await;
                }
                // The CLI echoes every `control_response` it receives back on
                // stdout, verbatim, `request_id` and all (M7, five of five in
                // the recording). It is what makes a wire's drop-by-id
                // exercised by every scripted turn rather than by one test.
                Some("control_response") => {
                    self.send(&frame).await;
                    self.note(&frame);
                }
                _ => {}
            }
        }
    }

    /// Something the wire asked of the CLI.
    async fn control(&mut self, frame: &serde_json::Value) {
        let request_id = frame["request_id"].as_str().unwrap_or_default().to_owned();

        match frame["request"]["subtype"].as_str() {
            Some("initialize") => {
                let prompt = frame["request"]["systemPrompt"].as_array().map(|lines| {
                    lines.iter().map(|line| line.as_str().unwrap_or_default().to_owned()).collect()
                });
                self.edit(|record| record.system_prompt = prompt);

                let mut reply = serde_json::json!({
                    "commands": [],
                    "output_style": "default",
                    "current_permission_mode": "default",
                    "session_state": "idle",
                });
                // Absent unless a script names models, so every suite written
                // before this field sees exactly the reply it always saw
                // (**D556**, Dv-18).
                if !self.script.models.is_empty() {
                    reply["models"] = self
                        .script
                        .models
                        .iter()
                        .map(|(value, name)| {
                            serde_json::json!({"value": value, "displayName": name})
                        })
                        .collect();
                }
                self.answer(&request_id, &reply).await;

                // Unprompted, right after the initialize reply. It says
                // whether an interactive login is in flight, never whether a
                // credential exists.
                self.send(&serde_json::json!({
                    "type": "auth_status",
                    "isAuthenticating": false,
                    "output": [],
                }))
                .await;

                // Dialling is left to `run`: this method is reachable from
                // inside `await_response`, and a dial from there would be
                // this async fn calling itself.
                self.should_dial = frame["request"]["sdkMcpServers"]
                    .as_array()
                    .is_some_and(|servers| !servers.is_empty());
            }
            // `interrupt` ends a turn, not a process, and an idle one is
            // answered with the queue it did not have to clear.
            Some("interrupt") => {
                self.answer(&request_id, &serde_json::json!({"still_queued": []})).await;
            }
            Some("get_usage") => {
                self.answer(&request_id, &serde_json::json!({"subscription_type": "max"})).await;
            }
            _ => {
                self.answer(&request_id, &serde_json::json!({})).await;
            }
        }
    }

    /// The CLI dialling ganja as an MCP server.
    async fn dial(&mut self) {
        let init = self
            .rpc(serde_json::json!({
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "claude-code", "version": DEFAULT_VERSION},
                },
            }))
            .await;

        // Only a server declaring `capabilities.tools` is asked for a roster.
        let lists_tools =
            init.as_ref().is_some_and(|reply| !reply["result"]["capabilities"]["tools"].is_null());

        self.notify(serde_json::json!({"method": "notifications/initialized"})).await;

        if lists_tools {
            let listed = self.rpc(serde_json::json!({"method": "tools/list"})).await;
            if let Some(tools) =
                listed.as_ref().and_then(|reply| reply["result"]["tools"].as_array())
            {
                let listed: Vec<String> = tools
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
                    .collect();
                self.edit(|record| record.tools_list = listed);
            }
        }
    }

    /// Plays the turn a `user` frame opened.
    async fn play(&mut self) {
        if self.script.silent {
            return;
        }

        let turn = self
            .script
            .turns
            .get(self.turn)
            .or_else(|| self.script.turns.last())
            .cloned()
            .unwrap_or_default();
        self.turn += 1;
        let played = self.turn;
        self.edit(|record| record.turns_played = played);

        // Re-emitted at the head of **every** turn, differing in `uuid`
        // alone: a wire must not treat it as once-per-process.
        let model = turn.init_model.clone().unwrap_or_else(|| FRESH_SPELLING.to_owned());
        let uuid = self.mint();
        self.send(&serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": self.record.lock().ok().map(|record| record.session_id.clone()),
            "model": model,
            "tools": [],
            "capabilities": ["interrupt_receipt_v1", "interrupt_cancel_queued_v1", "msg_lifecycle_v1"],
            "uuid": uuid,
        }))
        .await;

        // The `--replay-user-messages` echo, which arrives with the turn.
        self.send(&serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": self.record.lock().ok().and_then(|record| record.user_frames.last().cloned())},
            "isReplay": true,
        }))
        .await;

        if let Some(fallback) = &turn.fallback {
            self.send(&serde_json::json!({
                "type": "system",
                "subtype": "model_fallback",
                "model": fallback,
            }))
            .await;
        }

        for subtype in &turn.known_system {
            self.send(&serde_json::json!({
                "type": "system",
                "subtype": subtype,
                "estimated_tokens": 12,
                "estimated_tokens_delta": 4,
                "status": "requesting",
            }))
            .await;
        }
        if let Some(subtype) = &turn.unknown_system {
            self.send(&serde_json::json!({"type": "system", "subtype": subtype})).await;
        }

        if turn.refused {
            self.refuse().await;

            return;
        }

        for thinking in &turn.thinking {
            self.send(&serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "thinking", "thinking": thinking, "signature": "<sig>"}]},
            }))
            .await;
        }

        if turn.tool_calls.is_empty() {
            for text in &turn.text {
                self.send(&serde_json::json!({
                    "type": "assistant",
                    "message": {"content": [{"type": "text", "text": text}]},
                }))
                .await;
            }
        } else {
            self.call(&turn).await;

            return;
        }

        self.result(&turn).await;
    }

    /// The tool-calling half of a turn.
    async fn call(&mut self, turn: &Turn) {
        // One `assistant` frame carrying every `tool_use` block of the step:
        // it is what says how many asks to expect.
        let blocks: Vec<serde_json::Value> = turn
            .tool_calls
            .iter()
            .map(|call| {
                serde_json::json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": format!("mcp__ganja__{}", call.name),
                    "input": call.input,
                })
            })
            .collect();
        self.send(&serde_json::json!({"type": "assistant", "message": {"content": blocks}})).await;

        for call in &turn.tool_calls {
            if call.call_first {
                // The secondary path: the call itself is the ask.
                self.tools_call(call).await;
                self.ask(call).await;
            } else {
                let allowed = self.ask(call).await;
                if allowed {
                    self.tools_call(call).await;
                }
            }
        }

        // A tool-calling turn still ends in words: the model reads what the
        // tool returned and then says something about it.
        for text in &turn.text {
            self.send(&serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": text}]},
            }))
            .await;
        }

        self.result(turn).await;
    }

    /// `can_use_tool`, and whether it came back `allow`.
    async fn ask(&mut self, call: &Call) -> bool {
        let request_id = self.mint();
        self.send(&serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": format!("mcp__ganja__{}", call.name),
                "input": call.input,
                "tool_use_id": call.id,
            },
        }))
        .await;

        let Some(answer) = self.await_response(&request_id).await else {
            return false;
        };

        answer["response"]["behavior"].as_str() == Some("allow")
    }

    /// `tools/call`, with the correlator every answer is matched back by.
    async fn tools_call(&mut self, call: &Call) {
        let reply = self
            .rpc(serde_json::json!({
                "method": "tools/call",
                "params": {
                    "name": call.name,
                    "arguments": call.input,
                    "_meta": {"claudecode/toolUseId": call.id, "progressToken": 2},
                },
            }))
            .await;

        if let Some(content) =
            reply.as_ref().and_then(|reply| reply["result"]["content"].as_array())
        {
            let blocks: Vec<String> = content
                .iter()
                .map(|block| block["text"].as_str().unwrap_or_default().to_owned())
                .collect();
            self.edit(|record| record.mcp_results.push(blocks));
        }
    }

    /// The vendor safeguard refusing this turn.
    async fn refuse(&mut self) {
        self.refused_any = true;

        let refused_uuid = self.mint();
        self.send(&serde_json::json!({
            "type": "system",
            "subtype": "model_refusal_no_fallback",
            "original_model": FRESH_SPELLING,
            "api_refusal_category": "reasoning_extraction",
            "api_refusal_explanation": "This request was blocked as it seems to violate \
                 Anthropic's Terms of Service restrictions on reverse engineering or duplicating \
                 model outputs. To learn more, visit \
                 https://www.anthropic.com/legal/commercial-terms.",
            "refused_user_message_uuid": refused_uuid,
        }))
        .await;

        // The CLI's own `API Error:` banner, which is not model speech — the
        // wire must not emit it as text.
        self.send(&serde_json::json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "text",
                "text": "API Error: Opus 5 (1M context)'s safeguards flagged this message.\n\n\
                     Details: `[reasoning_extraction]`",
            }]},
        }))
        .await;

        // `subtype: "success"` with `is_error: true` — the pair a wire reading
        // `subtype` gets wrong.
        self.send(&serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": true,
            "stop_reason": "refusal",
            "terminal_reason": "api_error",
            "result": "API Error",
            "usage": {"input_tokens": 0, "output_tokens": 0},
        }))
        .await;
    }

    /// The turn's own `result`.
    async fn result(&mut self, turn: &Turn) {
        self.send(&serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "stop_reason": "end_turn",
            "result": turn.result,
            "usage": {
                "input_tokens": turn.usage.input_tokens,
                "output_tokens": turn.usage.output_tokens,
                "cache_read_input_tokens": turn.usage.cache_read_input_tokens,
                "cache_creation_input_tokens": turn.usage.cache_creation_input_tokens,
            },
        }))
        .await;
    }

    // ------------------------------------------------------------ plumbing

    /// Sends one JSON-RPC request inside an `mcp_message` and waits for the
    /// reply.
    async fn rpc(&mut self, mut message: serde_json::Value) -> Option<serde_json::Value> {
        message["jsonrpc"] = serde_json::Value::from("2.0");
        message["id"] = serde_json::Value::from(self.next_rpc);
        self.next_rpc += 1;

        let request_id = self.mint();
        self.send(&serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {"subtype": "mcp_message", "server_name": "ganja", "message": message},
        }))
        .await;

        let answer = self.await_response(&request_id).await?;

        Some(answer["response"]["mcp_response"].clone())
    }

    /// A JSON-RPC notification, which earns the empty success and no reply.
    async fn notify(&mut self, mut message: serde_json::Value) {
        message["jsonrpc"] = serde_json::Value::from("2.0");

        let request_id = self.mint();
        self.send(&serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {"subtype": "mcp_message", "server_name": "ganja", "message": message},
        }))
        .await;

        let _ = self.await_response(&request_id).await;
    }

    /// Reads until the answer to `request_id` arrives, echoing every
    /// `control_response` on the way — the echo included.
    async fn await_response(&mut self, request_id: &str) -> Option<serde_json::Value> {
        loop {
            let frame = self.recv().await?;

            match frame["type"].as_str() {
                Some("control_response") => {
                    self.send(&frame).await;
                    self.note(&frame);

                    if frame["response"]["request_id"].as_str() == Some(request_id) {
                        return Some(frame["response"].clone());
                    }
                }
                // A cancel can land while an ask is parked, which is exactly
                // the shape the wire's own cancel arm produces.
                Some("control_request") => self.control(&frame).await,
                Some("user") => {
                    let text = frame["message"]["content"].as_str().unwrap_or_default().to_owned();
                    self.edit(|record| record.user_frames.push(text));
                    self.queued += 1;
                }
                _ => {}
            }
        }
    }

    /// One short critical section per write, so a test may read the record
    /// while the fake is still running.
    fn edit(&self, change: impl FnOnce(&mut Record)) {
        if let Ok(mut record) = self.record.lock() {
            change(&mut record);
        }
    }

    /// Records what an answer carried, by kind.
    fn note(&mut self, frame: &serde_json::Value) {
        let response = &frame["response"]["response"];

        if let Some(message) = response["message"].as_str()
            && response["behavior"].as_str() == Some("deny")
        {
            let message = message.to_owned();
            self.edit(|record| record.deny_messages.push(message));
        }
    }

    /// One line in.
    async fn recv(&mut self) -> Option<serde_json::Value> {
        loop {
            let line = self.lines.next_line().await.ok().flatten()?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str(&line) {
                return Some(value);
            }
        }
    }

    /// One line out.
    async fn send(&mut self, value: &serde_json::Value) {
        let mut line = value.to_string();
        line.push('\n');
        let _ = self.out.write_all(line.as_bytes()).await;
        let _ = self.out.flush().await;
    }

    /// A success `control_response`.
    async fn answer(&mut self, request_id: &str, response: &serde_json::Value) {
        self.send(&serde_json::json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": response},
        }))
        .await;
    }

    /// A fresh id for something this side is asking.
    fn mint(&mut self) -> String {
        let id = format!("fake-{}", self.next_id);
        self.next_id += 1;

        id
    }
}

#[cfg(test)]
#[path = "fake_claude_tests.rs"]
mod tests;
