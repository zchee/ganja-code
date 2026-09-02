//! Two real processes, one shared task list: the lead files the work and a
//! `ganja` pane teammate claims it, finishes it and is read back.
//!
//! This is the wave's load-bearing drill, and what it is here to prove is the
//! half no single-process test can: that the four task tools reach the
//! documents in the team's own directory **from another process**, under the
//! claiming discipline two processes need. Nothing is faked into either
//! binary's environment — the lead runs in a pane of a private tmux server,
//! splits a second pane for its teammate the way a person's `/teammate spawn`
//! does, and both drive the same directory through their own registered
//! tools.
//!
//! Spec: this port's own team-orchestration behavior specification. Neither
//! upstream has a shared task list two agents work from, so there is nothing
//! of either's to port.
//!
//! **Hard-fails without tmux.** A pane test that skipped where there is no
//! tmux would be green on exactly the machines where nothing was tested.
//!
//! # What is asserted, in order
//!
//! 1. The lead's own turn files a task: the document is in the team directory
//!    it made, pending and owned by nobody.
//! 2. `/teammate spawn w1 --backend ganja` puts a second `ganja` in a pane of
//!    its own — a whole other process, with its own engine and its own tools.
//! 3. That process claims the task under **its own** name and completes it,
//!    and the documents are what say so: an owner it wrote, and a comment
//!    stamped with the same name it claimed under. Nothing in this test wrote
//!    either.
//! 4. The lead reads the list back through its own `task_list`, and what its
//!    screen shows is the status and the owner its teammate wrote.
//!
//! # The two scripts, deliberately apart
//!
//! Each process plays its own fake-provider script, and which one it gets is
//! decided by where the variable is set. The private server is born holding
//! the **member's** script, so every pane it ever makes inherits that one; the
//! lead's own pane is handed the **lead's** through tmux's `-e`, which reaches
//! that pane and no other. `GANJA_FAKE_SCRIPT` is not in `pane.rs`'s carried
//! environment, so the lead's override cannot travel to the pane it spawns —
//! which is what keeps the two conversations from playing each other's turns.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ganja_core::team::TeamFile;
use ganja_core::team::task::{Store, TaskId, TaskStatus};
use serde_json::json;

mod pane_lead;

use pane_lead::{Homes, Tmux};

/// How long each stage is given: two debug `ganja` binaries starting cold in
/// panes, each taking turns of its own.
const DEADLINE: Duration = Duration::from_secs(45);

/// How many lines of the lead's log a timed-out wait quotes.
const LOG_TAIL: usize = 80;

/// The teammate's name, which is also the owner its claim writes.
const MEMBER: &str = "w1";

/// What the lead files, short enough to read back off a pane's screen whole.
const SUBJECT: &str = "port the parser";

/// What the teammate says about the task, appearing nowhere else — so finding
/// it in the document means that process wrote it.
const NOTE: &str = "the lexer was the hard half, zarquon";

/// What the member's fake provider says once it is done, appearing nowhere
/// else — so finding it on the pane's screen means the member ran its turn.
const REPLY: &str = "pane-tasks-reply-zarquon";

/// The lead's own script.
const LEAD_SCRIPT: &str = "lead.json";

/// The member's, which the server is born holding.
const MEMBER_SCRIPT: &str = "member.json";

/// What the composer draws when nothing else owns the screen — the sign that
/// the next line typed reaches the composer rather than an overlay.
const COMPOSER: &str = "Ask ganja something";

/// The lead's first prompt. Its text reaches the model, which is playing a
/// script and ignores it; what matters is that a turn starts.
const FILE_PROMPT: &str = "file the work";

/// The lead's second, after the teammate has finished.
const LIST_PROMPT: &str = "how did it go";

/// The two homes, the two scripts, and this suite's reads of the team the
/// lead keeps under its config home.
struct Fixture {
    homes: Homes,
}

