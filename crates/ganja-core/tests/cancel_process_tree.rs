//! Proves a cancelled turn takes the whole process tree its command started
//! with it, on the platform that has no process group to signal.
//!
//! The Windows twin of `cancel_process_group.rs`, and it pins the same claim
//! about the same seam: the kill belongs to the shell tool
//! (`tool/shell.rs`, `kill_tree`) but runs *inside* the future that tool
//! returned, so a cancel that merely drops the future ends the shell alone —
//! through the handle's `kill_on_drop` — and leaves whatever the command forked
//! running. There the answer is `killpg`; here it is `taskkill /T`, which walks
//! the parent chain the kernel keeps.
//!
//! The witness is a file rather than a signal. Windows offers no way to ask
//! "is this tree still alive" without reaching for an API this workspace has no
//! crate for, so the grandchild announces itself, waits, and then tries to
//! announce that it survived. A second announcement means the kill did not
//! reach it. Its own binary, like its unix twin, because it runs a real shell
//! and a real tree.
//!
//! # Every wait here has a deadline, and the stream is never left unread
//!
//! Two rules this file follows deliberately, because breaking either one hangs
//! CI rather than failing it — and a test that hangs is worse than a test that
//! is missing, since it burns the runner's whole slow-timeout before saying
//! anything at all.
//!
//! The first: **`Engine::subscribe` is lossless**, so its queue applies
//! backpressure to the turn that fills it. A test that stops draining while it
//! polls the filesystem stops the very turn it is about to wait for, and then
//! waits for it forever. So the stream is drained by a task of its own for the
//! whole test and the assertions read what that task forwarded.
//!
//! The second: every await on that forwarded channel is wrapped in a timeout,
//! so a turn that never finishes fails this test in seconds with a message
//! naming what it was waiting for.

#![cfg(windows)]

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use ganja_core::{
    Engine,
    permission::Permissions,
    protocol::{Command, Event, FinishReason, PartBody, PermissionReply, ToolState},
    provider::{FakeProvider, fake},
    tool::Registry,
};
use tokio::sync::mpsc;

/// How long the command is given to fork the witness and say so. Generous,
/// because it covers a whole turn's worth of scripted streaming before the
/// shell is even spawned — and because starting a process on Windows is not the
/// cheap thing it is on unix.
const START_DEADLINE: Duration = Duration::from_secs(30);

/// How long the grandchild waits before claiming to have survived.
///
/// Long enough that the cancel comfortably lands first on a loaded runner — if
/// it did not, the witness would announce survival before anything tried to
/// kill it and the test would be measuring nothing.
const SURVIVAL_DELAY: Duration = Duration::from_secs(5);

/// How long the survival file is watched for after the cancel. Longer than
/// [`SURVIVAL_DELAY`], so a grandchild that lived would have had time to say so
/// and its silence means it was killed.
const SILENCE_WINDOW: Duration = Duration::from_secs(9);

/// How long the turn may take to report the cancel. Far short of the command it
/// was running, so a turn that waits the command out fails here rather than
/// hanging the suite.
const FINISH_BUDGET: Duration = Duration::from_secs(20);

/// How long the permission dialog may take to arrive.
const ASK_BUDGET: Duration = Duration::from_secs(30);

/// Between polls of the marker files.
const TICK: Duration = Duration::from_millis(50);

/// Names the stage the test just reached, on stderr, where a timeout report
/// will carry it.
///
/// The first lane run died at nextest's 240s kill with nothing to read even
/// though every await below carries its own budget — so whatever is stuck
/// sits before, or freezes, those budgets. Until that run's captured output
/// names the last stage reached, every theory is a guess; these lines are
/// the missing evidence, cheap enough to keep once they have answered.
fn stage(name: &str) {
    eprintln!("stage: {name}");
}

/// `path` as a POSIX shell will read it.
///
/// The command string is handed to a POSIX shell, which reads `\` as an escape,
/// so a native Windows path interpolated into a redirect would be eaten by it.
/// Git Bash accepts a forward-slash spelling of a drive path throughout.
fn posix(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// The next event, or a failure naming what it was that never arrived.
///
/// Every read of the stream goes through here. A bare `.next().await` is what
/// turned this suite's first draft into a four-minute CI hang instead of a
/// failure somebody could read.
async fn next_event(
    events: &mut mpsc::UnboundedReceiver<Event>,
    within: Duration,
    awaited: &str,
) -> Event {
    match tokio::time::timeout(within, events.recv()).await {
        Ok(Some(event)) => event,
        Ok(None) => panic!("the event stream ended before {awaited}"),
        Err(_) => panic!("{awaited} did not happen within {within:?}"),
    }
}

/// Waits for `marker` to appear, or says what never happened.
async fn wait_for(marker: &Path, what: &str) {
    let deadline = Instant::now() + START_DEADLINE;

    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "{what}: {} never appeared within {START_DEADLINE:?}",
            marker.display()
        );
        tokio::time::sleep(TICK).await;
    }
}

