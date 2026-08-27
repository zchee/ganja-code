use ganja_core::config::Overrides;

use super::assemble;

/// The cap a config names reaches the engine this seam builds.
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
#[test]
fn the_configured_cap_reaches_an_assembled_engine() {
    let data = tempfile::TempDir::new().expect("a temporary directory is creatable");
    let home = tempfile::TempDir::new().expect("a temporary directory is creatable");
    let project = tempfile::TempDir::new().expect("a temporary directory is creatable");
    // SAFETY: process-wide, so this belongs to a test that runs alone in
    // its process — which `nextest` gives every test, and which the rest
    // of this binary's unit tests do not contend for: none of them reads
    // the environment.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", data.path());
        std::env::set_var("GANJA_CONFIG_HOME", home.path());
        std::env::remove_var("GANJA_PROVIDER");
        std::env::remove_var("GANJA_MODEL");
    }
    std::fs::write(
        project.path().join("ganja.toml"),
        "[agents]\nconcurrency = 3\n",
    )
    .expect("the fixture config is writable");

    let assembled = assemble(project.path(), &Overrides::default())
        .expect("a project holding one config key assembles");

    assert_eq!(
        assembled.engine.concurrency(),
        3,
        "the assembled engine runs at the cap the config named"
    );
}
