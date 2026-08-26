use std::fs;

use tempfile::TempDir;

/// The listings' source tag reads the store's own layout: the component
/// after `installed/` is the plugin, and anything outside it is nobody's.
#[test]
fn a_skills_origin_inside_the_store_names_its_plugin() {
    let store = Store::at(std::path::PathBuf::from("/home/.config/ganja/plugins"));

    assert_eq!(
        store.plugin_of(std::path::Path::new(
            "/home/.config/ganja/plugins/installed/mattpocock-skills/skills"
        )),
        Some("mattpocock-skills".to_owned())
    );
    assert_eq!(
        store.plugin_of(std::path::Path::new("/home/.config/ganja/skills")),
        None,
        "ganja's own home is the user's, not a plugin's"
    );
    assert_eq!(
        store.plugin_of(std::path::Path::new(
            "/home/.config/ganja/plugins/installed"
        )),
        None,
        "the installed directory itself belongs to no plugin"
    );
}

use super::{
    Contribution, Manifest, Marketplace, PluginError, Source, Store, collect, collect_agents,
    collect_skill_costs, looks_like_git, split_frontmatter,
};
use crate::config::{HookHandler, McpServer};

/// Writes `text` to `root/relative`, creating directories as needed.
fn plant(root: &std::path::Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(path, text).expect("the fixture file is writable");
}

/// Runs git in `dir` for a fixture, identity pinned so `commit` works on
/// a machine with no git config of its own.
fn fixture_git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=Fixture",
        ])
        .args(args)
        .output()
        .expect("git spawns");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// `details` prices what a plugin puts in front of the model
/// (2026-08-15, Claude Code's own surface): identity off the manifest
/// with the marketplace entry's description standing in, skills priced
/// as roster-line always-on plus body on-invoke, agents likewise,
/// commands on-invoke only — the palette is UI — and the bare counts
/// for the surfaces that carry no prompt.
#[test]
fn details_prices_the_prompt_bearing_components() {
    let home = TempDir::new().expect("a temporary directory");
    let market = home.path().join("market");
    plant(
        &market,
        ".claude-plugin/marketplace.json",
        r#"{ "name": "m", "owner": { "name": "t" }, "plugins": [
                { "name": "priced", "source": "./priced",
                  "description": "the entry's own line" }
            ] }"#,
    );
    plant(
        &market,
        "priced/.claude-plugin/plugin.json",
        r#"{ "name": "priced", "version": "1.2.3" }"#,
    );
    plant(
        &market,
        "priced/skills/greet/SKILL.md",
        "---\nname: greet\ndescription: says hello politely\n---\nA body of some length here.",
    );
    plant(
        &market,
        "priced/agents/rev.md",
        "---\nname: rev\ndescription: reviews\n---\nYou review code carefully.",
    );
    plant(&market, "priced/commands/go.md", "run everything now");

    let store = Store::at(home.path().join("store"));
    store
        .add_marketplace(&market.display().to_string())
        .expect("the marketplace adds");
    store.install("priced", "m").expect("the plugin installs");

    let details = store.details("priced").expect("the details read");
    assert_eq!(details.version.as_deref(), Some("1.2.3"));
    assert_eq!(
        details.description.as_deref(),
        Some("the entry's own line"),
        "the marketplace entry's description stands in for a silent manifest"
    );
    assert_eq!(details.marketplace, "m");
    assert!(details.enabled);

    assert_eq!(details.skills.len(), 1);
    let skill = &details.skills[0];
    assert_eq!(skill.name, "greet");
    // "greet" is 5 chars (2 tokens up) and the description 20 (5): the
    // arithmetic is the estimate's whole contract.
    assert_eq!(skill.always_on, 2 + 5);
    assert_eq!(skill.on_invoke, 7, "28 body chars over four");

    assert_eq!(details.agents.len(), 1);
    assert!(details.agents[0].always_on > 0);
    assert!(details.agents[0].on_invoke > 0);

    assert_eq!(details.commands.len(), 1);
    assert_eq!(details.commands[0].always_on, 0, "the palette is UI");
    assert!(details.commands[0].on_invoke > 0);

    assert_eq!(
        details.always_on_total(),
        details.skills[0].always_on + details.agents[0].always_on
    );
    assert!(details.hooks.is_empty());

    let missing = store
        .details("nothing")
        .expect_err("an unknown plugin refuses");
    assert!(missing.to_string().contains("nothing"), "{missing}");
}

