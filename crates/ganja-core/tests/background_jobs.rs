//! Proves the background-execution contract acceptance criterion 2 pins:
//! `run_in_background: true` returns immediately while the process keeps
//! running, a turn's cancel leaves it running, the registry `bash_output`/
//! `kill_shell` both read from answers correctly against a *real* spawned
//! process (not the scripted fixtures `crates/ganja-core/src/job.rs`'s own
//! unit tests use), and engine shutdown ends it.
//!
//! Spec: Claude Code's `run_in_background`/`BashOutput`/`KillShell`; see
//! `crates/ganja-core/src/job.rs`'s module doc and **D454**/**D455**.
//!
//! Gated to unix for the same reason `cancel_process_group.rs` is and
//! `cancel_process_tree.rs` is its windows twin: the process-group witness
//! below needs `killpg`. A windows twin is recorded as a follow-up rather
//! than written here.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::stream::BoxStream;
use ganja_core::Engine;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Command, Event, PartBody, PermissionReply, ToolState};
use ganja_core::provider::{FakeProvider, fake};
use ganja_core::tool::Registry;
use ganja_core::tool::job::{Jobs as _, State};

/// How long a scripted permission ask and the tool's own completion are
/// given to arrive. Generous, because it covers a whole turn's worth of
/// scripted streaming before the shell is even spawned.
const ASK_BUDGET: Duration = Duration::from_secs(10);

/// How long a background job's delayed witness is watched for. Comfortably
/// longer than the delay itself, so a job that really kept running has time
/// to prove it.
const WITNESS_BUDGET: Duration = Duration::from_secs(5);

/// How long `bash_output`'s polls are given to deliver a line and an exit.
/// Generous on purpose: what those assertions are about is *which poll* a line
/// lands in, never how quickly it gets there, and a machine running this whole
/// workspace's suite in parallel can starve a child process — or the task
/// polling it — for seconds at a time.
const OUTPUT_BUDGET: Duration = Duration::from_secs(15);

/// How many 50ms spins the gated fixture below gives its gate before it gives
/// up. A safety net a passing run never reaches — it exists so a panicking run
/// cannot leave a shell spinning forever — and long enough (30s) that
/// [`OUTPUT_BUDGET`] is always the deadline that fires first, so a failure
/// reads as "the line never arrived" rather than as a shell that decided on
/// its own to stop waiting.
const GATE_SPINS: u32 = 600;

/// Between polls of a witness file.
const TICK: Duration = Duration::from_millis(20);

