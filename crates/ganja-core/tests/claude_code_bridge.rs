//! The `claude-code` wire end to end: a real [`Engine`] answering a real
//! `claude` CLI's `can_use_tool` over a real duplex (**D556**, W4 of
//! `.omc/plans/2026-09-08-claude-code-wire.md`).
//!
//! The specification is the live recording at
//! `crates/ganja-provider/tests/fixtures/claude-code-replay-run1.json` and the
//! wire W3 landed from it, not this file's prose: where the two disagree the
//! recording is right.
//!
//! **Why this suite exists and `ganja-provider`'s own does not cover it.** A
//! `can_use_tool` is answered by the *engine* — the permission ladder, the
//! hooks, the tool registry, the transcript — while the frames that carry it
//! are the *wire*'s. `ganja-provider` may not name `ganja-core`
//! (`depgate.toml`), so the only party that can drive both ends of one round
//! trip is this crate. What it drives is never a live `claude`: the far end of
//! every duplex here is [`ganja_testkit::fake_claude::replay`], and no case in
//! this file names a binary the wire could spawn.
//!
//! **A plain harness, not `harness = false`** (Dv-20): every `harness = false`
//! binary in this tree holds exactly one test because it mutates process-wide
//! state or re-execs itself as a child, and nothing here does either — the
//! double is handed its home explicitly and its binary is never spawned. What
//! that buys is the thing several cases below need: `start_paused = true`
//! gives each of them its own current-thread runtime with its own clock, so
//! one case advancing time cannot move another's timers.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ganja_core::Engine;
use ganja_core::permission::{Action, Permissions, Rule};
use ganja_core::protocol::{Command, Event, PermissionReply};
use ganja_core::provider::claude_code::process::{ChildIo, Signal, Spawner};
use ganja_core::provider::claude_code::{ClaudeCodeProvider, argv, binding};
use ganja_core::provider::{Provider, ProviderError};
use ganja_core::tool::Registry;
use ganja_testkit::fake_claude::{self, Call, Record, Script, Turn};
use ganja_testkit::{LogCapture, RecorderTool, drain, drain_answering};

/// The version the double reports, so nothing here trips the floor.
const VERSION: &str = "2.1.263 (Claude Code)";

/// The tool every case here gates.
const TOOL: &str = "lookup";

/// What that tool answers, so an assertion about the CLI's `tools/call` reply
/// has a string to look for.
const ANSWER: &str = "the answer";

/// The idle bound the eviction cases run under.
///
/// Short, and it changes nothing about what is proved: the bound is measured
/// on `tokio::time::Instant`, so a paused runtime reaches it by arithmetic
/// rather than by waiting, and a smaller number is only easier to read.
const BOUND: std::time::Duration = std::time::Duration::from_secs(30);

// ---------------------------------------------------------------------------
// The double
// ---------------------------------------------------------------------------

/// One process the wire spawned.
struct Spawned {
    argv: Vec<String>,
    cwd: PathBuf,
    record: Arc<Mutex<Record>>,
}

/// A [`Spawner`] whose children are [`fake_claude::replay`] over an in-process
/// duplex.
///
/// The same shape `ganja-provider`'s own `FakeCli` has and deliberately not
/// the same code: that one is `pub(crate)` to the crate that owns the wire,
/// and widening it would put a test double on the public surface of the very
/// thing whose public surface is the point. What *is* shared is what matters —
/// the script format and the record, both [`ganja_testkit`]'s, so the two
/// suites cannot drift into scripting different CLIs.
struct FakeCli {
    script: Script,
    spawns: Arc<Mutex<Vec<Spawned>>>,
}

impl FakeCli {
    fn new(script: Script) -> Arc<Self> {
        Arc::new(Self { script, spawns: Arc::new(Mutex::new(Vec::new())) })
    }

    /// How many processes the wire has spawned.
    fn count(&self) -> usize {
        self.spawns.lock().expect("the spawn list is never poisoned").len()
    }

    /// The `n`th spawn's argv.
    fn argv(&self, at: usize) -> Vec<String> {
        self.spawns.lock().expect("the spawn list is never poisoned")[at].argv.clone()
    }

    /// The indices of the spawns that served the **conversation**, in order.
    ///
    /// A one-shot — a title request, a compaction summary — runs in a scratch
    /// directory of its own, which is exactly what makes it invisible to the
    /// wire's table; the same fact separates it here. Without this a case
    /// asserting "the conversation opened a second record" would be counting a
    /// title request the engine happened to fire first, and would pass or fail
    /// on a race.
    fn conversation(&self) -> Vec<usize> {
        let spawns = self.spawns.lock().expect("the spawn list is never poisoned");
        let Some(first) = spawns.first() else {
            return Vec::new();
        };

        spawns
            .iter()
            .enumerate()
            .filter(|(_, spawn)| spawn.cwd == first.cwd)
            .map(|(at, _)| at)
            .collect()
    }

    /// The `n`th spawn's record, copied.
    fn record(&self, at: usize) -> Record {
        let spawns = self.spawns.lock().expect("the spawn list is never poisoned");

        spawns[at].record.lock().expect("the record is never poisoned").clone()
    }
}

