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
//! "is this group still alive" without reaching for an API this workspace has
//! no crate for, so the grandchild announces itself, waits, and then tries to
//! announce that it survived. A second announcement means the kill did not
//! reach it. Its own binary, like its unix twin, because it runs a real shell
//! and a real tree.

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

/// How long the command is given to fork the witness and say so. Generous,
/// because it covers a whole turn's worth of scripted streaming before the
/// shell is even spawned — and because starting a process on Windows is not
/// the cheap thing it is on unix.
const START_DEADLINE: Duration = Duration::from_secs(20);

/// How long the grandchild waits before claiming to have survived.
///
/// It has to outlast the kill sequence comfortably — otherwise a slow kill
/// would look like a failed one — and still be short enough that the check
/// below is not the slowest thing in the suite.
const SURVIVAL_DELAY: Duration = Duration::from_secs(3);

/// How long the survival file is watched for after the cancel. Longer than
/// [`SURVIVAL_DELAY`], so a grandchild that lived would have had time to say
/// so and its silence means it was killed.
const SILENCE_WINDOW: Duration = Duration::from_secs(8);

/// How long the turn may take to report the cancel. Far short of the command
/// it was running, so a turn that waits the command out fails here rather than
/// hanging the suite.
const FINISH_BUDGET: Duration = Duration::from_secs(10);

/// Between polls of the two marker files.
const TICK: Duration = Duration::from_millis(50);

/// `path` as a POSIX shell will read it.
///
/// The command string is handed to a POSIX shell, which reads `\` as an escape,
/// so a native Windows path interpolated into a redirect would be eaten by it.
/// Git Bash accepts a forward-slash spelling of a drive path throughout.
fn posix(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
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

#[tokio::test]
async fn cancelling_a_turn_kills_the_process_tree_of_the_command_it_was_running() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let started = dir.path().join("started");
    let survived = dir.path().join("survived");

    // The backgrounded subshell is the witness: it is a *grandchild* of this
    // process, so killing the shell alone — all that dropping the tool's future
    // achieves — leaves it running. It writes `started` the moment it exists,
    // which is what proves the test built its own attack, then waits and writes
    // `survived`. Only a kill that reached the whole tree stops the second
    // file from appearing.
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

    let engine = Engine::new(
        // Not `FakeProvider::default()`: that one takes its script from the
        // environment, and this test brings its own.
        Arc::new(FakeProvider::new("", Duration::ZERO).with_script(&script_path)),
        fake::MODEL,
        Arc::new(Registry::with_builtins()),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "run it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // `bash` asks by default, and nothing runs until the answer arrives.
    loop {
        match events.next().await {
            Some(Event::PermissionRequested { id, .. }) => {
                engine
                    .send(Command::ReplyPermission {
                        id,
                        reply: PermissionReply::Once,
                    })
                    .await
                    .expect("a reply is always accepted");
                break;
            }
            Some(_) => {}
            None => panic!("the engine stopped before it asked to run the command"),
        }
    }

    wait_for(&started, "the command never forked its witness").await;
    assert!(
        !survived.exists(),
        "the witness announced survival before anything cancelled it; \
         {SURVIVAL_DELAY:?} is too short to be a wait"
    );

    let issued = Instant::now();
    engine
        .send(Command::CancelTurn)
        .await
        .expect("a running engine accepts a cancel");

    let deadline = issued + SILENCE_WINDOW;
    while Instant::now() < deadline {
        assert!(
            !survived.exists(),
            "the grandchild outlived the cancel; the kill reached the shell and not the tree"
        );
        tokio::time::sleep(TICK).await;
    }

    // What the cancel looks like from outside is unchanged: the call's part
    // closes as an error carrying the cancel, and the turn finishes cancelled.
    let mut call_error = None;
    let reason = loop {
        match events.next().await {
            Some(Event::MessageFinished { reason, .. }) => break reason,
            Some(Event::PartUpdated { part, .. }) => {
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
            Some(_) => {}
            None => panic!("the turn never finished"),
        }
    };

    assert_eq!(reason, FinishReason::Cancelled);
    assert_eq!(
        call_error.as_deref(),
        Some("the call was cancelled"),
        "a cancelled call still closes as the cancel it was"
    );
    assert!(
        issued.elapsed() < SILENCE_WINDOW + FINISH_BUDGET,
        "the turn took {:?} to report the cancel",
        issued.elapsed()
    );
}
