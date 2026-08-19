//! `schema/ganja-config.schema.json`, checked against the loader it
//! describes.
//!
//! Spec: none upstream — opencode publishes its own config schema, and this
//! is parity rather than a divergence. What is ported is the *idea* that a
//! schema and its loader can drift apart silently; this file is what keeps
//! them from doing that, in both directions:
//!
//! - **Code → schema.** [`Config`] and the two MCP shapes are all
//!   `#[serde(deny_unknown_fields)]`. Feeding the real loader one bogus key
//!   makes serde's derived `Deserialize` impl report `"unknown field `x`,
//!   expected one of `a`, `b`, ...`` — [`jsonc_parser::ParseError`] only
//!   overrides `serde::de::Error::custom`, so that enumeration is serde's own
//!   unmodified wording. [`expected_fields`] parses it back out, and the tests
//!   below assert that set equals the schema's own `properties` keys — so a
//!   field added to a struct and forgotten in the schema fails here, not in
//!   an editor that silently stopped complaining about it.
//! - **Schema, on its own.** The schema must compile under Draft 2020-12, a
//!   document naming everything this build understands must validate against
//!   it, and the refusals the schema itself can express (`output_limit: 0`,
//!   an unknown key) must be refused by it.
//!
//! One binary, one environment-mutating test — the house rule every other
//! config suite here follows, because `Config::load_with` always consults the
//! global config tier and a plain `cargo test` runs a binary's tests on
//! parallel threads. The schema-only checks need no config discovery at all
//! and are safe to run as their own tests alongside it.

use std::{collections::BTreeSet, env, fs, path::Path};

use ganja_core::{
    Config, ConfigError, Overrides,
    config::{CONFIG_ENV, CONFIG_HOME_ENV},
};
use regex::Regex;
use serde_json::{Value, json};

/// The schema, parsed once per test. `include_str!` ties this file to the
/// schema under `schema/` at compile time — moving one without the other is
/// a build error, not a silent drift.
fn schema() -> Value {
    serde_json::from_str(include_str!("../../../schema/ganja-config.schema.json"))
        .expect("the schema is valid JSON")
}

/// The `properties` keys of `schema()`, or of one of its `$defs`.
fn schema_keys(schema: &Value, def: Option<&str>) -> BTreeSet<String> {
    let object = match def {
        Some(name) => &schema["$defs"][name],
        None => schema,
    };
    object["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("{def:?} has a properties object"))
        .keys()
        .cloned()
        .collect()
}

/// Parses the backtick-quoted field names out of serde's own
/// `"unknown field \`x\`, expected one of \`a\`, \`b\`, ..."` wording —
/// `OneOf`'s `Display` impl, reached through
/// `serde::de::Error::unknown_field`'s default implementation, which
/// [`jsonc_parser::ParseError`] does not override. Panics on a message that
/// does not contain "expected" at all, since that means the probe below did
/// not trip the refusal it was written to trip.
fn expected_fields(message: &str) -> BTreeSet<String> {
    let after_expected = message
        .split_once("expected")
        .unwrap_or_else(|| panic!("no \"expected\" in: {message}"))
        .1;
    let backtick_quoted = Regex::new(r"`([^`]+)`").expect("a fixed pattern compiles");
    backtick_quoted
        .captures_iter(after_expected)
        .map(|capture| capture[1].to_owned())
        .collect()
}

/// Loads `text` as the sole project-tier config file under `project`, the way
/// discovery would, and returns the parse error's message.
fn bogus_key_error(project: &Path, text: &str) -> String {
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::write(project.join("ganja.jsonc"), text).expect("the fixture file is writable");

    let error = Config::load_with(project, &Overrides::default())
        .expect_err("a bogus key is refused by name");
    let ConfigError::Parse { message, .. } = error else {
        panic!("expected a parse failure, got {error:?}");
    };
    message
}

/// A document naming every top-level key this build understands, and both
/// MCP shapes with every field they carry — the same coverage
/// `an_mcp_entry_carries_everything_the_two_shapes_hold` gives the loader's
/// own suite, reused here so the schema is checked against a document that
/// actually exercises it rather than an empty one.
const KITCHEN_SINK: &str = r#"{
  "$schema": "./schema/ganja-config.schema.json",
  "model": "anthropic/claude-sonnet-5",
  "small_model": "anthropic/claude-haiku-4.5",
  "default_provider": "anthropic",
  "default_agent": "build",
  "effort": "high",
  "agent": {
    "plan": { "description": "plans", "mode": "primary" }
  },
  "agents": { "concurrency": 2 },
  "teammates": { "shim_turn_timeout": 900 },
  "permission": { "bash": "ask" },
  "instructions": ["AGENTS.md"],
  "theme": "dracula",
  "theme_mode": "dark",
  "keybinds": { "redraw": "f6" },
  "shell": "/bin/zsh",
  "command": {
    "commit": { "template": "commit $ARGUMENTS", "description": "commit" }
  },
  "mcp": {
    "fs": {
      "type": "local",
      "command": ["bun", "x", "server"],
      "cwd": "tools",
      "environment": { "TOKEN": "x" },
      "timeout": 1234,
      "output_limit": 4096
    },
    "hub": {
      "type": "remote",
      "url": "https://mcp.example/mcp",
      "headers": { "Authorization": "Bearer x" },
      "enabled": false,
      "timeout": 5000,
      "output_limit": 8192
    },
    "auth": {
      "type": "remote",
      "url": "https://oauth.example/mcp",
      "oauth": {}
    }
  },
  "hooks": {
    "PreToolUse": [
      { "matcher": "Edit", "hooks": [{ "type": "command", "command": "./check.sh", "timeout": 5 }] }
    ]
  },
  "lsp": { "rust": { "command": ["rust-analyzer"] } },
  "provider": {
    "local-llama": {
      "dialect": "openai-chat-completions",
      "base_url": "http://127.0.0.1:8080",
      "key_env": "LOCAL_LLAMA_KEY",
      "headers": { "X-Custom": "1" }
    }
  },
  "webfetch": { "allow_private": true },
  "skills": { "paths": ["~/.claude/skills"], "urls": ["https://example/skills"] },
  "memory": true,
  "snapshot": false,
  "tui": {
    "notifications": ["turn-complete", "approval-requested"],
    "notification_method": "bel",
    "statusline": {
      "elements": ["git", "model", "context", "rate", "tokens", "session", "cwd", "todos"],
      "max_width": 160,
      "detail": true
    }
  },
  "openrouter": {
    "server_tools": ["web_search", "datetime"]
  }
}"#;

