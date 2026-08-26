use std::{ffi::OsString, path::PathBuf};

use ganja_protocol::team::MemberBackend;
use ganja_team::{MemberName, TeamName, TeamsRoot, mailbox};

use super::{
    BINARY, ClaudePane, PLAN_MODE_REQUIRED, TEAMS_DIRECTORY, arguments, carried_env, preamble,
    root_under,
};
// `shim::resolve` is the hoisted walk. These tests stayed here because what
// they pin is what *this* backend's binary resolution must refuse — a
// shadowing directory, a file this process may not execute — and they are
// the reason the hoist changed no behaviour.
use crate::teammate::{
    SpawnSpec, TeammateBackend as _,
    pane::CARRIED_ENV,
    shim::resolve,
    tmux::{REFUSED_NO_TMUX, TmuxError},
};

/// A spawn with every field a launch could be tempted to put on the line,
/// and a prompt wearing a canary.
fn spec() -> SpawnSpec {
    SpawnSpec {
        name: MemberName::parse("worker").expect("a member name"),
        team: TeamName::parse("session-abcd1234").expect("a team name"),
        lead: MemberName::lead(),
        root: TeamsRoot::new("/nowhere/teams"),
        backend: MemberBackend::Claude,
        agent_type: "general".to_owned(),
        model: "recorder-model".to_owned(),
        color: "blue".to_owned(),
        prompt: "sk-ant-CANARY-a-prompt-is-not-argv".to_owned(),
        cwd: PathBuf::from("/nowhere/project"),
        plan_mode_required: false,
        parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
        shell: crate::teammate::pane::PaneShell::default(),
        share: crate::teammate::pane::PaneShare::default(),
    }
}

fn strings(argv: Vec<OsString>) -> Vec<String> {
    argv.into_iter()
        .map(|argument| argument.into_string().expect("ascii"))
        .collect()
}

/// §4.1's five, then plan mode only when the spawn asked for it — and
/// never the prompt, the model, the agent type or a permission mode, for
/// the reasons in the module doc.
#[test]
fn the_launch_line_is_the_spawn_flags_and_plan_mode_when_it_was_asked_for() {
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

    assert_eq!(strings(arguments(&spec())), five);

    let posturing = strings(arguments(&SpawnSpec {
        plan_mode_required: true,
        ..spec()
    }));
    assert_eq!(posturing[..five.len()], five);
    assert_eq!(posturing[five.len()..], [PLAN_MODE_REQUIRED]);

    let line = posturing.join(" ");
    assert!(
        !line.contains("CANARY"),
        "the prompt rides the mailbox: {line}"
    );
    assert!(!line.contains("recorder-model"), "no model guess: {line}");
    assert!(!line.contains("general"), "no agent-type guess: {line}");
    assert!(
        !line.contains("--permission-mode"),
        "no permission mode is composed (D513): {line}"
    );
}

/// The composed line, as tmux is handed it: `exec`, the binary — bare,
/// because no byte of that path needs quoting — and never the prompt.
/// (The one-word login-shell hazard is a property of the *idle* argv,
/// pinned at `pane::SHELL`; this line is typed with `send-keys -l`,
/// which no shell re-reads.)
#[test]
fn the_composed_line_execs_the_binary_and_the_prompt_stays_off_it() {
    let line = crate::teammate::tmux::launch_line(
        &PathBuf::from("/usr/local/bin/claude"),
        &arguments(&spec()),
    )
    .expect("no NUL rides the spawn flags")
    .into_string()
    .expect("ascii");
    assert!(line.starts_with("exec /usr/local/bin/claude "), "{line}");
    // The canary again, on the *composed* line rather than on `arguments`
    // alone: the line is what tmux is handed and what `ps`
    // would print, so it is the value the §4.1-step-5 rule is really about.
    assert!(
        !line.contains("CANARY"),
        "the prompt rides the mailbox: {line}"
    );
}

/// The carried set is the `ganja` pane's closed list plus the one variable
/// this backend's root is a function of — and still no credential name.
#[test]
fn the_carried_environment_adds_the_claude_config_dir_and_nothing_else() {
    let mut expected: Vec<&str> = CARRIED_ENV.to_vec();
    expected.push("CLAUDE_CONFIG_DIR");
    assert_eq!(carried_env(), expected);

    for name in carried_env() {
        assert!(
            !name.contains("KEY") && !name.contains("PASSWORD") && !name.contains("TOKEN"),
            "{name} has no business on a pane's launch"
        );
    }
}

