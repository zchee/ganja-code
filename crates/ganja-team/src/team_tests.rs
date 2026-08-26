use std::path::Path;

use super::{MemberName, NameError, TeamName, TeamsRoot, resolve_unique};

#[test]
fn main_is_refused_as_a_member_name() {
    assert_eq!(MemberName::parse("main"), Err(NameError::Reserved));
    // A team is not a recipient, so the reservation does not reach it.
    assert!(TeamName::parse("main").is_ok());
    // And the reservation is exact: refusing a spelling a real `claude`
    // accepts would put the two builds' member lists out of step.
    assert!(MemberName::parse("Main").is_ok());
}

#[test]
fn a_colliding_name_gets_a_counter_suffix() {
    assert_eq!(
        resolve_unique("worker", ["team-lead"]).expect("no collision"),
        MemberName::parse("worker").expect("a valid name")
    );
    assert_eq!(
        resolve_unique("worker", ["worker"])
            .expect("one counter is enough")
            .as_str(),
        "worker-2"
    );
    assert_eq!(
        resolve_unique("worker", ["worker", "worker-2", "worker-3"])
            .expect("three counters are enough")
            .as_str(),
        "worker-4"
    );
    // §1.1 lowercases what is taken before comparing, so a differently
    // cased sibling still collides — which is what keeps two members off
    // one inbox file on a case-insensitive filesystem.
    assert_eq!(
        resolve_unique("Worker", ["worker"])
            .expect("the collision is case-insensitive")
            .as_str(),
        "Worker-2"
    );
    // A name with no room left for a counter is refused rather than
    // truncated into a different member's address.
    let longest = "w".repeat(super::NAME_MAX);
    assert_eq!(
        resolve_unique(&longest, [longest.as_str()]),
        Err(NameError::NoFreeCounter { desired: longest })
    );
}

#[test]
fn a_model_supplied_name_cannot_escape_the_teams_root() {
    let root = TeamsRoot::new("/tmp/teams");
    let team = TeamName::parse("session-224cbeab").expect("a valid team name");

    for hostile in [
        "..",
        "../../etc/passwd",
        "worker/../../..",
        "/etc/passwd",
        "worker/sub",
        "worker\\sub",
        ".hidden",
        "-flag",
        "worker\0",
        "worker\n",
        "wörker",
        "",
        &"w".repeat(super::NAME_MAX + 1),
    ] {
        assert!(
            matches!(MemberName::parse(hostile), Err(NameError::Shape { .. })),
            "{hostile:?} should not be a member name"
        );
        assert!(
            matches!(TeamName::parse(hostile), Err(NameError::Shape { .. })),
            "{hostile:?} should not be a team name"
        );
    }

    // And what does pass stays one component under the root, which is the
    // property the refusals above are protecting.
    let agent = MemberName::parse("demo-worker-1").expect("a valid member name");
    let inbox = root.inbox_path(&team, &agent);
    assert_eq!(
        inbox,
        Path::new("/tmp/teams/session-224cbeab/inboxes/demo-worker-1.json")
    );
    assert!(inbox.starts_with("/tmp/teams"));
    assert!(
        !inbox
            .components()
            .any(|component| { matches!(component, std::path::Component::ParentDir) }),
        "a built path never walks upward: {inbox:?}"
    );
}

#[test]
fn an_agent_id_is_the_name_and_the_team() {
    let team = TeamName::parse("session-224cbeab").expect("a valid team name");
    assert_eq!(
        MemberName::lead().agent_id(&team),
        "team-lead@session-224cbeab"
    );
}