/// The single environment-mutating test in this binary: everything that
/// needs the real loader (and therefore `Config::load_with`'s global-tier
/// discovery) lives here, sequentially, sharing one pinned `HOME`/
/// `XDG_CONFIG_HOME` so nothing here can read a real user's config and
/// nothing races another test over the same process-wide variables.
#[test]
fn the_schema_matches_what_the_real_loader_accepts() {
    let home = tempfile::tempdir().expect("a temporary directory");

    // SAFETY: this binary holds one environment-mutating test, so nothing
    // else in the process is reading the environment while it is written.
    unsafe {
        env::set_var("HOME", home.path());
        env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        env::set_var("XDG_DATA_HOME", home.path().join("data"));
        env::remove_var(CONFIG_HOME_ENV);
        env::remove_var(CONFIG_ENV);
    }

    let schema = schema();

    the_loader_names_exactly_the_top_level_keys_the_schema_lists(&home.path().join("top"), &schema);
    the_loader_names_exactly_the_fields_a_local_mcp_entry_accepts(
        &home.path().join("mcp-local"),
        &schema,
    );
    the_loader_names_exactly_the_fields_a_remote_mcp_entry_accepts(
        &home.path().join("mcp-remote"),
        &schema,
    );
    the_kitchen_sink_document_loads_through_the_real_loader(&home.path().join("kitchen-sink"));
    the_loader_refuses_a_non_loopback_mcp_url_that_the_schema_alone_would_accept(
        &home.path().join("non-loopback"),
    );
}

