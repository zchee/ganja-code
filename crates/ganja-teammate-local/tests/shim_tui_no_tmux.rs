//! **AC-4 (P28, D512).** A shim TUI spawn outside tmux refuses by name — the
//! **D501** sentence — and falls back to nothing: no headless child, no pane,
//! no member, and the stub CLI on the path is never run.
//!
//! A binary of its own because it unsets `$TMUX`, which is process-wide
//! state: the rest of the pane-mode suite (`shim_tui.rs`) points its backend
//! at a private server and never touches the environment. Here the backend
//! is left to read `$TMUX` exactly as production does, with a stub TUI on
//! the search path so the only thing missing is the session.

mod shim_support;

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ganja_core::teammate::TeammateRegistry;
use ganja_core::{Backends, Storage, Teammates};
use ganja_team::{MemberName, TeamName, TeamsRoot, mailbox};
use ganja_teammate_local::claude::ClaudePane;
use ganja_teammate_local::codex::Codex;
use ganja_teammate_local::pane::{GanjaPane, PaneShare, PaneShell};
use ganja_teammate_local::shim_tui::ShimTui;
use ganja_teammate_local::tmux::{self, REFUSED_NO_TMUX};
use ganja_testkit::AllowSpawn;
use shim_support::{Fake, SESSION_ID};

/// A stub that would behave like a composer if it were ever run — and
/// records that it was.
const STUB: &str = r#"#!/bin/sh
printf 'argv:%s\n' "$*" >> '@LOG@'
printf 'Ask Codex to do anything\n'
exec cat
"#;

/// A lead whose three shim slots are the pane-mode backend reading `$TMUX`,
/// the codex one pointed at the stub.
fn lead(
    home: &Path,
    path: OsString,
) -> (Arc<TeammateRegistry>, Arc<Teammates>, TeamsRoot, TeamName) {
    let registry = Arc::new(TeammateRegistry::for_session(home, SESSION_ID, home));
    let storage = Storage::open(home.join("storage"));
    let (shell, share) = (PaneShell::default(), PaneShare::default());
    let backends = Backends::new()
        .with_in_process(Arc::new(ganja_core::teammate::InProcess::new(
            Arc::new(ganja_core::provider::FakeProvider::new("on it", Duration::ZERO)),
            Arc::new(ganja_core::tool::Registry::new(Vec::new())),
            storage,
            |_: &ganja_core::teammate::SpawnSpec| ganja_core::permission::Permissions::default(),
        )))
        .with(Arc::new(GanjaPane::default()))
        .with(Arc::new(ClaudePane::default()))
        .with(Arc::new(ShimTui::new(Arc::new(Codex::new()), shell.clone(), share).searching(path)))
        .with(Arc::new(
            ShimTui::new(Arc::new(ganja_teammate_local::agy::Agy::new()), shell.clone(), share)
                .searching(OsString::new()),
        ))
        .with(Arc::new(
            ShimTui::new(Arc::new(ganja_teammate_local::grok::Grok::new()), shell, share)
                .searching(OsString::new()),
        ));
    let door = Arc::new(Teammates::new(Arc::clone(&registry), backends));
    let root = registry.root().clone();
    let team = registry.team().clone();

    (registry, door, root, team)
}

#[tokio::test]
async fn a_shim_tui_spawn_without_tmux_is_refused_by_name_and_runs_nothing() {
    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written. Both
    // variables go, because tmux exports both into a pane and either one left
    // behind would name the developer's own server.
    unsafe {
        std::env::remove_var(tmux::TMUX);
        std::env::remove_var(tmux::TMUX_PANE);
    }
    assert!(!tmux::hosted(), "the premise: this process is outside tmux");

    let home = ganja_testkit::temp_dir();
    let stub = Fake::install(&[("codex", STUB)], "tui");
    let (registry, door, root, team) = lead(home.path(), stub.path());

    for backend in ["codex", "agy", "grok"] {
        let refused = door
            .start(
                ganja_testkit::spawn("worker", Some(backend)),
                &ganja_testkit::caller(home.path()),
                &AllowSpawn,
            )
            .await
            .expect_err("a session outside tmux has no pane to give");
        assert!(
            refused.reason.ends_with(REFUSED_NO_TMUX),
            "{backend} refuses in the sentence that names the session as what is missing: {}",
            refused.reason
        );
        assert!(
            refused.reason.contains(backend),
            "and still says which surface was asked for: {}",
            refused.reason
        );
    }

    // No headless fallback and no pane: the stub on the path was never run.
    assert!(stub.received().is_empty(), "the stub CLI was never started: {:?}", stub.received());
    // And nothing of ours is left behind: no member, no seeded task.
    assert!(
        ganja_testkit::team_file(&root, &team)
            .map(|file| file.member("worker").is_none())
            .unwrap_or(true),
        "no member was recorded"
    );
    let inbox = root.inbox_path(&team, &MemberName::parse("worker").expect("a name"));
    assert_eq!(
        mailbox::read(&inbox).map(|contents| contents.valid.len()).unwrap_or(0),
        0,
        "the seeded prompt was taken back out"
    );
    assert!(registry.view().members.iter().all(|member| member.name != "worker"));
}