/// `path` as a POSIX shell will read it — the same translation `shell.rs`'s
/// own tests and `cancel_process_tree.rs` use, so a Windows checkout of this
/// suite (once a twin exists) is not this file's problem to solve.
fn posix(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Builds a single-turn scripted engine: one assistant reply, one `bash`
/// call carrying `run_in_background: true`.
fn engine_for(command: &str, dir: &Path) -> Engine {
    let script = serde_json::json!({
        "cadence_ms": 0,
        "turns": [{
            "text": "Running it in the background.",
            "tool_calls": [{
                "name": "bash",
                "args": { "command": command, "run_in_background": true },
            }],
        }],
    });
    let script_path = dir.join("script.json");
    std::fs::write(&script_path, script.to_string()).expect("the script is writable");

    Engine::new(
        Arc::new(FakeProvider::new("", Duration::ZERO).with_script(&script_path)),
        fake::MODEL,
        Arc::new(Registry::with_builtins()),
        Permissions::default(),
    )
}

/// Sends the prompt that starts the engine's one scripted turn, answers the
/// `bash` permission ask, and returns the `bash_id` the tool's completed
/// output named.
async fn run_prompt_and_await_bash_id(
    engine: &Engine,
    events: &mut BoxStream<'static, Event>,
) -> String {
    engine
        .send(Command::SendPrompt {
            text: "run it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    loop {
        match tokio::time::timeout(ASK_BUDGET, events.next())
            .await
            .expect("the background bash call should ask to run")
        {
            Some(Event::PermissionRequested { id, .. }) => {
                engine
                    .send(Command::ReplyPermission { id, reply: PermissionReply::Once })
                    .await
                    .expect("a reply is always accepted");
                break;
            }
            Some(_) => {}
            None => panic!("the engine stopped before it asked to run the command"),
        }
    }

    loop {
        match tokio::time::timeout(ASK_BUDGET, events.next())
            .await
            .expect("the background call should complete")
        {
            Some(Event::PartUpdated { part, .. }) => {
                if let PartBody::Tool { tool, state: ToolState::Completed { output, .. }, .. } =
                    part.body
                    && tool == "bash"
                {
                    let bash_id = output
                        .split('"')
                        .nth(1)
                        .expect("the reply names the job id between quotes")
                        .to_owned();
                    assert!(bash_id.starts_with("bash_"), "got {output:?}");
                    return bash_id;
                }
            }
            Some(_) => {}
            None => panic!("the turn never finished"),
        }
    }
}

/// Acceptance criterion 2, first half: the call returns immediately naming a
/// job id while the process demonstrably continues — a fixture writes a file
/// after a delay, and the file is asserted on disk.
#[tokio::test]
async fn run_in_background_returns_immediately_and_the_process_keeps_running() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let witness = dir.path().join("witness");
    let command = format!("sleep 0.3; echo yes > {}", posix(&witness));
    let engine = engine_for(&command, dir.path());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let started = Instant::now();
    run_prompt_and_await_bash_id(&engine, &mut events).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(300),
        "a backgrounded call should return before its own command's delay; took {elapsed:?}"
    );
    assert!(
        !witness.exists(),
        "the call returned before the delayed write, or nothing was actually backgrounded"
    );

    let deadline = Instant::now() + WITNESS_BUDGET;
    while !witness.exists() {
        assert!(
            Instant::now() < deadline,
            "the backgrounded command never wrote its witness within {WITNESS_BUDGET:?}"
        );
        tokio::time::sleep(TICK).await;
    }

    engine.shutdown_jobs().await;
}

/// Acceptance criterion 2: a turn's cancel does not reach a background job —
/// only the turn's own token fires, never the registry's root token.
#[tokio::test]
async fn a_turns_cancel_leaves_a_background_job_running() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let witness = dir.path().join("witness");
    let command = format!("sleep 0.5; echo yes > {}", posix(&witness));
    let engine = engine_for(&command, dir.path());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    // The job is registered and the tool call has already completed before
    // the cancel is issued: what is under test is that cancelling the *turn*
    // afterward does not retroactively reach a job it already handed off,
    // never whether a cancel can outrun the tool call starting at all — a
    // separate, already-pinned invariant (`session.rs`'s own
    // `a cancel that arrived before the tool was ever polled must not start
    // it`).
    run_prompt_and_await_bash_id(&engine, &mut events).await;

    engine.send(Command::CancelTurn).await.expect("a running engine accepts a cancel");

    let deadline = Instant::now() + WITNESS_BUDGET;
    while !witness.exists() {
        assert!(
            Instant::now() < deadline,
            "the background job never wrote its witness within {WITNESS_BUDGET:?}; \
             the turn's cancel reached it, which it must not"
        );
        tokio::time::sleep(TICK).await;
    }

    engine.shutdown_jobs().await;
}

