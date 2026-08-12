//! The plugin store walked end to end: a fixture marketplace on a local
//! path, added and installed through the real [`Store`], then read back
//! through the real config loader — the restart-shaped half of P14's
//! acceptance criterion 6 (the `/plugin` dialog half is the TUI's).
//!
//! One binary, one environment-mutating test — the house rule every config
//! suite here follows, because `Config::load_with` consults the global tier
//! and the plugin store through `GANJA_CONFIG_HOME`, and a plain `cargo
//! test` runs a binary's tests on parallel threads. Everything below runs
//! sequentially inside the single `#[test]`, against one pinned temporary
//! home, so nothing here can read a real user's config or plugins.

use std::{fs, path::Path};

use ganja_core::{
    Config, LspConfig, McpServer,
    config::{CONFIG_ENV, CONFIG_HOME_ENV},
    plugin::Store,
};

/// Writes `text` to `root/relative`, creating whatever directories it needs.
fn plant(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(path, text).expect("the fixture file is writable");
}

/// A marketplace holding one plugin that carries all five surfaces — the
/// fixture acceptance criterion 6 names — plus deliberate collisions with
/// the project config below, so precedence is pinned rather than assumed.
fn plant_marketplace(market: &Path) {
    plant(
        market,
        ".claude-plugin/marketplace.json",
        r#"{
          "name": "fixture-market",
          "owner": { "name": "The Suite" },
          "plugins": [
            {
              "name": "full",
              "source": "./plugins/full",
              "description": "one of everything"
            }
          ]
        }"#,
    );

    let plugin = "plugins/full";
    plant(
        market,
        &format!("{plugin}/.claude-plugin/plugin.json"),
        r#"{ "name": "full", "version": "1.0.0", "description": "one of everything" }"#,
    );
    plant(
        market,
        &format!("{plugin}/hooks/hooks.json"),
        r#"{"hooks": {
          "PreToolUse": [
            {"matcher": "Edit", "hooks": [
              {"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/check.sh"}
            ]}
          ]
        }}"#,
    );
    plant(
        market,
        &format!("{plugin}/.mcp.json"),
        r#"{"mcpServers": {
          "db": {"command": "${CLAUDE_PLUGIN_ROOT}/server", "args": ["--plugin-mode"]}
        }}"#,
    );
    plant(
        market,
        &format!("{plugin}/skills/greeter/SKILL.md"),
        "---\nname: greeter\ndescription: greets\n---\nSay hello.\n",
    );
    // Two agents: `reviewer` collides with the project config's own and must
    // lose to it; `helper` is the one that actually arrives.
    plant(
        market,
        &format!("{plugin}/agents/reviewer.md"),
        "---\nname: reviewer\ndescription: the plugin's reviewer\n---\nReview things.\n",
    );
    plant(
        market,
        &format!("{plugin}/agents/helper.md"),
        "---\nname: helper\ndescription: helps\n---\nHelp out.\n",
    );
    // Two servers: `go` collides with the config's own and must lose to it;
    // `fixturelsp` arrives.
    plant(
        market,
        &format!("{plugin}/.lsp.json"),
        r#"{
          "go": {"command": "plugin-gopls", "extensionToLanguage": {".go": "go"}},
          "fixturelsp": {"command": "fixture-lsp", "args": ["serve"], "extensionToLanguage": {".fx": "fixture"}}
        }"#,
    );
}

/// The project whose config the loader reads: a hook on the same event the
/// plugin hooks, an agent and an LSP server with the plugin's names, and a
/// skills path of its own — every collision the merge table decides.
fn plant_project(project: &Path) {
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::create_dir_all(project.join("own-skills")).expect("the fixture tree is creatable");
    plant(
        project,
        "ganja.jsonc",
        r#"{
          "hooks": {
            "PreToolUse": [
              {"hooks": [{"type": "command", "command": "config-pre.sh"}]}
            ]
          },
          "agent": {
            "reviewer": { "description": "the config's reviewer" }
          },
          "lsp": {
            "go": { "command": ["config-gopls"], "extensions": [".go"] }
          },
          "skills": { "paths": ["./own-skills"] }
        }"#,
    );
}

