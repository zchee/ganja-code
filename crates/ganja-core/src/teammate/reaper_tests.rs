use ganja_team::{MemberName, MemberRecord, Spawn, Surface, TeamFile, TeamName, TeamsRoot, record};

use super::{AGENT_ID, Fate, PARENT_SESSION_ID, Pane, argv_of, flagged, forget, verdict};
use crate::teammate::TeammateRegistry;

fn pane(id: &str, birth: &str) -> Pane {
    Pane {
        id: id.to_owned(),
        birth: birth.to_owned(),
    }
}

#[test]
fn a_recycled_pane_id_is_not_the_pane_that_was_recorded() {
    let recorded = pane("%142", "48213");

    assert!(recorded.is(&pane("%142", "48213")));
    // Same id, a different first process: tmux handed `%142` out again.
    // Whether that pid is larger or smaller is nothing — a pid is a name
    // the kernel reuses, not a clock.
    assert!(!recorded.is(&pane("%142", "52117")));
    assert!(!recorded.is(&pane("%142", "9")));
    // And a different id is not it either, whatever runs in it.
    assert!(!recorded.is(&pane("%143", "48213")));
}

/// The rule, as a table: only a pane that is both live and witnessed is
/// killed, and only a pane that could be looked at is decided about.
#[test]
fn only_a_live_and_witnessed_pane_is_ended() {
    let live = pane("%17", "48213");

    assert_eq!(verdict(Some(&live), Some(true)), None, "kill this one");
    assert_eq!(verdict(Some(&live), Some(false)), Some(Fate::Recycled));
    assert_eq!(verdict(Some(&live), None), Some(Fate::Undecided));
    assert_eq!(verdict(None, None), Some(Fate::Vanished));
    // A witness for a pane that is not there decides nothing: the pane is
    // what is missing, and it is missing either way.
    assert_eq!(verdict(None, Some(true)), Some(Fate::Vanished));
}

/// A record leaves the file when its teammate is demonstrably not running,
/// and stays when nothing could be established.
#[test]
fn only_a_settled_fate_drops_a_record() {
    assert!(Fate::Reaped.drops_the_record());
    assert!(Fate::Vanished.drops_the_record());
    assert!(Fate::Recycled.drops_the_record());
    assert!(!Fate::Undecided.drops_the_record());
}

/// The word rule, and the two collisions a substring test cannot see: a
/// member name that is a suffix of a sibling's, and a session id that is a
/// prefix of another's.
#[test]
fn a_flag_matches_the_word_after_it_and_never_a_substring_of_the_line() {
    let argv = "/x/ganja --agent-id rebuild@session-01998ad0 \
                    --parent-session-id 01998ad0-0000-7000-8000-000000000000";

    assert!(flagged(argv, AGENT_ID, "rebuild@session-01998ad0"));
    assert!(
        !flagged(argv, AGENT_ID, "build@session-01998ad0"),
        "`build` is a suffix of `rebuild` and is not the same teammate"
    );
    assert!(flagged(
        argv,
        PARENT_SESSION_ID,
        "01998ad0-0000-7000-8000-000000000000"
    ));
    assert!(
        !flagged(argv, PARENT_SESSION_ID, "01998ad0-0000-7000-8000-0"),
        "a prefix of a session id is a different lead"
    );
    // A flag that is there without its value, and a value that is there
    // without its flag, are both nothing.
    assert!(!flagged("/x/ganja --agent-id", AGENT_ID, "worker@t"));
    assert!(!flagged("/x/ganja worker@t", AGENT_ID, "worker@t"));
    // clap's other spelling, which a person may well type by hand.
    assert!(flagged(
        "/x/ganja --agent-id=worker@t",
        AGENT_ID,
        "worker@t"
    ));
    assert!(!flagged(
        "/x/ganja --agent-id-of=worker@t",
        AGENT_ID,
        "worker@t"
    ));
}

/// The witness's own mechanism, against the one process this test is sure
/// about: itself. Pins the `ps` invocation, which is the half of D506 a
/// machine can break without any test noticing.
#[tokio::test]
async fn a_processs_own_command_line_is_what_the_witness_reads() {
    let argv = argv_of(&std::process::id().to_string())
        .await
        .expect("a live process has a command line");

    assert!(!argv.is_empty(), "and it is not empty: {argv:?}");
    assert!(
        argv_of("0").await.is_none(),
        "a pid nothing answers for is unknown, never a 'no'"
    );
}

/// Dropping one member rewrites the document without it and leaves every
/// other row — the lead's included — where it was.
#[tokio::test]
async fn dropping_a_record_leaves_the_rest_of_the_team_file_alone() {
    let home = tempfile::tempdir().expect("a temporary teams root");
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-01998ad0").expect("a team name");
    let session = "01998ad0-0000-7000-8000-000000000000";
    let cwd = home.path().to_string_lossy().into_owned();

    let mut file = TeamFile::new(&team, session, cwd.clone(), record::now_millis());
    for (name, id) in [("worker", "%17"), ("scribe", "%18")] {
        file.members.push(MemberRecord::teammate(
            &MemberName::parse(name).expect("a member name"),
            &team,
            Spawn {
                agent_type: "general".to_owned(),
                model: "fake/fake".to_owned(),
                color: "blue".to_owned(),
                prompt: "watch the build".to_owned(),
                plan_mode_required: false,
                surface: Surface::Pane { id: id.to_owned() },
                cwd: cwd.clone(),
            },
            record::now_millis(),
        ));
    }
    let path = root.config_path(&team);
    std::fs::create_dir_all(path.parent().expect("a team directory"))
        .expect("the team directory is made");
    std::fs::write(
        &path,
        record::document(&file).expect("the team file encodes"),
    )
    .expect("the team file is written");

    let registry = TeammateRegistry::new(root, team, session, home.path());
    forget(&registry, "worker").await;

    let written: TeamFile =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the team file is read"))
            .expect("the team file decodes");
    let names: Vec<&str> = written
        .members
        .iter()
        .map(|member| member.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["team-lead", "scribe"],
        "only the dropped member is gone: {written:?}"
    );
}
