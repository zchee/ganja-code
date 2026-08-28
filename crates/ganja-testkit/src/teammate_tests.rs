use super::*;

/// The write half round-trips through the read half: what a seeder plants
/// is what a sweep finds.
#[test]
fn a_seeded_team_file_reads_back_with_its_members() {
    let home = crate::temp_dir();
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse(TEAM).expect("a team name");
    let worker = MemberName::parse("w1").expect("a member name");
    let spawn = Spawn {
        agent_type: "general".to_owned(),
        model: "fake/fake".to_owned(),
        color: "blue".to_owned(),
        prompt: "watch the build".to_owned(),
        plan_mode_required: false,
        surface: Surface::Pane { id: "%7".to_owned() },
        cwd: home.path().to_string_lossy().into_owned(),
    };

    let path = seed_team_file(&root, &team, LEAD_SESSION_ID, home.path(), &[(worker, spawn)]);

    assert_eq!(path, root.config_path(&team));
    let file = team_file(&root, &team).expect("the seeded file is on disk");
    assert!(file.member("w1").is_some(), "{file:?}");
    assert_eq!(teammates_recorded(&root, &team), vec!["w1".to_owned()]);
}

/// The fixture's codex backend resolves no binary at all.
///
/// The B1 hardening: this fixture is safe because an empty search path
/// resolves nothing, which is a property of `shim::resolve`'s
/// empty-and-relative component filter rather than of anything written
/// here. If that filter ever stops dropping the empty component, the
/// fixture lead would quietly find the developer's real `codex` and start
/// spending somebody's quota from inside an ordinary test run — a failure
/// whose only symptom is a slow suite. So it breaks loudly here instead.
#[test]
fn the_fixture_codex_backend_resolves_no_binary_at_all() {
    assert!(
        ganja_teammate_local::shim::resolve(&std::ffi::OsString::new(), "codex").is_none(),
        "an empty search path must resolve nothing, or the fixture lead spawns a real codex"
    );
}
