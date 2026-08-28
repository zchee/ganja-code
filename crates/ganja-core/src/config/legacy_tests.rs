//! What the reader that answers for the old files still has to get right.
//!
//! Two claims, and both are what the format change rests on. That the dialect
//! is still read the way it always was, so `ganja config migrate` converts
//! what a previous build loaded rather than an approximation of it. And that a
//! source this build would refuse at launch is refused *here* — the seven
//! post-decode checks run on this path exactly as they run on the TOML one, so
//! the command that converts a file cannot hand somebody a `ganja.toml` whose
//! first launch declines it.

use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::{Config, ConfigError, read};

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// Writes `text` to a legacy-named file and reads it back the way `migrate`
/// would.
fn parse(text: &str) -> Result<Config, ConfigError> {
    let directory = temporary();
    let path = directory.path().join("ganja.jsonc");
    fs::write(&path, text).expect("the fixture file is writable");

    read(&path)
}

#[test]
fn comments_and_trailing_commas_are_part_of_the_dialect() {
    let config = parse(
        r#"{
              // the model this project talks to
              "model": "anthropic/claude-sonnet-5",
              /* and the cheap one */
              "small_model": "anthropic/claude-haiku-4.5",
            }"#,
    )
    .expect("JSONC is what these files were written in");

    assert_eq!(config.model.as_deref(), Some("anthropic/claude-sonnet-5"));
    assert_eq!(config.small_model.as_deref(), Some("anthropic/claude-haiku-4.5"));
}

/// The `Option` in the decode is what makes this an empty config rather than a
/// type error about `null` — the one thing the two formats reach the same
/// answer by different routes, which is why it is pinned on both sides.
#[test]
fn a_file_holding_nothing_is_an_empty_config_rather_than_an_error() {
    for text in ["", "   \n  ", "// nothing but a comment\n"] {
        assert_eq!(
            parse(text).expect("an empty config file is legal"),
            Config::default(),
            "parsing {text:?}"
        );
    }
}

/// The curated key set is serde's and is read off the same [`Config`], so a
/// misspelling is refused on this path too — and by name.
#[test]
fn an_unknown_top_level_key_is_refused_by_name_here_too() {
    let error = parse(r#"{"modle": "anthropic/claude-sonnet-5"}"#)
        .expect_err("a misspelled key is a setting that does not work");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("modle"), "{message}");
}

/// Every one of the loader's seven post-decode refusals, on this path.
///
/// The narrow version of this — `McpServer::check` alone, the only one whose
/// authority was public — was what `migrate` could reach while it had its own
/// reader, and a legacy file whose hooks matcher did not compile translated
/// cleanly and failed at the *next* launch. Reading through this module is
/// what closed that, so all seven are named here rather than one: a check that
/// quietly stopped running on this path would put that hole back.
#[test]
fn a_source_the_loader_would_refuse_is_refused_at_the_legacy_read() {
    let cases = [
        // check_mcp
        (r#"{"mcp": {"docs": {"type": "local", "command": []}}}"#, "command"),
        // check_lsp
        (r#"{"lsp": {"zls": {"extensions": [".zig"]}}}"#, "command"),
        // check_providers
        (
            r#"{"provider": {"x": {"dialect": "openai-responses", "base_url": "not a url"}}}"#,
            "base_url",
        ),
        // check_hooks
        (
            r#"{"hooks": {"PreToolUse": [{"matcher": "Edit(",
                 "hooks": [{"type": "command", "command": "./x.sh"}]}]}}"#,
            "matcher",
        ),
        // check_agents
        (r#"{"agents": {"concurrency": 0}}"#, "concurrency"),
        // check_teammates
        (r#"{"teammates": {"shim_turn_timeout": 0}}"#, "shim_turn_timeout"),
        // check_openrouter
        (r#"{"openrouter": {"server_tools": ["not_a_tool"]}}"#, "not_a_tool"),
    ];

    for (text, named) in cases {
        let error = parse(text).unwrap_err();
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure for {text}, got {error:?}");
        };
        assert!(message.contains(named), "{text}: {message}");
    }
}

