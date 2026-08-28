use std::path::PathBuf;

use ganja_core::teammate::SpawnSpec;
use ganja_protocol::team::MemberBackend;
use ganja_team::{MemberName, TeamName, TeamsRoot};

use super::{CARRIED_ENV, DEFAULT_SHARE, PaneShare, PaneShell, SHELL, arguments};

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
