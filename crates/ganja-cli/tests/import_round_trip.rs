//! What the importer writes is what the next launch reads.
//!
//! The importer validates its own output before writing it, which proves the
//! bytes decode. This proves the other half — that the file lands where
//! `ganja_core::config` looks, and that every value survives the trip with its
//! meaning intact, permission order included.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables so that the in-process load and the subprocess that wrote the file
//! agree about where the config homes are, and a plain `cargo test` runs the
//! tests inside a binary on parallel threads. `XDG_CONFIG_HOME`,
//! `XDG_DATA_HOME` and `HOME` are redirected into a temporary tree — and
//! `GANJA_CONFIG_HOME` cleared — so the machine running the suite cannot
//! contribute a config of its own.

use std::{env, fs, num::NonZeroU64};

use assert_cmd::Command;
use ganja_core::{
    Config,
    config::{AgentMode, CONFIG_ENV, CONFIG_HOME_ENV, LspConfig, McpServer},
    provider::Dialect,
};
use ganja_permission::Action;

/// An imported `deny` is a rule this build carries out: it refuses the call
/// without asking anybody.
fn deny() -> Action {
    Action::Deny
}

#[test]
fn an_imported_config_is_one_the_next_launch_reads_back_whole() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let project = home.path().join("project");
    // A checkout, so the project walk stops here rather than climbing out of
    // the fixture and into whatever the temporary directory sits under.
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::write(
        project.join("opencode.jsonc"),
        include_str!("fixtures/opencode.jsonc"),
    )
    .expect("the fixture file is writable");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        env::set_var("XDG_DATA_HOME", home.path().join("data"));
        // The global tier and the `--global` destination both resolve through
        // ganja's config-home seam, which reaches past the XDG redirect:
        // `~/.ganja` through `HOME`, and `GANJA_CONFIG_HOME` past everything.
        env::set_var("HOME", home.path());
        env::remove_var(CONFIG_HOME_ENV);
        // Otherwise a developer's exported file would be read as a tier of its
        // own, on top of the one under test.
        env::remove_var(CONFIG_ENV);
    }

    Command::new(env!("CARGO_BIN_EXE_ganja"))
        .current_dir(&project)
        .args(["config", "import-opencode"])
        .assert()
        .success();

    let config = Config::load(&project).expect("the imported config is one ganja loads");

    assert_eq!(config.model.as_deref(), Some("anthropic/claude-sonnet-5"));
    assert_eq!(
        config.small_model, None,
        "the model that was only an {{env:}} token stays out"
    );
    assert_eq!(config.default_agent.as_deref(), Some("plan"));
    assert_eq!(config.theme.as_deref(), Some("tokyonight"));
    assert_eq!(config.shell.as_deref(), Some("/bin/zsh"));
    assert_eq!(
        config.instructions,
        vec!["AGENTS.md", "docs/{env:TEAM}/style.md"],
        "the entry that was only a token is gone, the one that embeds one is not"
    );

    // Order is the whole semantics of a rule set: evaluation is
    // last-match-wins, so a rule that moved is a rule that stopped applying.
    let rules: Vec<(String, String, Action)> = config
        .permission
        .rules()
        .into_iter()
        .map(|rule| (rule.permission, rule.pattern, rule.action))
        .collect();
    assert_eq!(
        rules,
        vec![
            // Derived from the legacy `tools` map, which keeps its position…
            ("webfetch".to_owned(), "*".to_owned(), deny()),
            // …and loses every tool the explicit rules also name.
            ("bash".to_owned(), "git status".to_owned(), Action::Allow),
            ("bash".to_owned(), "git *".to_owned(), Action::Ask),
            ("bash".to_owned(), "*".to_owned(), deny()),
            ("edit".to_owned(), "*".to_owned(), Action::Ask),
            ("read".to_owned(), "*".to_owned(), Action::Allow),
        ]
    );

    let review = &config.agent["review"];
    assert_eq!(review.model.as_deref(), Some("anthropic/claude-haiku-4-5"));
    assert_eq!(
        review.description.as_deref(),
        Some("reads a diff and complains")
    );
    assert_eq!(review.mode, Some(AgentMode::Subagent));
    assert_eq!(
        review
            .permission
            .rules()
            .into_iter()
            .map(|rule| (rule.permission, rule.action))
            .collect::<Vec<_>>(),
        vec![
            ("edit".to_owned(), deny()),
            ("webfetch".to_owned(), Action::Allow),
        ]
    );

    // A `mode` entry is an agent only the user can pick.
    let ship = &config.agent["ship"];
    assert_eq!(
        ship.prompt.as_deref(),
        Some("You ship what is already green.")
    );
    assert_eq!(ship.mode, Some(AgentMode::Primary));
    assert_eq!(ship.hidden, Some(false));

    let release = &config.command["release"];
    assert_eq!(release.template, "cut a release for $ARGUMENTS");
    assert_eq!(release.description.as_deref(), Some("tag and push"));
    assert_eq!(release.agent.as_deref(), Some("build"));
    assert_eq!(release.model, None);

    // The MCP and LSP entries are the half `validate` cannot prove on its own:
    // decoding is not the whole of what `Config::load` does to them, and the
    // rules it applies beyond decoding — a server with no program, a custom
    // language server with no extensions, an endpoint whose headers would go
    // out in the clear — are refusals this file has already got past by being
    // read at all.
    let McpServer::Local(fs) = &config.mcp["fs"] else {
        panic!("the local server stayed local: {:?}", config.mcp["fs"]);
    };
    assert_eq!(fs.command, ["mcp-fs", "--root", "."]);
    assert_eq!(fs.cwd.as_deref(), Some("./servers"));
    assert_eq!(fs.environment["MCP_FS_MODE"], "ro");
    assert!(fs.enabled);
    assert_eq!(fs.timeout.map(NonZeroU64::get), Some(45_000));

    let McpServer::Remote(docs) = &config.mcp["docs"] else {
        panic!("the remote server stayed remote: {:?}", config.mcp["docs"]);
    };
    assert_eq!(docs.url, "https://mcp.example.invalid/mcp");
    assert_eq!(
        docs.headers["Authorization"], "Bearer {env:DOCS_TOKEN}",
        "a header holding a token is carried verbatim, never expanded"
    );
    assert!(
        !config.mcp.contains_key("legacy"),
        "an entry naming no type described no server"
    );

    let Some(LspConfig::Servers(servers)) = &config.lsp else {
        panic!("the lsp map survived as a map: {:?}", config.lsp);
    };
    assert!(servers["rust"].disabled);
    assert_eq!(servers["rust"].command, None);
    assert_eq!(
        servers["nickel"].command.as_deref(),
        Some(&["nls".to_owned()][..])
    );
    assert_eq!(
        servers["nickel"].extensions.as_deref(),
        Some(&[".ncl".to_owned()][..])
    );
    assert_eq!(servers["nickel"].env["NICKEL_LOG"], "info");
    assert_eq!(
        servers["nickel"].initialization,
        Some(serde_json::json!({"eval": {"limit": 500}})),
        "an initialization block travels as the document it is"
    );
    // What separates these two is not the name — both name one of opencode's
    // builtins — but whether the entry needed that builtin to mean anything.
    assert_eq!(
        servers["deno"].command.as_deref(),
        Some(&["deno".to_owned(), "lsp".to_owned()][..]),
        "an entry that describes itself whole is a custom server here"
    );
    assert_eq!(
        servers["deno"].extensions.as_deref(),
        Some(&[".ts".to_owned(), ".tsx".to_owned()][..])
    );
    assert!(
        !servers.contains_key("typescript"),
        "an entry leaning on a definition this build does not have was written anyway"
    );

    // The provider table is the other half `validate` cannot prove alone: a
    // config's own load refuses an entry naming a builtin and one whose
    // endpoint would carry a key in the clear, so a file that got past both is
    // one the importer could not have written wrong.
    let local = &config.provider["local-llama"];
    assert_eq!(local.dialect, Dialect::OpenaiChatCompletions);
    assert_eq!(local.base_url, "http://127.0.0.1:11434/v1");
    assert_eq!(local.headers["x-route"], "gpu-0");
    assert_eq!(
        local.key_env, None,
        "opencode's entry held the key itself, and nothing invents the name of \
         a variable holding it"
    );
    assert!(
        !config.provider.contains_key("anthropic"),
        "an entry naming a provider this build ships is one `Config::load` \
         refuses, so writing it would have produced a file that does not read \
         back — this load is what proves it was not written"
    );

    assert!(
        !config.snapshots_enabled(),
        "an author who told opencode not to track files has not been overruled"
    );
}