impl Spawner for FakeCli {
    fn spawn(
        &self,
        _bin: &Path,
        argv: &[OsString],
        env: &argv::ChildEnv,
    ) -> Result<ChildIo, ProviderError> {
        let spelled: Vec<String> =
            argv.iter().map(|token| token.to_string_lossy().into_owned()).collect();

        let session_id = spelled
            .iter()
            .position(|token| token == "--session-id")
            .and_then(|at| spelled.get(at + 1))
            .cloned()
            .unwrap_or_default();

        let record = Arc::new(Mutex::new(Record {
            argv: spelled.clone(),
            cwd: env.cwd.display().to_string(),
            session_id,
            ..Record::default()
        }));

        let (wire_stdin, cli_stdin) = tokio::io::duplex(1 << 18);
        let (cli_stdout, wire_stdout) = tokio::io::duplex(1 << 18);
        let (exited, wait) = tokio::sync::oneshot::channel();

        // A conversation outlives its processes on this wire, so the script is
        // the **conversation's** answers and each process picks up where the
        // last one stopped. Without that, the fresh record after an eviction
        // or a rewind would replay the turn that caused it.
        let played: usize = self
            .spawns
            .lock()
            .expect("the spawn list is never poisoned")
            .iter()
            .filter(|spawn| spawn.cwd == env.cwd)
            .map(|spawn| spawn.record.lock().expect("the record is never poisoned").turns_played)
            .sum();
        let mut script = self.script.clone();
        script.turns = script.turns.split_off(played.min(script.turns.len()));

        self.spawns.lock().expect("the spawn list is never poisoned").push(Spawned {
            argv: spelled,
            cwd: env.cwd.clone(),
            record: Arc::clone(&record),
        });

        tokio::spawn(async move {
            let code = fake_claude::replay(cli_stdin, cli_stdout, &script, &record).await;
            record.lock().expect("the record is never poisoned").exit = code;
            let _ = exited.send(code);
        });

        Ok(ChildIo {
            stdin: Box::new(wire_stdin),
            stdout: Box::new(wire_stdout),
            stderr: None,
            exit: Box::pin(async move {
                let code = wait.await.unwrap_or_default();

                Ok(exit_status(code))
            }),
            kill: Box::new(|_: Signal| {}),
        })
    }
}

/// An `ExitStatus` carrying `code`.
fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;

    std::process::ExitStatus::from_raw(code << 8)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A wire on `cli`, with its per-key scratch and bindings under `home`.
///
/// Handed back behind an [`Arc`] the caller keeps, because two of the things
/// this suite asserts are the *wire's* own state rather than the engine's —
/// how many processes it is holding, and whether a teardown closed them — and
/// an engine takes its provider by value.
fn wired(cli: &Arc<FakeCli>, home: &Path) -> Arc<ClaudeCodeProvider> {
    Arc::new(ClaudeCodeProvider::with_parts(
        PathBuf::from("/nonexistent/claude"),
        VERSION.to_owned(),
        Arc::clone(cli) as Arc<dyn Spawner>,
        binding::Paths::under(home),
    ))
}

/// A script whose turns each call [`TOOL`] once and then answer `text`.
fn calls_the_tool(answers: &[&str]) -> Script {
    Script {
        turns: answers
            .iter()
            .enumerate()
            .map(|(nth, answer)| Turn {
                tool_calls: vec![Call {
                    id: format!("toolu_{nth}"),
                    name: TOOL.to_owned(),
                    input: serde_json::json!({"key": "alpha"}),
                    call_first: false,
                }],
                text: vec![(*answer).to_owned()],
                result: (*answer).to_owned(),
                ..Turn::default()
            })
            .collect(),
        ..Script::default()
    }
}

/// One rule over [`TOOL`], so a case says what it gates in one line.
fn rule(action: Action) -> Permissions {
    let mut permissions = Permissions::default();
    permissions.set_baseline(vec![Rule {
        permission: TOOL.to_owned(),
        pattern: "*".to_owned(),
        action,
    }]);

    permissions
}

/// A prompt, with the four optional fields every frontend leaves empty.
fn prompt(text: &str) -> Command {
    Command::SendPrompt {
        text: text.to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        session_mentions: Vec::new(),
        peers: Vec::new(),
    }
}

/// An engine on `provider`, holding the recorder tool, gated by `permissions`.
fn seated(
    provider: &Arc<ClaudeCodeProvider>,
    tool: Arc<RecorderTool>,
    permissions: Permissions,
) -> Engine {
    Engine::new(
        Arc::clone(provider) as Arc<dyn Provider>,
        ganja_core::provider::claude_code::DEFAULT_MODEL,
        Arc::new(Registry::new(vec![tool])),
        permissions,
    )
}

/// A script whose turns answer with text alone and call nothing.
fn says(answers: &[&str]) -> Script {
    Script {
        turns: answers
            .iter()
            .map(|answer| Turn {
                text: vec![(*answer).to_owned()],
                result: (*answer).to_owned(),
                ..Turn::default()
            })
            .collect(),
        ..Script::default()
    }
}

/// Waits until `cli` has spawned `want` processes, or gives up.
///
/// The title request is a detached task, so a case that asserted on the spawn
/// count straight after the turn would be asserting on a race. Bounded rather
/// than unbounded because a failure here should read as "the second spawn
/// never happened", not as a hung suite.
async fn spawns_reach(cli: &Arc<FakeCli>, want: usize) {
    for _ in 0..2_000 {
        if cli.count() >= want {
            return;
        }
        tokio::task::yield_now().await;
    }

    panic!("wanted {want} spawns, the wire made {}", cli.count());
}

/// Reads the stream up to the first permission dialog and hands its id back,
/// leaving the turn held open on it.
async fn held_at_dialog(
    events: &mut futures::stream::BoxStream<'static, Event>,
) -> ganja_core::protocol::PermissionId {
    use futures::StreamExt as _;

    loop {
        let event = events.next().await.expect("the dialog should arrive before the stream ends");
        if let Event::PermissionRequested { id, .. } = event {
            return id;
        }
    }
}

