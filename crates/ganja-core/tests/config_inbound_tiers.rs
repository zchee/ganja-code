//! Which tier reaches the two cross-session inbound keys (**D523**), proved
//! through the real loader.
//!
//! `tests/config.rs`'s five-tier ladder proves the ordinary later-wins order
//! for a key every tier may set. These two keys are the deliberate divergence
//! from it: a project file may only *tighten* `cross_session_inbound` —
//! replace the standing policy when strictly more severe, `accept < hold <
//! refuse` — and may not set `dialog_expiry` at all, while the trusted tiers
//! (the global config home, then the file `GANJA_CONFIG` names) keep the
//! ordinary order between themselves. The severity arithmetic itself is
//! pinned by the unit tests beside the code; what this binary adds is the
//! walk through `Config::load_with`, where the environment that decides
//! which file is which tier can be set.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and a plain `cargo test` runs the tests inside a binary on
//! parallel threads. `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `HOME` are
//! redirected into a temporary tree — and `GANJA_CONFIG_HOME` cleared — so
//! the machine running the suite cannot contribute a config of its own, and
//! nothing here can read or write a real user's state.

use std::{env, fs, path::Path};

use ganja_core::{
    Config, ConfigError, Overrides,
    config::{CONFIG_ENV, CONFIG_HOME_ENV, DialogExpiry, InboundPolicy},
};

/// Writes `text` to `path`, creating whatever directories it needs.
fn plant(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(path, text).expect("the fixture file is writable");
}

#[test]
fn the_inbound_keys_cross_the_tiers_under_the_tighten_only_rule() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let config_home = home.path().join("config");
    let global = config_home.join("ganja").join("ganja.jsonc");
    let explicit = home.path().join("explicit.jsonc");
    let project = home.path().join("project");
    // A checkout, so the project walk stops here instead of climbing out of
    // the fixture and into whatever the temporary directory sits under.
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        env::set_var("XDG_CONFIG_HOME", &config_home);
        env::set_var("XDG_DATA_HOME", home.path().join("data"));
        // The global tier resolves through `config_home()`, and two of its
        // three places reach past the XDG redirect: `~/.ganja` through `HOME`,
        // and `GANJA_CONFIG_HOME` past everything. Pin both, or a runner who
        // adopted either feature contributes a global config to this table.
        env::set_var("HOME", home.path());
        env::remove_var(CONFIG_HOME_ENV);
        env::remove_var(CONFIG_ENV);
    }

    let load = |project: &Path| Config::load_with(project, &Overrides::default());

    // A project file tightens the global tier's policy and never loosens it.
    for (global_value, project_value, expected) in [
        ("accept", "refuse", InboundPolicy::Refuse),
        ("refuse", "accept", InboundPolicy::Refuse),
        ("hold", "accept", InboundPolicy::Hold),
    ] {
        plant(
            &global,
            &format!(r#"{{"cross_session_inbound": "{global_value}"}}"#),
        );
        plant(
            &project.join("ganja.jsonc"),
            &format!(r#"{{"cross_session_inbound": "{project_value}"}}"#),
        );

        let config = load(&project).expect("both tiers parse");
        assert_eq!(
            config.cross_session_inbound,
            Some(expected),
            "global {global_value} under a project {project_value}"
        );
    }

    // The trusted half: the file `GANJA_CONFIG` names outranks the global
    // tier ordinarily — in the loosening direction too, because both files
    // are the person's own.
    plant(&global, r#"{"cross_session_inbound": "refuse"}"#);
    plant(&explicit, r#"{"cross_session_inbound": "accept"}"#);
    plant(&project.join("ganja.jsonc"), "{}");
    // SAFETY: as above.
    unsafe { env::set_var(CONFIG_ENV, &explicit) };

    let config = load(&project).expect("all three tiers parse");
    assert_eq!(
        config.cross_session_inbound,
        Some(InboundPolicy::Accept),
        "GANJA_CONFIG outranks the global tier"
    );

    // ...and a project file then tightens the value the trusted tiers
    // settled on, not the global one it never saw.
    plant(
        &project.join("ganja.jsonc"),
        r#"{"cross_session_inbound": "hold"}"#,
    );
    let config = load(&project).expect("all three tiers parse");
    assert_eq!(
        config.cross_session_inbound,
        Some(InboundPolicy::Hold),
        "the project file tightens what GANJA_CONFIG settled"
    );

    // `dialog_expiry` is the trusted tiers' to set, in the same order...
    plant(&global, r#"{"dialog_expiry": "10m"}"#);
    plant(&explicit, r#"{"dialog_expiry": "60s"}"#);
    plant(&project.join("ganja.jsonc"), "{}");

    let config = load(&project).expect("a trusted dialog_expiry loads");
    assert_eq!(
        config.dialog_expiry(),
        DialogExpiry::OneMinute,
        "the explicit file's window outranks the global one"
    );

    // ...and a project file that sets it fails the load naming the key and
    // the file. The loader canonicalises the walk's start, so the expected
    // path is spelled the same way.
    plant(&project.join("ganja.jsonc"), r#"{"dialog_expiry": "5m"}"#);
    let offender = fs::canonicalize(&project)
        .expect("the fixture directory resolves")
        .join("ganja.jsonc");

    let error = load(&project).expect_err("a checkout must not size the review window");
    let ConfigError::Parse { path, message } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert_eq!(path, &offender, "the complaint names the file that said it");
    assert!(message.contains("dialog_expiry"), "{message}");
}
