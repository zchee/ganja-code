//! Which of the two config names wins where both sit in the global directory.
//!
//! Its own binary because it rewrites `XDG_CONFIG_HOME`, and because
//! `tests/config.rs` — which holds the precedence table across all five tiers —
//! plants only one name globally and so cannot see this.
//!
//! Merging applies later over earlier, so the order a directory's files are
//! merged in *is* which one wins. The project walk reverses that order to make
//! `ganja.jsonc` beat `ganja.json`; the global tier has to do the same, or one
//! rule holds in a project and its opposite holds in the home directory.

use std::{env, fs};

use ganja_core::{
    Config, Overrides,
    config::CONFIG_ENV,
    provider::{self, fake},
};

#[test]
fn a_global_jsonc_outranks_a_global_json_beside_it() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let config_home = home.path().join("config");
    let global = config_home.join("ganja");
    let project = home.path().join("project");
    // A checkout, so the project walk stops here rather than climbing out of
    // the fixture — and one with no config of its own, so what is read is the
    // global tier and nothing else.
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::create_dir_all(&global).expect("the fixture config directory is creatable");

    for (name, model) in [("ganja.json", "json-model"), ("ganja.jsonc", "jsonc-model")] {
        fs::write(
            global.join(name),
            format!(r#"{{ "model": "{}/{model}" }}"#, fake::ID),
        )
        .expect("the fixture file is writable");
    }

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        env::set_var("XDG_CONFIG_HOME", &config_home);
        env::set_var("XDG_DATA_HOME", home.path().join("data"));
        env::remove_var(CONFIG_ENV);
        env::remove_var(provider::PROVIDER_ENV);
        env::remove_var(provider::MODEL_ENV);
    }

    let config = Config::load_with(&project, &Overrides::default()).expect("both files parse");

    assert_eq!(
        config.model.as_deref(),
        Some(format!("{}/jsonc-model", fake::ID).as_str()),
        "the same rule the project walk follows: jsonc beats json in one directory"
    );
}
