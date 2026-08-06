//! Where a session's model comes from when more than one thing names one.
//!
//! Five tiers can, and the order between them is the whole contract: the
//! global config, then the one file `GANJA_CONFIG` names, then the project's
//! own files, then the environment, then the flags a command line carried.
//! Each is proved by adding it on top of every tier below and watching the
//! answer change — a tier that only won because nothing else was set would
//! prove nothing.
//!
//! Everything here asks for the built-in fake provider, so the suite needs no
//! credential and reaches no network; what is under test is which *string* the
//! selection ends up carrying, not who answers it.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and a plain `cargo test` runs the tests inside a binary on
//! parallel threads. `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `HOME` are
//! redirected into a temporary tree — and `GANJA_CONFIG_HOME` cleared — so the
//! machine running the suite cannot contribute a config of its own, and so
//! nothing here can read or write a real user's state.

use std::{env, fs, path::Path};

use ganja_core::{
    Config, Overrides,
    config::{CONFIG_ENV, CONFIG_HOME_ENV},
    provider::{self, fake},
};

/// Writes a config file naming `model` and one instruction file.
fn plant(path: &Path, model: &str, instruction: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(
        path,
        format!(
            r#"{{
              // planted by tests/config.rs
              "model": "{}/{model}",
              "instructions": ["{instruction}"],
            }}"#,
            fake::ID
        ),
    )
    .expect("the fixture file is writable");
}

#[test]
fn each_tier_that_names_a_model_outranks_every_tier_below_it() {
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
        env::remove_var(provider::PROVIDER_ENV);
        env::remove_var(provider::MODEL_ENV);
    }

    // Cumulative: each row adds one tier on top of every tier before it, and
    // states what a session then asks for.
    let table: [(&str, &str); 6] = [
        ("nothing at all", fake::MODEL),
        ("the global config", "global-model"),
        (CONFIG_ENV, "explicit-model"),
        ("the project config", "project-model"),
        (provider::MODEL_ENV, "env-model"),
        ("--model", "flag-model"),
    ];

    let mut overrides = Overrides::default();
    for (index, (tier, expected)) in table.iter().enumerate() {
        match index {
            0 => {}
            1 => plant(&global, "global-model", "global.md"),
            2 => {
                plant(&explicit, "explicit-model", "explicit.md");
                // SAFETY: as above.
                unsafe { env::set_var(CONFIG_ENV, &explicit) };
            }
            3 => plant(&project.join("ganja.jsonc"), "project-model", "project.md"),
            4 => {
                // SAFETY: as above.
                unsafe {
                    env::set_var(provider::PROVIDER_ENV, fake::ID);
                    env::set_var(provider::MODEL_ENV, "env-model");
                }
            }
            _ => {
                overrides.model = Some(format!("{}/flag-model", fake::ID));
                overrides.agent = Some("plan".to_owned());
            }
        }

        let config = Config::load_with(&project, &overrides).expect("every planted tier parses");
        let selection = provider::select(&config).expect("the fake provider needs no credential");

        assert_eq!(selection.model, *expected, "after adding {tier}");
        assert_eq!(selection.provider.id(), fake::ID, "after adding {tier}");
        assert_eq!(
            selection.notice.is_some(),
            index == 0,
            "only a session nothing named a provider for is told so ({tier})"
        );
    }

    // Everything is still installed, so the last load is the one that saw all
    // five tiers at once.
    let config = Config::load_with(&project, &overrides).expect("every planted tier parses");

    assert_eq!(
        config.model.as_deref(),
        Some("fake/project-model"),
        "the files agree that the closest one wins, and the flag stays out of them"
    );
    assert_eq!(
        config.instructions,
        vec!["global.md", "explicit.md", "project.md"],
        "instructions are the one array that concatenates across tiers, in tier order"
    );
    assert_eq!(
        config.overrides.agent.as_deref(),
        Some("plan"),
        "the other flag rides along for whoever resolves agents"
    );
}
