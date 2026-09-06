use std::path::PathBuf;

use ganja_core::teammate::SpawnSpec;
use ganja_protocol::team::MemberBackend;
use ganja_team::{MemberName, TeamName, TeamsRoot};
use ganja_testkit::tmux::PrivateServer;

use super::{CARRIED_ENV, DEFAULT_SHARE, PaneShare, PaneShell, SHELL, arguments};
use crate::tmux::Server;

/// A spawn with every field a launch could be tempted to put on the line.
fn spec() -> SpawnSpec {
    SpawnSpec {
        name: MemberName::parse("worker").expect("a member name"),
        team: TeamName::parse("session-abcd1234").expect("a team name"),
        lead: MemberName::lead(),
        root: TeamsRoot::new("/nowhere/teams"),
        backend: MemberBackend::Ganja,
        agent_type: "general".to_owned(),
        model: "recorder-model".to_owned(),
        color: "blue".to_owned(),
        prompt: "sk-ant-CANARY-a-prompt-is-not-argv".to_owned(),
        cwd: PathBuf::from("/nowhere/project"),
        plan_mode_required: true,
        parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
    }
}

/// The five flags, in §4.1's order, each with its value — and **only**
/// those: the prompt is not on the line, and neither are the model, the
/// agent type or the plan posture, for the reasons in the module doc. No
/// posture rides the line at all (**D513**): `--auto` in particular is
/// not a word a lead ever composes.
/// The share is the default until a config names one, and the number
/// carried is the one named: what `-l` is handed is the teammates' side.
#[test]
fn the_pane_share_is_the_default_until_the_config_names_one() {
    assert_eq!(PaneShare::default().percent(), DEFAULT_SHARE);
    assert_eq!(DEFAULT_SHARE, 65, "lead 35, teammates 65 (2026-08-25)");
    assert_eq!(PaneShare::configured(40).percent(), 40);
}

#[test]
fn the_launch_line_is_the_five_spawn_flags_and_nothing_else() {
    let five = [
        "--agent-id",
        "worker@session-abcd1234",
        "--agent-name",
        "worker",
        "--team-name",
        "session-abcd1234",
        "--agent-color",
        "blue",
        "--parent-session-id",
        "01998ad0-0000-7000-8000-000000000000",
    ];
    let strings = |argv: Vec<std::ffi::OsString>| -> Vec<String> {
        argv.into_iter().map(|argument| argument.into_string().expect("ascii")).collect()
    };

    let argv = strings(arguments(&spec()));
    assert_eq!(argv, five);

    let line = argv.join(" ");
    assert!(!line.contains("CANARY"), "the prompt rides the mailbox: {line}");
    assert!(!line.contains("recorder-model"), "no model guess: {line}");
    assert!(!line.contains("general"), "no agent flag: {line}");
    assert!(!line.contains("plan"), "no plan-mode flag: {line}");
    assert!(!line.contains("--auto"), "no posture on the line: {line}");
}

/// The D502 re-import hazard's own guard: tmux hands a **one**-word
/// command to the person's login shell, which sources its rc files and
/// re-imports exactly the credentials the enumerated environment
/// withheld — so the idle argv is two words by construction, and
/// [`crate::tmux::Server::split`]'s debug assertion reads the
/// same rule at the seam.
#[test]
fn the_idle_shell_is_two_words_so_no_login_shell_rereads_it() {
    assert!(SHELL.len() >= 2, "{SHELL:?}");
    assert!(PaneShell::default().argv().len() >= 2);
    assert!(
        PaneShell::configured(vec!["/bin/zsh".to_owned()]).argv().len() >= 2,
        "a lone program is made two words"
    );
}

/// **D520.** A configured shell is the words the config gave, `-s`
/// appended only when it gave one; nothing given is the default.
#[test]
fn a_configured_shell_keeps_its_words_and_a_lone_program_gains_dash_s() {
    assert_eq!(PaneShell::default().words(), &SHELL[..]);
    assert_eq!(PaneShell::configured(vec!["/bin/zsh".to_owned()]).words(), ["/bin/zsh", "-s"]);
    assert_eq!(
        PaneShell::configured(vec!["/bin/zsh".to_owned(), "-f".to_owned()]).words(),
        ["/bin/zsh", "-f"],
        "two words are left exactly as written"
    );
    assert_eq!(PaneShell::configured(Vec::new()), PaneShell::default());
}