/// Code → schema, for [`Config`] itself: a bogus top-level key makes serde
/// enumerate every field it does accept, and that enumeration must be
/// exactly the schema's top-level `properties` — no more (a stale schema key
/// nothing loads any more), no fewer (a struct field the schema forgot).
fn the_loader_names_exactly_the_top_level_keys_the_schema_lists(project: &Path, schema: &Value) {
    let message = bogus_key_error(project, r#"{"zzz_schema_probe": 1}"#);
    let loader_fields = expected_fields(&message);
    let schema_fields = schema_keys(schema, None);

    assert_eq!(
        loader_fields, schema_fields,
        "Config's own fields (from serde's refusal: {message:?}) must be exactly \
         the schema's top-level properties"
    );
}

/// Code → schema, for `McpLocal`: the same probe, aimed at an MCP entry
/// nested inside a real config document, so the error is the one a person
/// configuring a local server would actually see.
fn the_loader_names_exactly_the_fields_a_local_mcp_entry_accepts(project: &Path, schema: &Value) {
    let message = bogus_key_error(
        project,
        r#"{"mcp": {"fs": {"type": "local", "command": ["x"], "zzz_schema_probe": 1}}}"#,
    );
    let loader_fields = expected_fields(&message);
    // `type` is the enum's internal tag: serde strips it from the map before
    // `McpLocal`'s own `deny_unknown_fields` ever sees it, so it never
    // appears in this refusal — but the schema still needs it as a property
    // to discriminate `oneOf` between the two shapes. Accounted for
    // explicitly rather than silently, the same way the top-level probe
    // above does not need to (`Config::schema` really is named `$schema`).
    let schema_fields: BTreeSet<String> = schema_keys(schema, Some("McpLocal"))
        .into_iter()
        .filter(|key| key != "type")
        .collect();

    assert_eq!(
        loader_fields, schema_fields,
        "McpLocal's own fields (from serde's refusal: {message:?}) must be exactly \
         the schema's McpLocal properties, minus the `type` tag"
    );
}

/// Code → schema, for `McpRemote` — the highest-churn shape of the two,
/// having grown `output_limit` (W5a) and `oauth` (W5b) since the draft this
/// schema started from.
fn the_loader_names_exactly_the_fields_a_remote_mcp_entry_accepts(project: &Path, schema: &Value) {
    let message = bogus_key_error(
        project,
        r#"{"mcp": {"hub": {"type": "remote", "url": "https://x/mcp", "zzz_schema_probe": 1}}}"#,
    );
    let loader_fields = expected_fields(&message);
    let schema_fields: BTreeSet<String> = schema_keys(schema, Some("McpRemote"))
        .into_iter()
        .filter(|key| key != "type")
        .collect();

    assert_eq!(
        loader_fields, schema_fields,
        "McpRemote's own fields (from serde's refusal: {message:?}) must be exactly \
         the schema's McpRemote properties, minus the `type` tag"
    );
}

/// A document naming every top-level key, and both MCP shapes with every
/// field, must load through the real loader without complaint — the other
/// half of [`a_kitchen_sink_document_also_validates_against_the_schema`],
/// which checks the same text against the schema alone.
fn the_kitchen_sink_document_loads_through_the_real_loader(project: &Path) {
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::write(project.join("ganja.jsonc"), KITCHEN_SINK).expect("the fixture file is writable");

    Config::load_with(project, &Overrides::default())
        .expect("a document naming every key this build understands loads");
}

/// What the schema cannot express, documented as evidence rather than a
/// comment: a `webfetch`-style loopback rule lives in [`check_mcp`], not in
/// any keyword `format: "uri"` can spell, so the schema alone would accept a
/// plaintext, non-loopback MCP URL that the real loader refuses. See
/// [`the_schema_accepts_what_only_the_loader_can_refuse`] for the schema-side
/// half of the same document.
fn the_loader_refuses_a_non_loopback_mcp_url_that_the_schema_alone_would_accept(project: &Path) {
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::write(
        project.join("ganja.jsonc"),
        r#"{"mcp": {"hub": {"type": "remote", "url": "http://example.com/mcp"}}}"#,
    )
    .expect("the fixture file is writable");

    let error = Config::load_with(project, &Overrides::default())
        .expect_err("a plaintext non-loopback MCP URL puts headers on the wire in the clear");
    let ConfigError::Parse { message, .. } = error else {
        panic!("expected a parse failure");
    };
    assert!(
        message.contains("loopback"),
        "the refusal should name why: {message}"
    );
}

/// The schema on its own: compiles under Draft 2020-12, with no external
/// resolution needed since every `$ref` here is local (`#/$defs/...`).
#[test]
fn the_schema_is_a_valid_draft_2020_12_document() {
    jsonschema::validator_for(&schema()).expect("the schema compiles under Draft 2020-12");
}

/// The schema, on its own, validates the same kitchen-sink document the
/// loader half of this suite feeds the real loader — the schema-only twin of
/// [`the_kitchen_sink_document_loads_through_the_real_loader`].
#[test]
fn a_kitchen_sink_document_also_validates_against_the_schema() {
    let validator = jsonschema::validator_for(&schema()).expect("the schema compiles");
    let instance: Value = serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");

    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "a document naming every key this build understands should validate cleanly: {errors:?}"
    );
}

