//! The translation, exercised on the shapes a legacy config can hold.
//!
//! What the *command* does with a file — which one it picks, where the result
//! lands, what it refuses to overwrite — is settled in `tests/config_migrate.rs`,
//! through the built binary. What is settled here is the translation itself,
//! which is where an inverted rule or a dropped value would come from.

use std::path::Path;

use super::*;

/// The path every fixture is attributed to, so a failure names something.
const FIXTURE: &str = "ganja.jsonc";

/// Translates `text` and hands back what would have been written, with the
/// report that explains it.
fn migrated(text: &str) -> (String, Report) {
    let (document, report) = translate(Path::new(FIXTURE), text).expect("the fixture translates");

    (document.to_string(), report)
}

/// The two configs a round trip compares: the source as this build reads it,
/// and the written file as this build reads it.
fn both(text: &str) -> (Config, Config, String) {
    let legacy = decode(Path::new(FIXTURE), text).expect("the fixture decodes");
    let (rendered, _) = migrated(text);
    let toml = toml_edit::de::from_str::<Config>(&rendered)
        .unwrap_or_else(|error| panic!("the migrated document decodes: {error}\n{rendered}"));

    (legacy, toml, rendered)
}

/// The tools a permission config names, in the order it will be evaluated in.
fn tools(config: &Config) -> Vec<String> {
    config
        .permission
        .rules()
        .into_iter()
        .map(|rule| rule.permission)
        .collect()
}

/// The failure this whole command is written around.
///
/// Permission rules are evaluated last-match-wins, so their order is their
/// meaning. A translation that rendered `bash`'s object as a `[permission.bash]`
/// sub-table would have to print it *after* `permission`'s own key-values —
/// TOML has no other spelling — which would hand the loader `edit, webfetch,
/// bash` where the source said `bash, webfetch, edit`, and the `deny` written
/// last would start losing to the `allow` written first with nothing to say
/// so. The fixture is deliberately spelled so that document order, sorted
/// order and the values-then-tables order a naive writer produces are three
/// different answers.
#[test]
fn a_permission_table_keeps_the_order_the_source_wrote() {
    let (legacy, toml, rendered) = both(
        r#"{
             "permission": {
               "bash": { "git status": "allow", "*": "deny" },
               "webfetch": "allow",
               "edit": "ask"
             }
           }"#,
    );

    assert_eq!(
        tools(&toml),
        ["bash", "bash", "webfetch", "edit"],
        "document order, not sorted order and not values-before-tables:\n{rendered}"
    );
    assert_eq!(
        legacy.permission.rules(),
        toml.permission.rules(),
        "the same rules, in the same order:\n{rendered}"
    );
}

/// The same, one level down: an agent carries a permission block of its own,
/// and it is read by the same type through the same rule.
#[test]
fn an_agent_permission_table_keeps_its_order_too() {
    let (legacy, toml, rendered) = both(
        r#"{
             "agent": {
               "build": {
                 "permission": {
                   "webfetch": "allow",
                   "bash": { "*": "deny" },
                   "edit": "ask"
                 }
               }
             }
           }"#,
    );

    let order = |config: &Config| {
        config.agent["build"]
            .permission
            .rules()
            .into_iter()
            .map(|rule| rule.permission)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        order(&toml),
        ["webfetch", "bash", "edit"],
        "an agent's block is ordered by the same rule:\n{rendered}"
    );
    assert_eq!(order(&legacy), order(&toml));
}

