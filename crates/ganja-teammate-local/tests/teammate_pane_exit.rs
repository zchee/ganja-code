//! A `ganja` pane teammate whose pane is killed out from under the lead
//! (**D541**): the member's own watch notices, posts an
//! [`Exited`](ganja_core::teammate::Exited) the lead's next pass retires it
//! on, and stops being counted alive.
//!
//! Spec: none. Neither upstream opencode nor Claude Code is being read here —
//! this is ganja's answer to bead `ganja-code-okip`, observed in the W2 live
//! check on 2026-08-28: after `tmux kill-pane` on a running pane teammate the
//! lead's `/teammate` kept listing it for forty seconds and beyond, because
//! nothing on this side ever asked whether the pane was still there.
//! [`ganja_teammate_local::reaper`] is a cold-start sweep of a *previous*
//! lead's orphans (**D506**), not a poll of this one's own panes.
//!
//! **Hard-fails without tmux**, for [`teammate_pane_lifecycle`]'s reason: a
//! pane test that skipped where there was no tmux would be green on exactly
//! the machines where nothing was tested.
//!
//! # Its own `main`
//!
//! `harness = false`, the third binary in this crate to be so, and for the
//! reason `tests/pane_support/mod.rs` states in full: the program `pane.rs`
//! runs in the pane is `current_exe()`, which inside a test binary is the test
//! binary carrying five flags libtest would refuse on sight.
//!
//! # What is asserted
//!
//! 1. The spawn is the ordinary one — the same door, the same registry, the
//!    same `pane.rs` — and the member is on the roster, alive.
//! 2. `tmux kill-pane` on its pane, which is what a person closing it does.
//! 3. The member stops being counted alive **before anything else touches
//!    it**: no pass has run and nothing called its kill, so the only thing
//!    that can have cleared that flag is its own watch — and that flag is what
//!    a `/teammate` render reads, which is the whole of what the bead observed.
//! 4. The lead's own pass then drains one `Exited` carrying `cli: None` (a
//!    `ganja` pane runs no CLI this build shims for), the `Ganja` backend,
//!    that pane's id, and `PaneFate::Closed` — read off the pane through the
//!    dead-only door rather than assumed — and retires the member on it the
//!    way a `shutdown_approved` would have: out of the team file, under the
//!    `backendType` its own record was written with.
//!
//! The drain is
//! [`take_exited`](ganja_core::teammate::TeammateRegistry::take_exited)'s,
//! reached through its **one production caller** rather than called here, and
//! accumulated across passes because it takes each entry exactly once: a test
//! that drained the registry itself would take the entry away from the very
//! pass whose retirement it then wanted to assert.

mod pane_support;

use std::time::Duration;

use ganja_core::teammate::PaneFate;
use ganja_protocol::team::MemberBackend;
use ganja_testkit::tmux::PrivateServer;
use pane_support::{IDLE_WINDOW, pane_child_if_asked, run_one, spawn_pane_worker};

/// How long the watch gets to notice. Two poll periods and a pass would do it;
/// this is a debug binary on a machine running the rest of the suite, so the
/// bound is generous and the *assertion* is that it happens at all.
const NOTICES: Duration = Duration::from_secs(30);

fn main() {
    pane_child_if_asked();
    run_one(
        "a_pane_teammate_whose_pane_is_killed_reports_its_own_exit_and_leaves_the_roster",
        a_pane_teammate_whose_pane_is_killed_reports_its_own_exit_and_leaves_the_roster(),
    );
}

async fn a_pane_teammate_whose_pane_is_killed_reports_its_own_exit_and_leaves_the_roster() {
    let server = PrivateServer::start(&IDLE_WINDOW, &[], &[]);
    let config_home = ganja_testkit::temp_dir();
    let project = ganja_testkit::temp_dir();
    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written.
    unsafe {
        server.enter();
        std::env::set_var(ganja_core::config::CONFIG_HOME_ENV, config_home.path());
        std::env::set_var("XDG_DATA_HOME", project.path().join("data"));
    }

    // 1. The ordinary spawn, and the member on the roster.
    let spawned =
        spawn_pane_worker(config_home.path(), project.path(), "hold the fort until you are told")
            .await;
    let pane_id = spawned.pane_id.clone();
    assert_eq!(spawned.registry.running(), 1, "the teammate is alive before anything happens");

    // 2. What a person closing the pane does, and what the live check did.
    server.run(&["kill-pane", "-t", &pane_id]);

    // 3. Nothing here has touched the member: no pass has run, and nothing
    // called its kill. So the only thing that can have taken it off the roster
    // is its own watch clearing the flag `/teammate` reads.
    ganja_testkit::eventually(NOTICES, "the pane's own watch to stop counting it", async || {
        (spawned.registry.running() == 0).then_some(())
    })
    .await;
    assert!(
        !spawned.registry.view().members.iter().any(|member| member.name == "worker"),
        "and `/teammate` stopped listing it"
    );

    // 4. The lead's pass drains the exit and retires the member on it. Each
    // entry is taken exactly once, so a pass that finds nothing yet must keep
    // whatever it took rather than let it evaporate on the next one.
    let mut exited_so_far = Vec::new();
    let mut retired_so_far = Vec::new();
    let exited =
        ganja_testkit::eventually(NOTICES, "the lead's pass to read the exit", async || {
            let pass = spawned.inbox.poll().await;
            exited_so_far.extend(pass.exited);
            retired_so_far.extend(pass.retired);
            exited_so_far.iter().find(|exited| exited.name == "worker").cloned()
        })
        .await;

    assert_eq!(exited.cli, None, "a `ganja` pane runs no CLI this build shims for");
    assert_eq!(exited.backend, MemberBackend::Ganja);
    assert_eq!(exited.pane_id, pane_id);
    assert_eq!(exited.pane, PaneFate::Closed, "what the pane was left as is read, not assumed");
    assert_eq!(exited.last_words, None, "no `remain-on-exit`, so there is no screen to quote");
    assert_eq!(
        exited_so_far.iter().filter(|exited| exited.name == "worker").count(),
        1,
        "one exit per member, whatever the watch does afterwards: {exited_so_far:?}"
    );

    let retired: Vec<(&str, Option<&str>, Option<&str>)> = retired_so_far
        .iter()
        .map(|gone| (gone.name.as_str(), gone.pane_id.as_deref(), gone.backend_type.as_deref()))
        .collect();
    assert_eq!(
        retired,
        vec![("worker", Some(pane_id.as_str()), Some("tmux"))],
        "retired once, naming its pane, under the word its own record was written with"
    );
    let file = ganja_testkit::team_file(&spawned.root, &spawned.team)
        .expect("the team file is still there");
    assert!(file.member("worker").is_none(), "the team file forgot it too: {file:?}");

    spawned.registry.shutdown().await;
    drop(server);
    drop(config_home);
    drop(project);
}