/// Refusals the schema itself can express must actually refuse — each case
/// starts from the kitchen sink (so it is otherwise valid) and breaks exactly
/// one thing the schema has a keyword for.
#[test]
fn the_schema_refuses_what_it_has_a_keyword_for() {
    let validator = jsonschema::validator_for(&schema()).expect("the schema compiles");
    let mut sink: Value = serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");

    sink["mcp"]["fs"]["output_limit"] = json!(0);
    assert!(
        !validator.is_valid(&sink),
        "output_limit: 0 is refused by check_mcp naming the server; the schema's \
         minimum should refuse it too"
    );

    let mut sink: Value = serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");
    sink["zzz_schema_probe"] = json!(1);
    assert!(
        !validator.is_valid(&sink),
        "an unknown top-level key is a hard load error; additionalProperties: false \
         should refuse it too"
    );

    let mut sink: Value = serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");
    sink["mcp"]["auth"]["oauth"]["scope"] = json!("read");
    assert!(
        !validator.is_valid(&sink),
        "McpOauth is an empty struct with deny_unknown_fields; the schema's \
         additionalProperties: false on McpOauth should refuse an extra key too"
    );

    let mut sink: Value = serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");
    sink["tui"]["zzz_schema_probe"] = json!(1);
    assert!(
        !validator.is_valid(&sink),
        "the tui table is curated with deny_unknown_fields; the schema's \
         additionalProperties: false on TuiConfig should refuse an unknown key too"
    );

    let mut sink: Value = serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");
    sink["tui"]["notifications"] = json!(["turn-done"]);
    assert!(
        !validator.is_valid(&sink),
        "an event name nothing announces is refused by the loader naming it; the \
         schema's closed NotificationEvent enum should refuse it too"
    );

    let mut sink: Value = serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");
    sink["tui"]["notification_method"] = json!("toast");
    assert!(
        !validator.is_valid(&sink),
        "a method nothing sends is refused by the loader naming it; the schema's \
         closed NotificationMethod enum should refuse it too"
    );

    let mut sink: Value = serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");
    sink["tui"]["statusline"]["zzz_schema_probe"] = json!(1);
    assert!(
        !validator.is_valid(&sink),
        "the statusline table is curated with deny_unknown_fields; the schema's \
         additionalProperties: false on StatuslineConfig should refuse an unknown key too"
    );

    let mut sink: Value = serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");
    sink["tui"]["statusline"]["elements"] = json!(["contextbar"]);
    assert!(
        !validator.is_valid(&sink),
        "an element name nothing renders is refused by the loader naming it; the \
         schema's closed StatuslineElement enum should refuse it too"
    );

    // The near-misses of the one element P16 added (**D484**). `rate` itself
    // rides the kitchen sink above; these pin that widening the enum by one
    // name widened it by exactly one.
    for near_miss in ["ratelimit", "rate-limit", "rates"] {
        let mut sink: Value =
            serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");
        sink["tui"]["statusline"]["elements"] = json!([near_miss]);
        assert!(
            !validator.is_valid(&sink),
            "{near_miss:?} is not the name the loader accepts, and the schema must \
             not accept it either"
        );
    }
}

/// The `tui.notifications` key takes either spelling, and the kitchen sink
/// can only carry one of them (the list). The boolean form is validated here
/// so the schema's `anyOf` is exercised on both arms.
#[test]
fn the_boolean_notification_spelling_also_validates() {
    let validator = jsonschema::validator_for(&schema()).expect("the schema compiles");

    for spelling in [json!(true), json!(false)] {
        let mut sink: Value =
            serde_json::from_str(KITCHEN_SINK).expect("the fixture is valid JSON");
        sink["tui"]["notifications"] = spelling.clone();
        assert!(
            validator.is_valid(&sink),
            "tui.notifications takes a bare boolean: {spelling}"
        );
    }
}

/// The other half of
/// [`the_loader_refuses_a_non_loopback_mcp_url_that_the_schema_alone_would_accept`]:
/// a semantic refusal only [`check_mcp`] makes (the credential-travel rule on
/// an MCP URL) is outside what `format: "uri"` can express, so the schema
/// alone accepts the same document the loader refuses. Documented as a
/// passing assertion rather than a comment, so a future tightening of the
/// schema that closes this gap is a visible test change, not a silent one.
#[test]
fn the_schema_accepts_what_only_the_loader_can_refuse() {
    let validator = jsonschema::validator_for(&schema()).expect("the schema compiles");
    let instance = json!({
        "mcp": { "hub": { "type": "remote", "url": "http://example.com/mcp" } }
    });

    assert!(
        validator.is_valid(&instance),
        "the loopback rule is check_mcp's alone; the schema's format: \"uri\" \
         has no opinion about the host, on purpose"
    );
}