/// The three marketplace verbs beyond add (2026-08-15): the listing
/// names origin, offers and installed plugins per marketplace; remove
/// refuses while installed plugins depend on it and deletes cleanly
/// once they are gone; update re-fetches from the recorded origin and
/// refuses a fetch that renamed itself.
#[test]
fn marketplaces_list_remove_and_update_from_their_recorded_origins() {
    let home = TempDir::new().expect("a temporary directory");
    let market = home.path().join("market");
    let manifest = |plugins: &str| {
        format!(r#"{{ "name": "verbs", "owner": {{ "name": "t" }}, "plugins": [{plugins}] }}"#)
    };
    plant(
        &market,
        ".claude-plugin/marketplace.json",
        &manifest(r#"{ "name": "one", "source": "./one" }"#),
    );
    plant(
        &market,
        "one/skills/one/SKILL.md",
        "---\nname: one\ndescription: d\n---\nx",
    );

    let store = Store::at(home.path().join("store"));
    store
        .add_marketplace(&market.display().to_string())
        .expect("the marketplace adds");
    store.install("one", "verbs").expect("its plugin installs");

    let listings = store.marketplaces().expect("the listing reads");
    assert_eq!(listings.len(), 1);
    assert_eq!(listings[0].name, "verbs");
    assert_eq!(listings[0].origin, market.display().to_string());
    assert_eq!(
        listings[0].offered,
        Ok(vec!["one".to_owned()]),
        "the offer roster comes off the copy's own file"
    );
    assert_eq!(listings[0].installed, vec!["one".to_owned()]);

    let refused = store
        .remove_marketplace("verbs")
        .expect_err("installed plugins hold the marketplace");
    assert!(
        refused.to_string().contains("one"),
        "the refusal names the dependents: {refused}"
    );

    // The origin grows a second plugin; update picks it up in place.
    plant(
        &market,
        ".claude-plugin/marketplace.json",
        &manifest(r#"{ "name": "one", "source": "./one" }, { "name": "two", "source": "./one" }"#),
    );
    let origin = store
        .update_marketplace("verbs")
        .expect("the update fetches");
    assert_eq!(origin, market.display().to_string());
    let updated = store.marketplaces().expect("the listing reads");
    assert_eq!(
        updated[0].offered,
        Ok(vec!["one".to_owned(), "two".to_owned()]),
        "the update replaced the copy"
    );

    // A rename upstream is refused, never silently forked.
    plant(
        &market,
        ".claude-plugin/marketplace.json",
        r#"{ "name": "renamed", "owner": { "name": "t" }, "plugins": [] }"#,
    );
    let forked = store
        .update_marketplace("verbs")
        .expect_err("a rename refuses");
    assert!(
        forked.to_string().contains("renamed"),
        "the refusal names both: {forked}"
    );

    store.remove("one").expect("the plugin removes");
    store
        .remove_marketplace("verbs")
        .expect("with no dependents the marketplace removes");
    assert!(store.marketplaces().expect("the listing reads").is_empty());
    let unknown = store
        .remove_marketplace("verbs")
        .expect_err("a second remove refuses");
    assert!(unknown.to_string().contains("added: none"), "{unknown}");
}

/// A remote source installs the **pinned** commit — never the branch's
/// newer tip — descending the entry's `path` inside the repository
/// (2026-08-15, the official marketplace's own `url`+`sha`+`path`
/// shape); and a source kind this build cannot fetch refuses by name.
#[test]
fn a_remote_source_installs_the_pinned_commit_and_unknown_kinds_refuse() {
    let home = TempDir::new().expect("a temporary directory");
    let repo = home.path().join("repo");
    plant(
        &repo,
        "pack/skills/hello/SKILL.md",
        "---\nname: hello\ndescription: the first version\n---\nv1",
    );
    fixture_git(&repo, &["init", "--quiet", "-b", "main"]);
    fixture_git(&repo, &["add", "."]);
    fixture_git(&repo, &["commit", "--quiet", "-m", "v1"]);
    let pinned = fixture_git(&repo, &["rev-parse", "HEAD"]);
    plant(
        &repo,
        "pack/skills/hello/SKILL.md",
        "---\nname: hello\ndescription: the second version\n---\nv2",
    );
    fixture_git(&repo, &["commit", "--quiet", "-am", "v2"]);

    let market = home.path().join("market");
    plant(
        &market,
        ".claude-plugin/marketplace.json",
        &format!(
            r#"{{
                  "name": "remote-market",
                  "owner": {{ "name": "The Suite" }},
                  "plugins": [
                    {{
                      "name": "hello",
                      "source": {{
                        "source": "url",
                        "url": "{url}",
                        "sha": "{pinned}",
                        "path": "pack"
                      }}
                    }},
                    {{
                      "name": "unfetchable",
                      "source": {{ "source": "npm", "package": "nope" }}
                    }}
                  ]
                }}"#,
            url = repo.display(),
        ),
    );

    let store = Store::at(home.path().join("store"));
    store
        .add_marketplace(&market.display().to_string())
        .expect("the marketplace adds");
    store
        .install("hello", "remote-market")
        .expect("the remote source installs");

    let installed = fs::read_to_string(store.plugin_root("hello").join("skills/hello/SKILL.md"))
        .expect("the pinned skill landed");
    assert!(
        installed.contains("the first version"),
        "the pin outranks the branch tip: {installed}"
    );

    let refused = store
        .install("unfetchable", "remote-market")
        .expect_err("an unfetchable kind refuses");
    assert!(
        refused.to_string().contains("cannot fetch"),
        "the refusal names the limit: {refused}"
    );
}

#[test]
fn a_manifest_with_keys_this_build_never_heard_of_still_loads() {
    let manifest = Manifest::parse(
        r#"{
              "name": "deployment-tools",
              "version": "1.2.0",
              "description": "deploys",
              "author": { "name": "A Person", "email": "a@example.com" },
              "homepage": "https://example.com",
              "keywords": ["deploy"],
              "engines": { "vscode": "^1.0.0" }
            }"#,
    )
    .expect("the manifest is Claude's file, and Claude ignores unknown keys");

    assert_eq!(manifest.name, "deployment-tools");
    assert_eq!(manifest.version.as_deref(), Some("1.2.0"));
    assert_eq!(
        manifest
            .author
            .expect("the author was written")
            .name
            .as_deref(),
        Some("A Person")
    );
}

