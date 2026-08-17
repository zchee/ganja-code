//! A `ganja` pane teammate's lifecycle, spawned through the `task` door on a
//! private tmux server: a real pane, alive, and killed when its
//! `shutdown_approved` is read (**AC-11**'s engine-side leg).
//!
//! Spec: Claude Code's teammates — §4.1's spawn sequence and §6.2's shutdown
//! handshake, read against this tree in §10.2 and §10.4. Upstream opencode has
//! no teammates and no counterpart to any of it.
//!
//! AC-11 as the spec spells it — `/team spawn w1 --backend pane` in a real
//! `ganja`, in a PTY — is `crates/ganja-cli/tests/teammate_pane.rs`, which needs
//! the pane child to be the real binary parsing the spawn flags. This binary is
//! the half a core test can hold: the same door, the same registry, the same
//! `pane.rs`, and a pane child that is *this binary* standing in for `ganja`.
//!
//! **Hard-fails without tmux.** A pane test that skipped where there was no
//! tmux would be green on exactly the machines where nothing was tested.
//!
//! # Its own `main`
//!
//! `harness = false`, because the program `pane.rs` runs in the pane is
//! `current_exe()` — this binary — carrying the five spawn flags, and libtest
//! would refuse them. `pane_support::pane_child_if_asked` is the pane's half:
//! it finds the team through `GANJA_CONFIG_HOME`, writes a report into the
//! lead's inbox, and waits to be killed. The test's half is everything below
//! it. One test, because it sets `TMUX`, `TMUX_PANE`, `GANJA_CONFIG_HOME` and
//! `XDG_DATA_HOME` for the whole process.
//!
//! # What is asserted, in order
//!
//! 1. The door answers `backend: "pane"` and the member record carries the
//!    pane's id under `tmuxPaneId` with `backendType: "tmux"`.
//! 2. tmux lists the pane, alive, with the pid `pane.rs` recorded as its
//!    birth — the pair the reaper matches on.
//! 3. The pane's process is *this* binary running as a teammate: its report
//!    reaches the lead's inbox through the lead's own §6.2 pass, carrying the
//!    five flags in `pane.rs`'s order.
//! 4. The shutdown handshake's lead half: a `shutdown_approved` from the
//!    teammate, read by the same pass, retires the member — the pane is gone
//!    from tmux, the roster and the team file.

mod pane_support;

use std::{sync::Arc, time::Duration};

use ganja_core::{
    protocol::team::{Frame, ShutdownApproved},
    teammate::{
        lead_inbox::LeadInbox,
        pane::{AGENT_COLOR, AGENT_ID, AGENT_NAME, PARENT_SESSION_ID, TEAM_NAME},
        reaper::Pane,
        tmux::{self, Server},
    },
    tool::{
        Tool as _,
        task::{Offered, TaskTool},
    },
};
use ganja_team::{MailboxMessage, MemberName, TeamFile, mailbox, record};
use pane_support::{
    PrivateServer, Report, SESSION_ID, ctx, lead, pane_child_if_asked, run_one, task_args, team_of,
    wait_for,
};

/// How long the pane's process gets to start and report. Generous: it is a
/// debug test binary being exec'd cold on a machine running the rest of the
/// suite.
const CHILD_STARTS: Duration = Duration::from_secs(30);

fn main() {
    pane_child_if_asked();
    run_one(
        "a_pane_teammate_spawned_with_backend_pane_is_created_and_killed_on_shutdown_approved",
        a_pane_teammate_spawned_with_backend_pane_is_created_and_killed_on_shutdown_approved(),
    );
}

