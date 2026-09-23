use std::sync::Arc;

use ganja_core::config::Overrides;
use ganja_core::judge::{Judge, MODEL, Screen, Tuning};
use ganja_core::tool::typesafe::Settings;

use super::{Judging, assemble};

/// The two scalars a config names reach the engine this seam builds.
///
/// `ganja-core`'s own suite pins what the cap *does* — two children at a
/// time and never more — over an engine it builds by hand
/// (`tests/parallel_subagents.rs`). What that suite cannot see is whether
/// a real session is ever handed the number, which is the half that was
/// missing: `agents.concurrency` was parsed, validated and documented
/// while every assembled engine ran at the default.
///
/// The three redirects are what make an assembly hermetic: the global
/// config tier, the data home a project's storage hangs under, and the
/// provider the environment would otherwise choose. Without them this
/// reads whatever config the machine running the suite happens to hold.
#[tokio::test]
async fn the_configured_cap_reaches_an_assembled_engine() {
    let data = tempfile::TempDir::new().expect("a temporary directory is creatable");
    let home = tempfile::TempDir::new().expect("a temporary directory is creatable");
    let project = tempfile::TempDir::new().expect("a temporary directory is creatable");
    // SAFETY: process-wide, so this belongs to a test that runs alone in
    // its process — which `nextest` gives every test. The one other test
    // here that sets these variables points them at directories of its own
    // and removes the same names and two more TypeSafe ones, so neither can
    // be handed a developer's value.
    //
    // The redirects are what make an assembly hermetic, and `assemble` reads
    // one more variable than it used to: **D564** overlays `evaluate` when
    // `TYPESAFE_API_KEY` is set, so a developer with a key exported would
    // otherwise assemble a different roster here than CI does. It is removed
    // for the same reason `GANJA_PROVIDER` is.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", data.path());
        std::env::set_var("GANJA_CONFIG_HOME", home.path());
        std::env::remove_var("GANJA_PROVIDER");
        std::env::remove_var("GANJA_MODEL");
        std::env::remove_var("TYPESAFE_API_KEY");
    }
    std::fs::write(
        project.path().join("ganja.toml"),
        "auto_compact_threshold = 60\n[agents]\nconcurrency = 3\n",
    )
    .expect("the fixture config is writable");

    let assembled = assemble(project.path(), &Overrides::default(), Judging::Withhold)
        .await
        .expect("a project holding two config keys assembles");

    assert_eq!(
        assembled.engine.concurrency(),
        3,
        "the assembled engine runs at the cap the config named"
    );
    // The same half, for the same reason, for the auto-compaction percentage
    // (**D566**): `ganja-core` pins what the percentage does, and nothing there
    // can see whether a real headless session is handed it.
    assert_eq!(
        assembled.engine.compact_threshold().percent(),
        60,
        "the assembled engine compacts at the percentage the config named"
    );
}

/// **D567**: the assembly builds at most one judge in a process, because the
/// judge's cap of eight requests in flight and its breaker are process-wide
/// only while there is exactly one.
///
/// The process's judge is built first, over a loopback base that nothing
/// here ever sends to, and every assembly after it that asks for one is
/// handed **that** judge rather than building its own — which is also why
/// this sets no TypeSafe variable and removes any a developer exported: an
/// assembly that read the environment would come back with no judge at all,
/// and the check below would fail. The config names a source, so an
/// assembly that comes back with a judge got it from the process. `serve`'s
/// door is handed none at all.
#[tokio::test]
async fn every_assembly_in_a_process_shares_its_one_judge_and_serve_gets_none() {
    let data = tempfile::TempDir::new().expect("a temporary directory is creatable");
    let home = tempfile::TempDir::new().expect("a temporary directory is creatable");
    let project = tempfile::TempDir::new().expect("a temporary directory is creatable");
    // SAFETY: as in the test above, whose variables these are. The TypeSafe
    // settings go too: an assembly that read the environment instead of
    // sharing the process's judge must build nothing here, and never one
    // over a developer's exported key and the vendor's real host.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", data.path());
        std::env::set_var("GANJA_CONFIG_HOME", home.path());
        std::env::remove_var("GANJA_PROVIDER");
        std::env::remove_var("GANJA_MODEL");
        std::env::remove_var("TYPESAFE_API_KEY");
        std::env::remove_var("TYPESAFE_BASE_URL");
        std::env::remove_var("TYPESAFE_DEFAULT_MODEL");
    }
    let flagged = project.path().join("flagged.toml");
    std::fs::write(&flagged, "[evaluate]\nscreen = [\"webfetch\"]\n")
        .expect("the fixture config is writable");
    let overrides = Overrides { config_file: Some(flagged), ..Overrides::default() };
    let settings = Settings::from_parts(
        "sk-assembly-unit-key".to_owned(),
        "http://127.0.0.1:9",
        MODEL.to_owned(),
    )
    .expect("a loopback base and the measured model are accepted");
    let screen = Screen { webfetch: true, ..Screen::default() };

    let first = Judge::for_process(&ganja_core::Config::default(), |_| {
        Judge::from_settings(Some(settings), screen, Tuning::SHIPPED)
    })
    .expect("the first build in a process is the process's judge");
    let run = assemble(project.path(), &overrides, Judging::Build)
        .await
        .expect("a project whose config names a source assembles");
    let again = assemble(project.path(), &overrides, Judging::Build)
        .await
        .expect("a second assembly in the same process assembles");
    let serve = assemble(project.path(), &overrides, Judging::Withhold)
        .await
        .expect("the serve door assembles");

    for (door, judge) in [("the first assembly", run.judge), ("the second", again.judge)] {
        let judge = judge.unwrap_or_else(|| panic!("{door} asked for a judge and got none"));
        assert!(Arc::ptr_eq(&judge, &first), "{door} built a second judge in one process");
    }
    assert!(serve.judge.is_none(), "the serve door builds no judge whatever the config names");
}