// ---------------------------------------------------------------------------
// The permission round trip
// ---------------------------------------------------------------------------

/// **AC-4.2.** Under an *ask* rule one `can_use_tool` raises exactly one
/// dialog, the answer runs the tool exactly once, and the CLI's `tools/call`
/// reply carries the tool's own output.
///
/// This is the whole bridge in one turn: the ask is the wire's, the dialog is
/// the engine's, the execution is the registry's, and the answer goes back out
/// over the same process.
#[tokio::test]
async fn one_ask_raises_one_dialog_and_the_answer_runs_the_tool_exactly_once() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(calls_the_tool(&["found it"]));
    let (tool, calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let engine = seated(&wired(&cli, home.path()), tool, rule(Action::Ask));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("look it up")).await.expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let dialogs =
        seen.iter().filter(|event| matches!(event, Event::PermissionRequested { .. })).count();
    assert_eq!(dialogs, 1, "one ask is one dialog, not one per frame");
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "answered once, so run once"
    );

    let record = cli.record(0);
    let [result] = record.mcp_results.as_slice() else {
        panic!("one call is one answer, got {:?}", record.mcp_results);
    };
    assert!(
        result.iter().any(|block| block.contains(ANSWER)),
        "the CLI was handed the tool's own output, got {result:?}"
    );
    assert!(record.deny_messages.is_empty(), "an allowed call is refused nowhere");
}

/// **AC-4.3.** A *deny* answer reaches the CLI as a `deny{message}` and no
/// `tools/call` follows it.
///
/// The negative half is the point: a wire that refused the model in words
/// while still running the tool would satisfy every assertion about the
/// message.
#[tokio::test]
async fn a_denied_dialog_reaches_the_cli_as_a_refusal_and_runs_nothing() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(calls_the_tool(&["never mind"]));
    let (tool, calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let engine = seated(&wired(&cli, home.path()), tool, rule(Action::Ask));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("look it up")).await.expect("an idle engine accepts a prompt");
    drain_answering(&engine, &mut events, PermissionReply::Reject).await;

    let record = cli.record(0);
    let [denied] = record.deny_messages.as_slice() else {
        panic!("one refused call is one deny, got {:?}", record.deny_messages);
    };
    assert!(
        ganja_tool::permission_text::is_refusal(denied),
        "the CLI is told this was a refusal, not a failed tool: {denied:?}"
    );
    assert!(record.mcp_results.is_empty(), "and no tools/call followed it");
    assert!(calls.lock().expect("the call log is never poisoned").is_empty(), "so nothing ran");
}

/// **AC-4.6**, the execution-site invariant. The CLI never sees a
/// `tools/call` before the engine's dialog has been answered.
///
/// `drain_answering` answers as it goes, which would hide the ordering, so
/// this case holds the dialog open, reads the record *while it is held*, and
/// only then answers. What that pins is that the wire waits — a bridge which
/// executed first and asked afterwards would satisfy AC-4.2 exactly.
#[tokio::test]
async fn the_cli_sees_no_tools_call_until_the_dialog_has_been_answered() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(calls_the_tool(&["found it"]));
    let (tool, calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let engine = seated(&wired(&cli, home.path()), tool, rule(Action::Ask));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("look it up")).await.expect("an idle engine accepts a prompt");
    let dialog = held_at_dialog(&mut events).await;

    assert!(
        cli.record(0).mcp_results.is_empty(),
        "the answer is what releases the call, so nothing has answered the CLI yet"
    );
    assert!(calls.lock().expect("the call log is never poisoned").is_empty(), "and nothing ran");

    engine
        .send(Command::ReplyPermission { id: dialog, reply: PermissionReply::Once })
        .await
        .expect("a reply is never refused");
    drain(&mut events).await;

    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "and the answer is what ran it"
    );
    assert_eq!(cli.record(0).mcp_results.len(), 1, "and what answered the CLI");
}

// ---------------------------------------------------------------------------
// What the wire holds, and what a person is told about it
// ---------------------------------------------------------------------------