async fn a_pane_teammate_spawned_with_backend_pane_is_created_and_killed_on_shutdown_approved() {
    let server = PrivateServer::start(&[]);
    let config_home = ganja_testkit::temp_dir();
    let project = ganja_testkit::temp_dir();
    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written.
    unsafe {
        server.enter();
        std::env::set_var(ganja_core::config::CONFIG_HOME_ENV, config_home.path());
        std::env::set_var("XDG_DATA_HOME", project.path().join("data"));
    }
    assert!(
        tmux::hosted(),
        "the premise: this process is now inside tmux"
    );

    let (registry, door) = lead(config_home.path(), project.path());
    let (root, team) = team_of(&registry);
    let door = Arc::new(door);
    let tool = TaskTool::new(&[Offered {
        name: "general".to_owned(),
        description: None,
    }]);
    let ctx = ctx(project.path(), Arc::clone(&door));

    // 1. Through the task door, on the pane surface.
    let output = tool
        .run(
            task_args("worker", "pane", "watch the build and say what breaks"),
            &ctx,
        )
        .await
        .expect("the door spawns a pane teammate inside tmux");
    assert_eq!(
        output.metadata.get("backend").and_then(|on| on.as_str()),
        Some("pane"),
        "the surface it really runs on: {output:?}"
    );
    let file: TeamFile = serde_json::from_str(
        &std::fs::read_to_string(root.config_path(&team)).expect("the team file is written"),
    )
    .expect("the team file decodes");
    let member = file
        .member("worker")
        .unwrap_or_else(|| panic!("the pane teammate joined the team: {file:?}"))
        .clone();
    assert!(
        member.tmux_pane_id.starts_with('%'),
        "§2.2's tmuxPaneId is the pane's own id: {member:?}"
    );
    assert_eq!(
        member.backend_type.as_deref(),
        Some("tmux"),
        "and the record says it is a pane: {member:?}"
    );
    let pane_id = member.tmux_pane_id.clone();

    // 2. tmux agrees: the pane is live, and the pair it is listed under —
    // read back through production's own listing — is `(pane_id, pid)`.
    // That the pair the registry *recorded* is this one is not read here (a
    // handle is the registry's own); it is proved at the tail, where a kill by
    // the recorded pair ends this very pane and a second one finds it gone.
    let live = Server::at(server.socket(), None)
        .panes()
        .await
        .expect("the private server lists its panes");
    let pane = live
        .iter()
        .find(|pane| pane.id == pane_id)
        .unwrap_or_else(|| panic!("the pane {pane_id} is on the server: {live:?}"))
        .clone();
    assert!(
        pane.birth.parse::<u32>().is_ok(),
        "the second half of the pair is a pid: {pane:?}"
    );
    assert_eq!(registry.running(), 1, "and the registry holds it");
    assert_eq!(
        server.title(&pane_id),
        "worker",
        "§4.1 step 3: the pane wears the teammate's name"
    );

    // 3. The pane's process is this binary, running as the teammate: it finds
    // the team through the carried config home and reports to the lead — read
    // through the lead's own inbox pass, the way a real lead reads it.
    let inbox = LeadInbox::new(Arc::clone(&registry));
    let report = wait_for(
        CHILD_STARTS,
        "the pane's report to reach the lead",
        async || {
            let pass = inbox.poll().await;
            pass.messages
                .into_iter()
                .find(|message| message.from == "worker")
                .map(|message| {
                    serde_json::from_str::<Report>(&message.body).unwrap_or_else(|error| {
                        panic!("the pane wrote a report: {error} in {message:?}")
                    })
                })
        },
    )
    .await;
    assert_eq!(
        report.argv,
        [
            AGENT_ID,
            &format!("worker@{}", team.as_str()),
            AGENT_NAME,
            "worker",
            TEAM_NAME,
            team.as_str(),
            AGENT_COLOR,
            member.color.as_deref().expect("a spawn assigns a colour"),
            PARENT_SESSION_ID,
            SESSION_ID,
        ],
        "the pane was launched with the five spawn flags and nothing else"
    );
    assert_eq!(
        report.config_home.as_deref(),
        Some(config_home.path().to_str().expect("utf-8")),
        "the pane reads the lead's config home"
    );

    // 4. The lead half of §6.2: the teammate's `shutdown_approved`, read by the
    // same pass, retires the member — and the pane goes with it.
    let lead_inbox = root.inbox_path(&team, &MemberName::lead());
    mailbox::write(
        &lead_inbox,
        MailboxMessage::from_frame(
            "worker",
            &Frame::ShutdownApproved(ShutdownApproved {
                request_id: "shutdown-1".to_owned(),
                from: "worker".to_owned(),
                timestamp: record::now_iso8601(),
                pane_id: Some(pane_id.clone()),
                backend_type: Some("tmux".to_owned()),
            }),
            record::now_iso8601(),
        )
        .expect("a frame encodes"),
    )
    .expect("the approval is written");
    let pass = inbox.poll().await;
    assert_eq!(
        pass.retired
            .iter()
            .map(|gone| gone.name.as_str())
            .collect::<Vec<_>>(),
        vec!["worker"],
        "the pass read the approval: {pass:?}"
    );
    assert_eq!(
        pass.retired[0].pane_id.as_deref(),
        Some(pane_id.as_str()),
        "and it names the pane: {pass:?}"
    );

    let after = Server::at(server.socket(), None)
        .panes()
        .await
        .expect("the private server still lists");
    assert!(
        !after.iter().any(|live| pane.is(live)),
        "the pane was killed on shutdown_approved: {after:?}"
    );
    assert_eq!(registry.running(), 0, "the roster forgot it");
    let file: TeamFile = serde_json::from_str(
        &std::fs::read_to_string(root.config_path(&team)).expect("the team file is still there"),
    )
    .expect("the team file decodes");
    assert!(
        file.member("worker").is_none(),
        "and so did the team file: {file:?}"
    );

    // Idempotent on the way out: killing what is already gone is nothing.
    let killed = Server::at(server.socket(), None)
        .kill(&Pane {
            id: pane_id,
            birth: pane.birth,
        })
        .await
        .expect("a second kill is answered");
    assert_eq!(killed, tmux::Killed::AlreadyGone);

    registry.shutdown().await;
    drop(server);
    drop(config_home);
    drop(project);
}
