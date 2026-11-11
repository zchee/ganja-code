//! Which tier may declare a provider endpoint, proved through the real
//! loader.
//!
//! The unit tests beside the code pin `Config::merge_project`'s refusal of a
//! config-declared endpoint; what this binary adds is the walk through
//! `Config::load_with`, where the environment that decides which file is
//! which tier can be set. The tiers under test: the global config home and
//! the file `--config` (or `GANJA_CONFIG`) names are trusted and may declare
//! endpoints; a discovered project file may not, however it was reached —
//! naming a discovered `ganja.toml` explicitly does not elevate it, because
//! discovery still finds the same file and reads it as the checkout's.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and a plain `cargo test` runs the tests inside a binary on
//! parallel threads. `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `HOME` are
//! redirected into a temporary tree — and `GANJA_CONFIG_HOME` cleared — so
//! the machine running the suite cannot contribute a config of its own, and
//! nothing here can read or write a real user's state.

use std::path::Path;
use std::{env, fs};

use ganja_core::config::{CONFIG_ENV, CONFIG_HOME_ENV};
use ganja_core::{Config, ConfigError, Overrides, provider};

/// Writes `text` to `path`, creating whatever directories it needs.
fn plant(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(path, text).expect("the fixture file is writable");
}

/// One declared endpoint, written identically at every tier: which tier the
/// file *is* is the whole subject here, and a tier is a place rather than a
/// spelling. Loopback and keyless, so the refusal proven below is visibly
/// about the tier and not about a credential or a remote host.
const DECLARED: &str = "model = \"local-llama/test\"\n\
                        [provider.local-llama]\n\
                        dialect = \"openai-chat-completions\"\n\
                        base_url = \"http://127.0.0.1:11434/v1\"\n";

#[tokio::test]
async fn a_provider_endpoint_is_declared_by_trusted_tiers_alone() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let project = home.path().join("project");
    // A checkout, so the project walk stops here instead of climbing out of
    // the fixture and into whatever the temporary directory sits under.
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    let project = fs::canonicalize(project).expect("the project path resolves like discovery");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
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

    a_non_discovered_explicit_endpoint_is_selectable(home.path(), &project).await;
    a_global_declaration_loads_beside_a_project_options_entry(
        &home.path().join("config").join("ganja").join("ganja.toml"),
        &project,
    );
    an_explicit_discovered_file_is_still_a_project_file(&project);
    a_discovered_custom_endpoint_is_refused(&project);
}

/// The refusing half: the checkout's own file declares the endpoint, and the
/// load fails before any provider is constructed, naming the file and the key
/// and echoing neither the URL nor anything else the entry wrote.
fn a_discovered_custom_endpoint_is_refused(project: &Path) {
    plant(&project.join("ganja.toml"), DECLARED);

    let error = Config::load_with(project, &Overrides::default())
        .expect_err("a discovered file must not declare an endpoint");
    let ConfigError::Parse { path, message } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert_eq!(path, &project.join("ganja.toml"), "the refusal names the discovered file");
    assert!(message.contains("provider.local-llama"), "{message}");
    assert!(message.contains("trusted tiers"), "{message}");
    assert!(!message.contains("127.0.0.1"), "the refusal echoes no URL: {message}");

    fs::remove_file(project.join("ganja.toml")).expect("the fixture file is removable");
}

/// `--config` naming the discovered file does not elevate it: discovery still
/// finds the same `ganja.toml` and reads it as the checkout's, so the load is
/// refused exactly as it is without the flag.
fn an_explicit_discovered_file_is_still_a_project_file(project: &Path) {
    let discovered = project.join("ganja.toml");
    plant(&discovered, DECLARED);

    let error = Config::load_with(
        project,
        &Overrides { config_file: Some(discovered.clone()), ..Overrides::default() },
    )
    .expect_err("naming the checkout's file explicitly does not make it the person's");
    let ConfigError::Parse { path, message } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert_eq!(path, &discovered);
    assert!(message.contains("provider.local-llama"), "{message}");

    fs::remove_file(discovered).expect("the fixture file is removable");
}

/// The trusting half: the same entry in a file discovery never finds — named
/// by `--config` — loads, and the endpoint it declares is the one a session
/// then runs as.
async fn a_non_discovered_explicit_endpoint_is_selectable(home: &Path, project: &Path) {
    let explicit = home.join("explicit.toml");
    plant(&explicit, &format!("{DECLARED}key_env = \"PROVIDER_TIER_FIXTURE_KEY\"\n"));
    // SAFETY: this binary holds one test and no provider task is running.
    unsafe { env::set_var("PROVIDER_TIER_FIXTURE_KEY", "synthetic-provider-tier-key") };

    let config = Config::load_with(
        project,
        &Overrides { config_file: Some(explicit), ..Overrides::default() },
    )
    .expect("a non-discovered explicit file is the person's own");
    assert_eq!(config.declared_endpoints(), vec!["local-llama".to_owned()]);

    let selection = provider::select(&config).await.expect("the trusted endpoint is selectable");
    assert_eq!(selection.provider.id(), "local-llama");
    assert_eq!(selection.model, "test");
}

/// The positive control pair: a global declaration survives a checkout that
/// carries the one provider entry a project file may still write — a
/// builtin's `options` table.
fn a_global_declaration_loads_beside_a_project_options_entry(global: &Path, project: &Path) {
    plant(global, DECLARED);
    plant(&project.join("ganja.toml"), "[provider.chatgpt.options]\nservice_tier = \"priority\"\n");

    let config = Config::load_with(project, &Overrides::default())
        .expect("a builtin options entry stays the project tier's to set");
    assert_eq!(config.declared_endpoints(), vec!["local-llama".to_owned()]);
    assert!(
        config.provider["chatgpt"].options.is_some(),
        "the checkout's options entry still merges"
    );

    fs::remove_file(global).expect("the fixture file is removable");
    fs::remove_file(project.join("ganja.toml")).expect("the fixture file is removable");
}
