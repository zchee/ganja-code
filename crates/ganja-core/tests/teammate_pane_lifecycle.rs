//! A `ganja` pane teammate's lifecycle, spawned through the `task` door on a
//! private tmux server: a real pane, alive, and killed when its
//! `shutdown_approved` is read (**AC-11**'s engine-side leg).
//!
//! Spec: Claude Code's teammates — §4.1's spawn sequence and §6.2's shutdown
//! handshake, read against this tree in §10.2 and §10.4. Upstream opencode has
//! no teammates and no counterpart to any of it.
//!
//! AC-11 as the spec spells it — `/team spawn w1 --backend ganja` in a real
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
//! # What is asserted
//!
//! 1. The door answers `backend: "ganja"` and the member record carries the
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

use ganja_core::protocol::team::{Frame, ShutdownApproved};
use ganja_core::teammate::reaper::Pane;
use ganja_core::teammate::tmux::{self, Server};
use ganja_team::{MailboxMessage, MemberName, mailbox, record};
use ganja_testkit::tmux::PrivateServer;
use pane_support::{expected_argv, pane_child_if_asked, run_one, spawn_pane_worker};

fn main() {
    pane_child_if_asked();
    run_one(
        "a_pane_teammate_spawned_with_backend_ganja_is_created_and_killed_on_shutdown_approved",
        a_pane_teammate_spawned_with_backend_ganja_is_created_and_killed_on_shutdown_approved(),
    );
}

async fn a_pane_teammate_spawned_with_backend_ganja_is_created_and_killed_on_shutdown_approved() {
    let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
    let config_home = ganja_testkit::temp_dir();
    let project = ganja_testkit::temp_dir();
    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written.
    unsafe {
        server.enter();
        std::env::set_var(ganja_core::config::CONFIG_HOME_ENV, config_home.path());
        std::env::set_var("XDG_DATA_HOME", project.path().join("data"));
    }
    assert!(tmux::hosted(), "the premise: this process is now inside tmux");

    // 1 and 3, the shared spine: through the task door on the pane surface,
    // the member record written, and the child's report read back through the
    // lead's own inbox pass.
    let spawned = spawn_pane_worker(
        config_home.path(),
        project.path(),
        "watch the build and say what breaks",
    )
    .await;
    assert_eq!(
        spawned.member.backend_type.as_deref(),
        Some("tmux"),
        "and the record says it is a pane: {:?}",
        spawned.member
    );
    let pane_id = spawned.pane_id.clone();
    assert_eq!(
        spawned.report.argv,
        expected_argv(&spawned.team, &spawned.member),
        "the pane was launched with the five spawn flags and nothing else"
    );
    assert_eq!(
        spawned.report.config_home.as_deref(),
        Some(config_home.path().to_str().expect("utf-8")),
        "the pane reads the lead's config home"
    );

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
    assert!(pane.birth.parse::<u32>().is_ok(), "the second half of the pair is a pid: {pane:?}");
    assert_eq!(spawned.registry.running(), 1, "and the registry holds it");
    assert_eq!(server.title(&pane_id), "worker", "§4.1 step 3: the pane wears the teammate's name");

    // 4. The lead half of §6.2: the teammate's `shutdown_approved`, read by the
    // same pass, retires the member — and the pane goes with it.
    let lead_inbox = spawned.root.inbox_path(&spawned.team, &MemberName::lead());
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
    let pass = spawned.inbox.poll().await;
    assert_eq!(
        pass.retired.iter().map(|gone| gone.name.as_str()).collect::<Vec<_>>(),
        vec!["worker"],
        "the pass read the approval: {pass:?}"
    );
    assert_eq!(
        pass.retired[0].pane_id.as_deref(),
        Some(pane_id.as_str()),
        "and it names the pane: {pass:?}"
    );

    let after =
        Server::at(server.socket(), None).panes().await.expect("the private server still lists");
    assert!(
        !after.iter().any(|live| pane.is(live)),
        "the pane was killed on shutdown_approved: {after:?}"
    );
    assert_eq!(spawned.registry.running(), 0, "the roster forgot it");
    let file = ganja_testkit::team_file(&spawned.root, &spawned.team)
        .expect("the team file is still there");
    assert!(file.member("worker").is_none(), "and so did the team file: {file:?}");

    // Idempotent on the way out: killing what is already gone is nothing.
    let killed = Server::at(server.socket(), None)
        .kill(&Pane { id: pane_id, birth: pane.birth })
        .await
        .expect("a second kill is answered");
    assert_eq!(killed, tmux::Killed::AlreadyGone);

    spawned.registry.shutdown().await;
    drop(server);
    drop(config_home);
    drop(project);
}