#[test]
fn a_plugin_name_that_traverses_is_refused_by_name() {
    for hostile in ["../escape", "a/b", "a\\b", "..", ".", "", "   "] {
        let error = Manifest::parse(&format!(r#"{{"name": {}}}"#, serde_json::json!(hostile)))
            .expect_err("a name that walks out of the store is refused");
        let PluginError::Parse { message, .. } = &error else {
            panic!("expected a parse refusal, got {error:?}");
        };
        assert!(
            !message.is_empty(),
            "the refusal for {hostile:?} says something"
        );
    }
}

#[test]
fn a_marketplace_lists_its_plugins_with_their_sources() {
    let market = Marketplace::parse(
        r#"{
              "name": "company-tools",
              "owner": { "name": "DevTools Team", "email": "devtools@example.com" },
              "plugins": [
                { "name": "formatter", "source": "./plugins/formatter", "description": "formats" },
                { "name": "deployer", "source": { "source": "github", "repo": "co/deploy" } }
              ]
            }"#,
    )
    .expect("the spec's own example shape parses");

    assert_eq!(market.name, "company-tools");
    assert_eq!(market.plugins.len(), 2);
    assert_eq!(
        market.plugins[0].source,
        Source::Path("./plugins/formatter".to_owned())
    );
    assert!(matches!(market.plugins[1].source, Source::Remote(_)));
}

#[test]
fn a_marketplace_naming_one_plugin_twice_is_refused_by_name() {
    let error = Marketplace::parse(
        r#"{
              "name": "m",
              "owner": { "name": "o" },
              "plugins": [
                { "name": "twin", "source": "./a" },
                { "name": "twin", "source": "./b" }
              ]
            }"#,
    )
    .expect_err("two entries cannot share one install directory");

    let PluginError::Parse { message, .. } = &error else {
        panic!("expected a parse refusal, got {error:?}");
    };
    assert!(message.contains("twin"), "{message}");
}

#[test]
fn an_absolute_or_traversing_source_is_refused_by_name() {
    for hostile in ["/etc/passwd", "./ok/../../escape", "../sibling"] {
        let error = Marketplace::parse(&format!(
            r#"{{"name": "m", "owner": {{"name": "o"}}, "plugins": [
                    {{"name": "p", "source": {}}}
                ]}}"#,
            serde_json::json!(hostile)
        ))
        .expect_err("a source may point only into its own marketplace");

        let PluginError::Parse { message, .. } = &error else {
            panic!("expected a parse refusal, got {error:?}");
        };
        assert!(
            message.contains('p'),
            "the refusal names the plugin: {message}"
        );
    }
}