impl Fixture {
    fn new() -> Self {
        let homes = Homes::new();
        // The lead: file the task, then read the list back.
        //
        // A fake-provider script is played one entry per **request**, and a
        // session makes requests this test does not ask for — a title, at a
        // moment of its own choosing. So the listing is scripted **twice**,
        // back to back, and the entries after it say nothing: whichever of
        // the two the second prompt lands on, it lists, and whichever it does
        // not becomes the step that ends the turn. Pinning one exact index
        // would be pinning a race.
        homes.script(
            LEAD_SCRIPT,
            json!([
                {
                    "text": "Filing it.",
                    "tool_calls": [{"name": "task_create", "args": {
                        "subject": SUBJECT,
                        "description": "start from the spec",
                    }}],
                },
                {"text": "Filed."},
                {
                    "text": "Reading the list.",
                    "tool_calls": [{"name": "task_list", "args": {}}],
                },
                {
                    "text": "Reading the list.",
                    "tool_calls": [{"name": "task_list", "args": {}}],
                },
                {"text": "The team finished it."},
                {"text": "The team finished it."},
                {"text": "The team finished it."},
            ]),
        );
        // The member: claim the task and finish it in **one** turn, so that
        // which of its own next two requests is the title and which is the
        // step cannot matter.
        homes.script(
            MEMBER_SCRIPT,
            json!([
                {
                    "text": "Taking it.",
                    "tool_calls": [
                        {"name": "task_update", "args": {
                            "task_id": "1",
                            "owner": MEMBER,
                            "status": "in_progress",
                        }},
                        {"name": "task_update", "args": {
                            "task_id": "1",
                            "status": "completed",
                            "add_comment": NOTE,
                        }},
                    ],
                },
                {"text": REPLY},
                {"text": REPLY},
            ]),
        );

        Self { homes }
    }

    /// The config home the lead runs under, and therefore where the team —
    /// and its task list — lives.
    fn config_home(&self) -> PathBuf {
        self.homes.config_home()
    }

    /// The environment the **server** is born from, and so what every pane
    /// inherits: the member's script among it, since the member's pane is one
    /// the lead makes rather than one this test does.
    fn server_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", self.homes.data().display().to_string()),
            ("GANJA_PROVIDER", "fake".to_owned()),
            ("GANJA_FAKE_SCRIPT", self.homes.project().join(MEMBER_SCRIPT).display().to_string()),
            ("GANJA_DISABLE_MODELS_FETCH", "1".to_owned()),
        ]
    }

    /// What the lead's own pane is additionally given (`-e`): where its things
    /// are, and its own script in place of the server's.
    fn lead_env(&self) -> Vec<(&'static str, String)> {
        pane_lead::lead_env(&self.homes, &self.homes.project().join(LEAD_SCRIPT))
    }

    /// The team directory the lead made — the only one under the config home.
    fn team_dir(&self) -> Option<PathBuf> {
        let mut dirs: Vec<PathBuf> = fs::read_dir(self.config_home().join("teams"))
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        assert!(dirs.len() <= 1, "one lead is one team: {dirs:?}");

        dirs.into_iter().next()
    }

    fn team_file(&self) -> Option<TeamFile> {
        let text = fs::read_to_string(self.team_dir()?.join("config.json")).ok()?;

        Some(serde_json::from_str(&text).expect("the team file this build wrote decodes"))
    }

    /// The shared task list, read the way any other process on this machine
    /// would read it — which is the whole point: nothing here goes through
    /// either running binary to see what it did.
    fn tasks(&self) -> Option<Store> {
        Some(Store::new(self.team_dir()?.join("tasks")))
    }
}