#[test]
fn an_installed_plugin_contributes_all_five_surfaces_until_disabled() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let config_home = home.path().join("ganja-home");
    fs::create_dir_all(&config_home).expect("the config home is creatable");

    // SAFETY: this binary holds one environment-mutating test, so nothing
    // else in the process is reading the environment while it is written.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
        std::env::set_var(CONFIG_HOME_ENV, &config_home);
        std::env::remove_var(CONFIG_ENV);
    }

    let market = home.path().join("market");
    plant_marketplace(&market);
    let project = home.path().join("project");
    plant_project(&project);

    // Before anything is installed, the loader serves exactly the config.
    let bare = Config::load(&project).expect("the project config loads");
    assert_eq!(bare.hooks["PreToolUse"].len(), 1);
    assert!(!bare.mcp.keys().any(|key| key.starts_with("plugin:")));

    // The store the loader will discover is the one under the config home —
    // the same resolution `GANJA_CONFIG_HOME` moves everything else with.
    let store = Store::discover().expect("the config home resolves");
    let added = store
        .add_marketplace(&market.display().to_string())
        .expect("a local marketplace adds");
    assert_eq!(added, "fixture-market", "the marketplace file names it");
    store
        .install("full", "fixture-market")
        .expect("the fixture plugin installs");

    let loaded = Config::load(&project).expect("the project config loads with the plugin");

    // Hooks append: the plugin's group runs *beside* the config's for the
    // same event, never displacing it — D473's whole point.
    let pre = &loaded.hooks["PreToolUse"];
    assert_eq!(pre.len(), 2, "config group and plugin group, both");
    let ganja_core::HookHandler::Command(first) = &pre[0].hooks[0];
    assert_eq!(
        first.command, "config-pre.sh",
        "the config's group is untouched, first"
    );
    let ganja_core::HookHandler::Command(second) = &pre[1].hooks[0];
    assert!(
        second.command.ends_with("/check.sh") && !second.command.contains("${"),
        "the plugin's group follows, its root placeholder substituted: {}",
        second.command
    );

    // MCP arrives namespaced, collision-free by construction.
    let McpServer::Local(db) = &loaded.mcp["plugin:full:db"] else {
        panic!("the plugin's command entry becomes a local server");
    };
    assert!(db.command[0].ends_with("/server"));
    assert_eq!(db.command[1], "--plugin-mode");

    // Skills roots concat after the config's own, which keeps ranking first.
    assert_eq!(loaded.skills.paths.len(), 2);
    assert_eq!(loaded.skills.paths[0], "./own-skills");
    assert!(
        loaded.skills.paths[1].ends_with("/skills"),
        "the plugin's skills directory is appended: {}",
        loaded.skills.paths[1]
    );

    // Agents merge per key: the config-declared `reviewer` wins its name,
    // the plugin's `helper` arrives.
    assert_eq!(
        loaded.agent["reviewer"].description.as_deref(),
        Some("the config's reviewer"),
        "explicit config wins the collision"
    );
    assert_eq!(loaded.agent["helper"].description.as_deref(), Some("helps"));

    // LSP merges per key under the same rule.
    let Some(LspConfig::Servers(lsp)) = &loaded.lsp else {
        panic!("the lsp key holds the merged server map");
    };
    assert_eq!(
        lsp["go"].command.as_deref(),
        Some(["config-gopls".to_owned()].as_slice()),
        "explicit config wins the collision"
    );
    assert_eq!(
        lsp["fixturelsp"].command.as_deref(),
        Some(["fixture-lsp".to_owned(), "serve".to_owned()].as_slice())
    );

    // `ganja plugin list`'s data and the load path agree because they are
    // one collector's answer: everything the listing names is what the
    // loader just served.
    let listings = store.list().expect("the store lists");
    assert_eq!(listings.len(), 1);
    let listing = &listings[0];
    assert_eq!(listing.name, "full");
    assert!(listing.enabled);
    assert_eq!(listing.marketplace, "fixture-market");
    for component in [
        "hook PreToolUse",
        "mcp db",
        "skills",
        "agent helper",
        "agent reviewer",
        "lsp fixturelsp",
        "lsp go",
    ] {
        assert!(
            listing.components.iter().any(|line| line == component),
            "the listing names {component}: {:?}",
            listing.components
        );
    }

    // Disabling withdraws every contribution without touching the disk copy.
    store
        .set_enabled("full", false)
        .expect("the plugin disables");
    let disabled = Config::load(&project).expect("the config loads with the plugin disabled");
    assert_eq!(
        disabled.hooks["PreToolUse"].len(),
        1,
        "only the config's group"
    );
    assert!(!disabled.mcp.contains_key("plugin:full:db"));
    assert_eq!(disabled.skills.paths, vec!["./own-skills".to_owned()]);
    assert!(!disabled.agent.contains_key("helper"));
    let Some(LspConfig::Servers(lsp)) = &disabled.lsp else {
        panic!("the config's own lsp map remains");
    };
    assert!(!lsp.contains_key("fixturelsp"));

    // Re-enabling brings them back — the disk copy never moved.
    store
        .set_enabled("full", true)
        .expect("the plugin re-enables");
    let again = Config::load(&project).expect("the config loads with the plugin back");
    assert_eq!(again.hooks["PreToolUse"].len(), 2);

    // Removal deletes the copy and the state entry; the loader is back to
    // exactly the config, and the marketplace stays added.
    store.remove("full").expect("the plugin removes");
    assert!(!store.plugin_root("full").exists());
    let removed = Config::load(&project).expect("the config loads after removal");
    assert_eq!(removed.hooks["PreToolUse"].len(), 1);
    assert!(store.list().expect("the store lists").is_empty());
    assert!(
        store
            .state()
            .expect("the state reads")
            .marketplaces
            .contains_key("fixture-market"),
        "removing a plugin does not forget where it came from"
    );
}