#[test]
fn git_sources_are_told_apart_from_local_directories() {
    for git in [
        "https://github.com/a/b",
        "git@github.com:a/b.git",
        "ssh://host/repo",
        "file:///tmp/market",
        "/tmp/market.git",
    ] {
        assert!(looks_like_git(git), "{git} should clone");
    }
    for local in ["./market", "/tmp/market", "market"] {
        assert!(!looks_like_git(local), "{local} should copy");
    }
}

#[test]
fn frontmatter_splits_into_fields_and_body() {
    let (fields, body) = split_frontmatter(
        "---\nname: reviewer\ndescription: Reviews code carefully\nmodel: anthropic/claude-sonnet-5\n---\nYou review code.\nLine two.",
    );

    assert_eq!(fields["name"], "reviewer");
    assert_eq!(fields["description"], "Reviews code carefully");
    assert_eq!(fields["model"], "anthropic/claude-sonnet-5");
    assert_eq!(body.trim(), "You review code.\nLine two.");

    let (none, all) = split_frontmatter("just a body");
    assert!(none.is_empty());
    assert_eq!(all, "just a body");
}

/// The rows the hand-rolled parser this module once carried read
/// differently from [`ganja_tool::frontmatter`] — pinned here so a
/// plugin's markdown can never again parse two ways from a project's
/// (the shared reader's own "two parsers waiting to disagree" hazard).
#[test]
fn a_plugin_file_reads_through_the_same_grammar_as_a_projects() {
    // A block-scalar description is the block, not the literal `|` — the
    // skill estimate in `ganja plugin list` was priced from one char.
    let (fields, _) = split_frontmatter(
        "---\nname: reviewer\ndescription: |\n  Reviews code.\n  Carefully.\n---\nBody.",
    );
    assert_eq!(fields["description"], "Reviews code.\nCarefully.");

    // A byte-order mark ahead of the fence must not cost somebody their
    // frontmatter.
    let (fields, body) = split_frontmatter("\u{feff}---\nname: bom\n---\nBody.");
    assert_eq!(fields["name"], "bom");
    assert_eq!(body, "Body.");

    // The closing fence owns its whole line: a line merely starting with
    // `---` does not end the block early.
    let (fields, body) = split_frontmatter("---\nname: dashes\n---extra\n---\nBody.");
    assert_eq!(fields["name"], "dashes");
    assert_eq!(body, "Body.");

    // A key with no scalar value (a block-list header like `tools:`) is
    // present and empty rather than silently dropped — the same answer
    // an agent file's own loader gives.
    let (fields, _) = split_frontmatter("---\ntools:\n  - read\n---\nBody.");
    assert_eq!(fields["tools"], "");
}

/// The callers' half of the parser swap: the shared reader answers a
/// valueless `name:` as present-and-empty, and the fallback the sibling
/// loaders pair with that answer has to fire here too — an agent lands
/// under its file stem and a skill cost under its directory, exactly as
/// `agent.rs` and the skill tool answer the same file.
#[test]
fn an_empty_name_falls_back_to_the_stem_instead_of_dropping_the_component() {
    let plugin = TempDir::new().expect("a temporary directory");
    let root = plugin.path();
    plant(
        root,
        "agents/stemmed.md",
        "---\nname:\nmodel:\n---\nYou review.",
    );
    plant(
        root,
        "skills/pricing/SKILL.md",
        "---\nname:\ndescription: Prices things\n---\nBody.",
    );

    let agents = collect_agents(root, "demo");
    let config = agents
        .get("stemmed")
        .expect("the agent loads under its file stem");
    assert_eq!(
        config.model, None,
        "an empty model is no model, as agent.rs answers it"
    );

    let mut costs = Vec::new();
    collect_skill_costs(&root.join("skills"), &mut costs);
    assert_eq!(costs.len(), 1);
    assert_eq!(
        costs[0].name, "pricing",
        "an empty name prices under the directory"
    );
}