/// Acceptance criterion 2: `bash_output`'s own registry answers only what is
/// new since the last poll and reports status, against a real process the
/// `bash` tool spawned — not a test double.
///
/// Every assertion here is about **content**: which poll a line is delivered
/// in, and that no line is delivered twice. Nothing asserts how long anything
/// took. Two properties of the seam under test make that harder than it looks,
/// and both are what the shape below is for:
///
/// 1. The second line is released by a *gate file this test writes*, not by a
///    `sleep` the fixture runs. A wall-clock gap only separates the two polls
///    while the machine is idle; a gate separates them by causality, so "the
///    first poll cannot contain `two`" is true by construction rather than by
///    luck. (This is what starved twice under full workspace parallelism —
///    `ganja-code-j1m`.)
/// 2. `Jobs::output` **drains**: every call takes what has arrived since the
///    last one. So the second phase accumulates across polls instead of
///    trusting whichever single read happens to observe the exit — a poll that
///    sees `two` while the state is still `Running` would otherwise throw that
///    line away, and the next poll would report an empty chunk beside the exit.
#[tokio::test]
async fn output_is_delivered_once_and_then_only_whats_new() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let gate = dir.path().join("gate");
    let command = format!(
        "echo one; \
         i=0; while [ $i -lt {GATE_SPINS} ] && [ ! -f \"{gate}\" ]; \
         do i=$((i+1)); sleep 0.05; done; \
         echo two",
        gate = posix(&gate),
    );
    let engine = engine_for(&command, dir.path());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    let bash_id = run_prompt_and_await_bash_id(&engine, &mut events).await;

    // Polled rather than slept for: the child process still has to actually
    // start before "one" can appear, and a fixed sleep races that startup on a
    // loaded machine.
    let deadline = Instant::now() + OUTPUT_BUDGET;
    let first = loop {
        let read = engine.jobs().output(&bash_id).await.expect("a known id answers");
        if !read.chunk.is_empty() {
            break read;
        }
        assert!(
            Instant::now() < deadline,
            "the job never produced any output within {OUTPUT_BUDGET:?}"
        );
        tokio::time::sleep(TICK).await;
    };
    assert!(first.chunk.contains("one"), "got {:?}", first.chunk);
    // The gate is still on disk-absent here, so the fixture physically cannot
    // have printed its second line yet: this holds however starved the machine
    // was between the spawn and the poll above.
    assert!(!first.chunk.contains("two"), "got {:?}", first.chunk);

    std::fs::write(&gate, "go").expect("the gate is writable");

    let deadline = Instant::now() + OUTPUT_BUDGET;
    let mut delivered = String::new();
    let mut exit = None;
    loop {
        let read = engine.jobs().output(&bash_id).await.expect("a known id answers");
        delivered.push_str(&read.chunk);
        if let State::Exited { code } = read.status.state {
            exit = Some(code);
        }
        if delivered.contains("two") && exit.is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the job never delivered its second line and exited within \
             {OUTPUT_BUDGET:?}; everything since the first poll was {delivered:?}"
        );
        tokio::time::sleep(TICK).await;
    }
    assert!(!delivered.contains("one"), "the first poll already delivered it: {delivered:?}");
    assert_eq!(exit, Some(Some(0)));

    engine.shutdown_jobs().await;
}

/// Acceptance criterion 2: `kill_shell`'s own registry ends the whole
/// process group a `bash`-registered job started — the
/// `a_timeout_kills_the_command_and_everything_it_forked` idiom, replayed
/// against a background job instead of a foreground timeout.
#[tokio::test]
async fn killing_a_registered_job_ends_its_whole_process_tree() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let forked = dir.path().join("forked");
    let survived = dir.path().join("survived");
    let command = format!(
        "( echo yes > {forked}; sleep 3; echo yes > {survived} ) & sleep 30",
        forked = posix(&forked),
        survived = posix(&survived),
    );
    let engine = engine_for(&command, dir.path());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    let bash_id = run_prompt_and_await_bash_id(&engine, &mut events).await;

    let deadline = Instant::now() + WITNESS_BUDGET;
    while !forked.exists() {
        assert!(Instant::now() < deadline, "the grandchild never forked");
        tokio::time::sleep(TICK).await;
    }

    let killed = engine.jobs().kill(&bash_id).await.expect("a running job can be killed");
    assert_eq!(killed.state, State::Killed);

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!survived.exists(), "the grandchild outlived the kill; the tree was not reached");

    engine.shutdown_jobs().await;
}

/// Acceptance criterion 2: engine shutdown kills a running job outright.
#[tokio::test]
async fn engine_shutdown_kills_a_running_background_job() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let forked = dir.path().join("forked");
    let survived = dir.path().join("survived");
    let command = format!(
        "( echo yes > {forked}; sleep 3; echo yes > {survived} ) & sleep 30",
        forked = posix(&forked),
        survived = posix(&survived),
    );
    let engine = engine_for(&command, dir.path());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    run_prompt_and_await_bash_id(&engine, &mut events).await;

    let deadline = Instant::now() + WITNESS_BUDGET;
    while !forked.exists() {
        assert!(Instant::now() < deadline, "the grandchild never forked");
        tokio::time::sleep(TICK).await;
    }

    engine.shutdown_jobs().await;

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!survived.exists(), "a job outlived engine shutdown; the tree was not reached");
}