/// The root is the variable when there is one, the home when there is not,
/// and nothing at all when there is neither — with an empty variable read
/// as unset.
#[test]
fn the_teams_root_follows_the_config_dir_and_falls_back_to_the_home() {
    let named = root_under(
        Some(OsString::from("/tmp/claude-home")),
        Some(PathBuf::from("/home/somebody")),
    )
    .expect("a named config dir is a root");
    assert_eq!(
        named.inbox_path(
            &TeamName::parse("session-abcd1234").expect("a team name"),
            &MemberName::lead(),
        ),
        PathBuf::from("/tmp/claude-home")
            .join(TEAMS_DIRECTORY)
            .join("session-abcd1234")
            .join("inboxes")
            .join("team-lead.json")
    );

    let fallen = root_under(None, Some(PathBuf::from("/home/somebody")))
        .expect("a home is a root when the variable is unset");
    assert_eq!(
        fallen.config_path(&TeamName::parse("session-abcd1234").expect("a team name")),
        PathBuf::from("/home/somebody/.claude/teams/session-abcd1234/config.json")
    );

    assert_eq!(
        root_under(Some(OsString::new()), Some(PathBuf::from("/home/somebody"))),
        Some(fallen),
        "an empty variable is unset, not the root directory"
    );
    assert!(root_under(None, None).is_none());
}

/// §5.5.1, as the thing a worker actually reads: its lead by name, and
/// `main` named as the address that will not work.
#[test]
fn the_preamble_names_the_lead_and_says_main_is_not_an_address() {
    let seeded = preamble(&spec());
    assert!(seeded.contains("team-lead"), "{seeded}");
    assert!(seeded.contains("main"), "{seeded}");
    assert!(
        seeded.ends_with("sk-ant-CANARY-a-prompt-is-not-argv"),
        "the task is what the message ends with: {seeded}"
    );
}

/// The one place the repo's "two literals agreeing proves nothing" rule
/// inverts: **D514** moved this message onto the shared frame, and what
/// that migration must not have done is change a byte a real `claude` has
/// been reading since P25. So the pre-refactor text is pinned as a literal,
/// deliberately, and may go once D514 is old.
#[test]
fn the_preamble_is_byte_for_byte_what_it_was_before_the_shared_frame() {
    let spec = spec();

    assert_eq!(
        preamble(&spec),
        format!(
            "You are worker, a teammate on the team session-abcd1234. Your lead is team-lead.\n\n\
                 Address the lead by that name — `SendMessage(to: \"team-lead\")`. Do **not** address \
                 \"main\": you are the main conversation of your own session, so it has no parent for \
                 \"main\" to name and the send fails. Everything after this arrives the same way this \
                 did, through your inbox.\n\n\
                 Your task:\n\n{}",
            spec.prompt
        )
    );
}

/// A session with no tmux refuses in the sentence AC-16 asserts — the same
/// one the `ganja` pane refuses in, because one door must not say two
/// things about one missing session.
#[test]
fn a_session_without_tmux_is_refused_in_the_sentence_the_other_pane_uses() {
    let refused = ClaudePane::refused(&TmuxError::NotHosted);
    assert_eq!(refused.backend, MemberBackend::Claude);
    assert_eq!(refused.reason, REFUSED_NO_TMUX);
    assert!(
        refused.to_string().contains("claude"),
        "and still names the surface asked for: {refused}"
    );
}

/// The delivery and backend answers are pinned beside the other backends'
/// in `tests/teammate_backends.rs`; what is this file's alone is the inbox
/// ownership the registry's seed-skip reads.
#[test]
fn a_claude_pane_owns_its_inbox_so_the_registry_must_not_seed_it() {
    assert!(
        ClaudePane.owns_inbox(),
        "the registry must not write a second message into this inbox"
    );
}

/// **One** message in the teammate's inbox, and it is the preamble.
///
/// The defect this pins: with the registry seeding too, the bare
/// prompt landed here first and a real `claude` read the one message that
/// does not tell it how to address its lead. Drivable without a tmux server
/// or a `claude` on the machine, because seeding is file work and nothing
/// else — which is why it had no coverage at all and now does.
#[tokio::test]
async fn seeding_leaves_exactly_one_message_and_it_is_the_preamble() {
    let home = tempfile::tempdir().expect("a temporary claude config home");
    let root = TeamsRoot::new(home.path().join(TEAMS_DIRECTORY));
    let spec = spec();

    let seeded = ClaudePane::seed(&spec, &root)
        .await
        .expect("the seed lands");

    let inbox = root.inbox_path(&spec.team, &spec.name);
    let held = mailbox::read(&inbox).expect("the inbox reads").valid;
    assert_eq!(held.len(), 1, "one writer, one message: {held:?}");
    assert_eq!(held[0].from, spec.lead.as_str());
    assert_eq!(held[0].text, preamble(&spec));
    assert!(
        held[0].text.contains("Do **not** address"),
        "and it is the message that says so: {}",
        held[0].text
    );
    assert_eq!(
        mailbox::identity(&held[0]),
        seeded,
        "the identity handed back names the entry that landed"
    );
    // The lead's inbox exists before the pane can answer into it, which is
    // what keeps two processes from racing to create it — and what the
    // lead's own pass over this root reads.
    assert!(
        mailbox::read(&root.inbox_path(&spec.team, &spec.lead))
            .expect("the lead's inbox reads")
            .valid
            .is_empty(),
        "seeded, and empty"
    );
}