/// The collector is one function on purpose — `ganja plugin list` and the
/// load path both call it, which is what keeps their answers identical.
#[test]
fn a_full_plugin_directory_yields_all_six_surfaces() {
    let plugin = TempDir::new().expect("a temporary directory");
    let root = plugin.path();
    plant(
        root,
        "hooks/hooks.json",
        r#"{"hooks": {
              "PreToolUse": [
                {"matcher": "Edit", "hooks": [
                  {"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/check.sh"}
                ]}
              ],
              "Setup": [
                {"hooks": [{"type": "command", "command": "never-fires.sh"}]}
              ]
            }}"#,
    );
    plant(
        root,
        ".mcp.json",
        r#"{"mcpServers": {
              "db": {"command": "${CLAUDE_PLUGIN_ROOT}/server", "args": ["--x"], "env": {"P": "${CLAUDE_PLUGIN_ROOT}/data"}},
              "hub": {"url": "https://mcp.example/mcp", "headers": {"X-A": "1"}},
              "clear": {"url": "http://example.com/mcp"}
            }}"#,
    );
    plant(root, "skills/reviewer/SKILL.md", "# a skill\n");
    plant(
        root,
        "commands/brief.md",
        "---\ndescription: brief me\nargument-hint: <topic>\nagent: plan\n---\n\
             read !`${CLAUDE_PLUGIN_ROOT}/summarize` about $ARGUMENTS\n",
    );
    // Not Markdown, so not a command — the file loader's own rule, which
    // this surface inherits rather than restates.
    plant(root, "commands/notes.txt", "just notes\n");
    plant(
        root,
        "agents/reviewer.md",
        "---\nname: reviewer\ndescription: Reviews\n---\nYou review.\n",
    );
    plant(
        root,
        ".lsp.json",
        r#"{
              "go": {"command": "gopls", "args": ["serve"], "extensionToLanguage": {".go": "go"}},
              "broken": {"command": "x"}
            }"#,
    );

    let found: Contribution = collect(root, "fixture");

    let pre = &found.hooks["PreToolUse"];
    assert_eq!(
        pre.len(),
        1,
        "the Setup event this build does not fire is skipped"
    );
    assert!(!found.hooks.contains_key("Setup"));
    let HookHandler::Command(command) = &pre[0].hooks[0];
    assert!(
        command.command.starts_with(&root.display().to_string()),
        "the plugin-root placeholder is substituted: {}",
        command.command
    );

    assert_eq!(
        found.mcp.len(),
        2,
        "the clear-wire entry is skipped like check_mcp would refuse it"
    );
    let McpServer::Local(db) = &found.mcp["db"] else {
        panic!("a command entry becomes a local server");
    };
    assert_eq!(db.command[0], format!("{}/server", root.display()));
    assert_eq!(db.command[1], "--x");
    assert_eq!(db.environment["P"], format!("{}/data", root.display()));
    assert!(matches!(&found.mcp["hub"], McpServer::Remote(_)));

    assert_eq!(
        found.skills_root.as_deref(),
        Some(root.join("skills").as_path())
    );

    assert_eq!(
        found.commands.keys().collect::<Vec<_>>(),
        vec!["brief"],
        "the command loader's own rules decide what is a command file"
    );
    let brief = &found.commands["brief"];
    assert_eq!(brief.description.as_deref(), Some("brief me — <topic>"));
    assert_eq!(brief.agent.as_deref(), Some("plan"));
    assert_eq!(
        brief.template,
        format!("read !`{}/summarize` about $ARGUMENTS\n", root.display()),
        "the plugin-root placeholder is substituted in a template too"
    );

    let reviewer = &found.agents["reviewer"];
    assert_eq!(reviewer.description.as_deref(), Some("Reviews"));
    assert_eq!(reviewer.prompt.as_deref(), Some("You review."));

    assert_eq!(
        found.lsp.len(),
        1,
        "an entry with no extensionToLanguage is skipped"
    );
    let go = &found.lsp["go"];
    assert_eq!(
        go.command.as_deref(),
        Some(["gopls".to_owned(), "serve".to_owned()].as_slice())
    );
    assert_eq!(
        go.extensions.as_deref(),
        Some([".go".to_owned()].as_slice())
    );
}

#[test]
fn an_empty_or_missing_plugin_directory_contributes_nothing() {
    let plugin = TempDir::new().expect("a temporary directory");

    let found = collect(plugin.path(), "empty");
    assert!(found.described().is_empty());

    let gone = collect(&plugin.path().join("never-made"), "gone");
    assert!(gone.described().is_empty());
}
