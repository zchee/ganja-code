//! A config file in the format this build has left is refused from the global
//! directory too, and by its own path.
//!
//! Its own binary because it rewrites `XDG_CONFIG_HOME`, and because
//! `tests/config.rs` — which holds the precedence table across all five tiers
//! — plants only what loads and so can never reach this.
//!
//! The global home is the tier somebody is least likely to remember, which is
//! the whole reason it is pinned separately: a checkout can be searched, a
//! home directory is where a file sits for years. The refusal names the path
//! rather than the format, so one launch says which file to convert, and it
//! fires whether or not a `ganja.toml` is sitting beside it — reading the new
//! file and skipping the old one silently is the ignored-setting failure this
//! config system exists to refuse.

use std::{env, fs};

use ganja_core::config::{CONFIG_ENV, LEGACY_FILES};
use ganja_core::provider::{self, fake};
use ganja_core::{Config, ConfigError, Overrides};

#[test]
fn a_legacy_file_in_the_global_home_is_refused_by_path() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let config_home = home.path().join("config");
    let global = config_home.join("ganja");
    let project = home.path().join("project");
    // A checkout, so the project walk stops here rather than climbing out of
    // the fixture — and one with no config of its own, so what answers is the
    // global tier and nothing else.
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::create_dir_all(&global).expect("the fixture config directory is creatable");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        env::set_var("XDG_CONFIG_HOME", &config_home);
        env::set_var("XDG_DATA_HOME", home.path().join("data"));
        env::remove_var(CONFIG_ENV);
        env::remove_var(provider::PROVIDER_ENV);
        env::remove_var(provider::MODEL_ENV);
    }

    // The `ganja.toml` that would have answered, planted first so that every
    // refusal below is one the loader made with a perfectly good config in
    // hand.
    let modern = global.join("ganja.toml");
    fs::write(&modern, format!("model = \"{}/toml-model\"\n", fake::ID))
        .expect("the fixture file is writable");

    let config = Config::load_with(&project, &Overrides::default())
        .expect("a global home holding only the config it reads loads");
    assert_eq!(config.model.as_deref(), Some(format!("{}/toml-model", fake::ID).as_str()));

    for name in LEGACY_FILES {
        let legacy = global.join(name);
        fs::write(&legacy, format!(r#"{{ "model": "{}/old-model" }}"#, fake::ID))
            .expect("the fixture file is writable");

        let error = Config::load_with(&project, &Overrides::default())
            .expect_err("a file in the old format is answered for, not skipped");
        let ConfigError::Legacy { path } = &error else {
            panic!("expected a legacy refusal for {name}, got {error:?}");
        };
        assert_eq!(path, &legacy, "the refusal names the file to convert");
        assert!(
            error.to_string().contains("ganja config migrate"),
            "and the command that converts it: {error}"
        );

        fs::remove_file(&legacy).expect("the fixture file is removable");
    }
}
