use std::path::PathBuf;
use std::sync::Arc;

use ganja_core::teammate::{Lent, PaneFate, SETTLE, SpawnSpec, Spawned as _};
use ganja_protocol::team::MemberBackend;
use ganja_team::{MemberName, ShimCli, TeamName, TeamsRoot};
use ganja_testkit::tmux::PrivateServer;

use crate::pane::PaneMember;
use crate::reaper::Pane;
use crate::shim_tui::{Readiness, TuiPane};
use crate::tmux::Server;

pub(crate) struct Fixture {
    pub private: PrivateServer,
    pub server: Server,
    pub pane: Pane,
    pub spec: SpawnSpec,
    pub lent: Lent,
}

impl Fixture {
    pub async fn new() -> Self {
        let private = PrivateServer::start(&["/bin/sh", "-s"], &[], &[]);
        let id = private.split(None, &[], &["/bin/sh", "-s"]);
        let server = Server::at(private.socket(), None);
        let pane = server
            .panes()
            .await
            .expect("private panes")
            .into_iter()
            .find(|pane| pane.id == id)
            .expect("the test pane is live");
        let spec = SpawnSpec {
            name: MemberName::parse("bounded-worker").expect("member name"),
            team: TeamName::parse("session-deadline").expect("team name"),
            lead: MemberName::lead(),
            root: TeamsRoot::new("/unused/teams"),
            backend: MemberBackend::Ganja,
            agent_type: "general".to_owned(),
            model: "fake".to_owned(),
            color: "blue".to_owned(),
            prompt: "unused".to_owned(),
            cwd: PathBuf::from("/unused"),
            plan_mode_required: false,
            parent_session_id: "deadline-test".to_owned(),
        };
        let (exits, _) = tokio::sync::mpsc::unbounded_channel();
        let lent = Lent {
            lead_inbox: PathBuf::from("/unused/lead.json"),
            cancel: tokio_util::sync::CancellationToken::new(),
            dialogs: None,
            exits,
            registry: std::sync::Weak::new(),
        };
        Self { private, server, pane, spec, lent }
    }

    pub fn freeze(&self) -> FrozenServer {
        let pid = self
            .private
            .run(&["display-message", "-p", "#{pid}"])
            .trim()
            .parse::<i32>()
            .expect("private server pid");
        // SAFETY: this pid belongs to the private test server, not to the user's server.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        FrozenServer(pid)
    }
}

pub(crate) struct FrozenServer(i32);

impl Drop for FrozenServer {
    fn drop(&mut self) {
        // SAFETY: resume the private server before its owner sends kill-server, even on panic.
        unsafe { libc::kill(self.0, libc::SIGCONT) };
    }
}

pub(crate) struct RecordingTmux {
    _directory: tempfile::TempDir,
    pub server: Server,
    calls: PathBuf,
}