/// Polls `read` every 50ms until it answers, or panics with `what`, the
/// lead's screen and the tail of the log the lead — and the member sharing its
/// data home — traced, after [`DEADLINE`].
fn wait_for<T>(
    what: &str,
    tmux: &Tmux,
    fixture: &Fixture,
    lead: &str,
    mut read: impl FnMut() -> Option<T>,
) -> T {
    let started = Instant::now();
    loop {
        if let Some(found) = read() {
            return found;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "waited {DEADLINE:?} for {what} and it did not happen; the lead's screen:\n{}\n{}",
            tmux.screen(lead),
            pane_lead::log_tail(&pane_lead::log_dir(&fixture.homes), LOG_TAIL)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The task, as the documents hold it right now — or nothing, where the list
/// does not exist yet or has not got that far.
fn task(fixture: &Fixture, id: &TaskId) -> Option<ganja_core::team::task::Task> {
    fixture.tasks()?.get(id).ok()
}

/// **W3's acceptance drill.** A lead and a `ganja` pane teammate drive one
/// shared list end to end: filed by one process, claimed and completed by
/// another, read back by the first.
#[test]
fn a_lead_and_a_pane_teammate_drive_one_task_list_end_to_end() {
    let fixture = Fixture::new();
    let tmux = Tmux::start(&fixture.server_env());
    let one = TaskId::parse("1").expect("the first id a counter issues");

    // The lead, in a pane of its own in the project directory — so tmux gives
    // it `TMUX` and `TMUX_PANE` itself. Two words on purpose (`env` and the
    // binary): a one-word command would go through the login shell.
    let lead = tmux.split(
        fixture.homes.project(),
        &fixture.lead_env(),
        &["/usr/bin/env", env!("CARGO_BIN_EXE_ganja")],
    );
    wait_for("the lead to draw its composer", &tmux, &fixture, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });

    // 1. The lead's own turn files the work — before there is anybody to do
    // it, which is the order the pipeline actually runs in.
    tmux.type_line(&lead, FILE_PROMPT);
    let filed =
        wait_for("the lead to file the task", &tmux, &fixture, &lead, || task(&fixture, &one));
    assert_eq!(filed.subject, SUBJECT);
    assert_eq!(filed.status, TaskStatus::Pending, "a filed task is pending");
    assert!(filed.owner.is_empty(), "and belongs to nobody: {filed:?}");

    // 2. A second `ganja`, in a pane of its own: another process, another
    // engine, the same team directory.
    wait_for("the composer to take the next line", &tmux, &fixture, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    tmux.type_line(&lead, &format!("/teammate spawn {MEMBER} --backend ganja"));
    let member = wait_for("the member record", &tmux, &fixture, &lead, || {
        fixture
            .team_file()?
            .member(MEMBER)
            .cloned()
            .filter(|member| member.tmux_pane_id.starts_with('%'))
    });
    let pane = member.tmux_pane_id.clone();
    wait_for("the launch line to reach the pane", &tmux, &fixture, &lead, || {
        (tmux.current_command(&pane) == "ganja").then_some(())
    });

    // 3. That process claims the task and finishes it. The documents are what
    // say so — the owner it wrote, and a comment stamped with the name it
    // claimed under, which is the name its launch line carried and not
    // anything its arguments could have chosen.
    let done = wait_for("the teammate to finish the task", &tmux, &fixture, &lead, || {
        task(&fixture, &one).filter(|task| task.status == TaskStatus::Completed)
    });
    assert_eq!(done.owner, MEMBER, "the claim wrote the teammate's own name: {done:?}");
    assert_eq!(
        done.comments.iter().map(|comment| comment.from.as_str()).collect::<Vec<_>>(),
        [MEMBER],
        "and so did the comment"
    );
    assert_eq!(done.comments[0].text, NOTE);
    wait_for("the member's own turn to finish on its screen", &tmux, &fixture, &lead, || {
        tmux.screen(&pane).contains(REPLY).then_some(())
    });

    // 4. And the lead reads it back through its own `task_list`: the status
    // and the owner another process wrote, on the lead's screen.
    wait_for("the composer to take the next line", &tmux, &fixture, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    tmux.type_line(&lead, LIST_PROMPT);
    wait_for("the lead's listing to show the finished task", &tmux, &fixture, &lead, || {
        let screen = tmux.screen(&lead);
        (screen.contains("[completed]") && screen.contains(MEMBER)).then_some(())
    });

    // Both leave cleanly, with nothing left running: the lead asks, the
    // member approves and its pane goes, and then the lead's own does.
    wait_for("the composer to come back", &tmux, &fixture, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    tmux.type_line(&lead, &format!("/teammate shutdown {MEMBER}"));
    wait_for("the pane to be killed on the approval", &tmux, &fixture, &lead, || {
        (!tmux.panes().contains(&pane)).then_some(())
    });
    wait_for("the composer to come back", &tmux, &fixture, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    tmux.key(&lead, "C-c");
    wait_for("the lead to leave", &tmux, &fixture, &lead, || {
        (!tmux.panes().contains(&lead)).then_some(())
    });
    assert_eq!(tmux.panes().len(), 1, "only the server's first pane is left");
    drop(tmux);
}