/// The closed list holds directory names and never a credential's.
#[test]
fn no_credential_name_is_in_the_carried_environment() {
    for name in CARRIED_ENV {
        assert!(
            !name.contains("KEY") && !name.contains("PASSWORD") && !name.contains("TOKEN"),
            "{name} has no business on a pane's launch"
        );
    }
}

/// A pane's corner and width, as `list-panes` prints them for one window.
struct Corner {
    id: String,
    left: u16,
    top: u16,
    width: u16,
}

/// The lead's window, read back as corners — the geometry a placement
/// decision is judged by, since where a pane sits is a fact about a screen.
fn corners(server: &PrivateServer, lead: &str) -> Vec<Corner> {
    server
        .run(&["list-panes", "-t", lead, "-F", "#{pane_id} #{pane_left} #{pane_top} #{pane_width}"])
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.split(' ');
            let mut next = || fields.next().expect("four fields per pane").to_owned();
            let id = next();
            let (left, top, width) = (next(), next(), next());
            Corner {
                id,
                left: left.parse().expect("a column"),
                top: top.parse().expect("a row"),
                width: width.parse().expect("a width"),
            }
        })
        .collect()
}

/// **Bead `lr79`.** Three spawns that overlap — what one assistant step's
/// concurrent `task` calls produce (**D462**) — land in **one** column, not
/// three.
///
/// Each backend builds its own [`Server`] per spawn, so the three servers
/// here are the shape production has; what serializes them is the placement
/// gate, and nothing else. The failure this pins is a check-then-act: three
/// spawns read "no column yet" before any of them had split, and each took
/// the lead's own pane as its target, so the lead was divided three times and
/// every teammate opened a column of its own.
///
/// Geometry is the assertion because geometry is the bug: the three panes
/// share a left edge right of the lead's, they sit at three different heights
/// in it, and the lead plus one column plus one divider is the whole window —
/// which is the arithmetic of a lead that was split exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn concurrent_spawns_share_one_column_instead_of_each_opening_one() {
    let private = PrivateServer::start(&["sleep", "3600"], &[], &[]);
    let lead = private.first_pane().to_owned();
    // Read back rather than restated: the width is the testkit's own choice,
    // and a copy of it here would fail the arithmetic assertion below naming
    // the wrong cause on the day that choice moves.
    let width: u32 = private
        .run(&["display-message", "-p", "-t", &lead, "#{window_width}"])
        .trim()
        .parse()
        .expect("tmux answers a window width");
    let cwd = ganja_testkit::temp_dir();
    let spawn = |name: &str| SpawnSpec {
        name: MemberName::parse(name).expect("a member name"),
        cwd: cwd.path().to_path_buf(),
        ..spec()
    };
    let split = |name: &str| {
        let server = Server::at(private.socket(), Some(lead.clone()));
        let spec = spawn(name);
        async move {
            super::split_idle_shell(
                &server,
                &spec,
                &[],
                &PaneShell::default(),
                PaneShare::default(),
                MemberBackend::Ganja,
                "ganja teammate",
            )
            .await
            .expect("a pane is split")
        }
    };

    let (first, second, third) = tokio::join!(split("w1"), split("w2"), split("w3"));

    let corners = corners(&private, &lead);
    assert_eq!(corners.len(), 4, "the lead and its three teammates");
    let edge = corners.iter().find(|corner| corner.id == lead).expect("the lead is in its window");
    let column: Vec<&Corner> = [&first, &second, &third]
        .iter()
        .map(|pane| {
            corners.iter().find(|corner| corner.id == pane.id).expect("each split pane is listed")
        })
        .collect();

    let lefts: std::collections::BTreeSet<u16> = column.iter().map(|corner| corner.left).collect();
    assert_eq!(
        lefts.len(),
        1,
        "the three teammates share one column: {:?}",
        column.iter().map(|corner| (&corner.id, corner.left, corner.top)).collect::<Vec<_>>()
    );
    let left = *lefts.iter().next().expect("one left edge");
    assert!(left > edge.left, "and it is right of the lead ({left} > {})", edge.left);

    let tops: std::collections::BTreeSet<u16> = column.iter().map(|corner| corner.top).collect();
    assert_eq!(tops.len(), 3, "stacked, one under another");

    // A lead split once keeps everything the column and its divider do not
    // take; a lead split three times keeps a sliver.
    assert_eq!(
        u32::from(edge.width) + u32::from(column[0].width) + 1,
        width,
        "the window is the lead, one teammates' column, and the divider between them"
    );
}
