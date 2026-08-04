//! Proves a cancelled turn takes the whole process tree its command started
//! with it, instead of orphaning what that command forked.
//!
//! The kill itself belongs to the shell tool (`tool/shell.rs`, `kill_tree`),
//! but it runs *inside* the future that tool returned, so what this really
//! pins is the agent loop: a cancel that drops the tool's future never
//! reaches the kill, the handle's own `kill_on_drop` ends the shell alone, and
//! the group survives the turn. Only a real process group can witness that,
//! so this suite starts one and asks the kernel about it afterwards.

#![cfg(unix)]

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use ganja_core::{
    Command, Engine, Event, FinishReason, PartBody, PermissionReply, Permissions, Registry,
    ToolState,
    provider::{FakeProvider, fake},
};

/// How long the command is given to announce its process group. Generous,
/// because it covers a whole turn's worth of scripted streaming before the
/// shell is even spawned.
const START_DEADLINE: Duration = Duration::from_secs(10);

/// How long the group is given to die after the cancel. The shell tool's own
/// sequence is a 200ms `SIGTERM` grace plus a 100ms output drain, and the
/// loop's grace bounds the whole thing at 500ms; the rest is slack for a
/// loaded machine. The command sleeps for five minutes, so a group still
/// alive at this deadline is a group nothing killed.
const GROUP_DEADLINE: Duration = Duration::from_secs(3);

/// How long the turn may take to report the cancel. Far short of the command
/// it was running, so a turn that waits the command out fails here rather
/// than hanging the suite.
const FINISH_BUDGET: Duration = Duration::from_secs(5);

/// Between polls of the pid file and of the group.
const TICK: Duration = Duration::from_millis(20);

/// Kills the group the test started, however the test ends. An assertion that
/// fires halfway through must not leave a five-minute `sleep` behind on the
/// machine that ran it.
struct Reaper(std::cell::Cell<Option<libc::pid_t>>);

impl Reaper {
    fn watching(pgid: libc::pid_t) -> Self {
        Self(std::cell::Cell::new(Some(pgid)))
    }

    /// Stands down once the group is known dead. A group id is only reserved
    /// while the group still has a member, so signalling one that has since
    /// been recycled would reach somebody else's process.
    fn stand_down(&self) {
        self.0.set(None);
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        let Some(pgid) = self.0.get() else {
            return;
        };

        // SAFETY: `killpg` takes two integers, reads no memory and owns no
        // resource. The group is the one this test spawned, and it has not
        // been stood down, so it was still alive when it was last looked at
        // and its id is still its own.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

/// Whether the group still holds a process.
fn group_is_alive(pgid: libc::pid_t) -> bool {
    // SAFETY: as in [`Reaper::drop`] — two integers in, one out. Signal `0`
    // delivers nothing at all; it only asks whether the group is still there,
    // which is exactly the question.
    unsafe { libc::killpg(pgid, 0) == 0 }
}

/// The group id the command wrote, once it has written one.
///
/// A file that exists but has not been written yet reads as empty, so the
/// parse is the readiness check.
async fn wait_for_pgid(pidfile: &Path) -> libc::pid_t {
    let deadline = Instant::now() + START_DEADLINE;

    loop {
        if let Ok(text) = std::fs::read_to_string(pidfile)
            && let Ok(pgid) = text.trim().parse::<libc::pid_t>()
        {
            return pgid;
        }

        assert!(
            Instant::now() < deadline,
            "the command never reported its process group; {} holds {:?}",
            pidfile.display(),
            std::fs::read_to_string(pidfile).ok()
        );
        tokio::time::sleep(TICK).await;
    }
}

#[tokio::test]
async fn cancelling_a_turn_kills_the_process_group_of_the_command_it_was_running() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let pidfile = dir.path().join("pgid");

    // The backgrounded sleep is the witness: it is a *grandchild* of this
    // process, so killing the shell alone — all that dropping the tool's
    // future achieves, through `kill_on_drop` — leaves it running and its
    // group alive. Only `killpg` reaches it. `$$` is the shell's own pid, and
    // the tool spawned that shell as the leader of a fresh group, so it
    // doubles as the group id. It is written *after* the fork on purpose: a
    // readable pid file then proves the witness already exists, and no cancel
    // can land in between.
    let command = format!("sleep 300 & echo $$ > {}; sleep 300", pidfile.display());
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

    let pgid = wait_for_pgid(&pidfile).await;
    let reaper = Reaper::watching(pgid);
    assert!(
        group_is_alive(pgid),
        "the command should still be running when it is cancelled"
    );

    let issued = Instant::now();
    engine
        .send(Command::CancelTurn)
        .await
        .expect("a running engine accepts a cancel");

    let deadline = issued + GROUP_DEADLINE;
    while group_is_alive(pgid) {
        assert!(
            Instant::now() < deadline,
            "process group {pgid} outlived the cancel by {GROUP_DEADLINE:?}; \
             the tool's future was dropped before it could kill the group"
        );
        tokio::time::sleep(TICK).await;
    }
    reaper.stand_down();

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
    let elapsed = issued.elapsed();

    assert_eq!(reason, FinishReason::Cancelled);
    assert_eq!(
        call_error.as_deref(),
        Some("the call was cancelled"),
        "a cancelled call still closes as the cancel it was"
    );
    assert!(
        elapsed < FINISH_BUDGET,
        "the turn took {elapsed:?} to report the cancel, budget is {FINISH_BUDGET:?}"
    );
}