#[test]
fn cancelling_a_turn_kills_the_process_tree_of_the_command_it_was_running() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime builds");
    runtime.block_on(drill());
    stage("story complete, runtime still up");
    // The lane proved the whole story above and then died at the runner's
    // kill: "returning" printed, the harness's own result line never did. The
    // default drop joins whatever a cancelled child's plumbing left in the
    // blocking pool, with no bound; this gives that join five seconds, which
    // is generous for anything legitimate. The drill's claim — the tree dies
    // — was already made by then, so a straggler in teardown is recorded
    // weather, not a kill that failed.
    runtime.shutdown_timeout(Duration::from_secs(5));
    stage("runtime down");
}

/// The drill itself, on the runtime whose teardown the wrapper bounds.
async fn drill() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let started = dir.path().join("started");
    let survived = dir.path().join("survived");

    // The backgrounded subshell is the witness: it is a *grandchild* of this
    // process, so killing the shell alone — all that dropping the tool's future
    // achieves — leaves it running. It writes `started` the moment it exists,
    // which is what proves the test built its own attack, then waits and writes
    // `survived`. Only a kill that reached the whole tree stops the second file
    // from appearing.
    let command = format!(
        "( echo yes > {started}; sleep {delay}; echo yes > {survived} ) & sleep 300",
        started = posix(&started),
        delay = SURVIVAL_DELAY.as_secs(),
        survived = posix(&survived),
    );
    let script = serde_json::json!({
        "cadence_ms": 0,
        "turns": [{
            "text": "Running it.",
            "tool_calls": [{"name": "bash", "args": {"command": command}}],
        }],
    });
    let script_path = dir.path().join("script.json");
    std::fs::write(&script_path, script.to_string()).expect("the script is writable");
    stage("script written");

    let engine = Engine::new(
        // Not `FakeProvider::default()`: that one takes its script from the
        // environment, and this test brings its own.
        Arc::new(FakeProvider::new("", Duration::ZERO).with_script(&script_path)),
        fake::MODEL,
        Arc::new(Registry::with_builtins()),
        Permissions::default(),
    );
    stage("engine built, registry probed for a shell");
    let mut stream = engine.subscribe().await.expect("the first subscriber wins");
    stage("subscribed");

    // See the module docs: the subscriber is lossless, so it is drained for the
    // whole test by a task that does nothing else. Everything below reads the
    // channel it forwards to, which no assertion can stall.
    let (sender, mut events) = mpsc::unbounded_channel();
    let drain = tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            if sender.send(event).is_err() {
                return;
            }
        }
    });

    engine
        .send(Command::SendPrompt {
            text: "run it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    stage("prompt accepted");

    // `bash` asks by default, and nothing runs until the answer arrives.
    loop {
        if let Event::PermissionRequested { id, .. } =
            next_event(&mut events, ASK_BUDGET, "the tool asked to run the command").await
        {
            stage("permission asked");
            engine
                .send(Command::ReplyPermission {
                    id,
                    reply: PermissionReply::Once,
                })
                .await
                .expect("a reply is always accepted");
            stage("permission answered");
            break;
        }
    }

    wait_for(&started, "the command never forked its witness").await;
    stage("witness started");
    assert!(
        !survived.exists(),
        "the witness announced survival before anything cancelled it; \
         {SURVIVAL_DELAY:?} is too short a wait to prove a kill"
    );

    let issued = Instant::now();
    engine
        .send(Command::CancelTurn)
        .await
        .expect("a running engine accepts a cancel");
    stage("cancel issued");

    // The turn is drained to its finish *first*. What the cancel looks like
    // from outside is unchanged: the call's part closes as an error carrying
    // the cancel, and the turn finishes cancelled.
    let mut call_error = None;
    let reason = loop {
        match next_event(
            &mut events,
            FINISH_BUDGET,
            "the cancelled turn finished (if this is where it stops, the kill \
             did not reach the command and the tool is still waiting on it)",
        )
        .await
        {
            Event::MessageFinished { reason, .. } => break reason,
            Event::PartUpdated { part, .. } => {
                if let PartBody::Tool {
                    tool,
                    state: ToolState::Error { error, .. },
                    ..
                } = part.body
                    && tool == "bash"
                {
                    call_error = Some(error);
                }
            }
            _ => {}
        }
    };

    stage("turn finished");
    assert_eq!(reason, FinishReason::Cancelled);
    assert_eq!(
        call_error.as_deref(),
        Some("the call was cancelled"),
        "a cancelled call still closes as the cancel it was"
    );
    stage("cancel closed as itself");

    // Only now is there nothing left to keep reading, so the wait for the
    // witness's silence can be a plain sleep. It runs past the moment the
    // grandchild would have spoken.
    let waited = issued.elapsed();
    if let Some(remaining) = SILENCE_WINDOW.checked_sub(waited) {
        tokio::time::sleep(remaining).await;
    }
    stage("silence window elapsed");
    assert!(
        !survived.exists(),
        "the grandchild outlived the cancel by {SILENCE_WINDOW:?}; \
         the kill reached the shell and not the tree"
    );

    drain.abort();
    // The last line the story can speak from inside the runtime; the wrapper
    // marks the teardown boundary after it.
    stage("returning");
}