/// One config, two dialects, one value — the whole 1:1 claim of the format
/// change made mechanical, and the reason `migrate` can compare the two sides
/// for equality before it writes anything.
///
/// The fixture is deliberately the awkward half of the file rather than a
/// handful of scalars: the array-of-tables `hooks` block, an ordered
/// `permission` table, the two maps keyed by a name somebody chose (`mcp`,
/// `provider`), and a nested `tui` table. Those are where a translation
/// between the two spellings can quietly change a shape; `model` cannot.
#[test]
fn the_same_config_in_both_dialects_loads_to_the_same_value() {
    let old = parse(
        r#"{
              "$schema": "https://ganja.example/config.json",
              "model": "anthropic/claude-sonnet-5",
              "permission": {
                "webfetch": "allow",
                "bash": { "git status": "allow", "*": "ask" }
              },
              "tui": {
                "notification_method": "bel",
                "statusline": { "elements": ["model", "rate"], "detail": true }
              },
              "mcp": {
                "docs": { "type": "local", "command": ["bun", "x", "docs"], "timeout": 1234 }
              },
              "provider": {
                "local-llama": {
                  "dialect": "openai-chat-completions",
                  "base_url": "http://127.0.0.1:11434/v1"
                }
              },
              "hooks": {
                "PreToolUse": [
                  {
                    "matcher": "Edit|Write",
                    "hooks": [{ "type": "command", "command": "./check.sh", "timeout": 5 }]
                  }
                ]
              }
            }"#,
    )
    .expect("the legacy fixture parses");

    let directory = temporary();
    let path = directory.path().join("ganja.toml");
    fs::write(
        &path,
        r#"
            "$schema" = "https://ganja.example/config.json"
            model = "anthropic/claude-sonnet-5"

            [permission]
            webfetch = "allow"

            [permission.bash]
            "git status" = "allow"
            "*" = "ask"

            [tui]
            notification_method = "bel"

            [tui.statusline]
            elements = ["model", "rate"]
            detail = true

            [mcp.docs]
            type = "local"
            command = ["bun", "x", "docs"]
            timeout = 1234

            [provider.local-llama]
            dialect = "openai-chat-completions"
            base_url = "http://127.0.0.1:11434/v1"

            [[hooks.PreToolUse]]
            matcher = "Edit|Write"

            [[hooks.PreToolUse.hooks]]
            type = "command"
            command = "./check.sh"
            timeout = 5
        "#,
    )
    .expect("the fixture file is writable");
    let new =
        super::super::read(&path).expect("the TOML fixture parses").expect("the fixture exists");

    assert_eq!(old, new);
}

/// Nothing discovers its way here, so an absent file is a caller's mistake
/// rather than the common case the loader's own read treats it as.
#[test]
fn a_file_that_is_not_there_is_an_error_on_this_path() {
    let directory = temporary();
    let missing = directory.path().join("ganja.jsonc");

    let error = read(&missing).expect_err("this reader is only ever handed a named file");

    assert!(matches!(&error, ConfigError::Read { path, .. } if path == &missing));
}

/// The refusal that sent somebody here names a path, and this reader answers
/// for exactly that path — no directory walk, no second name tried.
#[test]
fn the_file_read_is_the_file_named() {
    let directory = temporary();
    let named = directory.path().join("ganja.json");
    fs::write(&named, r#"{"model": "anthropic/claude-sonnet-5"}"#)
        .expect("the fixture file is writable");
    fs::write(directory.path().join("ganja.jsonc"), r#"{"model": "openai/gpt-5.6"}"#)
        .expect("the fixture file is writable");

    let config = read(&named).expect("the named file parses");

    assert_eq!(config.model.as_deref(), Some("anthropic/claude-sonnet-5"));
    assert!(Path::new(&named).is_file(), "and the read left it exactly where it was");
}