/// A launch refused after the seed leaves nothing behind — the claude root's
/// inbox included, which the registry's own unwind cannot reach.
#[tokio::test]
async fn a_refused_launch_takes_the_seeded_prompt_back_out() {
    let home = tempfile::tempdir().expect("a temporary claude config home");
    let root = TeamsRoot::new(home.path().join(TEAMS_DIRECTORY));
    let spec = spec();
    let seeded = ClaudePane::seed(&spec, &root)
        .await
        .expect("the seed lands");

    crate::teammate::unseed_inbox(
        root.inbox_path(&spec.team, &spec.name),
        Some(seeded),
        spec.name.as_str(),
    )
    .await;

    let inbox = root.inbox_path(&spec.team, &spec.name);
    assert!(
        mailbox::read(&inbox)
            .expect("the inbox reads")
            .valid
            .is_empty(),
        "a prompt nothing will read does not stay in a mailbox"
    );
    assert!(inbox.exists(), "the inbox itself is left where it was");
}

/// The `PATH` search returns the first runnable file, skips directories and
/// candidates this process cannot execute, and never interprets an empty
/// entry as the working directory.
///
/// Unix-only because the fixtures use Unix permission classes to establish
/// which candidate the test process may execute.
#[cfg(unix)]
#[test]
fn the_binary_is_the_first_path_entry_holding_something_runnable() {
    use std::os::unix::fs::PermissionsExt as _;

    let home = tempfile::tempdir().expect("a temporary PATH");
    // A directory by the right name, then a file nobody may run, then the
    // one a resolve should find.
    let shadow = home.path().join("shadow");
    let unrunnable = home.path().join("unrunnable");
    let real = home.path().join("real");
    std::fs::create_dir_all(shadow.join(BINARY)).expect("a directory in the way");
    for directory in [&unrunnable, &real] {
        std::fs::create_dir_all(directory).expect("a PATH entry");
    }
    let decoy = unrunnable.join(BINARY);
    let found = real.join(BINARY);
    for (path, mode) in [(&decoy, 0o644), (&found, 0o755)] {
        std::fs::write(path, "#!/bin/sh\n").expect("a candidate is written");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("its mode is set");
    }

    let path = std::env::join_paths([
        std::path::Path::new(""),
        shadow.as_path(),
        unrunnable.as_path(),
        real.as_path(),
    ])
    .expect("a PATH joins");

    assert_eq!(resolve(&path, BINARY).as_deref(), Some(found.as_path()));
    assert!(
        resolve(std::ffi::OsStr::new(""), BINARY).is_none(),
        "an empty PATH resolves nothing rather than the working directory"
    );

    let shadow_only = std::env::join_paths([shadow.as_path()]).expect("a PATH joins");
    assert!(
        resolve(&shadow_only, BINARY).is_none(),
        "a directory is not a file"
    );

    let unrunnable_only = std::env::join_paths([unrunnable.as_path()]).expect("a PATH joins");
    assert!(
        resolve(&unrunnable_only, BINARY).is_none(),
        "a file this process may not run is skipped"
    );

    let home_only = std::env::join_paths([home.path()]).expect("a PATH joins");
    assert!(resolve(&home_only, "absent").is_none());
}

/// An execute bit for another permission class does not make a binary
/// runnable by the process that owns it.
#[cfg(unix)]
#[test]
fn an_execute_bit_for_another_permission_class_does_not_make_the_binary_runnable() {
    use std::os::unix::fs::PermissionsExt as _;

    // SAFETY: `geteuid` only reads the process credentials and has no
    // memory-safety preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // POSIX gives root special X_OK handling: any execute bit suffices,
        // so this permission-class discriminator does not exist for root.
        return;
    }

    let home = tempfile::tempdir().expect("a temporary PATH");
    let candidate = home.path().join(BINARY);
    std::fs::write(&candidate, "#!/bin/sh\n").expect("a candidate is written");
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o001))
        .expect("only another permission class may execute it");

    let mode = std::fs::metadata(&candidate)
        .expect("the candidate has metadata")
        .permissions()
        .mode();
    assert_ne!(
        mode & 0o111,
        0,
        "the old any-execute-bit check would accept this candidate"
    );

    let path = std::env::join_paths([home.path()]).expect("a PATH joins");
    assert!(
        resolve(&path, BINARY).is_none(),
        "access(2) rejects another class's execute permission for the owner"
    );
}
