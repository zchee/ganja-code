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

use std::time::Duration;

use ganja_core::team::task::{Store, TaskId, TaskStatus};
use serde_json::json;

mod pane_lead;

use pane_lead::{COMPOSER, Homes, Tmux};

/// How long each stage is given: two debug `ganja` binaries starting cold in
/// panes, each taking turns of its own.
const DEADLINE: Duration = Duration::from_secs(45);

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

    /// The environment the **server** is born from, and so what every pane
    /// inherits: the member's script among it, since the member's pane is one
    /// the lead makes rather than one this test does.
    fn server_env(&self) -> Vec<(&'static str, String)> {
        pane_lead::server_env(&self.homes, Some(&self.homes.project().join(MEMBER_SCRIPT)))
    }

    /// What the lead's own pane is additionally given (`-e`): where its things
    /// are, and its own script in place of the server's.
    fn lead_env(&self) -> Vec<(&'static str, String)> {
        pane_lead::lead_env(&self.homes, Some(&self.homes.project().join(LEAD_SCRIPT)))
    }

    /// The shared task list, read the way any other process on this machine
    /// would read it — which is the whole point: nothing here goes through
    /// either running binary to see what it did.
    fn tasks(&self) -> Option<Store> {
        Some(Store::new(pane_lead::team_dir(&self.homes)?.join("tasks")))
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
    let tmux = Tmux::start(&fixture.homes, &fixture.server_env(), DEADLINE);
    let one = TaskId::parse("1").expect("the first id a counter issues");

    // The lead, in a pane of its own in the project directory — so tmux gives
    // it `TMUX` and `TMUX_PANE` itself.
    let lead = tmux.lead(&fixture.homes, &fixture.lead_env());

    // Before anything else: this lead's socket is in the fixture's own
    // directory, not in the developer's `/tmp/ganja-<uid>/`. A lead binds
    // whether or not it leads anybody (**D542**), so a drill that let it use
    // the default would add a `.sock`, a `.json` record and a `.lock` to the
    // socket directory this machine's own sessions are listed from — the
    // `.lock` for good, since a lock file is never removed. Asserted where
    // the flag can actually be observed to have worked rather than where it
    // is spelled: an ignored `--socket-dir` reads as an empty directory here.
    let bound = tmux.wait_for("the lead to bind its socket", &lead, || {
        let found = pane_lead::bound_sockets(&fixture.homes);
        (!found.is_empty()).then_some(found)
    });
    assert_eq!(bound.len(), 1, "one session is one socket: {bound:?}");
    assert!(
        bound[0].with_extension("json").exists(),
        "and its registration record is beside it: {bound:?}"
    );

    // 1. The lead's own turn files the work — before there is anybody to do
    // it, which is the order the pipeline actually runs in.
    tmux.type_line(&lead, FILE_PROMPT);
    let filed = tmux.wait_for("the lead to file the task", &lead, || task(&fixture, &one));
    assert_eq!(filed.subject, SUBJECT);
    assert_eq!(filed.status, TaskStatus::Pending, "a filed task is pending");
    assert!(filed.owner.is_empty(), "and belongs to nobody: {filed:?}");

    // 2. A second `ganja`, in a pane of its own: another process, another
    // engine, the same team directory.
    // Typed into an **idle** lead, the way `team_continuation_pane.rs` types
    // its own spawn line (bead `d61w`): the composer's placeholder is drawn
    // whenever the buffer is empty, a streaming reply included, so it is no
    // sign the filing turn ended — and a spawn landing before that turn's
    // tail leaves the guard a team to read, which continues the turn and
    // spends script entries this drill's step 4 still needs (bead `f4di`).
    // The wait at step 1 is what makes `ready` mean "ended" here: the bar
    // reads it before a turn starts too.
    tmux.wait_for("the filing turn to have ended", &lead, || {
        pane_lead::idle(&tmux.screen(&lead)).then_some(())
    });
    tmux.type_line(&lead, &format!("/teammate spawn {MEMBER} --backend ganja"));
    let member = tmux.wait_for("the member record", &lead, || {
        pane_lead::team_file(&fixture.homes)?
            .member(MEMBER)
            .cloned()
            .filter(|member| member.tmux_pane_id.starts_with('%'))
    });
    let pane = member.tmux_pane_id.clone();
    // Waited on the **member's** pane from here to the end of step 3: what
    // these three watch is that process, so its screen is what a timeout has
    // to quote. The lead's comes along beside it, since a spawn that was
    // refused says so on the lead's bar and nowhere else.
    tmux.wait_for("the launch line to reach the pane", &pane, || {
        (tmux.current_command(&pane) == "ganja").then_some(())
    });

    // 3. That process claims the task and finishes it. The documents are what
    // say so — the owner it wrote, and a comment stamped with the name it
    // claimed under, which is the name its launch line carried and not
    // anything its arguments could have chosen.
    let done = tmux.wait_for("the teammate to finish the task", &pane, || {
        task(&fixture, &one).filter(|task| task.status == TaskStatus::Completed)
    });
    assert_eq!(done.owner, MEMBER, "the claim wrote the teammate's own name: {done:?}");
    assert_eq!(
        done.comments.iter().map(|comment| comment.from.as_str()).collect::<Vec<_>>(),
        [MEMBER],
        "and so did the comment"
    );
    assert_eq!(done.comments[0].text, NOTE);
    tmux.wait_for("the member's own turn to finish on its screen", &pane, || {
        tmux.screen(&pane).contains(REPLY).then_some(())
    });

    // 4. And the lead reads it back through its own `task_list`: the status
    // and the owner another process wrote, on the lead's screen.
    // The status bar's own word rather than the composer's placeholder (bead
    // `d61w`): the placeholder is drawn whenever the buffer is empty, a
    // streaming reply included, so it is no sign that the filing turn ended —
    // and this line is a **prompt**, which typed into a running turn is a
    // steer instead. What makes `ready` mean "ended" here is the wait at step
    // 1 having already proved that turn ran a tool; the bar reads `ready`
    // before a turn starts too. The `/teammate shutdown` lines below keep the
    // placeholder: a UI command runs from `submit` ahead of the steer branch,
    // so it is the same command whichever it lands in. The spawn line above
    // waits on `idle` for a different reason — the team it creates is what
    // would make the filing turn continue (bead `f4di`).
    tmux.wait_for("the filing turn to have ended", &lead, || {
        pane_lead::idle(&tmux.screen(&lead)).then_some(())
    });
    tmux.type_line(&lead, LIST_PROMPT);
    tmux.wait_for("the lead's listing to show the finished task", &lead, || {
        // The listing's own line, whole — `ganja_tool::tasklist` renders one
        // as `<id> [<status>] owner <name> — <subject>`. The member's name on
        // its own carries nothing here: it has been on this screen since the
        // `/teammate spawn w1` line was typed into it. What only the listing
        // can put there is the owner *beside this task's own subject*, which
        // is the pair another process wrote and this one read back.
        tmux.screen(&lead)
            .contains(&format!("[completed] owner {MEMBER} — {SUBJECT}"))
            .then_some(())
    });

    // Both leave cleanly, with nothing left running: the lead asks, the
    // member approves and its pane goes, and then the lead's own does.
    tmux.wait_for("the composer to come back", &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    tmux.type_line(&lead, &format!("/teammate shutdown {MEMBER}"));
    tmux.wait_for("the pane to be killed on the approval", &lead, || {
        (!tmux.panes().contains(&pane)).then_some(())
    });
    tmux.wait_for("the composer to come back", &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    tmux.key(&lead, "C-c");
    tmux.wait_for("the lead to leave", &lead, || (!tmux.panes().contains(&lead)).then_some(()));
    assert_eq!(tmux.panes().len(), 1, "only the server's first pane is left");
}