/// The equality the command asserts before writing, over a document carrying
/// one of every shape a config file has: an ordered table, two maps keyed by a
/// name somebody chose, a nested table and the array-of-tables `hooks` block.
#[test]
fn a_whole_config_reads_back_as_the_one_that_was_read() {
    let (legacy, toml, rendered) = both(
        r#"{
             // A comment, which does not survive and is not supposed to.
             "$schema": "https://ganja.example/config.json",
             "model": "anthropic/claude-sonnet-5",
             "permission": { "webfetch": "allow", "bash": { "git *": "ask" } },
             "tui": {
               "notification_method": "bel",
               "statusline": { "elements": ["model", "rate"], "detail": true }
             },
             "mcp": {
               "docs": { "type": "local", "command": ["bun", "x", "docs"], "timeout": 1234 }
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
    );

    assert_eq!(legacy, toml, "one config, two spellings:\n{rendered}");
}

/// A key somebody chose that TOML would have to quote is quoted, rather than
/// producing a document that does not parse.
#[test]
fn a_name_that_needs_quoting_is_quoted() {
    let (legacy, toml, rendered) = both(
        r#"{
             "mcp": {
               "docs.internal server": { "type": "local", "command": ["run"] }
             }
           }"#,
    );

    assert!(
        rendered.contains(r#""docs.internal server""#),
        "the header quotes the name:\n{rendered}"
    );
    assert_eq!(legacy, toml);
}

/// TOML has no `null`. A null *property* is the one case with no consequence —
/// serde reads an absent key and an explicit null as the same `None` — so it is
/// dropped, and reported so nobody has to infer that from silence.
#[test]
fn a_null_property_is_dropped_and_reported() {
    let (legacy, toml, _) = both(r#"{ "model": null, "theme": "gruvbox" }"#);
    let (_, report) = migrated(r#"{ "model": null, "theme": "gruvbox" }"#);

    assert_eq!(legacy, toml, "an absent key and a null one load the same");
    assert_eq!(
        report.skipped,
        [("model".to_owned(), reason::NULL.to_owned())]
    );
}

/// A null *element* is the case with a consequence: dropping it shortens the
/// list, which is a different setting, and there is no spelling that keeps it.
#[test]
fn a_null_array_element_refuses() {
    let error = translate(
        Path::new(FIXTURE),
        r#"{ "instructions": ["./AGENTS.md", null] }"#,
    )
    .expect_err("a null element has no TOML spelling");

    let message = format!("{error}");
    assert!(
        message.contains("instructions[1]") && message.contains("shorten"),
        "the refusal names the element and why it is not dropped: {message}"
    );
}

/// A comment is gone whatever the writer does, so the one thing left worth
/// doing is saying which lines held one.
#[test]
fn every_comment_line_is_reported_by_number() {
    let text = "{\n  // one\n  \"model\": \"anthropic/x\",\n\n  /* two\n     three */\n  \"theme\": \"gruvbox\"\n}\n";

    assert_eq!(
        comments(Path::new(FIXTURE), text).expect("the fixture parses"),
        [2, 5, 6],
        "a line comment names its line and a block comment names every line it covered"
    );
}

/// A file holding nothing, or nothing but comments, is an empty config rather
/// than an error — so it is an empty document rather than one.
#[test]
fn a_source_holding_only_comments_becomes_an_empty_document() {
    let (rendered, report) = migrated("// nothing but this\n");

    assert_eq!(rendered, "");
    assert!(report.mapped.is_empty() && report.skipped.is_empty());
}

/// An empty object is a value, not an absence: `oauth = {}` is how a remote
/// MCP entry asks for a login, and a header suppressed as "empty" would delete
/// the request.
#[test]
fn an_empty_object_keeps_the_header_that_is_its_whole_value() {
    let (legacy, toml, rendered) = both(
        r#"{
             "mcp": {
               "docs": { "type": "remote", "url": "https://example.test/mcp", "oauth": {} }
             }
           }"#,
    );

    assert!(
        rendered.contains("[mcp.docs.oauth]"),
        "the marker keeps its header:\n{rendered}"
    );
    assert_eq!(legacy, toml);
}

/// A table holding nothing but other tables needs no header of its own, and
/// the one place that is visible is the file somebody reads afterwards.
#[test]
fn a_table_of_tables_is_left_implicit() {
    let (rendered, _) =
        migrated(r#"{ "mcp": { "docs": { "type": "local", "command": ["run"] } } }"#);

    assert!(
        !rendered.contains("[mcp]\n"),
        "no bare header above the entry it introduces:\n{rendered}"
    );
    assert!(rendered.contains("[mcp.docs]"), "{rendered}");
}

/// A number that no TOML integer holds is refused rather than rounded into a
/// float, because a writer that rounded would change a setting silently.
#[test]
fn an_integer_too_large_for_toml_refuses() {
    let error = translate(
        Path::new(FIXTURE),
        r#"{ "mcp": { "docs": { "type": "local", "command": ["run"], "timeout": 99999999999999999999 } } }"#,
    )
    .expect_err("an integer past 64 signed bits has no TOML spelling");

    assert!(
        format!("{error}").contains("64"),
        "the refusal says what would not fit: {error}"
    );
}

/// The report is the output, so a key that travelled is a row that names the
/// shape it took.
#[test]
fn every_top_level_key_is_a_row() {
    let (_, report) = migrated(
        r#"{
             "model": "anthropic/x",
             "permission": { "edit": "ask" },
             "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "./x.sh" }] }] }
           }"#,
    );

    assert_eq!(
        report.mapped,
        [
            ("model".to_owned(), "model".to_owned()),
            ("permission".to_owned(), "[permission]".to_owned()),
            ("hooks".to_owned(), "[hooks]".to_owned()),
        ]
    );
}

/// The loader refuses an MCP entry it could not start *after* decoding it, so
/// a source carrying one is refused here rather than translated into a
/// `ganja.toml` the next launch declines.
///
/// The MCP case and no other: `McpServer::check` is the only one of the
/// loader's seven post-decode checks with a public authority to call, so this
/// is the only one [`decode`] can make. The module doc says what the other
/// six cost until `config::legacy` runs them in-crate, and this test's name
/// is kept narrow so it cannot be read as covering them.
#[test]
fn an_mcp_entry_this_build_could_not_start_is_refused_before_it_is_translated() {
    let error = decode(
        Path::new(FIXTURE),
        r#"{ "mcp": { "docs": { "type": "local", "command": [] } } }"#,
    )
    .expect_err("an entry with nothing to run is not one this build loads");

    assert!(
        format!("{error}").contains(FIXTURE),
        "the refusal names the file that said it: {error}"
    );
}