/// **AC-4.6b.** A title request after a first turn spawns a *second* process
/// that exits at its own `result`, while the conversation's process stays
/// held — `held_entries` reads 1 throughout.
///
/// The count is the assertion. A one-shot that entered the table would be
/// indistinguishable from the conversation's own entry by every other
/// observable, and would then be evicted, closed and paid for as if a
/// conversation had lost its process.
#[tokio::test]
async fn a_title_request_spawns_a_one_shot_beside_the_held_conversation() {
    let home = ganja_testkit::temp_dir();
    let data = ganja_testkit::temp_dir();
    let cli = FakeCli::new(says(&["found it", "A Title"]));
    let (tool, _calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let provider = wired(&cli, home.path());
    let engine = Engine::persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        ganja_core::provider::claude_code::DEFAULT_MODEL,
        Arc::new(Registry::new(vec![tool])),
        rule(Action::Allow),
        ganja_core::storage::Storage::open(data.path().join("sessions.db")),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("look it up")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    assert_eq!(provider.held_entries(), 1, "the conversation's own process, held");

    spawns_reach(&cli, 2).await;
    assert_eq!(
        provider.held_entries(),
        1,
        "and still one: a one-shot enters no table entry of its own"
    );

    let one_shot = cli.record(1);
    assert_ne!(one_shot.cwd, cli.record(0).cwd, "it ran in a scratch directory of its own");
    assert!(
        !one_shot.argv.iter().any(|token| token == "--resume"),
        "and like every other spawn on this wire it resumes nothing: {:?}",
        one_shot.argv
    );
}

/// **AC-4.16**, first half. After a turn the wire reports what the vendor
/// actually served, and a scripted `system/model_fallback` moves that spelling
/// and nothing else.
///
/// `requested` stays ganja's own word for what the session asked — `default`,
/// which on this wire means "no `--model` at all" — because the request did
/// not change when the vendor's answer did.
#[tokio::test]
async fn the_served_model_is_reported_after_a_turn_and_a_fallback_moves_only_it() {
    let home = ganja_testkit::temp_dir();
    let mut script = says(&["found it", "still here"]);
    script.turns[0].init_model = Some(fake_claude::FRESH_SPELLING.to_owned());
    script.turns[1].fallback = Some("claude-sonnet-5".to_owned());
    let cli = FakeCli::new(script);
    let (tool, _calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let provider = wired(&cli, home.path());
    let engine = seated(&provider, tool, rule(Action::Allow));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    assert_eq!(provider.served_model(), None, "nothing has been served yet");

    engine.send(prompt("hello")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let served = provider.served_model().expect("a turn has been served");
    assert_eq!(
        served.requested,
        ganja_core::provider::claude_code::DEFAULT_MODEL,
        "ganja's own word for what was asked"
    );
    assert_eq!(served.served, fake_claude::FRESH_SPELLING, "and the vendor's for what answered");

    engine.send(prompt("again")).await.expect("the engine is idle again");
    drain(&mut events).await;

    let after = provider.served_model().expect("the second turn was served too");
    assert_eq!(after.served, "claude-sonnet-5", "a fallback moves the vendor's spelling");
    assert_eq!(after.requested, served.requested, "and nothing about what was asked for");
}

/// **Dv-14.** `Engine::shutdown_provider` closes every held process — the CLI
/// sees EOF and the table empties.
///
/// The teardown door exists because a `ganja` that exits without it leaves up
/// to `HELD_CAP` authenticated node runtimes alive until their own idle bound
/// closes them. Reached through the `Arc<dyn Provider>` an engine already
/// holds, so no exit path names a concrete wire.
#[tokio::test]
async fn shutting_the_engine_down_closes_every_held_process() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(says(&["found it"]));
    let (tool, _calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let provider = wired(&cli, home.path());
    let engine = seated(&provider, tool, rule(Action::Allow));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("hello")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    assert_eq!(provider.held_entries(), 1, "one conversation, one held process");

    engine.shutdown_provider().await;

    assert_eq!(provider.held_entries(), 0, "the table is empty");
    assert_eq!(
        cli.record(0).exit,
        0,
        "and the child really ended rather than being abandoned to its idle bound"
    );

    // Idempotent, like every other shutdown door on the engine.
    engine.shutdown_provider().await;
    assert_eq!(provider.held_entries(), 0);
}

/// **AC-4.16**, second half. An idle eviction fills the wire's eviction slot,
/// and the fresh record that pays for it empties the slot again.
///
/// The slot is live across exactly that gap, which is what lets a frontend
/// write a notice on one edge and take it down on the other. Driven under
/// `start_paused`, so the bound is proved rather than waited out: at real time
/// this case would take the whole idle bound.
///
/// What the eviction costs is the point of the sentence a person reads: the
/// fresh record opens with an assistant-free preamble, so the model no longer
/// has its own earlier replies.
#[tokio::test(start_paused = true)]
async fn an_idle_eviction_fills_the_slot_and_the_fresh_record_empties_it() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(says(&["found it", "still here"]));
    let (tool, _calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let provider = Arc::new(
        ClaudeCodeProvider::with_parts(
            PathBuf::from("/nonexistent/claude"),
            VERSION.to_owned(),
            Arc::clone(&cli) as Arc<dyn Spawner>,
            binding::Paths::under(home.path()),
        )
        .with_idle_bound(BOUND),
    );
    let engine = seated(&provider, tool, rule(Action::Allow));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("hello")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    assert_eq!(provider.held_entries(), 1, "the conversation is holding a process");
    assert_eq!(provider.last_eviction(), None, "and nothing has been evicted");

    tokio::time::advance(BOUND + std::time::Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    for _ in 0..2_000 {
        if provider.last_eviction().is_some() {
            break;
        }
        tokio::task::yield_now().await;
    }

    let evicted = provider.last_eviction().expect("the bound closed the idle process");
    assert_eq!(provider.held_entries(), 0, "and the table gave the entry up");
    assert!(!evicted.key.is_empty(), "the wire's own key, which the notice never names");

    engine.send(prompt("still there?")).await.expect("the engine is idle");
    drain(&mut events).await;

    assert_eq!(
        provider.last_eviction(),
        None,
        "the fresh record that paid for it empties the slot"
    );
    assert_eq!(cli.count(), 2, "and it really is a fresh process rather than a resumed one");
    assert!(
        !cli.argv(1).iter().any(|token| token == "--resume"),
        "which this wire never resumes: {:?}",
        cli.argv(1)
    );
}

/// **AC-4.3b.** A `PreToolUse` hook that exits 2 reaches the CLI as a
/// `deny{message}` carrying the hook's own reason, and no `tools/call`
/// follows.
///
/// The end-to-end pin that a hook refusal and a permission refusal read alike
/// on this wire. They travel the same field and are recognised by the same
/// predicate, so a wire that could tell one from the other would be reporting
/// a refusal as a failed tool — which is exactly what `HOOK_REFUSED_PREFIX`
/// and its literal pin in `session_tests.rs` exist to prevent.
#[tokio::test]
async fn a_hook_that_exits_two_reaches_the_cli_as_a_refusal_carrying_its_reason() {
    const REASON: &str = "the repo forbids touching vendored files";

    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(calls_the_tool(&["never mind"]));
    let (tool, calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let hooks = ganja_core::hook::Hooks::new(
        &hook_block(&format!("echo '{REASON}' >&2; exit 2")),
        home.path(),
    )
    .expect("the block describes one hook");
    // Allowed by rule, so the only thing that can refuse this call is the
    // hook — which is what makes the assertion below about the hook.
    let engine = seated(&wired(&cli, home.path()), tool, rule(Action::Allow)).with_hooks(hooks);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("look it up")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let record = cli.record(0);
    let [denied] = record.deny_messages.as_slice() else {
        panic!("one blocked call is one deny, got {:?}", record.deny_messages);
    };
    assert!(
        denied.contains(REASON),
        "the hook's own words reach the model, which is the point of blocking with a \
         message: {denied:?}"
    );
    assert!(
        ganja_tool::permission_text::is_refusal(denied),
        "and the wire reads it as a refusal rather than as a failed tool: {denied:?}"
    );
    assert!(record.mcp_results.is_empty(), "no tools/call followed it");
    assert!(calls.lock().expect("the call log is never poisoned").is_empty(), "so nothing ran");
}

/// A `hooks` block running `command` on every `PreToolUse`.
fn hook_block(
    command: &str,
) -> std::collections::BTreeMap<String, Vec<ganja_core::config::HookMatcher>> {
    use ganja_core::config::{HookCommand, HookHandler, HookMatcher};

    std::collections::BTreeMap::from([(
        ganja_core::hook::HookEvent::PreToolUse.name().to_owned(),
        vec![HookMatcher {
            matcher: None,
            hooks: vec![HookHandler::Command(HookCommand {
                command: command.to_owned(),
                timeout: None,
            })],
        }],
    )])
}

// ---------------------------------------------------------------------------
// What opens a fresh record, and what does not
// ---------------------------------------------------------------------------

/// **AC-4.6c**, the `/model` half. A model change mid-conversation closes the
/// held process and opens a fresh record carrying the new `--model`, with a
/// preamble that quotes no assistant text.
///
/// The two values a person *chose* — model and effort — are the only ones that
/// do this. Keeping a chosen model stale would bill the opening model under a
/// status bar saying otherwise, where the machinery nobody picked (the system
/// prompt's hash, the roster) is left alone precisely because nobody picked
/// it.
///
/// What the fresh record costs is the assistant's own earlier words, which is
/// why the preamble is checked line by line rather than merely for its
/// presence.
#[tokio::test]
async fn a_model_switch_opens_a_fresh_record_whose_preamble_quotes_no_assistant() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(calls_the_tool(&["found it", "still here"]));
    let (tool, _calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let provider = wired(&cli, home.path());
    let engine = seated(&provider, tool, rule(Action::Allow));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("look it up")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    assert_eq!(cli.conversation().len(), 1, "one conversation, one process");

    engine
        .send(Command::SwitchModel { model: "opus".to_owned() })
        .await
        .expect("an uncataloged wire serves any spelling");
    engine.send(prompt("and again")).await.expect("the engine is idle");
    drain(&mut events).await;

    let records = cli.conversation();
    let [first, second] = records.as_slice() else {
        panic!("the chosen model changed, so the record should have, got {records:?}");
    };
    assert_eq!(
        cli.record(*first).exit,
        0,
        "and the first process was closed rather than abandoned"
    );

    let argv = cli.argv(*second);
    let at = argv.iter().position(|token| token == "--model").expect("the fresh record names it");
    assert_eq!(argv[at + 1], "opus", "with the model the person chose");
    assert!(!argv.iter().any(|token| token == "--resume"), "and resuming nothing: {argv:?}");

    let opening = first_frame(&cli, *second);
    assert!(opening.contains("[Conversation so far]"), "it carries a preamble: {opening:?}");
    assert_no_assistant_text(&opening);
}

/// **AC-4.6c**, the `/effort` half (**Dv-16**). An effort change
/// mid-conversation closes the held process and opens a fresh record carrying
/// the new `--effort`.
///
/// The door this drives is the one Dv-16 opened: `switch_effort` used to
/// refuse any named effort on an uncataloged provider, which left this wire's
/// five documented levels reachable by nothing. They are the *wire's* roster
/// rather than any model's — the CLI's own flag publishes them — so
/// `provider::efforts_for` answers them where the catalog has no row, and the
/// engine, the request assembly and the TUI's chooser all read that one
/// definition.
#[tokio::test]
async fn an_effort_switch_opens_a_fresh_record_carrying_the_new_effort() {
    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(says(&["found it", "still here"]));
    let (tool, _calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let (log, _guard) = capturing();
    let provider = wired(&cli, home.path());
    let engine = seated(&provider, tool, rule(Action::Allow));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("look it up")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    assert_eq!(cli.conversation().len(), 1, "one conversation, one process");
    assert!(
        !cli.argv(0).iter().any(|token| token == "--effort"),
        "and it opened under no effort at all: {:?}",
        cli.argv(0)
    );

    engine
        .send(Command::SwitchEffort { effort: Some("xhigh".to_owned()) })
        .await
        .expect("this wire's own roster is what the door reads");
    engine.send(prompt("and again")).await.expect("the engine is idle");
    drain(&mut events).await;

    let records = cli.conversation();
    let [first, second] = records.as_slice() else {
        panic!("the chosen effort changed, so the record should have, got {records:?}");
    };
    assert_eq!(cli.record(*first).exit, 0, "the first process was closed rather than abandoned");

    let argv = cli.argv(*second);
    let at = argv.iter().position(|token| token == "--effort").expect("the fresh record names it");
    assert_eq!(argv[at + 1], "xhigh", "with the effort the person chose");
    assert!(!argv.iter().any(|token| token == "--resume"), "and resuming nothing: {argv:?}");
    assert!(
        log.logged().contains(r#"reason="effort""#),
        "and the wire says which of the two chosen values moved:\n{}",
        log.logged()
    );

    let opening = first_frame(&cli, *second);
    assert_no_assistant_text(&opening);
}

/// The other half of Dv-16's ruling: nothing moved for any other wire.
///
/// Cursor is uncataloged too and publishes no effort vocabulary at all, so a
/// named effort there is still refused by name — which is what keeps the
/// widened door about *this* wire rather than about the uncataloged tier.
#[tokio::test]
async fn an_effort_on_a_wire_that_owns_no_roster_is_still_refused_by_name() {
    let engine = Engine::new(
        Arc::new(ganja_core::provider::CursorProvider::default()),
        "default",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );

    let refused = engine
        .send(Command::SwitchEffort { effort: Some("high".to_owned()) })
        .await
        .expect_err("cursor's tier has no efforts to select from");

    assert!(
        matches!(refused, ganja_core::EngineError::UncatalogedEffort { .. }),
        "by the same refusal it always gave: {refused:?}"
    );
    assert!(refused.to_string().contains("cursor"), "naming the provider: {refused}");
}

/// **AC-4.4.** A rewind mid-conversation closes the held process and opens a
/// fresh record whose preamble carries `[User]`, `[Tool Call]` and
/// `[Tool Result]` lines and nothing else.
///
/// A rewind is the one arm the *wire* detects rather than being told about:
/// an id the process was sent is gone from the request, so the live process
/// holds a conversation ganja no longer has. The assistant-free preamble is
/// the vendor safeguard's price — it refused 3/3 rendered transcripts that
/// quoted the model's own words and served every one that did not — so the
/// line-by-line check is the pin on that, not a style assertion.
#[tokio::test]
async fn a_rewind_opens_a_fresh_record_carrying_only_user_and_tool_lines() {
    let home = ganja_testkit::temp_dir();
    let data = ganja_testkit::temp_dir();
    let cli = FakeCli::new(calls_the_tool(&["found it", "second", "third"]));
    let (tool, _calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let provider = wired(&cli, home.path());
    let engine = Engine::persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        ganja_core::provider::claude_code::DEFAULT_MODEL,
        Arc::new(Registry::new(vec![tool])),
        rule(Action::Allow),
        ganja_core::storage::Storage::open(data.path().join("sessions.db")),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("look it up")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    engine.send(prompt("and again")).await.expect("the engine is idle");
    let second_turn = drain(&mut events).await;
    assert_eq!(cli.conversation().len(), 1, "two turns rode one process");

    // Conversation-scoped, so the rewind needs no working-tree snapshots: what
    // this case is about is the id the wire can no longer find, and putting
    // files back would only add a git directory to the fixture.
    engine
        .send(Command::RevertTo {
            message_id: first_user_message(&second_turn),
            scope: ganja_core::protocol::RevertScope::Conversation,
        })
        .await
        .expect("the second prompt is a checkpoint in the live window");
    engine.send(prompt("different question")).await.expect("the engine is idle");
    drain(&mut events).await;

    let records = cli.conversation();
    let [first, second] = records.as_slice() else {
        panic!("the conversation ganja holds is not the one that process had, got {records:?}");
    };
    assert_eq!(cli.record(*first).exit, 0, "so the first process was closed");

    let opening = first_frame(&cli, *second);
    assert_no_assistant_text(&opening);
    for line in opening.lines().filter(|line| line.starts_with('[')) {
        assert!(
            ["[Conversation so far]", "[User]", "[Tool Call]", "[Tool Result]"]
                .iter()
                .any(|allowed| line.starts_with(allowed)),
            "a preamble line outside the three the safeguard served: {line:?}\nin {opening:?}"
        );
    }
}

/// The id of the first user message these events opened with.
///
/// Read off the stream rather than out of the store, because what a revert
/// needs is a checkpoint in the **live window** and that is what the stream
/// just described.
fn first_user_message(seen: &[Event]) -> ganja_core::protocol::MessageId {
    seen.iter()
        .find_map(|event| match event {
            Event::MessageStarted { message, .. }
                if message.role == ganja_core::protocol::Role::User =>
            {
                Some(message.id.clone())
            }
            _ => None,
        })
        .expect("a turn opens with the user message that started it")
}

/// The text of the first `user` frame the `n`th spawn was handed.
fn first_frame(cli: &Arc<FakeCli>, at: usize) -> String {
    let record = cli.record(at);

    record.user_frames.first().cloned().unwrap_or_else(|| {
        panic!("spawn {at} was handed no user frame at all: {:?}", record.user_frames)
    })
}

/// Asserts a preamble quotes none of the model's own words.
///
/// Two checks rather than one, because they fail differently: the label is
/// what a renderer would emit if somebody re-enabled assistant turns, and the
/// answer text is what would leak if a renderer emitted the words without the
/// label.
fn assert_no_assistant_text(frame: &str) {
    assert!(!frame.contains("[Assistant]"), "an assistant label in the preamble: {frame:?}");
    assert!(!frame.contains("found it"), "the model's own earlier words in it: {frame:?}");
}

/// **AC-4.13**, the deferred arm on allow. A steer typed while a tool runs
/// reaches the CLI **one turn later**, and the answer to the held call carries
/// the tool's output and nothing else.
///
/// The deferral is the measurement, not a preference: the real CLI reads both
/// blocks and declines the second as injection (M19), so ganja sends the
/// steer where it will be read — at the head of the next turn's own `user`
/// frame, on the **same** process, ahead of the new prompt.
#[tokio::test]
async fn a_steer_typed_while_a_tool_runs_rides_the_next_turn_rather_than_this_one() {
    const STEER: &str = "actually, look at the other file";

    let home = ganja_testkit::temp_dir();
    let cli = FakeCli::new(calls_the_tool(&["found it", "and again"]));
    let (tool, _calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let provider = wired(&cli, home.path());
    let engine = seated(&provider, tool, rule(Action::Ask));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("look it up")).await.expect("an idle engine accepts a prompt");
    let dialog = held_at_dialog(&mut events).await;
    let frames_before = cli.record(0).user_frames.len();

    engine
        .send(Command::Steer {
            id: "steer-1".to_owned(),
            text: STEER.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a steer is accepted while a turn is held");
    engine
        .send(Command::ReplyPermission { id: dialog, reply: PermissionReply::Once })
        .await
        .expect("a reply is never refused");
    // Answering rather than draining: a steered turn may ask again, and a
    // second dialog nobody answered would hang this case rather than fail it.
    drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let record = cli.record(0);
    assert_eq!(
        record.user_frames.len(),
        frames_before,
        "no `user` frame reached the CLI between the turn's opening and its result: {:?}",
        record.user_frames
    );
    let [result] = record.mcp_results.as_slice() else {
        panic!("one call is one answer, got {:?}", record.mcp_results);
    };
    assert!(result.iter().any(|block| block.contains(ANSWER)), "the tool's output, {result:?}");
    assert!(
        !result.iter().any(|block| block.contains(STEER)),
        "and the steer nowhere in the tool result: {result:?}"
    );

    engine.send(prompt("next question")).await.expect("the engine is idle");
    drain_answering(&engine, &mut events, PermissionReply::Once).await;

    assert_eq!(cli.conversation().len(), 1, "and no second process was opened for it");
    let next = cli
        .record(0)
        .user_frames
        .last()
        .cloned()
        .expect("the next turn wrote a frame on the same process");
    let steer_at = next.find(STEER).expect("the steer rides the next frame: {next:?}");
    let prompt_at = next.find("next question").expect("beside the new prompt: {next:?}");
    assert!(steer_at < prompt_at, "the steer comes first, then the new prompt: {next:?}");
}

/// **AC-4.6c**, the `/agent` half. An agent switch mid-conversation does
/// **not** open a fresh record: the turn after it rides the same process, and
/// the stale system prompt is logged once. Only an idle eviction gets that
/// conversation a process running the new prompt.
///
/// The asymmetry with `/model` is the ruling, and it is about who chose what.
/// A model is a thing a person picked and is billed for, so keeping it stale
/// would bill the opening model under a status bar saying otherwise. A system
/// prompt is machinery — nobody sat down and chose a hash — so the process
/// keeps what it opened with and pays nothing for the difference but a log
/// line.
///
/// `start_paused`, because the second half of the case is what happens after
/// the bound rather than after a wait.
#[tokio::test(start_paused = true)]
async fn an_agent_switch_keeps_the_process_and_only_an_eviction_opens_a_new_one() {
    let home = ganja_testkit::temp_dir();
    let project = ganja_testkit::temp_dir();
    let cli = FakeCli::new(says(&["found it", "still here", "new prompt now"]));
    let (tool, _calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let provider = Arc::new(
        ClaudeCodeProvider::with_parts(
            PathBuf::from("/nonexistent/claude"),
            VERSION.to_owned(),
            Arc::clone(&cli) as Arc<dyn Spawner>,
            binding::Paths::under(home.path()),
        )
        .with_idle_bound(BOUND),
    );
    let (log, _guard) = capturing();
    let engine =
        seated(&provider, tool, rule(Action::Allow)).with_agents(two_agents(project.path()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("look it up")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    assert_eq!(cli.conversation().len(), 1, "one conversation, one process");

    engine
        .send(Command::SwitchAgent { name: OTHER_AGENT.to_owned() })
        .await
        .expect("the registry holds a second primary");
    engine.send(prompt("and again")).await.expect("the engine is idle");
    drain(&mut events).await;

    assert_eq!(
        cli.conversation().len(),
        1,
        "a system prompt nobody chose closes nothing: the turn rode the same process"
    );
    assert_eq!(
        log.logged().matches("stale_system").count(),
        1,
        "and it is said once rather than every turn:\n{}",
        log.logged()
    );

    // Only the bound gets that conversation a process running the new prompt.
    tokio::time::advance(BOUND + std::time::Duration::from_secs(1)).await;
    for _ in 0..2_000 {
        if provider.held_entries() == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    engine.send(prompt("after the bound")).await.expect("the engine is idle");
    drain(&mut events).await;

    let records = cli.conversation();
    let [_, fresh] = records.as_slice() else {
        panic!("the eviction is what opens the next record, got {records:?}");
    };
    assert_eq!(
        cli.record(*fresh).system_prompt.as_ref().map(Vec::len),
        Some(1),
        "and that record really did carry a system prompt of its own"
    );
}

/// The agent this case switches to.
const OTHER_AGENT: &str = "plan";

/// A registry holding the session's own agent and one more to switch to.
///
/// Two primaries is the whole requirement: what the case needs is a switch
/// that changes the system prompt, and the prompt is what an agent brings.
fn two_agents(project: &Path) -> Arc<ganja_core::agent::Registry> {
    let mut agent = std::collections::BTreeMap::new();
    agent.insert(
        OTHER_AGENT.to_owned(),
        ganja_core::config::AgentConfig {
            prompt: Some("a different system prompt entirely".to_owned()),
            description: Some("the second primary this case switches to".to_owned()),
            mode: Some(ganja_core::config::AgentMode::Primary),
            ..ganja_core::config::AgentConfig::default()
        },
    );
    let config = ganja_core::Config { agent, ..ganja_core::Config::default() };

    Arc::new(
        ganja_core::agent::Registry::build(&config, project).expect("the table resolves an agent"),
    )
}

/// A subscriber this case can read its own log lines back out of.
fn capturing() -> (LogCapture, tracing::subscriber::DefaultGuard) {
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);

    (capture, guard)
}

/// **AC-4.14**, and **Dv-22**'s end-to-end pin. Every `/team`
/// auto-continuation is **one `user` frame on the held process**: no EOF, no
/// second spawn, no preamble, and the block verbatim each time.
///
/// A continuation is a request rather than a message — the lead's own turn
/// keeps going — so on a wire that holds a process the honest shape is the
/// cheapest one there is. Anything else pays for the whole conversation again
/// on every continuation, which is the one thing this wire's arm table exists
/// to avoid.
///
/// This case is what found Dv-22. Before it, the first continuation rode the
/// process and **every one after it respawned**: the guards block is minted
/// with a fresh id per request, the wire recorded that id as conversation
/// state, and the next request's different id in the same position read as a
/// rewind. Five continuations therefore meant five paid records. The count
/// below — one spawn, five frames — is the assertion that would redden if the
/// flag stopped being honoured.
#[tokio::test]
async fn every_team_continuation_is_one_frame_on_the_process_that_answered() {
    let home = ganja_testkit::temp_dir();
    let project = ganja_testkit::temp_dir();
    // Long enough that the lead's own continuation budget runs out before the
    // script does: a fake that ran dry would end its process, and the fresh
    // record that followed would look like the wire choosing to respawn.
    let cli = FakeCli::new(says(&["talked"; 12]));
    let (tool, _calls) = RecorderTool::new(TOOL, "lookup ran", ANSWER);
    let provider = wired(&cli, home.path());
    let (_root, _team, registry, _door) = ganja_testkit::team_with(
        home.path(),
        Arc::new(ganja_core::provider::FakeProvider::new("on it", std::time::Duration::ZERO)),
        Arc::new(Registry::new(Vec::new())),
        ganja_core::storage::Storage::open(home.path().join("teammate-storage")),
        |_| Permissions::default(),
    );
    let engine = Engine::persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        ganja_core::provider::claude_code::DEFAULT_MODEL,
        Arc::new(Registry::new(vec![tool])),
        rule(Action::Allow),
        ganja_core::storage::Storage::open(home.path().join("lead-storage")),
    )
    .with_teammates(Arc::clone(&registry), ganja_testkit::externals())
    // One task nobody has finished, which is the other half of what makes a
    // turn continue: a live team alone would end the turn as usual.
    .with_tasks(Arc::new(ganja_testkit::StaticTasks::new(vec![ganja_testkit::task(
        "1",
        ganja_tool::tasklist::Status::InProgress,
        "helper",
        "keep going",
        &[],
    )])));
    let (log, _guard) = capturing();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    // A member that is *alive*, which is what `live_team` reads — not one with
    // a turn in flight.
    engine
        .teammates()
        .expect("this session leads a team")
        .start(
            ganja_testkit::spawn_with_prompt("helper", Some("in-process"), "keep going"),
            &ganja_testkit::caller(project.path()),
            &ganja_testkit::AllowSpawn,
        )
        .await
        .expect("an in-process teammate starts on a session that has a store");

    engine.send(prompt("get them going")).await.expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    assert_eq!(
        cli.conversation().len(),
        1,
        "every continuation rode the process that answered: no EOF and no second spawn"
    );
    assert!(
        !log.logged().contains(r#"reason="rewind""#),
        "and none of them read as a rewind:\n{}",
        log.logged()
    );

    let frames = cli.record(cli.conversation()[0]).user_frames;
    let [opening, continued @ ..] = frames.as_slice() else {
        panic!("the process was written to at least once: {frames:?}");
    };
    assert!(opening.contains("get them going"), "the first frame is the prompt: {opening:?}");
    assert!(
        continued.len() > 1,
        "the turn continued more than once, which is what Dv-22 made possible: {frames:?}"
    );
    for block in continued {
        assert!(
            block.contains("<team_still_working>"),
            "each continuation is the block, verbatim: {block:?}"
        );
        assert!(
            !block.contains("[Conversation so far]"),
            "carrying no preamble — the process already has the conversation: {block:?}"
        );
        assert!(
            !block.contains("get them going"),
            "and not the prompt again either, which a fresh record would have re-sent: {block:?}"
        );
    }
}