impl RecordingTmux {
    pub fn holding(from: usize, replies: &[(&str, i32)]) -> Self {
        use std::fmt::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("recording tmux directory");
        let binary = directory.path().join("tmux");
        let calls = directory.path().join("calls");
        std::fs::write(directory.path().join("count"), "0\n").expect("initial command count");
        let mut script = format!(
            "#!/bin/sh\nread -r count < \"$2/count\"\ncount=$((count + 1))\nprintf '%s\\n' \"$count\" > \"$2/count\"\nprintf '%s\\n' \"$3\" >> \"$2/calls\"\nif [ \"$count\" -ge {from} ]; then exec /bin/sleep 60; fi\ncase \"$count\" in\n"
        );
        for (index, (reply, code)) in replies.iter().enumerate() {
            assert!(!reply.contains('\''));
            writeln!(script, "{}) printf '%s\\n' '{reply}'; exit {code};;", index + 1)
                .expect("script reply");
        }
        script.push_str("esac\n");
        std::fs::write(&binary, script).expect("recording tmux executable");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))
            .expect("executable permission");
        let server = Server::at(directory.path(), None).with_binary(binary);
        Self { _directory: directory, server, calls }
    }

    pub fn commands(&self) -> Vec<String> {
        std::fs::read_to_string(&self.calls)
            .expect("tmux calls were recorded")
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[tokio::test]
async fn every_pane_kill_command_obeys_the_cleanup_ceiling() {
    let fixture = Fixture::new().await;
    for (held, replies) in [
        (1, vec![]),
        (2, vec![("0 %42 2147483647", 0)]),
        (3, vec![("0 %42 2147483647", 0), ("refused", 1)]),
    ] {
        let tmux = RecordingTmux::holding(held, &replies);
        let member = PaneMember::new(
            Pane { id: "%42".into(), birth: "2147483647".into() },
            "test",
            tmux.server.clone(),
            &fixture.spec,
            &fixture.lent,
        );
        tokio::time::timeout(SETTLE * 2, member.kill())
            .await
            .expect("the whole identity-checked kill has one deadline");
        assert_eq!(tmux.commands().len(), held, "the intended command was held");
        assert!(!member.alive());
    }
}

#[tokio::test]
async fn every_tui_cleanup_path_obeys_the_member_ceiling() {
    let fixture = Fixture::new().await;
    let scenarios = [
        (1, vec![]),
        // A live group that never disappears holds both grace polls and the fallback kill.
        (2, vec![("0 %42 2147483647", 0)]),
        // No live pane: the dead-only close lists, closes, then checks again.
        (2, vec![("", 0)]),
        (3, vec![("", 0), ("%42 1", 0)]),
        (4, vec![("", 0), ("%42 1", 0), ("", 0)]),
        // A live-again pane falls back to identity-checked kill and its failure relist.
        (3, vec![("", 0), ("%42 0", 0)]),
        (4, vec![("", 0), ("%42 0", 0), ("0 %42 2147483647", 0)]),
        (5, vec![("", 0), ("%42 0", 0), ("0 %42 2147483647", 0), ("refused", 1)]),
    ];
    for (held, replies) in scenarios {
        let tmux = RecordingTmux::holding(held, &replies);
        let pane = Arc::new(TuiPane::new(
            ShimCli::Codex,
            MemberBackend::Codex,
            Pane { id: "%42".into(), birth: "2147483647".into() },
            tmux.server.clone(),
            Readiness::Seen,
        ));
        let member = crate::shim_tui::TuiMember::new(
            Arc::clone(&pane),
            fixture.spec.clone(),
            fixture.lent.clone(),
        );
        tokio::time::timeout(SETTLE * 6, member.kill())
            .await
            .expect("all TUI cleanup operations fit the documented member ceiling");
        assert!(tmux.commands().len() >= held, "the intended command was held");
        assert!(!member.alive());
        assert!(pane.cancel().is_cancelled());
    }
}

#[tokio::test]
async fn a_stalled_tmux_does_not_hold_pane_kill_or_its_second_call() {
    let fixture = Fixture::new().await;
    let member = PaneMember::new(
        fixture.pane.clone(),
        "test",
        fixture.server.clone(),
        &fixture.spec,
        &fixture.lent,
    );
    let frozen = fixture.freeze();

    tokio::time::timeout(SETTLE * 2, member.kill()).await.expect("pane kill has its own deadline");
    assert!(!member.alive(), "local state is retired even when tmux cannot confirm the kill");
    drop(frozen);
    assert!(fixture.private.panes().contains(&fixture.pane.id), "timeout is not confirmed removal");
    tokio::time::timeout(SETTLE * 2, member.kill())
        .await
        .expect("a second kill retries an unconfirmed pane");
    assert!(!fixture.private.panes().contains(&fixture.pane.id));
    tokio::time::timeout(SETTLE / 2, member.kill())
        .await
        .expect("a confirmed kill performs no more tmux work");
}

#[tokio::test]
async fn a_stalled_tmux_leaves_tui_teardown_unconfirmed_and_idempotent() {
    let fixture = Fixture::new().await;
    let pane = TuiPane::new(
        ShimCli::Codex,
        MemberBackend::Codex,
        fixture.pane.clone(),
        fixture.server.clone(),
        Readiness::Seen,
    );
    let frozen = fixture.freeze();

    let fate = tokio::time::timeout(SETTLE * 2, pane.end())
        .await
        .expect("TUI teardown has its own deadline");
    assert_eq!(fate, PaneFate::Left, "a held listing cannot confirm closure");
    assert!(pane.cancel().is_cancelled(), "the runner is cancelled despite the timeout");
    drop(frozen);
    assert!(fixture.private.panes().contains(&fixture.pane.id));
    assert_eq!(
        tokio::time::timeout(SETTLE * 6, pane.end())
            .await
            .expect("a second end retries an unconfirmed pane"),
        PaneFate::Closed
    );
    assert!(!fixture.private.panes().contains(&fixture.pane.id));
    assert_eq!(
        tokio::time::timeout(SETTLE / 2, pane.end())
            .await
            .expect("a confirmed end performs no tmux work"),
        PaneFate::Closed
    );
}
