use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::{
    AgentMode, AgentsConfig, Config, ConfigError, Dialect, DialogExpiry, HookCommand, HookHandler,
    HookMatcher, InboundPolicy, LspConfig, McpOauth, McpServer, NonZeroU64, NotificationEvent,
    NotificationMethod, Notifications, Overrides, ResponsesOptions, ServiceTier, StatuslineConfig,
    StatuslineElement, TeamlessSend, ThemeMode, existing, merge_files, model_bound_to,
    project_files, read, responses, split_model,
};
use crate::judge::Screen;
use crate::permission::{Action, Decision, Permissions, Rule};

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// Writes `text` to `path`, creating whatever directories it needs.
fn plant(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(path, text).expect("the fixture file is writable");
}

/// The rules `spelled` names, in the shape [`PermissionConfig::rules`]
/// hands them over — which is what the order assertions read in.
fn rules(spelled: &[(&str, &str, Action)]) -> Vec<Rule> {
    spelled
        .iter()
        .map(|(permission, pattern, action)| Rule {
            permission: (*permission).to_owned(),
            pattern: (*pattern).to_owned(),
            action: action.clone(),
        })
        .collect()
}

/// Parses `text` as a config file, the way discovery would.
fn parse(text: &str) -> Result<Config, ConfigError> {
    let directory = temporary();
    let path = directory.path().join("ganja.toml");
    plant(&path, text);

    read(&path).map(|config| config.expect("the fixture exists"))
}

/// A document is a table, so an empty one has no keys to miss, and every field
/// of `Config` is optional or defaulted — which is what makes a file holding
/// nothing a config rather than a type error.
#[test]
fn a_file_holding_nothing_is_an_empty_config_rather_than_an_error() {
    for text in ["", "   \n  ", "# nothing but a comment\n"] {
        assert_eq!(
            parse(text).expect("an empty config file is legal"),
            Config::default(),
            "parsing {text:?}"
        );
    }
}

/// The curated key set is serde's, so it survived the format change whole:
/// the refusal is the same refusal, and it still names the key.
#[test]
fn an_unknown_top_level_key_is_refused_by_name() {
    let error = parse(r#"modle = "anthropic/claude-sonnet-5""#)
        .expect_err("a misspelled key is a setting that does not work");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("modle"), "{message}");
}

/// A parse failure names what went wrong and where, and never the line it
/// happened on.
///
/// `toml::de::Error`'s own `Display` reproduces the offending line with a
/// caret under it, which here would mean printing back a line of somebody's
/// config — and an `mcp` entry's `headers` map is the one place a bearer token
/// is spelled, whose values this build withholds even from `ganja mcp get`.
/// The message is built from the accessors instead, so those bytes cannot ride
/// out through a terminal somebody is sharing or a log somebody keeps.
#[test]
fn a_parse_failure_names_the_position_and_never_the_line_it_happened_on() {
    let error = parse(concat!(
        "[mcp.vendor]\n",
        "type = \"remote\"\n",
        "url = \"https://example.test\"\n",
        "headers = { Authorization = \"Bearer NEVER-PRINT-ME }\n",
    ))
    .expect_err("an unterminated string is not a config");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("line 4"), "{message}");
    assert!(message.contains("column"), "{message}");
    assert!(!message.contains("NEVER-PRINT-ME"), "{message}");
}

/// `$schema` is not a bare key in TOML, so it is written quoted — and it is
/// still read, because refusing what an editor wrote would be a startup
/// failure about an annotation.
///
/// The second half is what a `.toml` file will actually carry: taplo takes its
/// schema from a `#:schema` directive on the first line, which is a comment
/// and reaches no parser at all. Both spellings load, and neither is consulted
/// by anything.
#[test]
fn a_quoted_schema_key_is_read_and_the_taplo_directive_is_just_a_comment() {
    let quoted = parse(r#""$schema" = "https://ganja.example/config.json""#)
        .expect("a quoted $schema key is legal");
    assert_eq!(quoted.schema.as_deref(), Some("https://ganja.example/config.json"));

    let directive = parse(
        "#:schema https://ganja.example/config.json\nmodel = \"anthropic/claude-sonnet-5\"\n",
    )
    .expect("a directive is a comment");
    assert_eq!(directive.schema, None);
    assert_eq!(directive.model.as_deref(), Some("anthropic/claude-sonnet-5"));
}

/// The one key here whose absence means *yes*. Upstream reads it as
/// `snapshot !== false`, so a config that never heard of it still snapshots
/// — which is what makes `/undo` work without anybody configuring it.
#[test]
fn snapshots_are_on_until_a_config_says_false() {
    let absent = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(absent.snapshot, None);
    assert!(absent.snapshots_enabled());

    let asked = parse(r#"snapshot = true"#).expect("it parses");
    assert_eq!(asked.snapshot, Some(true));
    assert!(asked.snapshots_enabled());

    let refused = parse(r#"snapshot = false"#).expect("it parses");
    assert_eq!(refused.snapshot, Some(false));
    assert!(!refused.snapshots_enabled());
}

/// A tier that says nothing about snapshots leaves the tier below it
/// alone; one that says `false` outranks a `true` above it.
#[test]
fn a_closer_tier_decides_snapshots_only_when_it_mentions_them() {
    let mut merged = parse(r#"snapshot = true"#).expect("it parses");
    merged.merge(parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses"));
    assert_eq!(merged.snapshot, Some(true));

    merged.merge(parse(r#"snapshot = false"#).expect("it parses"));
    assert_eq!(merged.snapshot, Some(false));
    assert!(!merged.snapshots_enabled());
}

/// And the key that reads the other way round: memory is **off** until a
/// config asks for it (**D478**), because switching it on adds standing
/// prompt weight and a door to write outside the worktree.
#[test]
fn memory_is_off_until_a_config_asks_for_it() {
    let absent = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(absent.memory, None);
    assert!(!absent.memory_enabled());

    let asked = parse(r#"memory = true"#).expect("it parses");
    assert_eq!(asked.memory, Some(true));
    assert!(asked.memory_enabled());

    // A tier that says nothing leaves the tier below it alone, and a
    // closer `false` still wins — the reason the field is an `Option`.
    let mut merged = asked;
    merged.merge(parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses"));
    assert!(merged.memory_enabled(), "silence is not a refusal");
    merged.merge(parse(r#"memory = false"#).expect("it parses"));
    assert!(!merged.memory_enabled());
}

/// The schema budget for MCP tools (**D492**): absent is 32, and the two
/// extremes both mean something — 0 defers every server, huge disables
/// deferral — so neither is refused.
#[test]
fn tool_defer_threshold_is_thirty_two_until_a_config_says_otherwise() {
    let absent = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(absent.tool_defer_threshold, None);
    assert_eq!(absent.defer_threshold(), 32);

    let zero = parse(r#"tool_defer_threshold = 0"#).expect("0 defers every server");
    assert_eq!(zero.defer_threshold(), 0);

    let huge = parse(r#"tool_defer_threshold = 100000"#).expect("a huge budget disables deferral");
    assert_eq!(huge.defer_threshold(), 100_000);

    // A tier that says nothing leaves the tier below it alone; a closer
    // tier's number wins.
    let mut merged = zero;
    merged.merge(parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses"));
    assert_eq!(merged.defer_threshold(), 0, "silence is not an opinion");
    merged.merge(parse(r#"tool_defer_threshold = 8"#).expect("it parses"));
    assert_eq!(merged.defer_threshold(), 8);
}

/// The auto-compaction percentage (**D566**): absent is 90 — the figure the
/// trigger carried as a constant before the key existed — and both ends of the
/// range mean something a person might want.
#[test]
fn auto_compact_threshold_is_ninety_until_a_config_says_otherwise() {
    let absent = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(absent.auto_compact_threshold, None);
    assert_eq!(absent.compact_threshold().percent(), 90);

    let half = parse(r#"auto_compact_threshold = 50"#).expect("half a budget is an answer");
    assert_eq!(half.compact_threshold().percent(), 50);

    let full = parse(r#"auto_compact_threshold = 100"#).expect("only when the budget is full");
    assert_eq!(full.compact_threshold().percent(), 100);

    let one = parse(r#"auto_compact_threshold = 1"#).expect("the other end of the range");
    assert_eq!(one.compact_threshold().percent(), 1);

    // A tier that says nothing leaves the tier below it alone; a closer
    // tier's number wins.
    let mut merged = half;
    merged.merge(parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses"));
    assert_eq!(merged.compact_threshold().percent(), 50, "silence is not an opinion");
    merged.merge(parse(r#"auto_compact_threshold = 70"#).expect("it parses"));
    assert_eq!(merged.compact_threshold().percent(), 70);
}

/// Outside `1..=100` the key is refused **by name**, both ends of it: zero
/// compacts a conversation that has not happened, and past a hundred is
/// auto-compaction off wearing a number.
#[test]
fn an_auto_compact_threshold_outside_the_range_is_refused_by_name() {
    for wrong in [
        r#"auto_compact_threshold = 0"#,
        r#"auto_compact_threshold = 101"#,
        r#"auto_compact_threshold = 1000"#,
    ] {
        let error = parse(wrong).expect_err("a percentage is 1 to 100");
        let message = error.to_string();
        assert!(message.contains("auto_compact_threshold"), "the refusal names the key: {message}");
        assert!(message.contains("between 1 and 100"), "and the range it takes: {message}");
    }

    // A string, a float or a negative is serde's own positioned type refusal,
    // exactly as it is for `tool_defer_threshold` beside it.
    for wrong in [
        r#"auto_compact_threshold = "most""#,
        r#"auto_compact_threshold = 90.5"#,
        r#"auto_compact_threshold = -1"#,
    ] {
        let error = parse(wrong).expect_err("a percentage is an unsigned integer");
        assert!(
            error.to_string().contains("expected u64"),
            "the refusal says what the key takes: {error}"
        );
    }
}

/// The key takes a count and nothing else: a string, a float or a
/// negative is serde's own type refusal — positioned, like every curated
/// scalar's here; it is unknown *keys* that are refused by name.
#[test]
fn a_tool_defer_threshold_that_is_not_a_count_is_refused() {
    for wrong in [
        r#"tool_defer_threshold = "many""#,
        r#"tool_defer_threshold = 1.5"#,
        r#"tool_defer_threshold = -1"#,
    ] {
        let error = parse(wrong).expect_err("a threshold is an unsigned integer");
        assert!(
            error.to_string().contains("expected usize"),
            "the refusal says what the key takes: {error}"
        );
    }
}

/// Claude's own block, pasted whole: the shape is kept so that it can be.
#[test]
fn a_hooks_block_parses_into_its_groups_and_handlers() {
    let config = parse(
        r#"
            [[hooks.PreToolUse]]
            matcher = "Edit|Write"
            hooks = [
              { type = "command", command = "./check.sh", timeout = 5 },
              { type = "command", command = "./log.sh" },
            ]

            [[hooks.SessionStart]]
            hooks = [{ type = "command", command = "git status" }]
        "#,
    )
    .expect("the documented shape parses");

    let pre = &config.hooks["PreToolUse"];
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0].matcher.as_deref(), Some("Edit|Write"));
    assert_eq!(
        pre[0].hooks,
        vec![
            HookHandler::Command(HookCommand {
                command: "./check.sh".to_owned(),
                timeout: Some(5),
            }),
            HookHandler::Command(HookCommand { command: "./log.sh".to_owned(), timeout: None }),
        ]
    );
    // An absent matcher is the common case and stays absent rather than
    // becoming an empty string that means the same thing in one more way.
    assert_eq!(config.hooks["SessionStart"][0].matcher, None);
}

/// The same block in TOML, where a list of objects is an **array of tables**
/// and the nested list of handlers is a second one under it. The type is
/// unchanged, so this is a test about the spelling — and the spelling is the
/// thing a person migrating a `hooks` block has to get right, since it is the
/// one shape in the file that does not read as a plain assignment.
#[test]
fn a_toml_hooks_block_is_an_array_of_tables_holding_the_same_groups() {
    let config = parse(
        r#"
            [[hooks.PreToolUse]]
            matcher = "Edit|Write"

            [[hooks.PreToolUse.hooks]]
            type = "command"
            command = "./check.sh"
            timeout = 5

            [[hooks.PreToolUse.hooks]]
            type = "command"
            command = "./log.sh"

            [[hooks.SessionStart]]

            [[hooks.SessionStart.hooks]]
            type = "command"
            command = "git status"
        "#,
    )
    .expect("the array-of-tables shape parses");

    let pre = &config.hooks["PreToolUse"];
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0].matcher.as_deref(), Some("Edit|Write"));
    assert_eq!(
        pre[0].hooks,
        vec![
            HookHandler::Command(HookCommand {
                command: "./check.sh".to_owned(),
                timeout: Some(5),
            }),
            HookHandler::Command(HookCommand { command: "./log.sh".to_owned(), timeout: None }),
        ]
    );
    assert_eq!(config.hooks["SessionStart"][0].matcher, None);
}

#[test]
fn an_unknown_hook_event_is_refused_by_name() {
    let error =
        parse(r#"hooks = { PreToolUsage = [{ hooks = [{ type = "command", command = "x" }] }] }"#)
            .expect_err("a hook that never fires is worse than one that fails");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("PreToolUsage"), "{message}");
    assert!(
        message.contains("PreToolUse") && message.contains("PreCompact"),
        "the useful half of \"no such event\" is which ones there are: {message}"
    );
}

#[test]
fn an_unknown_hook_handler_type_is_refused_by_name() {
    let error = parse(r#"hooks = { Stop = [{ hooks = [{ type = "webhook", url = "x" }] }] }"#)
        .expect_err("this build runs command handlers and says so");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("webhook") && message.contains("command"), "{message}");
}

#[test]
fn a_hook_handler_with_no_command_is_refused() {
    for text in [
        r#"hooks = { Stop = [{ hooks = [{ type = "command", command = "" }] }] }"#,
        r#"hooks = { Stop = [{ hooks = [{ type = "command", command = "   " }] }] }"#,
    ] {
        let error = parse(text).expect_err("a handler with nothing to run is not one");
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("no command"), "{message}");
    }
}

/// A matcher that is not a regular expression would match nothing, forever,
/// without saying so — which is the one failure mode a config check exists
/// for.
#[test]
fn a_matcher_that_is_not_a_regular_expression_is_refused() {
    let error = parse(
            r#"hooks = { PreToolUse = [{ matcher = "(unclosed", hooks = [{ type = "command", command = "x" }] }] }"#,
        )
        .expect_err("a matcher nothing can compile is a group that never fires");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("PreToolUse"), "{message}");
}

/// The singular key is a map of definitions and the plural one is a
/// settings object; a config may write either without the other, and a
/// concurrency nobody named is the documented default.
#[test]
fn agents_is_the_settings_object_beside_the_agent_map() {
    let config = parse(r#"agent = { plan = { description = "plans" } }"#).expect("it parses");
    assert_eq!(
        config.agents.concurrency(),
        AgentsConfig::DEFAULT_CONCURRENCY,
        "absent is the default, not zero"
    );

    let config = parse(r#"agents = { concurrency = 1 }"#).expect("it parses");
    assert_eq!(config.agents.concurrency(), 1, "one is upstream's behavior, asked for on purpose");
    assert!(config.agent.is_empty(), "and it defined no agents while doing it");
}

/// Zero is the one value whose consequence is a batch that never starts,
/// and it is invisible until a turn delegates.
#[test]
fn a_concurrency_of_zero_is_refused_by_name() {
    let error =
        parse(r#"agents = { concurrency = 0 }"#).expect_err("a cap of nothing is not a cap");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("agents.concurrency"), "{message}");
}

/// The plural is the settings object here too, and an absent key leaves
/// the per-CLI defaults — which live in the shim, not in this file —
/// alone.
#[test]
fn teammates_carries_the_one_deadline_a_person_can_move() {
    let config = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(
        config.teammates.shim_turn_timeout(),
        None,
        "absent leaves each CLI's own default alone"
    );

    let config = parse(r#"teammates = { shim_turn_timeout = 90 }"#).expect("it parses");
    assert_eq!(
        config.teammates.shim_turn_timeout(),
        Some(std::time::Duration::from_secs(90)),
        "the key is seconds, like a hook's timeout"
    );

    let error = parse(r#"teammates = { shim_turn_timeout = 0 }"#)
        .expect_err("a deadline of nothing is not a deadline");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("teammates.shim_turn_timeout"), "{message}");

    let error = parse(r#"teammates = { shim_turn_timout = 90 }"#)
        .expect_err("a misspelled key is refused rather than ignored");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("shim_turn_timout"), "{message}");
}

/// **D556.** The one key of the `claude_code` table is seconds, absent is
/// 600, a bound of nothing is refused by name, a misspelling is refused by
/// name too, and a later tier's number wins over an earlier one's while
/// silence leaves it alone — the five things D556 asks of it.
#[test]
fn claude_code_idle_bound_is_seconds_defaults_to_ten_minutes_and_is_refused_at_zero() {
    let absent = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(
        absent.claude_code.idle_bound(),
        std::time::Duration::from_secs(600),
        "absent is the wire's own default, spelled once in the wire"
    );

    let named = parse(r#"claude_code = { idle_bound = 900 }"#).expect("it parses");
    assert_eq!(
        named.claude_code.idle_bound(),
        std::time::Duration::from_secs(900),
        "the key is seconds, like a teammate's turn deadline"
    );

    let error = parse(r#"claude_code = { idle_bound = 0 }"#)
        .expect_err("a bound of nothing evicts every process before its first turn");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("claude_code.idle_bound"), "{message}");

    let error = parse(r#"claude_code = { idle_bounds = 900 }"#)
        .expect_err("a misspelled key is refused rather than ignored");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("idle_bounds"), "{message}");

    let mut merged = named;
    merged.merge(parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses"));
    assert_eq!(
        merged.claude_code.idle_bound(),
        std::time::Duration::from_secs(900),
        "silence is not an opinion"
    );
    merged.merge(parse(r#"claude_code = { idle_bound = 30 }"#).expect("it parses"));
    assert_eq!(merged.claude_code.idle_bound(), std::time::Duration::from_secs(30));
}

/// **D520.** `teammates.shell` is a command line split as a shell would
/// split it; absent leaves the pane door's `/bin/sh -s` alone, and a
/// value that is nothing is refused rather than spawning into nothing.
/// The column's share is a percentage the lead's side is the rest of:
/// absent leaves the door's default, a value inside 1..=99 is carried,
/// and either edge is refused by name — a split that gives one side
/// nothing is no split.
#[test]
fn teammates_pane_share_is_a_percentage_and_refused_at_either_edge() {
    let config = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(config.teammates.pane_share(), None);

    let config = parse(r#"teammates = { pane_share = 60 }"#).expect("it parses");
    assert_eq!(config.teammates.pane_share(), Some(60));

    for edge in [r#"teammates = { pane_share = 0 }"#, r#"teammates = { pane_share = 100 }"#] {
        let error = parse(edge).expect_err("a column of nothing or everything is refused");
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("teammates.pane_share"), "{message}");
    }
}

#[test]
fn teammates_shell_is_a_command_line_and_nothing_is_refused() {
    let config = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(config.teammates.pane_shell(), None);

    let config = parse(r#"teammates = { shell = "/bin/zsh -f" }"#).expect("it parses");
    assert_eq!(config.teammates.pane_shell(), Some(vec!["/bin/zsh".to_owned(), "-f".to_owned()]));

    let config = parse(r#"teammates = { shell = "'/opt/my shell/zsh'" }"#).expect("it parses");
    assert_eq!(
        config.teammates.pane_shell(),
        Some(vec!["/opt/my shell/zsh".to_owned()]),
        "quoting is a shell's own"
    );

    for empty in [r#"teammates = { shell = "" }"#, r#"teammates = { shell = "   " }"#] {
        let error = parse(empty).expect_err("a shell of nothing is refused");
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("teammates.shell"), "{message}");
    }
    let error = parse(r#"teammates = { shell = "/bin/zsh '" }"#)
        .expect_err("an unbalanced quote cannot be split");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("teammates.shell"), "{message}");
}

#[test]
fn cross_session_inbound_parses_its_three_values_and_absent_is_unset() {
    let config = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(config.cross_session_inbound, None, "absent is unset, not a fourth policy");

    for (spelled, parsed) in [
        ("accept", InboundPolicy::Accept),
        ("hold", InboundPolicy::Hold),
        ("refuse", InboundPolicy::Refuse),
    ] {
        let config = parse(&format!(r#"cross_session_inbound = "{spelled}""#))
            .expect("a policy this build has is a config value");
        assert_eq!(config.cross_session_inbound, Some(parsed));
    }
}

/// The refusal names the key — the whole reason the type deserializes by
/// hand — and lists the vocabulary, on a wrong string and a wrong type
/// alike.
#[test]
fn an_inbound_policy_nothing_admits_is_refused_naming_the_key() {
    for bogus in [r#"cross_session_inbound = "sometimes""#, r#"cross_session_inbound = true"#] {
        let error = parse(bogus).expect_err("a policy nothing admits is refused");
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("cross_session_inbound"), "{message}");
        for admitted in ["accept", "hold", "refuse"] {
            assert!(
                message.contains(admitted),
                "and the refusal lists what would have worked: {message}"
            );
        }
    }
}

#[test]
fn dialog_expiry_parses_its_four_values_and_absent_is_five_minutes() {
    let config = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(config.dialog_expiry, None, "absence travels to the merge");
    assert_eq!(config.dialog_expiry(), DialogExpiry::FiveMinutes, "and reads as the default");

    for (spelled, parsed, seconds) in [
        ("60s", DialogExpiry::OneMinute, Some(60)),
        ("5m", DialogExpiry::FiveMinutes, Some(300)),
        ("10m", DialogExpiry::TenMinutes, Some(600)),
        ("never", DialogExpiry::Never, None),
    ] {
        let config = parse(&format!(r#"dialog_expiry = "{spelled}""#))
            .expect("a window this build has is a config value");
        assert_eq!(config.dialog_expiry, Some(parsed));
        assert_eq!(
            config.dialog_expiry().deadline().map(|deadline| deadline.as_secs()),
            seconds,
            "{spelled} and its wall-clock meaning travel together"
        );
    }

    let error = parse(r#"dialog_expiry = "90s""#).expect_err("a window nothing times is refused");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("dialog_expiry"), "{message}");
    assert!(message.contains("never"), "and the refusal lists what would have worked: {message}");
}

#[test]
fn teamless_send_parses_its_two_values_and_absent_is_unasked() {
    let config = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    assert_eq!(config.teamless_send, None, "absence travels to the merge");
    assert_eq!(config.teamless_send(), TeamlessSend::Unasked, "and reads as the default");

    for (spelled, parsed) in [("unasked", TeamlessSend::Unasked), ("ask", TeamlessSend::Ask)] {
        let config = parse(&format!(r#"teamless_send = "{spelled}""#))
            .expect("a posture this build has is a config value");
        assert_eq!(config.teamless_send, Some(parsed));
    }
}

/// The refusal names the key — the whole reason the type deserializes by
/// hand — and lists the vocabulary, on a wrong string and a wrong type
/// alike.
#[test]
fn a_teamless_send_nothing_admits_is_refused_naming_the_key() {
    for bogus in [r#"teamless_send = "always""#, r#"teamless_send = true"#] {
        let error = parse(bogus).expect_err("a posture nothing admits is refused");
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("teamless_send"), "{message}");
        for admitted in ["unasked", "ask"] {
            assert!(
                message.contains(admitted),
                "and the refusal lists what would have worked: {message}"
            );
        }
    }
}

/// The merge seam of the one deliberate divergence from later-wins: a
/// project file replaces the standing policy only when strictly more
/// severe, while the trusted tiers keep the ordinary overlay between
/// themselves. `tests/config_inbound_tiers.rs` proves the same through
/// the real loader, where the environment that orders the tiers can be
/// set.
#[test]
fn a_project_file_can_tighten_but_never_loosen() {
    for (standing, project, expected) in [
        ("accept", "refuse", InboundPolicy::Refuse),
        ("refuse", "accept", InboundPolicy::Refuse),
        ("hold", "accept", InboundPolicy::Hold),
        ("hold", "refuse", InboundPolicy::Refuse),
        ("refuse", "refuse", InboundPolicy::Refuse),
    ] {
        let mut merged = parse(&format!(r#"cross_session_inbound = "{standing}""#))
            .expect("the trusted tier parses");
        merged
            .merge_project(
                parse(&format!(r#"cross_session_inbound = "{project}""#))
                    .expect("the project tier parses"),
                Path::new("ganja.toml"),
            )
            .expect("tightening is never an error");
        assert_eq!(
            merged.cross_session_inbound,
            Some(expected),
            "{standing} under a project {project}"
        );
    }

    // An unset standing value has nothing to loosen: the first file to
    // say anything establishes the policy.
    let mut merged = parse("").expect("an empty config parses");
    merged
        .merge_project(
            parse(r#"cross_session_inbound = "accept""#).expect("it parses"),
            Path::new("ganja.toml"),
        )
        .expect("establishing a value is not loosening one");
    assert_eq!(merged.cross_session_inbound, Some(InboundPolicy::Accept));

    // Between trusted tiers the key keeps later-wins — the explicit
    // `GANJA_CONFIG` file outranks the global one by merge order, in the
    // loosening direction too, because both are the person's own files.
    let mut merged = parse(r#"cross_session_inbound = "refuse""#).expect("it parses");
    merged.merge(parse(r#"cross_session_inbound = "accept""#).expect("it parses"));
    assert_eq!(merged.cross_session_inbound, Some(InboundPolicy::Accept));

    // And every other key still merges ordinarily on the project path.
    let mut merged = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");
    merged
        .merge_project(
            parse(r#"model = "openai/gpt-5.6""#).expect("it parses"),
            Path::new("ganja.toml"),
        )
        .expect("an ordinary key is no error");
    assert_eq!(merged.model.as_deref(), Some("openai/gpt-5.6"));
}

/// The same seam for the sender-side sibling (**D531**): the project
/// tier tightens on `unasked (0) < ask (1)` through the one `tighten`
/// spelling `cross_session_inbound` merges under, and the trusted tiers
/// keep later-wins between themselves — which is what makes the file
/// `GANJA_CONFIG` names outrank the global tier, in both directions,
/// because both are the person's own files.
#[test]
fn a_project_file_can_tighten_teamless_send_but_never_loosen() {
    // A project `ask` lands over an absent standing value: the first
    // tier to say anything establishes the posture.
    let mut merged = parse("").expect("an empty config parses");
    merged
        .merge_project(
            parse(r#"teamless_send = "ask""#).expect("the project tier parses"),
            Path::new("ganja.toml"),
        )
        .expect("establishing a value is not loosening one");
    assert_eq!(merged.teamless_send, Some(TeamlessSend::Ask));

    // And over an explicit global `unasked`: unasked → ask is the
    // tightening direction, a checkout demanding more oversight.
    let mut merged = parse(r#"teamless_send = "unasked""#).expect("it parses");
    merged
        .merge_project(
            parse(r#"teamless_send = "ask""#).expect("it parses"),
            Path::new("ganja.toml"),
        )
        .expect("tightening is never an error");
    assert_eq!(merged.teamless_send, Some(TeamlessSend::Ask));

    // A project `unasked` over a person's `ask` silently fails to
    // loosen — the same mechanism, not a refusal.
    let mut merged = parse(r#"teamless_send = "ask""#).expect("it parses");
    merged
        .merge_project(
            parse(r#"teamless_send = "unasked""#).expect("it parses"),
            Path::new("ganja.toml"),
        )
        .expect("a loosening project value is ignored, never an error");
    assert_eq!(merged.teamless_send, Some(TeamlessSend::Ask));

    // Between trusted tiers the key keeps later-wins — the explicit
    // `GANJA_CONFIG` file outranks the global one by merge order, in
    // the loosening direction too, because both are the person's own
    // files.
    let mut merged = parse(r#"teamless_send = "ask""#).expect("it parses");
    merged.merge(parse(r#"teamless_send = "unasked""#).expect("it parses"));
    assert_eq!(merged.teamless_send, Some(TeamlessSend::Unasked));
}

/// The other divergence at the same seam: the error names the key and
/// the file, and the same value stays a trusted tier's to set.
#[test]
fn a_project_dialog_expiry_is_refused_by_name() {
    let mut merged = parse("").expect("an empty config parses");
    let project = parse(r#"dialog_expiry = "60s""#).expect("the value itself parses");

    let error = merged
        .merge_project(project, Path::new("/checkout/ganja.toml"))
        .expect_err("a checkout must not size the review window");
    let ConfigError::Parse { path, message } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert_eq!(path, Path::new("/checkout/ganja.toml"));
    assert!(message.contains("dialog_expiry"), "{message}");

    // The same key between trusted tiers is an ordinary overlay.
    let mut merged = parse(r#"dialog_expiry = "10m""#).expect("it parses");
    merged.merge(parse(r#"dialog_expiry = "60s""#).expect("it parses"));
    assert_eq!(merged.dialog_expiry(), DialogExpiry::OneMinute);
}

/// `true` is both moments and `false` is none — the same answer absent
/// gives — so a config only ever writes the boolean to say something.
#[test]
fn a_boolean_notifications_key_switches_both_moments_at_once() {
    let config = parse(r#"tui = { notifications = true }"#).expect("it parses");
    assert!(config.tui.notifies(NotificationEvent::TurnComplete));
    assert!(config.tui.notifies(NotificationEvent::ApprovalRequested));

    let config = parse(r#"tui = { notifications = false }"#).expect("it parses");
    assert!(!config.tui.notifies(NotificationEvent::TurnComplete));
    assert!(!config.tui.notifies(NotificationEvent::ApprovalRequested));

    let config = parse("").expect("it parses");
    assert!(!config.tui.notifies(NotificationEvent::TurnComplete), "absent is none of them");
}

/// The list form is a selection, not a switch: only the moments it names
/// are announced.
#[test]
fn a_notification_list_names_exactly_the_moments_it_announces() {
    let config = parse(r#"tui = { notifications = ["turn-complete"] }"#).expect("it parses");
    assert!(config.tui.notifies(NotificationEvent::TurnComplete));
    assert!(!config.tui.notifies(NotificationEvent::ApprovalRequested));
}

/// A misspelled event would be a notification that never fires, silently —
/// `check_hooks`'s argument, applied to a smaller vocabulary.
#[test]
fn an_event_name_nothing_announces_is_refused_by_name() {
    let error = parse(r#"tui = { notifications = ["turn-done"] }"#)
        .expect_err("an event nothing announces is refused");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("turn-done"), "{message}");
    assert!(
        message.contains("turn-complete"),
        "and the refusal lists what would have worked: {message}"
    );
}

/// The method vocabulary is as closed as the event one.
#[test]
fn a_notification_method_nothing_sends_is_refused_by_name() {
    let error = parse(r#"tui = { notification_method = "toast" }"#)
        .expect_err("a method nothing sends is refused");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("toast"), "{message}");
    assert!(message.contains("osc9"), "and the refusal lists what would have worked: {message}");
}

/// The `tui` table is curated like the top level: a key it does not have
/// is a setting that would silently not work.
#[test]
fn a_key_the_tui_table_does_not_have_is_refused_by_name() {
    let error =
        parse(r#"tui = { zzz_probe = 1 }"#).expect_err("an unknown key inside tui is refused");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("zzz_probe"), "{message}");
}

/// The gateway's own tools, opted into by name (**D489**) — and the two
/// ways to get the key wrong, both refused where somebody can read the
/// refusal instead of a mid-turn 400.
#[test]
fn the_openrouter_table_takes_a_roster_and_refuses_a_name_outside_it() {
    let config = parse(r#"openrouter = { server_tools = ["web_search", "datetime"] }"#)
        .expect("a roster of published names parses");
    assert_eq!(config.openrouter.server_tools, ["web_search", "datetime"]);

    // Absent is empty, which is the whole opt-in: they bill per call.
    assert!(parse("").expect("an empty config parses").openrouter.server_tools.is_empty());

    let error = parse(r#"openrouter = { server_tools = ["web_serach"] }"#)
        .expect_err("a name this gateway does not serve is refused");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("web_serach"), "{message}");
    assert!(
        message.contains("web_search") && message.contains("experimental__search_models"),
        "and the refusal lists what would have worked: {message}"
    );

    // The table is curated like every other one here.
    let error = parse(r#"openrouter = { zzz_probe = 1 }"#)
        .expect_err("an unknown key inside openrouter is refused");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("zzz_probe"), "{message}");
}

/// Between the person's own files the roster is later-wins, replaced whole
/// like every list here but `instructions`: the explicit tier outranks the
/// global one, and a tier that says nothing about the table keeps the one
/// below it.
#[test]
fn a_closer_trusted_tier_replaces_the_openrouter_server_tools() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    let explicit = directory.path().join("explicit.toml");
    let silent = directory.path().join("silent.toml");
    plant(&global, r#"openrouter = { server_tools = ["web_search", "datetime"] }"#);
    plant(&explicit, r#"openrouter = { server_tools = ["web_fetch"] }"#);
    plant(&silent, r#"model = "openrouter/anthropic/claude-3""#);

    let config = merge_files(std::slice::from_ref(&global)).expect("the global tier parses");
    assert_eq!(
        config.openrouter.server_tools,
        ["web_search", "datetime"],
        "a single tier's roster reaches the merged config"
    );

    let config = merge_files(&[global.clone(), explicit]).expect("both tiers parse");
    assert_eq!(config.openrouter.server_tools, ["web_fetch"], "the closer trusted tier replaces");

    let config = merge_files(&[global, silent]).expect("both tiers parse");
    assert_eq!(
        config.openrouter.server_tools,
        ["web_search", "datetime"],
        "a tier silent on the table keeps the one below it"
    );
}

/// A checkout's file is not the person's: the tools on this list run on
/// the gateway and bill per call, so a project file may only narrow what
/// the trusted tiers granted — never add to it, never fail the load.
#[test]
fn a_project_file_can_only_narrow_the_openrouter_server_tools() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    let project = directory.path().join("ganja.toml");
    plant(&global, r#"openrouter = { server_tools = ["web_search", "datetime"] }"#);

    for (listed, expected) in [
        // A subset narrows.
        (r#"["datetime"]"#, vec!["datetime"]),
        // A name the trusted tiers did not grant is dropped, not added.
        (r#"["datetime", "shell"]"#, vec!["datetime"]),
        // Nothing in common leaves nothing.
        (r#"["shell"]"#, vec![]),
        // Naming everything granted, and more, changes nothing but the extra.
        (r#"["web_search", "datetime", "image_generation"]"#, vec!["web_search", "datetime"]),
    ] {
        plant(&project, &format!("openrouter = {{ server_tools = {listed} }}"));
        let mut merged =
            merge_files(std::slice::from_ref(&global)).expect("the global tier parses");
        merged
            .merge_project(
                read(&project).expect("the project file parses").expect("it exists"),
                &project,
            )
            .expect("no valid project value is fatal");
        assert_eq!(merged.openrouter.server_tools, expected, "project lists {listed}");
    }

    // A project file silent on the table leaves the grant alone.
    plant(&project, r#"model = "openrouter/anthropic/claude-3""#);
    let mut merged = merge_files(&[global]).expect("the global tier parses");
    merged
        .merge_project(read(&project).expect("it parses").expect("it exists"), &project)
        .expect("an ordinary key is no error");
    assert_eq!(merged.openrouter.server_tools, ["web_search", "datetime"]);

    // With nothing granted, a project cannot turn anything on.
    plant(&project, r#"openrouter = { server_tools = ["shell", "web_fetch"] }"#);
    let mut merged = Config::default();
    merged
        .merge_project(read(&project).expect("it parses").expect("it exists"), &project)
        .expect("an ungranted roster is dropped, never an error");
    assert!(merged.openrouter.server_tools.is_empty(), "{:?}", merged.openrouter.server_tools);
}

/// Narrowing is never silent: every name dropped on either side — a granted
/// tool the project file left out, and a listed tool nobody granted — gets
/// exactly one warning that names the tool and the file that dropped it.
#[test]
fn each_openrouter_server_tool_a_project_file_drops_is_warned_once() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    let project = directory.path().join("ganja.toml");
    plant(&global, r#"openrouter = { server_tools = ["web_search", "datetime"] }"#);
    plant(&project, r#"openrouter = { server_tools = ["datetime", "web_fetch"] }"#);
    let mut merged = merge_files(&[global]).expect("the global tier parses");

    let (capture, _guard) = ganja_testkit::LogCapture::install(tracing::Level::WARN);
    merged
        .merge_project(read(&project).expect("it parses").expect("it exists"), &project)
        .expect("narrowing is never fatal");

    assert_eq!(merged.openrouter.server_tools, ["datetime"]);
    let logged = capture.logged();
    let lines: Vec<&str> =
        logged.lines().filter(|line| line.contains("openrouter.server_tools")).collect();
    assert_eq!(lines.len(), 2, "one warning per dropped name, no more: {logged}");
    let path = project.display().to_string();
    for tool in ["web_search", "web_fetch"] {
        let naming: Vec<&&str> =
            lines.iter().filter(|line| line.contains(&format!("tool={tool}"))).collect();
        assert_eq!(naming.len(), 1, "exactly one warning names {tool}: {logged}");
        assert!(naming[0].contains(&path), "the warning for {tool} names the file: {logged}");
    }
    assert!(!logged.contains("tool=datetime"), "a kept tool is not warned about: {logged}");
}

/// The screen a test expects, spelled the way the grammar spells its three
/// kinds of source.
fn screening(webfetch: bool, websearch: bool, mcp: &[&str]) -> Screen {
    Screen { webfetch, websearch, mcp: mcp.iter().map(|server| (*server).to_owned()).collect() }
}

/// Overlays the project file at `path` onto `config`, the call
/// `Config::load_with` makes for every project file its walk finds.
fn merge_project_file(config: &mut Config, path: &Path) {
    config
        .merge_project(read(path).expect("the project file parses").expect("it exists"), path)
        .expect("no valid project value is fatal");
}

/// What an `evaluate.screen` warning says when this project file's own `mcp`
/// table took the entry off the screen.
const REMOVED: &str = "evaluate.screen: this project file defines or redefines this server, so \
                       the entry is removed";
/// What it says when the file's list left out a source the screen held.
const NARROWED: &str = "evaluate.screen: this project file narrows the screened source away";
/// What it says when the file's list named a source the screen did not hold.
const NOT_IN_SCREEN: &str = "evaluate.screen: a project file may only narrow the screen the \
                             tiers above it left; this source is not in it and is ignored";

/// Asserts that `logged` holds exactly the `evaluate.screen` warnings
/// `expected` lists, in order — each one's text, the entry it names and the
/// file — so a warning that says the wrong thing fails as surely as a
/// missing one.
fn assert_screen_warnings(logged: &str, expected: &[(&str, &str, &Path)]) {
    let lines: Vec<&str> = logged.lines().filter(|line| line.contains("evaluate.screen")).collect();
    assert_eq!(lines.len(), expected.len(), "one warning per dropped entry: {logged}");
    for (line, (says, entry, path)) in lines.iter().zip(expected) {
        assert!(line.contains(says), "the warning for {entry} says {says:?}: {line}");
        assert!(line.contains(&format!("entry={entry:?}")), "it names {entry}: {line}");
        assert!(line.contains(&format!("path={path:?}")), "it names the file: {line}");
    }
}

/// **D567**, criterion 7: every shape the grammar accepts resolves to the
/// source it names, a colon inside an MCP server's name included — which is
/// how a plugin's server is named. Default off holds by construction: no
/// table, a table with no list, and an empty list all screen nothing.
#[test]
fn a_trusted_screen_list_resolves_to_the_sources_it_names() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    plant(
        &global,
        "[evaluate]\nscreen = [\"webfetch\", \"websearch\", \"mcp:github\", \"mcp:plugin:foo:bar\"]\n",
    );

    let config = merge_files(std::slice::from_ref(&global)).expect("the global tier parses");
    assert_eq!(
        config.evaluate_screen(),
        screening(true, true, &["github", "plugin:foo:bar"]),
        "each entry reaches the resolved screen"
    );

    for text in ["", "[evaluate]\n", "[evaluate]\nscreen = []\n"] {
        plant(&global, text);
        let config = merge_files(std::slice::from_ref(&global)).expect("the global tier parses");
        assert_eq!(config.evaluate_screen(), Screen::default(), "{text:?} screens nothing");
    }
}

/// **D567**, criterion 8: an entry outside the grammar, and an entry named
/// twice, fail the load — naming the key, the entry and the shapes that
/// would have worked, `check_openrouter`'s shape. A wildcard is outside the
/// grammar wherever it stands, and the refusal says there is none; so is a
/// control character in a server's name, which the narrowing warnings would
/// otherwise write into the person's log raw — a newline there forges a line
/// of ganja's own. A colon inside a server's name is neither.
#[test]
fn a_screen_entry_outside_the_grammar_or_named_twice_is_refused_with_the_accepted_shapes() {
    const OUTSIDE: &str = "there is no wildcard, so name each server";
    const TWICE: &str = "twice; name each source once";
    let forged = "mcp:x\n2026-09-23T04:00:00.000000Z  WARN ganja_core::config: forged line";
    for (list, entry, says) in [
        (r#"["web"]"#, "web", OUTSIDE),
        (r#"["mcp:"]"#, "mcp:", OUTSIDE),
        (r#"["mcp"]"#, "mcp", OUTSIDE),
        (r#"["WebFetch"]"#, "WebFetch", OUTSIDE),
        (r#"["*"]"#, "*", OUTSIDE),
        (r#"["mcp:*"]"#, "mcp:*", OUTSIDE),
        (r#"["mcp:github-*"]"#, "mcp:github-*", OUTSIDE),
        (
            r#"["mcp:x\n2026-09-23T04:00:00.000000Z  WARN ganja_core::config: forged line"]"#,
            forged,
            OUTSIDE,
        ),
        // C1, the other half of what `char::is_control` and the schema's
        // pattern both call a control character.
        (r#"["mcp:a\u0085b"]"#, "mcp:a\u{85}b", OUTSIDE),
        (r#"["websearch", "webfetch", "websearch"]"#, "websearch", TWICE),
        (r#"["mcp:github", "mcp:github"]"#, "mcp:github", TWICE),
    ] {
        let error = parse(&format!("[evaluate]\nscreen = {list}\n"))
            .expect_err("the entry is refused at load");
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure for {list}, got {error:?}");
        };
        assert!(message.contains("evaluate.screen"), "{list}: the key is named: {message}");
        assert!(message.contains(&format!("{entry:?}")), "{list}: the entry is named: {message}");
        assert!(
            message.contains(r#""webfetch", "websearch", or "mcp:<server>""#),
            "{list}: the accepted shapes are listed: {message}"
        );
        assert!(message.contains(says), "{list}: the refusal says {says:?}: {message}");
        assert!(!message.contains('\n'), "{list}: the refusal is one line: {message}");
    }

    let config = parse("[evaluate]\nscreen = [\"mcp:plugin:foo:bar\"]\n")
        .expect("a colon inside a server's name is part of the name");
    assert_eq!(config.evaluate_screen(), screening(false, false, &["plugin:foo:bar"]));

    let error = parse("[evaluate]\nzzz_probe = 1\n").expect_err("the table is curated");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("zzz_probe"), "{message}");
}

/// **D567**, criterion 9: between the person's own files the list is
/// later-wins and replaced whole, and an empty list is how a closer trusted
/// tier switches screening off — while a tier silent on the table keeps the
/// one below it.
#[test]
fn a_closer_trusted_tier_replaces_the_screen_and_an_empty_list_switches_it_off() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    let explicit = directory.path().join("explicit.toml");
    plant(&global, "[evaluate]\nscreen = [\"webfetch\"]\n");

    plant(&explicit, "[evaluate]\nscreen = [\"websearch\"]\n");
    let config = merge_files(&[global.clone(), explicit.clone()]).expect("both tiers parse");
    assert_eq!(config.evaluate_screen(), screening(false, true, &[]), "the closer tier replaces");

    plant(&explicit, "[evaluate]\nscreen = []\n");
    let config = merge_files(&[global.clone(), explicit.clone()]).expect("both tiers parse");
    assert_eq!(config.evaluate_screen(), Screen::default(), "an empty list is off");

    plant(&explicit, "model = \"openai/gpt-5.6\"\n");
    let config = merge_files(&[global, explicit]).expect("both tiers parse");
    assert_eq!(
        config.evaluate_screen(),
        screening(true, false, &[]),
        "a tier silent on the table keeps the one below it"
    );
}

/// **D567**, criterion 10: a project file may only narrow what the trusted
/// tiers named — never add a source, never fail the load.
#[test]
fn a_project_file_can_only_narrow_the_screen() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    let project = directory.path().join("ganja.toml");
    plant(&global, "[evaluate]\nscreen = [\"webfetch\", \"websearch\", \"mcp:github\"]\n");

    for (listed, expected) in [
        // A subset narrows.
        (r#"["websearch"]"#, screening(false, true, &[])),
        // A source the trusted tiers did not name is dropped, not added.
        (r#"["mcp:other"]"#, Screen::default()),
        (r#"["mcp:github", "mcp:other"]"#, screening(false, false, &["github"])),
        // An empty list narrows to nothing.
        ("[]", Screen::default()),
    ] {
        plant(&project, &format!("[evaluate]\nscreen = {listed}\n"));
        let mut merged =
            merge_files(std::slice::from_ref(&global)).expect("the global tier parses");
        merge_project_file(&mut merged, &project);
        assert_eq!(merged.evaluate_screen(), expected, "project lists {listed}");
    }

    // A project file silent on the table inherits the trusted screen.
    plant(&project, "model = \"openai/gpt-5.6\"\n");
    let mut merged = merge_files(std::slice::from_ref(&global)).expect("the global tier parses");
    merge_project_file(&mut merged, &project);
    assert_eq!(merged.evaluate_screen(), screening(true, true, &["github"]));

    // With nothing named by a trusted tier, a project turns nothing on.
    plant(&project, "[evaluate]\nscreen = [\"webfetch\"]\n");
    let mut merged = Config::default();
    merge_project_file(&mut merged, &project);
    assert_eq!(merged.evaluate_screen(), Screen::default());
}

/// Narrowing is never silent, bfaca0b's posture for `openrouter`: every
/// entry dropped on either side — a screened source the project file's list
/// left out, and a listed source the screen did not hold — gets exactly one
/// warning that names it and the file and says which of the two it was. An
/// empty list switches the screen off, so it warns once per source it drops.
#[test]
fn each_source_a_project_file_drops_from_the_screen_is_warned_once() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    let project = directory.path().join("ganja.toml");
    plant(&global, "[evaluate]\nscreen = [\"webfetch\", \"websearch\"]\n");
    let project_path = project.as_path();

    for (listed, expected, warned) in [
        (
            r#"["websearch", "mcp:other"]"#,
            screening(false, true, &[]),
            &[(NARROWED, "webfetch", project_path), (NOT_IN_SCREEN, "mcp:other", project_path)][..],
        ),
        (
            "[]",
            Screen::default(),
            &[(NARROWED, "webfetch", project_path), (NARROWED, "websearch", project_path)][..],
        ),
        // Naming exactly what was screened drops nothing, and says nothing.
        (r#"["websearch", "webfetch"]"#, screening(true, true, &[]), &[][..]),
    ] {
        plant(&project, &format!("[evaluate]\nscreen = {listed}\n"));
        let mut merged =
            merge_files(std::slice::from_ref(&global)).expect("the global tier parses");
        let (capture, _guard) = ganja_testkit::LogCapture::install(tracing::Level::WARN);
        merge_project_file(&mut merged, &project);

        assert_eq!(merged.evaluate_screen(), expected, "project lists {listed}");
        assert_screen_warnings(&capture.logged(), warned);
    }

    // A trusted tier that said nothing granted nothing, so every listed
    // source is one the screen did not hold.
    plant(&project, "[evaluate]\nscreen = [\"webfetch\"]\n");
    let mut merged = Config::default();
    let (capture, _guard) = ganja_testkit::LogCapture::install(tracing::Level::WARN);
    merge_project_file(&mut merged, &project);

    assert_eq!(merged.evaluate_screen(), Screen::default());
    assert_screen_warnings(&capture.logged(), &[(NOT_IN_SCREEN, "webfetch", project_path)]);
}

/// A line or paragraph separator and a bidi control are not control
/// characters, so the grammar lets a project file list an entry holding one;
/// the warning that names it writes it escaped. Raw, U+2028 starts a new line
/// in a reader that honours it — one beginning with the checkout's text — and
/// U+202E reverses how the rest of the line displays. The file's path is
/// written escaped too, at every warning a project file can draw here: a
/// checkout names the directories below its root as freely as its entries.
#[test]
fn a_separator_or_bidi_control_in_a_dropped_entry_is_logged_escaped() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    let project = directory.path().join("sub\u{2028}FORGED-FROM-DIRNAME").join("ganja.toml");
    plant(&global, "[evaluate]\nscreen = [\"webfetch\", \"mcp:gone\", \"mcp:mine\"]\n");
    plant(
        &project,
        "[evaluate]\nscreen = [\"webfetch\", \"mcp:x\\u2028FORGED\", \"mcp:y\\u202eDESREVER\"]\n\n\
         [webfetch]\nallow_private = true\n\n\
         [mcp.mine]\ntype = \"local\"\ncommand = [\"./mine-mcp\"]\n",
    );
    let mut merged = merge_files(std::slice::from_ref(&global)).expect("the global tier parses");

    let (capture, _guard) = ganja_testkit::LogCapture::install(tracing::Level::WARN);
    merge_project_file(&mut merged, &project);

    let logged = capture.logged();
    assert_eq!(merged.evaluate_screen(), screening(true, false, &[]));
    assert_screen_warnings(
        &logged,
        &[
            (REMOVED, "mcp:mine", project.as_path()),
            (NARROWED, "mcp:gone", project.as_path()),
            (NOT_IN_SCREEN, "mcp:x\u{2028}FORGED", project.as_path()),
            (NOT_IN_SCREEN, "mcp:y\u{202e}DESREVER", project.as_path()),
        ],
    );
    let lifted: Vec<&str> =
        logged.lines().filter(|line| line.contains("webfetch.allow_private")).collect();
    assert_eq!(lifted.len(), 1, "the lifted guard is warned about once: {logged}");
    assert!(lifted[0].contains(&format!("path={project:?}")), "it names the file: {logged}");
    assert!(logged.contains(r#"entry="mcp:x\u{2028}FORGED""#), "escaped: {logged}");
    assert!(logged.contains(r#"entry="mcp:y\u{202e}DESREVER""#), "escaped: {logged}");
    assert_eq!(
        logged.matches(r"sub\u{2028}FORGED-FROM-DIRNAME").count(),
        5,
        "each warning writes the directory name escaped: {logged}"
    );
    assert!(
        !logged.contains(['\u{2028}', '\u{202e}']),
        "neither character reaches the log raw: {logged:?}"
    );
    // Every line boundary a Unicode-aware reader may honour, not only `\n`:
    // five warnings are five lines to all of them.
    let boundaries = [
        '\n', '\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        '\u{2029}',
    ];
    let lines = logged.split(boundaries).filter(|line| !line.is_empty()).count();
    assert_eq!(lines, 5, "one line per warning, however the log is split: {logged:?}");
}

/// **D567**, criterion 11: a server a project file defines or redefines is
/// never screened — the person agreed to send *their* server's results, and a
/// checkout that replaced the server replaced what they agreed to. One
/// warning names the entry and the file; a server the file defines that was
/// not screened is no news and gets none.
#[test]
fn a_project_file_that_defines_a_screened_server_unscreens_it_with_one_warning() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    let project = directory.path().join("ganja.toml");
    plant(&global, "[evaluate]\nscreen = [\"webfetch\", \"mcp:github\"]\n");
    plant(
        &project,
        "[mcp.github]\ntype = \"local\"\ncommand = [\"./github-mcp\"]\n\n\
         [mcp.docs]\ntype = \"local\"\ncommand = [\"./docs-mcp\"]\n",
    );
    let mut merged = merge_files(std::slice::from_ref(&global)).expect("the global tier parses");

    let (capture, _guard) = ganja_testkit::LogCapture::install(tracing::Level::WARN);
    merge_project_file(&mut merged, &project);

    assert_eq!(merged.evaluate_screen(), screening(true, false, &[]));
    assert!(merged.mcp.contains_key("github"), "the project's server itself still loads");
    assert_screen_warnings(&capture.logged(), &[(REMOVED, "mcp:github", project.as_path())]);
}

/// A file that defines a screened server and also lists it gets one line for
/// that entry, the removal: the entry was named by the tiers above, so a
/// second line saying it is not in their screen would be the log contradicting
/// itself about one entry. Where the tiers above never screened the server,
/// nothing was removed, and the one line is the true one — the listed entry
/// is not in the screen and is ignored.
#[test]
fn a_project_file_that_defines_and_lists_a_server_is_warned_once_for_that_entry() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    let project = directory.path().join("ganja.toml");
    plant(
        &project,
        "[mcp.github]\ntype = \"local\"\ncommand = [\"./github-mcp\"]\n\n\
         [evaluate]\nscreen = [\"webfetch\", \"mcp:github\"]\n",
    );

    plant(&global, "[evaluate]\nscreen = [\"webfetch\", \"mcp:github\"]\n");
    let mut merged = merge_files(std::slice::from_ref(&global)).expect("the global tier parses");
    let (capture, _guard) = ganja_testkit::LogCapture::install(tracing::Level::WARN);
    merge_project_file(&mut merged, &project);

    assert_eq!(merged.evaluate_screen(), screening(true, false, &[]));
    assert_screen_warnings(&capture.logged(), &[(REMOVED, "mcp:github", project.as_path())]);

    plant(&global, "[evaluate]\nscreen = [\"webfetch\"]\n");
    let mut merged = merge_files(std::slice::from_ref(&global)).expect("the global tier parses");
    let (capture, _guard) = ganja_testkit::LogCapture::install(tracing::Level::WARN);
    merge_project_file(&mut merged, &project);

    assert_eq!(merged.evaluate_screen(), screening(true, false, &[]));
    assert_screen_warnings(&capture.logged(), &[(NOT_IN_SCREEN, "mcp:github", project.as_path())]);
}

/// **D567**, criterion 12: removal is permanent. An outer project file that
/// redefines a server takes it off the screen, and an inner file listing it
/// again cannot put it back — the inner list is intersected with a screen
/// that no longer holds it, so each file's step gets its own warning, and
/// the inner one's says what is true of it: the source is not in the screen
/// the files above it left, not that the person never named it.
#[test]
fn an_inner_project_file_cannot_screen_a_server_an_outer_one_redefined() {
    let directory = temporary();
    let global = directory.path().join("global.toml");
    let outer = directory.path().join("ganja.toml");
    let inner = directory.path().join("inner").join("ganja.toml");
    plant(&global, "[evaluate]\nscreen = [\"websearch\", \"mcp:github\"]\n");
    plant(&outer, "[mcp.github]\ntype = \"local\"\ncommand = [\"./github-mcp\"]\n");
    plant(&inner, "[evaluate]\nscreen = [\"websearch\", \"mcp:github\"]\n");
    let mut merged = merge_files(std::slice::from_ref(&global)).expect("the global tier parses");

    let (capture, _guard) = ganja_testkit::LogCapture::install(tracing::Level::WARN);
    // Outermost first, the order `project_files` walks in.
    merge_project_file(&mut merged, &outer);
    merge_project_file(&mut merged, &inner);

    assert_eq!(merged.evaluate_screen(), screening(false, true, &[]));
    assert_screen_warnings(
        &capture.logged(),
        &[(REMOVED, "mcp:github", outer.as_path()), (NOT_IN_SCREEN, "mcp:github", inner.as_path())],
    );
}

/// OSC 9 degrades to nothing on a terminal that ignores it, which is the
/// right failure for a default.
#[test]
fn the_notification_method_is_osc9_until_a_config_says_otherwise() {
    let config = parse(r#"tui = {}"#).expect("it parses");
    assert_eq!(config.tui.notification_method(), NotificationMethod::Osc9);

    let config = parse(r#"tui = { notification_method = "bel" }"#).expect("it parses");
    assert_eq!(config.tui.notification_method(), NotificationMethod::Bel);
}

/// The table merges the way `agents` and `webfetch` do: field by field,
/// with a tier that says nothing leaving the tier below it alone.
#[test]
fn a_closer_tier_overlays_the_tui_table_field_by_field() {
    let mut merged =
        parse(r#"tui = { notifications = true, notification_method = "bel" }"#).expect("it parses");
    merged.merge(parse(r#"tui = { notifications = ["approval-requested"] }"#).expect("it parses"));

    assert_eq!(
        merged.tui.notifications,
        Some(Notifications::Events(vec![NotificationEvent::ApprovalRequested])),
        "the closer tier's selection replaces"
    );
    assert_eq!(
        merged.tui.notification_method(),
        NotificationMethod::Bel,
        "the method it said nothing about stays"
    );
}

/// The roster is user-ordered and exact: what the config names, in the
/// order it names it, is what the bar renders (**D469**).
#[test]
fn a_statusline_roster_keeps_the_order_the_config_wrote() {
    let config = parse(
        r#"tui = { statusline = { elements = ["model", "context", "tokens"], max_width = 120, detail = true } }"#,
    )
    .expect("it parses");

    let statusline = config.tui.statusline.expect("the table was written");
    assert_eq!(
        statusline.elements,
        Some(
            vec![StatuslineElement::Model, StatuslineElement::Context, StatuslineElement::Tokens,]
        )
    );
    assert_eq!(statusline.max_width, Some(120));
    assert_eq!(statusline.detail, Some(true));
}

/// D524's segment gained its element name: a roster places the held
/// count like any other element, so the loader accepts the word.
#[test]
fn a_statusline_roster_may_name_the_held_count() {
    let config =
        parse(r#"tui = { statusline = { elements = ["held", "dialogs"] } }"#).expect("it parses");

    assert_eq!(
        config.tui.statusline.expect("the table was written").elements,
        Some(vec![StatuslineElement::Held, StatuslineElement::Dialogs]),
    );
}

/// An element name nothing renders is refused naming it — serde's closed
/// enum, the same refusal an unknown notification event gets.
#[test]
fn an_element_name_nothing_renders_is_refused_by_name() {
    let error = parse(r#"tui = { statusline = { elements = ["contextbar"] } }"#)
        .expect_err("an unknown element name is refused");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("contextbar"), "{message}");
    assert!(
        message.contains("context"),
        "the refusal should list what the roster does have: {message}"
    );
}

/// W5's element joins the roster vocabulary the same way: a roster may name
/// `task-list`, and the name is its own rather than a second spelling of
/// `tasks` — the two count different things, and a config that meant one and
/// got the other would be told nothing.
#[test]
fn a_statusline_roster_may_name_the_shared_task_list() {
    let config = parse(r#"tui = { statusline = { elements = ["tasks", "task-list"] } }"#)
        .expect("it parses");

    assert_eq!(
        config.tui.statusline.expect("the table was written").elements,
        Some(vec![StatuslineElement::Tasks, StatuslineElement::TaskList]),
    );
}

/// And the near-misses of it are refused by name, the discipline every
/// widening of this enum is held to.
#[test]
fn a_near_miss_of_the_task_list_element_is_refused_by_name() {
    for near_miss in ["tasklist", "task_list", "tasks-list"] {
        let error = parse(&format!(r#"tui = {{ statusline = {{ elements = ["{near_miss}"] }} }}"#))
            .expect_err("only the name something renders is accepted");

        assert!(error.to_string().contains(near_miss), "the refusal names it: {error}");
    }
}

/// The statusline table is curated like the `tui` table above it.
#[test]
fn a_key_the_statusline_table_does_not_have_is_refused_by_name() {
    let error = parse(r#"tui = { statusline = { zzz_probe = 1 } }"#)
        .expect_err("an unknown key inside statusline is refused");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("zzz_probe"), "{message}");
}

/// Field by field, like the rest of the `tui` table: a project that only
/// reorders elements keeps the global tier's width cap, and a tier that
/// says nothing leaves the whole table alone.
#[test]
fn a_closer_tier_overlays_the_statusline_table_field_by_field() {
    let mut merged = parse(r#"tui = { statusline = { elements = ["model"], max_width = 100 } }"#)
        .expect("it parses");
    merged.merge(
        parse(r#"tui = { statusline = { elements = ["context", "tokens"] } }"#).expect("it parses"),
    );

    let statusline = merged.tui.statusline.expect("the table survives the merge");
    assert_eq!(
        statusline.elements,
        Some(vec![StatuslineElement::Context, StatuslineElement::Tokens]),
        "the closer tier's list replaces wholesale"
    );
    assert_eq!(statusline.max_width, Some(100), "the width it said nothing about stays");

    let mut untouched = parse(r#"tui = { statusline = { detail = true } }"#).expect("it parses");
    untouched.merge(parse(r#"tui = {}"#).expect("it parses"));
    assert_eq!(
        untouched.tui.statusline,
        Some(StatuslineConfig { elements: None, max_width: None, detail: Some(true) }),
        "a tier that says nothing leaves the table alone"
    );
}

/// Per event, wholesale — the `mcp` arm's semantics, applied for the reason
/// stated at the merge: these are commands, and a global one a project
/// deliberately left out must not keep running underneath it.
#[test]
fn a_closer_tier_replaces_one_hook_event_and_leaves_the_others() {
    let mut merged = parse(
        r#"
            [[hooks.PreToolUse]]
            hooks = [{ type = "command", command = "global-pre" }]

            [[hooks.Stop]]
            hooks = [{ type = "command", command = "global-stop" }]
        "#,
    )
    .expect("it parses");
    merged.merge(
        parse(
            r#"hooks = { PreToolUse = [{ hooks = [{ type = "command", command = "project-pre" }] }] }"#,
        )
        .expect("it parses"),
    );

    assert_eq!(
        merged.hooks["PreToolUse"],
        vec![HookMatcher {
            matcher: None,
            hooks: vec![HookHandler::Command(HookCommand {
                command: "project-pre".to_owned(),
                timeout: None,
            })],
        }],
        "the project's list is the list, not an addition to the global one"
    );
    assert_eq!(
        merged.hooks["Stop"][0].hooks,
        vec![HookHandler::Command(HookCommand {
            command: "global-stop".to_owned(),
            timeout: None,
        })],
        "an event the closer tier said nothing about is untouched"
    );
}

#[test]
fn an_absent_lsp_key_is_no_language_servers_at_all() {
    let config = parse(r#"model = "anthropic/claude-sonnet-5""#).expect("it parses");

    assert_eq!(config.lsp, None, "LSP is opt-in, and this config did not opt in");
}

#[test]
fn the_lsp_key_takes_a_bare_boolean() {
    for (text, expected) in [("true", true), ("false", false)] {
        let config = parse(&format!("lsp = {text}")).expect("a boolean is a shape");

        assert_eq!(config.lsp, Some(LspConfig::Enabled(expected)), "for {text}");
    }
}

#[test]
fn an_lsp_entry_carries_every_field_it_may_hold() {
    let config = parse(
        r#"
            [lsp.zls]
            command = ["zls", "--enable-debug-log"]
            extensions = [".zig", ".zon"]
            env = { ZLS_HOME = "/opt/zls" }
            initialization = { zls = { enable_build_on_save = true } }
        "#,
    )
    .expect("a full entry parses");

    let Some(LspConfig::Servers(entries)) = &config.lsp else {
        panic!("the value is a map of servers");
    };
    let zls = &entries["zls"];
    assert_eq!(
        zls.command.as_deref(),
        Some(["zls".to_owned(), "--enable-debug-log".to_owned()].as_slice())
    );
    assert_eq!(zls.extensions.as_deref(), Some([".zig".to_owned(), ".zon".to_owned()].as_slice()));
    assert!(!zls.disabled);
    assert_eq!(zls.env["ZLS_HOME"], "/opt/zls");
    assert_eq!(
        zls.initialization,
        Some(serde_json::json!({"zls": {"enable_build_on_save": true}}))
    );
}

#[test]
fn disabling_a_builtin_is_the_one_legal_entry_with_no_command() {
    let config = parse(r#"lsp = { rust = { disabled = true } }"#)
        .expect("this is how a builtin is switched off");

    let Some(LspConfig::Servers(entries)) = &config.lsp else {
        panic!("the value is a map of servers");
    };
    assert!(entries["rust"].disabled);
    assert_eq!(entries["rust"].command, None);
}

#[test]
fn an_lsp_entry_with_no_command_is_refused_by_name() {
    let error = parse(r#"lsp = { rust = { extensions = [".rs"] } }"#)
        .expect_err("a server with no program is not a server");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("rust"), "{message}");
    assert!(message.contains("command"), "{message}");
}

#[test]
fn a_custom_lsp_server_without_extensions_is_refused_in_upstreams_words() {
    let error = parse(r#"lsp = { zls = { command = ["zls"] } }"#)
        .expect_err("nothing tells ganja which files zls claims");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(
        message.contains("For custom LSP servers, 'extensions' array is required."),
        "{message}"
    );
    assert!(message.contains("zls"), "the message names the entry: {message}");
}

#[test]
fn a_builtin_without_extensions_inherits_them_instead_of_being_refused() {
    let config = parse(r#"lsp = { rust = { command = ["ra-multiplex"] } }"#)
        .expect("a builtin has extensions to inherit");

    let Some(LspConfig::Servers(entries)) = &config.lsp else {
        panic!("the value is a map of servers");
    };
    assert_eq!(entries["rust"].extensions, None, "inherited, not written");
}

#[test]
fn an_unknown_field_inside_an_lsp_entry_is_refused_by_name() {
    let error = parse(r#"lsp = { rust = { command = ["x"], rootMarkers = ["Cargo.toml"] } }"#)
        .expect_err("upstream has no such key either");

    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(message.contains("rootMarkers"), "{message}");
}

#[test]
fn an_mcp_entry_carries_everything_the_two_shapes_hold() {
    let config = parse(
        r#"
            [mcp.fs]
            type = "local"
            command = ["bun", "x", "server"]
            cwd = "tools"
            environment = { TOKEN = "x" }
            timeout = 1234
            output_limit = 4096

            [mcp.hub]
            type = "remote"
            url = "https://mcp.example/mcp"
            headers = { Authorization = "Bearer x" }
            enabled = false

            [mcp.auth]
            type = "remote"
            url = "https://oauth.example/mcp"
            oauth = {}
        "#,
    )
    .expect("both shapes parse");

    let McpServer::Local(local) = &config.mcp["fs"] else {
        panic!("the first entry is local");
    };
    assert_eq!(local.command, ["bun", "x", "server"]);
    assert_eq!(local.cwd.as_deref(), Some("tools"));
    assert_eq!(local.environment["TOKEN"], "x");
    assert!(local.enabled, "an entry that says nothing connects");
    assert_eq!(local.timeout.map(NonZeroU64::get), Some(1234));
    assert_eq!(local.output_limit, Some(4096));

    let McpServer::Remote(remote) = &config.mcp["hub"] else {
        panic!("the second entry is remote");
    };
    assert_eq!(remote.url, "https://mcp.example/mcp");
    assert_eq!(remote.headers["Authorization"], "Bearer x");
    assert!(!remote.enabled);
    assert_eq!(remote.timeout, None);
    assert_eq!(
        remote.output_limit, None,
        "an entry that says nothing about its budget gets the global default"
    );
    assert_eq!(remote.oauth, None, "an entry that says nothing has no login");

    let McpServer::Remote(auth) = &config.mcp["auth"] else {
        panic!("the third entry is remote");
    };
    assert_eq!(
        auth.oauth,
        Some(McpOauth::default()),
        "`oauth: {{}}` turns discovery and dynamic registration on for this server"
    );
}

/// Every one of these is a config that would otherwise have described a
/// server nothing could reach, silently.
#[test]
fn an_mcp_entry_that_describes_no_reachable_server_is_refused_by_name() {
    let cases = [
        // Upstream skips a type-less entry with a log line; a config that
        // names a server means to have one.
        (r#"mcp = { x = { command = ["a"] } }"#, "type"),
        (r#"mcp = { x = { type = "local", command = [] } }"#, "x"),
        (r#"mcp = { x = { type = "remote", url = "http://mcp.example/mcp" } }"#, "loopback"),
        (r#"mcp = { x = { type = "remote", url = "not a url" } }"#, "url"),
        // A zero-millisecond budget is not a budget.
        (r#"mcp = { x = { type = "local", command = ["a"], timeout = 0 } }"#, "0"),
        // A zero-byte output budget refuses every result, named by
        // `check_mcp` rather than by serde's generic NonZeroU64 message —
        // `output_limit` is a plain `u64` for exactly this reason.
        (r#"mcp = { x = { type = "local", command = ["a"], output_limit = 0 } }"#, "output_limit"),
    ];

    for (text, named) in cases {
        let error = parse(text).expect_err(&format!("{text} describes no server"));
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure for {text}, got {error:?}");
        };
        assert!(message.contains(named), "{text}: {message}");
    }
}

/// The three refusals above, reached the way the two *writers* reach them
/// — `ganja mcp add` and `ganja config import-opencode` both hold a decoded
/// [`McpServer`] and no config file, and both call this method rather than
/// spelling the rules a second and third time.
///
/// Pinned here as well as through the loader because "one authority" is
/// only true while the method itself refuses all three: the loader test
/// above would still pass if a writer's copy of a rule drifted.
#[test]
fn the_post_decode_checks_are_one_method_three_callers_share() {
    let cases = [
        (
            r#"type = "local"
command = []"#,
            "empty command",
        ),
        (
            r#"type = "local"
command = ["a"]
output_limit = 0"#,
            "output_limit of 0",
        ),
        (
            r#"type = "remote"
url = "http://mcp.example/mcp""#,
            "loopback",
        ),
        (
            r#"type = "remote"
url = "not a url""#,
            "no valid url",
        ),
    ];

    for (text, named) in cases {
        let server: McpServer =
            toml::from_str(text).unwrap_or_else(|error| panic!("{text}: {error}"));
        let message = server.check("x").expect_err(&format!("{text} describes no usable server"));
        assert!(message.contains(named), "{text}: {message}");
        assert!(message.contains("\"x\""), "and names the server: {message}");
    }

    let fine: McpServer = toml::from_str(
        r#"type = "local"
command = ["a"]"#,
    )
    .expect("the fixture is a server");
    assert_eq!(fine.check("x"), Ok(()));
}

/// Every refusal `read` makes after decoding is about what a config *says*,
/// not how it was spelled, so all seven answer for a `ganja.toml` too. Pinned
/// as one table rather than seven tests because what is under test is that the
/// checks sit past the fork in `decode` — one of them landing on the wrong
/// side of it is the failure this catches.
#[test]
fn the_post_decode_refusals_answer_for_a_toml_file_too() {
    let cases = [
        ("mcp", "[mcp.docs]\ntype = \"local\"\ncommand = []\n", "empty command"),
        ("lsp", "[lsp.mine]\nextensions = [\".x\"]\n", "has no command"),
        (
            "provider",
            "[provider.anthropic]\ndialect = \"anthropic-messages\"\nbase_url = \"https://x.example/v1\"\n",
            "already ships",
        ),
        (
            "hooks",
            "[[hooks.PreToolUse]]\nmatcher = \"[\"\n\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"x\"\n",
            "matcher",
        ),
        ("agents", "[agents]\nconcurrency = 0\n", "concurrency"),
        ("teammates", "[teammates]\nshim_turn_timeout = 0\n", "shim_turn_timeout"),
        ("openrouter", "[openrouter]\nserver_tools = [\"telepathy\"]\n", "telepathy"),
        ("claude_code", "[claude_code]\nidle_bound = 0\n", "claude_code.idle_bound"),
    ];

    for (key, text, named) in cases {
        let error = parse(text).expect_err(&format!("the {key} table describes nothing usable"));
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure for {key}, got {error:?}");
        };
        assert!(message.contains(named), "{key}: {message}");
    }
}

/// `Serialize` beside `Deserialize`, so a caller that writes an entry —
/// `ganja mcp add` building one, a listing asked for JSON — spells it out
/// of the type the loader reads back rather than by hand.
#[test]
fn an_mcp_entry_survives_the_round_trip_through_its_own_serialization() {
    for text in [
        r#"mcp = { x = { type = "local", command = ["bun", "x"], environment = { K = "v" }, output_limit = 4096 } }"#,
        r#"mcp = { x = { type = "remote", url = "https://mcp.example/mcp", headers = { X-A = "1" }, oauth = {} } }"#,
    ] {
        let config = parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
        let written = serde_json::to_string(&config.mcp["x"]).expect("the entry serializes");
        let read: McpServer = serde_json::from_str(&written)
            .unwrap_or_else(|error| panic!("what was written reads back: {written}: {error}"));
        assert_eq!(read, config.mcp["x"], "{written}");
    }
}

/// The same rule the provider endpoints obey, and the same reason: a
/// remote entry's `headers` is where somebody puts a token.
#[test]
fn a_remote_server_may_be_plain_http_only_to_loopback() {
    let allowed = [
        "https://mcp.example/mcp",
        "http://127.0.0.1:8000/mcp",
        "http://localhost:8000/mcp",
        "http://[::1]:8000/mcp",
    ];
    for url in allowed {
        let text = format!(r#"mcp.x = {{ type = "remote", url = "{url}" }}"#);
        parse(&text).unwrap_or_else(|error| panic!("{url} is reachable: {error}"));
    }

    let refused = [
        // A host that merely contains the address, and a host that merely
        // starts with the name: both belong to whoever registered them.
        "http://127.0.0.1.evil.test/mcp",
        "http://localhost.evil.test/mcp",
        "http://127.0.0.1@evil.test/mcp",
    ];
    for url in refused {
        let text = format!(r#"mcp.x = {{ type = "remote", url = "{url}" }}"#);
        parse(&text).expect_err(url);
    }
}

/// An entry replaces rather than merging, because the two shapes carry
/// different keys.
#[test]
fn a_provider_entry_carries_every_field_it_may_hold() {
    let config = parse(
        r#"
            [provider.local-llama]
            dialect = "openai-chat-completions"
            base_url = "http://127.0.0.1:11434/v1"
            key_env = "LLAMA_API_KEY"
            headers = { x-route = "gpu-0" }

            [provider.gateway]
            dialect = "anthropic-messages"
            base_url = "https://messages.example/v1"

            [provider.proxy]
            dialect = "openai-responses"
            base_url = "https://responses.example/v1"
        "#,
    )
    .expect("all three dialects parse");

    let local = &config.provider["local-llama"];
    assert_eq!(local.dialect, Some(Dialect::OpenaiChatCompletions));
    assert_eq!(local.base_url.as_deref(), Some("http://127.0.0.1:11434/v1"));
    assert_eq!(local.key_env.as_deref(), Some("LLAMA_API_KEY"));
    assert_eq!(local.headers["x-route"], "gpu-0");

    let gateway = &config.provider["gateway"];
    assert_eq!(gateway.dialect, Some(Dialect::AnthropicMessages));
    assert_eq!(
        gateway.key_env, None,
        "an entry that names no variable is answered by the store alone"
    );
    assert!(gateway.headers.is_empty());

    assert_eq!(
        config.provider["proxy"].dialect,
        Some(Dialect::OpenaiResponses),
        "an endpoint serving the Responses surface names that mapping"
    );
}

/// Every one of these is a config that would otherwise have described an
/// endpoint no session could reach — or, in the first case, one that would
/// have been written, loaded and then never consulted.
#[test]
fn a_provider_entry_that_describes_no_usable_endpoint_is_refused_by_name() {
    let cases = [
        // Selection matches the builtins first, so this entry would be
        // dead the moment it loaded.
        (
            r#"provider = { anthropic = { dialect = "anthropic-messages", base_url = "https://proxy.example" } }"#,
            "anthropic",
        ),
        // **AC-0.10.** The refusal is `PROVIDERS`'s to make, so an id joining
        // that list joins this one — and `chatgpt` did, with **D555**. The
        // stated cost of the split: a config that pointed this name at a proxy
        // yesterday is refused by name today.
        (
            r#"provider = { chatgpt = { dialect = "openai-responses", base_url = "https://proxy.example" } }"#,
            "chatgpt",
        ),
        // A dialect is a request/response mapping, and there is no arm for
        // one this build does not implement.
        (r#"provider = { x = { dialect = "gemini", base_url = "https://a.test" } }"#, "gemini"),
        // Required: guessing the wire from a URL is how an Anthropic body
        // reaches a chat-completions server.
        (r#"provider = { x = { base_url = "https://a.test" } }"#, "dialect"),
        (r#"provider = { x = { dialect = "anthropic-messages" } }"#, "base_url"),
        // A key in a config file is the one thing that must not travel, so
        // the key upstream spells it with is not a key here.
        (
            r#"provider = { x = { dialect = "anthropic-messages", base_url = "https://a.test", api_key = "sk-canary" } }"#,
            "api_key",
        ),
        (
            r#"provider = { x = { dialect = "openai-chat-completions", base_url = "http://gateway.example/v1" } }"#,
            "loopback",
        ),
        (
            r#"provider = { x = { dialect = "openai-chat-completions", base_url = "not a url" } }"#,
            "base_url",
        ),
        // A blank variable names none, and would read as "there is no key"
        // — which sends somebody to fix a store that was never the problem.
        (
            r#"provider = { x = { dialect = "openai-chat-completions", base_url = "https://a.test", key_env = "  " } }"#,
            "key_env",
        ),
    ];

    for (text, named) in cases {
        let error = parse(text).expect_err(&format!("{text} describes no endpoint"));
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure for {text}, got {error:?}");
        };
        assert!(message.contains(named), "{text}: {message}");
    }

    // A dialect nobody implements is refused with the three that exist
    // named back, because "gemini is not one of them" is only half an
    // answer.
    let error = parse(r#"provider = { x = { dialect = "gemini", base_url = "https://a.test" } }"#)
        .expect_err("there is no fourth mapping");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };
    assert!(
        message.contains("openai-chat-completions")
            && message.contains("openai-responses")
            && message.contains("anthropic-messages"),
        "{message}"
    );
}

/// The refusal `text` earns, whole. Every assertion below compares the
/// **entire** sentence rather than a substring: these are the sentences
/// somebody reads instead of spending a turn on a 400, and a test that only
/// checks a key name would not notice one losing the probe date or the id.
fn refusal(text: &str) -> String {
    let error = parse(text).expect_err("this document is refused");
    let ConfigError::Parse { message, .. } = error else {
        panic!("expected a parse failure for {text}");
    };
    message
}

/// The options this document configures for `id` on `model`.
fn options(text: &str, id: &str, model: &str) -> ResponsesOptions {
    parse(text)
        .expect("this document loads")
        .provider
        .get(id)
        .and_then(|entry| entry.options.as_ref())
        .expect("this id carries an options table")
        .for_model(model)
}

/// A builtin's entry is an `options` table and nothing else,
/// and only on the two ids that read one (**D563**).
#[test]
fn a_builtin_provider_entry_may_carry_options_and_nothing_else() {
    assert!(
        parse("[provider.chatgpt.options]\nservice_tier = \"priority\"\n").is_ok(),
        "the one shape a builtin's entry can take"
    );

    assert_eq!(
        refusal("[provider.chatgpt]\nbase_url = \"https://x\"\n"),
        "provider \"chatgpt\" is one this build already ships; a `provider` entry for it may \
         carry `options` and nothing else, and its endpoint moves with its own base-URL variable"
    );
    assert_eq!(
        refusal("[provider.anthropic.options]\nservice_tier = \"default\"\n"),
        "provider \"anthropic\" takes no `options`; this build reads them on the two Responses \
         ids, chatgpt and openai"
    );
}

/// The declared-endpoint arm keeps both of its requirements and
/// gains one refusal: an endpoint this build has never spoken to has no
/// measured option surface to curate.
#[test]
fn a_config_declared_endpoint_still_needs_its_dialect_and_base_url() {
    assert_eq!(
        refusal("[provider.proxy]\ndialect = \"openai-responses\"\n"),
        "provider \"proxy\" is a config-declared endpoint and needs `base_url`"
    );
    assert_eq!(
        refusal("[provider.proxy]\nbase_url = \"https://p\"\n"),
        "provider \"proxy\" is a config-declared endpoint and needs `dialect`"
    );
    assert_eq!(
        refusal(
            "[provider.proxy]\ndialect = \"openai-responses\"\nbase_url = \"https://p\"\n\
             [provider.proxy.options]\nservice_tier = \"default\"\n"
        ),
        "provider \"proxy\" is a config-declared endpoint and takes no `options`"
    );
}

/// What a config *declares* is an endpoint, and a builtin's
/// `options` entry declares none — so it never appears in a listing of what
/// this project declares, where it would send somebody looking for an entry
/// they did not write.
#[test]
fn declared_endpoints_never_name_a_builtin() {
    let config = parse(
        "[provider.chatgpt.options]\nservice_tier = \"priority\"\n\
         [provider.proxy]\ndialect = \"openai-responses\"\nbase_url = \"https://p\"\n",
    )
    .expect("both entries load");

    assert_eq!(config.declared_endpoints(), vec!["proxy".to_owned()]);
    assert_eq!(config.provider.len(), 2, "both are still entries in the table");
}

/// A key the seat rejects is refused under `chatgpt` naming
/// the key, the id and the probe — and loads under `openai`, where it is one
/// of that id's own.
///
/// The three whose refusal was not a bare `Unsupported parameter` get
/// sentences of their own, because "not here" would tell somebody nothing
/// about what the backend actually did with the value they wrote.
#[test]
fn a_platform_only_key_under_chatgpt_is_refused_naming_the_probe() {
    let cases = [
        (
            "temperature = 0.2",
            "provider \"chatgpt\" cannot carry `options.temperature`: the ChatGPT seat rejects \
             it (`Unsupported parameter: temperature`, probe 2026-09-16); it is an `openai` key",
        ),
        (
            "reasoning = { mode = \"pro\" }",
            "provider \"chatgpt\" cannot carry `options.reasoning.mode`: the ChatGPT seat \
             rejects `pro` (`reasoning.mode is not supported with this model`, probe \
             2026-09-16) and `standard` is already its default; it is an `openai` key",
        ),
        (
            "prompt_cache_key = \"k\"",
            "provider \"chatgpt\" cannot carry `options.prompt_cache_key`: the ChatGPT seat \
             replaces it with a key of its own on every response (probe 2026-09-16), so a value \
             here would do nothing; it is an `openai` key",
        ),
        (
            "reasoning = { summary = \"concise\" }",
            "provider \"chatgpt\" cannot carry `options.reasoning.summary`: only `auto` has been \
             measured on the ChatGPT seat (probe 2026-09-16) and the wire already sends it; the \
             key is configurable on `openai`",
        ),
    ];

    for (key, expected) in cases {
        assert_eq!(refusal(&format!("[provider.chatgpt.options]\n{key}\n")), expected);
        assert!(
            parse(&format!("[provider.openai.options]\n{key}\n")).is_ok(),
            "{key} is an openai key"
        );
    }

    assert_eq!(
        options("[provider.openai.options]\ntemperature = 0.2\n", "openai", "gpt-5.5").temperature,
        Some(0.2)
    );
}

/// `fast` is a config spelling of `priority`: the backend answers
/// the word itself with `Unsupported service_tier: fast`, so it is rewritten
/// at load and nothing ever writes it back out.
#[test]
fn fast_is_read_as_priority_and_never_written() {
    for id in ["chatgpt", "openai"] {
        let options =
            options(&format!("[provider.{id}.options]\nservice_tier = \"fast\"\n"), id, "gpt-5.5");

        assert_eq!(options.service_tier, Some(ServiceTier::Priority));
        assert_eq!(ServiceTier::Priority.as_str(), "priority");

        let written = serde_json::to_string(&options).expect("the table serializes");
        assert!(!written.contains("fast"), "the alias must not survive a round trip: {written}");
    }

    assert_eq!(
        refusal("[provider.chatgpt.options]\nservice_tier = \"flex\"\n"),
        "provider \"chatgpt\" cannot carry `options.service_tier = \"flex\"`: the ChatGPT seat \
         rejects it (`Unsupported service_tier: flex`, probe 2026-09-16); the seat takes default, \
         priority, ultrafast"
    );
    assert!(
        parse("[provider.openai.options]\nservice_tier = \"flex\"\n").is_ok(),
        "the platform takes flex"
    );
}

/// **D563, probe 2026-09-17.** The two tiers the platform refused are refused
/// under `openai` in the platform's own words — and `ultrafast` still loads
/// under `chatgpt`, where it was taken — so a config never spends a request
/// on a tier this build has already watched fail.
#[test]
fn a_tier_the_platform_refused_is_refused_under_openai_naming_the_probe() {
    assert_eq!(
        refusal("[provider.openai.options]\nservice_tier = \"scale\"\n"),
        "provider \"openai\" cannot carry `options.service_tier = \"scale\"`: the OpenAI platform \
         rejects it (`Invalid value: 'scale'. Supported values are: 'auto', 'default', 'fast', \
         'flex', and 'priority'.`, probe 2026-09-17); the platform takes auto, default, flex, \
         priority"
    );
    assert_eq!(
        refusal("[provider.openai.options]\nservice_tier = \"ultrafast\"\n"),
        "provider \"openai\" cannot carry `options.service_tier = \"ultrafast\"`: the OpenAI \
         platform rejects it (a 500, `Invalid service_tier argument`, probe 2026-09-17); the \
         platform takes auto, default, flex, priority"
    );
    assert_eq!(
        refusal("[provider.openai.options.model.\"gpt-5.6-sol\"]\nservice_tier = \"ultrafast\"\n"),
        "provider \"openai\" cannot carry `options.service_tier = \"ultrafast\"`: the OpenAI \
         platform rejects it (a 500, `Invalid service_tier argument`, probe 2026-09-17); the \
         platform takes auto, default, flex, priority",
        "measured on gpt-5.6-sol itself, so a per-model entry is no way around it"
    );
    assert!(parse("[provider.chatgpt.options]\nservice_tier = \"ultrafast\"\n").is_ok());
    assert_eq!(
        refusal("[provider.chatgpt.options]\nservice_tier = \"scale\"\n"),
        "provider \"chatgpt\" cannot carry `options.service_tier = \"scale\"`: the ChatGPT seat \
         rejects it (`Unsupported service_tier: scale`, probe 2026-09-16); the seat takes default, \
         priority, ultrafast",
        "the seat's own refusal is unchanged"
    );
}

/// **D563, probe 2026-09-17.** Each id refuses one key the
/// other takes, and says what its backend did with it: the platform refuses
/// `access_programs` as not enabled for the organization, and the seat takes
/// `context_management` and does nothing with it.
#[test]
fn a_key_one_id_was_measured_to_refuse_or_ignore_loads_under_the_other() {
    let access = "access_programs = { cyber = \"standard\" }";
    assert_eq!(
        refusal(&format!("[provider.openai.options]\n{access}\n")),
        "provider \"openai\" cannot carry `options.access_programs`: the OpenAI platform rejects \
         it (`The access_programs parameter is not enabled for this organization.`, probe \
         2026-09-17); it is a `chatgpt` key"
    );
    assert!(parse(&format!("[provider.chatgpt.options]\n{access}\n")).is_ok());

    let compaction = "context_management = [{ type = \"compaction\", compact_threshold = 20000 }]";
    assert_eq!(
        refusal(&format!("[provider.chatgpt.options]\n{compaction}\n")),
        "provider \"chatgpt\" cannot carry `options.context_management`: the ChatGPT seat accepts \
         it and does nothing with it (no compaction item and no fewer input tokens on a 56k-token \
         transcript past a 20k threshold, probe 2026-09-17); it is an `openai` key"
    );
    assert!(parse(&format!("[provider.openai.options]\n{compaction}\n")).is_ok());
}

/// A hosted tool's `type` is curated and everything else about it
/// passes through, because the settings a hosted tool takes move when the
/// vendor moves them and the one thing a wrong value makes unrecoverable is
/// the type.
#[test]
fn a_hosted_tool_type_is_curated_and_the_rest_of_the_entry_is_not() {
    let carried = options(
        "[provider.chatgpt.options]\n\
         server_tools = [{ type = \"web_search\", search_context_size = \"low\" }, \
         { type = \"image_generation\" }]\n",
        "chatgpt",
        "gpt-5.5",
    );

    assert_eq!(carried.server_tools.len(), 2);
    assert_eq!(carried.server_tools[0].kind, "web_search");
    assert_eq!(carried.server_tools[0].rest["search_context_size"].as_str(), Some("low"));
    assert!(carried.server_tools[1].rest.is_empty());

    assert_eq!(
        refusal("[provider.chatgpt.options]\nserver_tools = [{ type = \"code_interpreter\" }]\n"),
        "provider \"chatgpt\" cannot carry a `server_tools` entry of type \"code_interpreter\": \
         the ChatGPT seat rejects it (`Unsupported tool type: code_interpreter`, probe \
         2026-09-16); the seat takes web_search, image_generation"
    );
    assert!(
        parse("[provider.openai.options]\nserver_tools = [{ type = \"code_interpreter\" }]\n")
            .is_ok(),
        "the platform runs it"
    );

    // Refused on **both** ids, and for neither id's reason: it runs on the
    // client, so a hosted advertisement of it promises an executor this build
    // does not offer through that door.
    for id in ["chatgpt", "openai"] {
        assert_eq!(
            refusal(&format!("[provider.{id}.options]\nserver_tools = [{{ type = \"shell\" }}]\n")),
            format!(
                "provider \"{id}\" cannot carry a `server_tools` entry of type \"shell\": it \
                 needs client-side execution, which this build does not offer through a hosted \
                 tool"
            )
        );
    }
}

/// Only a builtin a custom advertisement can actually carry may be
/// named, and the refusal hands back the list rather than leaving somebody to
/// guess which of their tools qualify.
#[test]
fn a_custom_tool_must_be_a_builtin_with_one_required_string() {
    assert!(parse("[provider.chatgpt.options]\ncustom_tools = [\"bash\"]\n").is_ok());

    assert_eq!(
        refusal("[provider.chatgpt.options]\ncustom_tools = [\"edit\"]\n"),
        format!(
            "provider \"chatgpt\" cannot carry `custom_tools = [\"edit\"]`: only a builtin with \
             exactly one required string argument can be advertised as a custom tool; those are {}",
            responses::options::CUSTOM_TOOLS.join(", ")
        )
    );
}

/// Two more per-id narrowings, both unprobed rather than rejected
/// on the seat — which is why the sentences say so instead of quoting a
/// backend error nobody has seen.
#[test]
fn a_hosted_tool_choice_and_a_platform_include_are_refused_under_chatgpt() {
    assert_eq!(
        refusal("[provider.chatgpt.options]\ntool_choice = { type = \"web_search\" }\n"),
        "provider \"chatgpt\" cannot carry a hosted `tool_choice` of type \"web_search\": that \
         form is unprobed on the ChatGPT seat (probe 2026-09-16 registered web_search and \
         image_generation as tools and never named one in `tool_choice`); it is admitted on \
         `openai` only"
    );
    assert_eq!(
        refusal("[provider.chatgpt.options]\ninclude = [\"message.output_text.logprobs\"]\n"),
        format!(
            "provider \"chatgpt\" cannot carry `include = [\"message.output_text.logprobs\"]`; \
             the values this id takes are {}",
            responses::options::SEAT_INCLUDE.join(", ")
        )
    );

    // The two forms that are not hosted stay reachable on the seat, which is
    // what makes the refusal above about the *form* rather than about the key.
    assert!(parse("[provider.chatgpt.options]\ntool_choice = \"auto\"\n").is_ok());
    assert!(
        parse(
            "[provider.chatgpt.options]\n\
             tool_choice = { type = \"allowed_tools\", mode = \"auto\", tools = [] }\n"
        )
        .is_ok()
    );
}

/// The per-model overlay is field-wise and one deep: the entry for
/// the model a request names replaces what it sets, and leaves everything
/// else the provider-wide table said.
#[test]
fn a_per_model_table_wins_over_the_provider_wide_value() {
    let text = "[provider.chatgpt.options]\n\
                service_tier = \"priority\"\n\
                parallel_tool_calls = true\n\
                [provider.chatgpt.options.model.\"gpt-5.6-sol\"]\n\
                service_tier = \"ultrafast\"\n";

    let sol = options(text, "chatgpt", "gpt-5.6-sol");
    assert_eq!(sol.service_tier, Some(ServiceTier::Ultrafast));
    assert_eq!(sol.parallel_tool_calls, Some(true), "an unset key leaves the wide value alone");
    assert!(sol.model.is_empty(), "the answer for one model carries no table of its own");

    assert_eq!(
        options(text, "chatgpt", "gpt-5.5").service_tier,
        Some(ServiceTier::Priority),
        "a model the table does not name reads the provider-wide value"
    );
}

/// The overlay is one deep, and a second level is refused rather
/// than ignored: nobody could read a two-level lookup off a config file, and
/// an ignored table is a setting whose author still believes it applies.
#[test]
fn a_per_model_entry_cannot_nest_a_model_table() {
    assert_eq!(
        refusal(
            "[provider.chatgpt.options.model.\"gpt-5.5\".model.\"gpt-5.6-sol\"]\n\
             service_tier = \"ultrafast\"\n"
        ),
        "provider \"chatgpt\" nests a `model` table inside `options.model.\"gpt-5.5\"`; a \
         per-model entry cannot carry one"
    );
}

/// A per-model entry is held to the **same** per-id gate as the table around
/// it — otherwise `[provider.chatgpt.options.model."gpt-5.5"]` would be a way
/// to write a key the seat 400s and hear nothing about it until a turn died.
#[test]
fn a_per_model_entry_is_gated_exactly_as_the_table_around_it() {
    assert_eq!(
        refusal("[provider.chatgpt.options.model.\"gpt-5.5\"]\ntemperature = 0.2\n"),
        "provider \"chatgpt\" cannot carry `options.temperature`: the ChatGPT seat rejects it \
         (`Unsupported parameter: temperature`, probe 2026-09-16); it is an `openai` key"
    );
}

/// An id that carries no `options` table answers nothing, which is every id
/// but the two — and, for those two, a config that wrote none.
#[test]
fn an_id_with_no_options_table_answers_nothing() {
    let config = parse("[provider.chatgpt.options]\nservice_tier = \"priority\"\n")
        .expect("the document loads");
    let carries = |config: &Config, id: &str| {
        config.provider.get(id).and_then(|entry| entry.options.as_ref()).is_some()
    };

    assert!(carries(&config, "chatgpt"));
    assert!(!carries(&config, "openai"));
    assert!(!carries(&config, "anthropic"));
    assert!(!carries(&Config::default(), "chatgpt"));
}

/// The same rule the provider endpoints obey, and the same reason twice
/// over: the credential travels in a header on every request, and so does
/// anything in `headers`.
#[test]
fn a_configured_endpoint_may_be_plain_http_only_to_loopback() {
    let allowed = [
        "https://gateway.example/v1",
        "http://127.0.0.1:11434/v1",
        "http://localhost:8080",
        "http://[::1]:8080/v1",
    ];
    let refused = [
        "http://gateway.example/v1",
        "http://127.0.0.1.evil.test/v1",
        "http://localhost.evil.test/v1",
        "http://127.0.0.1@evil.test/v1",
    ];

    for base_url in allowed {
        let text = format!(
            r#"provider.x = {{ dialect = "openai-chat-completions", base_url = "{base_url}" }}"#
        );
        parse(&text).unwrap_or_else(|error| panic!("{base_url} is reachable: {error}"));
    }
    for base_url in refused {
        let text = format!(
            r#"provider.x = {{ dialect = "openai-chat-completions", base_url = "{base_url}" }}"#
        );
        let error = parse(&text).expect_err(base_url);
        // A base URL is allowed to carry a credential in its userinfo, so
        // the refusal describes the rule rather than quoting the URL.
        assert!(
            !error.to_string().contains(base_url),
            "{base_url} was echoed back by its own refusal: {error}"
        );
    }
}

/// A closer tier redeclaring a provider means *that* provider: the fields
/// are one description of one endpoint, so a half-merged entry would
/// present the old tier's credential to the new tier's host.
#[test]
fn a_closer_tier_replaces_a_whole_provider_entry() {
    let directory = temporary();
    let outer = directory.path().join("outer.toml");
    let inner = directory.path().join("inner.toml");
    plant(
        &outer,
        r#"provider = { x = { dialect = "openai-chat-completions", base_url = "https://old.test/v1", key_env = "OLD_KEY" } }"#,
    );
    plant(
        &inner,
        r#"provider = { x = { dialect = "anthropic-messages", base_url = "https://new.test" } }"#,
    );

    let config = merge_files(&[outer, inner]).expect("both tiers parse");
    let entry = &config.provider["x"];
    assert_eq!(entry.dialect, Some(Dialect::AnthropicMessages));
    assert_eq!(entry.base_url.as_deref(), Some("https://new.test"));
    assert_eq!(
        entry.key_env, None,
        "the replaced entry's variable must not survive onto the new host"
    );
}

#[test]
fn a_closer_tier_replaces_a_whole_mcp_entry() {
    let directory = temporary();
    let outer = directory.path().join("outer.toml");
    let inner = directory.path().join("inner.toml");
    plant(&outer, r#"mcp = { x = { type = "local", command = ["old"], cwd = "here" } }"#);
    plant(&inner, r#"mcp = { x = { type = "remote", url = "https://new.test/mcp" } }"#);

    let config = merge_files(&[outer, inner]).expect("both tiers parse");
    let McpServer::Remote(remote) = &config.mcp["x"] else {
        panic!("the closer tier decides what the entry is");
    };
    assert_eq!(remote.url, "https://new.test/mcp");
}

/// Nested maps stay open on purpose: an agent definition written for a
/// later build, or for upstream, still loads here.
#[test]
fn an_unknown_key_inside_an_agent_is_carried_rather_than_refused() {
    let config =
        parse(r#"agent = { build = { temperature = 0.2, steps = 40, model = "openai/gpt-5.6" } }"#)
            .expect("an agent definition stays open");

    assert_eq!(config.agent["build"].model.as_deref(), Some("openai/gpt-5.6"));
}

#[test]
fn a_malformed_file_names_itself_and_where_it_stopped() {
    let directory = temporary();
    let path = directory.path().join("ganja.toml");
    plant(&path, "model =\n");

    let error = read(&path).expect_err("a broken config file is fatal");
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse failure, got {error:?}");
    };

    assert!(message.contains("line 1"), "{message}");
    assert!(error.to_string().contains(&path.display().to_string()), "{error}");
}

#[test]
fn a_config_file_asked_for_by_name_has_to_exist() {
    let directory = temporary();
    let missing = directory.path().join("nowhere.toml");
    let overrides = Overrides { config_file: Some(missing.clone()), ..Overrides::default() };

    let error = Config::load_with(directory.path(), &overrides)
        .expect_err("an explicit config file is a request");

    assert!(matches!(error, ConfigError::Missing { path } if path == missing));
}

/// A file `GANJA_CONFIG` or `--config` names can be called anything, and the
/// name discovery would have refused is refused there too — by extension,
/// because that is all a path chosen by somebody else has to go on.
///
/// The default falls the other way from discovery's: an extension this build
/// does not know, or none at all, is read. Somebody who names a file means
/// that file, and the only way to be wrong about it is to look like the format
/// that is gone.
#[test]
fn an_explicitly_named_file_is_refused_only_when_its_extension_is_the_old_one() {
    let directory = temporary();

    for name in ["elsewhere.toml", "elsewhere.conf", "elsewhere"] {
        let path = directory.path().join(name);
        plant(&path, r#"model = "openai/gpt-5.6""#);
        let config = read(&path)
            .unwrap_or_else(|error| panic!("{name} is a file somebody named: {error}"))
            .expect("the fixture exists");
        assert_eq!(config.model.as_deref(), Some("openai/gpt-5.6"));
    }

    // Both legacy extensions, and the refusal names the path that carries it
    // rather than the format in general.
    for name in ["elsewhere.jsonc", "elsewhere.json"] {
        let path = directory.path().join(name);
        plant(&path, r#"{"model": "anthropic/claude-sonnet-5"}"#);

        let error = read(&path).expect_err("the old dialect is not read");
        assert!(matches!(&error, ConfigError::Legacy { path: named } if named == &path));
        assert!(
            error.to_string().contains("ganja config migrate"),
            "the refusal says what to run: {error}"
        );
    }

    // And the refusal is the *name*, not the contents: a file spelling
    // perfectly good TOML under a legacy extension is refused unread, because
    // reading it would be the second format this landing exists to end.
    let confused = directory.path().join("confused.jsonc");
    plant(&confused, r#"model = "openai/gpt-5.6""#);
    assert!(matches!(read(&confused), Err(ConfigError::Legacy { .. })));
}

/// The order rules were written in is the order they are evaluated in, and
/// evaluation is last-match-wins — so a map that sorted its keys would
/// silently change which rule decides a call.
#[test]
fn permission_rules_keep_the_order_they_were_written_in() {
    let config = parse(
        r#"permission = { webfetch = "allow", bash = { "git status" = "allow", "git *" = "ask", "*" = "deny" }, edit = "ask" }"#,
    )
    .expect("a permission object is a config key");

    assert_eq!(
        config.permission.rules(),
        rules(&[
            ("webfetch", "*", Action::Allow),
            ("bash", "git status", Action::Allow),
            ("bash", "git *", Action::Ask),
            // A rule this build cannot carry out is still a rule: `deny`
            // survives as itself rather than being flattened to `ask`.
            ("bash", "*", Action::Deny),
            ("edit", "*", Action::Ask),
        ])
    );
}

/// The same claim against the TOML arm, and the reason it is a separate test
/// with a fixture shaped like this one.
///
/// A TOML document may spell one table's keys in two places: `[permission]`
/// opens it, another table interrupts, and `[permission.bash]` re-enters it
/// further down. A parser that buffered those into a sorted map — which is
/// what `toml::Table` is without `preserve_order`, and what any detour through
/// a value type would produce — would hand the loader `bash, edit, webfetch`
/// where the file said `webfetch, edit, bash`. Evaluation is last-match-wins,
/// so that inversion would silently hand a call to the wrong rule: the `deny`
/// written last would lose to an `allow` written first, and nothing would say
/// so. Hence the interleaved `[tui]` in the middle, which is what makes this
/// fixture different from the one above rather than a translation of it.
///
/// Both spellings a document may reach the same table by are exercised, since
/// they are two paths through the parser rather than one: table headers, and
/// **dotted keys** written at the document's own level. The second is the one
/// somebody hand-writing a short `permission` block is most likely to use, and
/// it is what the plan's pre-mortem asks for by name.
#[test]
fn permission_rules_keep_document_order_across_interleaved_toml_tables() {
    let spellings = [
        // Opened by a header, interrupted by an unrelated table, re-entered by
        // a sub-table header further down.
        (
            "table headers",
            r#"
                [permission]
                webfetch = "allow"
                edit = "ask"

                # An unrelated table between the two halves of `permission`,
                # which is what forces the parser to hold the first half
                # somewhere.
                [tui]
                notification_method = "bel"

                [permission.bash]
                "git status" = "allow"
                "git *" = "ask"
                "*" = "deny"
            "#,
        ),
        // The same table, opened by dotted keys at the document's own level
        // and interrupted twice — once by an unrelated key between two of its
        // own, and once by a table — before a header re-enters it.
        (
            "dotted keys",
            r#"
                permission.webfetch = "allow"
                model = "anthropic/claude-sonnet-5"
                permission.edit = "ask"

                [tui]
                notification_method = "bel"

                [permission.bash]
                "git status" = "allow"
                "git *" = "ask"
                "*" = "deny"
            "#,
        ),
    ];

    for (spelling, text) in spellings {
        let config = parse(text).expect("a permission table is a config key");

        assert_eq!(
            config.permission.rules(),
            rules(&[
                ("webfetch", "*", Action::Allow),
                ("edit", "*", Action::Ask),
                ("bash", "git status", Action::Allow),
                ("bash", "git *", Action::Ask),
                ("bash", "*", Action::Deny),
            ]),
            "document order, not sorted order, spelled with {spelling}"
        );
    }
}

/// And the order the loader preserved is the order the engine *decides* by —
/// the half the pin above cannot see.
///
/// The pin asserts a list; this asserts an answer. Between them sits
/// `Permissions`, which evaluates last-match-wins, so a reader that sorted
/// would not merely reorder a `Vec` somebody could inspect: it would hand a
/// different verdict to a call nobody would think to re-check. That is the
/// failure this landing's pre-mortem calls the worst it can produce, so it is
/// asserted through the real engine over a real TOML file rather than inferred
/// from the two facts on either side of it.
///
/// The fixture discriminates at **both** levels a sorting reader would touch:
/// the tool keys (`webfetch` before the catch-all `"*"`, which sorts first)
/// and one tool's patterns (`"git push"` before `"git *"`, which also sorts
/// first). Rather than leave a reader to do that alphabetical arithmetic in
/// their head, the sorted ordering is built here and asserted to reach the
/// *opposite* verdict on both — so a fixture that had stopped discriminating
/// fails those assertions instead of passing vacuously.
#[test]
fn a_toml_loaded_config_decides_calls_in_document_order() {
    let config = parse(
        r#"
            [permission]
            webfetch = "allow"
            "*" = "ask"

            [tui]
            notification_method = "bel"

            [permission.bash]
            "git push" = "allow"
            "git *" = "deny"
        "#,
    )
    .expect("a permission table is a config key");

    // What a reader that sorted its keys would have produced instead: the
    // same rules, ranked by name rather than by where the file put them.
    let mut sorted = config.permission.rules();
    sorted.sort_by(|left, right| {
        (&left.permission, &left.pattern).cmp(&(&right.permission, &right.pattern))
    });

    let decide = |rules: Vec<Rule>, tool: &str, args: serde_json::Value| {
        let mut permissions = Permissions::default();
        permissions.set_baseline(rules);

        permissions.gate(tool, &args).action
    };

    // A `git push` is covered by three rules; the last one the document
    // spelled is the `deny`, and it is the one that decides.
    let call = serde_json::json!({ "command": "git push" });
    assert_eq!(decide(config.permission.rules(), "bash", call.clone()), Decision::Deny);
    assert_eq!(
        decide(sorted.clone(), "bash", call),
        Decision::Allow,
        "the fixture discriminates: sorted, the narrower deny stops being last"
    );

    // And at the other level: a `webfetch` is covered by its own rule and by
    // the catch-all written after it, so the catch-all decides.
    let none = serde_json::json!({});
    assert_eq!(decide(config.permission.rules(), "webfetch", none.clone()), Decision::Ask);
    assert_eq!(
        decide(sorted, "webfetch", none),
        Decision::Allow,
        "the fixture discriminates here too: sorted, the catch-all sinks to first"
    );
}

#[test]
fn a_bare_action_covers_every_tool() {
    let config = parse(r#"permission = "ask""#).expect("a bare action is legal");

    let rules = config.permission.rules();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].permission, "*");
    assert_eq!(rules[0].pattern, "*");
    assert_eq!(rules[0].action, Action::Ask);
}

/// Upstream's `mergeDeep`: a re-specified key keeps its position and takes
/// the new value, a new key appends, and a value that is not an object
/// replaces rather than merging.
#[test]
fn merging_permissions_keeps_positions_and_adds_what_is_new() {
    let mut base =
        parse(r#"permission = { bash = { "git *" = "allow", "*" = "ask" }, edit = "ask" }"#)
            .expect("the base tier parses");
    let project = parse(
        r#"permission = { bash = { "*" = "deny", "cargo *" = "allow" }, webfetch = "allow" }"#,
    )
    .expect("the project tier parses");

    base.merge(project);

    let rules: Vec<(String, String, Action)> = base
        .permission
        .rules()
        .into_iter()
        .map(|rule| (rule.permission, rule.pattern, rule.action))
        .collect();
    assert_eq!(
        rules,
        vec![
            ("bash".to_owned(), "git *".to_owned(), Action::Allow),
            ("bash".to_owned(), "*".to_owned(), Action::Deny),
            ("bash".to_owned(), "cargo *".to_owned(), Action::Allow),
            ("edit".to_owned(), "*".to_owned(), Action::Ask),
            ("webfetch".to_owned(), "*".to_owned(), Action::Allow),
        ]
    );
}

#[test]
fn a_bare_action_replaces_the_rules_it_is_merged_over() {
    let mut base =
        parse(r#"permission = { bash = "allow", edit = "allow" }"#).expect("base parses");
    base.merge(parse(r#"permission = "ask""#).expect("the override parses"));

    let rules = base.permission.rules();
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0].permission, "*");
    assert_eq!(rules[0].action, Action::Ask);
}

#[test]
fn merging_replaces_scalars_deepens_objects_and_concatenates_instructions() {
    let mut base = parse(
        r#"model = "anthropic/claude-sonnet-5"
theme = "gruvbox"
instructions = ["docs/style.md", "docs/shared.md"]
agent = { build = { model = "openai/gpt-5.6", description = "builds" } }
keybinds = { app_exit = "ctrl+c" }"#,
    )
    .expect("the base tier parses");
    base.merge(
        parse(
            r#"model = "openai/gpt-5.6"
theme_mode = "light"
instructions = ["docs/shared.md", "docs/local.md"]
agent = { build = { description = "still builds", hidden = true } }
keybinds = { palette_open = "ctrl+p" }"#,
        )
        .expect("the project tier parses"),
    );

    assert_eq!(base.model.as_deref(), Some("openai/gpt-5.6"));
    assert_eq!(base.theme.as_deref(), Some("gruvbox"), "untouched keys stay");
    assert_eq!(base.theme_mode, Some(ThemeMode::Light));
    assert_eq!(
        base.instructions,
        vec!["docs/style.md", "docs/shared.md", "docs/local.md"],
        "instructions concatenate, deduplicated, in order"
    );
    let build = &base.agent["build"];
    assert_eq!(build.model.as_deref(), Some("openai/gpt-5.6"));
    assert_eq!(build.description.as_deref(), Some("still builds"));
    assert_eq!(build.hidden, Some(true));
    assert_eq!(base.keybinds.len(), 2);
}

#[test]
fn the_curated_keys_all_parse() {
    let config = parse(
            r#""$schema" = "https://ganja.invalid/config.json"
model = "anthropic/claude-sonnet-5"
small_model = "anthropic/claude-haiku-4.5"
default_provider = "openai"
default_agent = "plan"
agent = { plan = { mode = "primary", disable = false } }
agents = { concurrency = 3 }
permission = { bash = "ask" }
instructions = ["AGENTS.md"]
theme = "tokyonight"
theme_mode = "dark"
keybinds = { agent_cycle = "tab" }
shell = "/bin/zsh"
command = { ship = { template = "release $ARGUMENTS", agent = "build" } }
mcp = { fs = { type = "local", command = ["bun", "x", "mcp-fs"], output_limit = 8192 } }
hooks = { Stop = [{ hooks = [{ type = "command", command = "notify" }] }] }
provider = { local-llama = { dialect = "openai-chat-completions", base_url = "http://127.0.0.1:11434/v1" } }"#,
        )
        .expect("every curated key is a key");

    assert!(config.schema.is_some());
    assert_eq!(config.default_provider.as_deref(), Some("openai"));
    assert_eq!(config.default_agent.as_deref(), Some("plan"));
    assert_eq!(config.agent["plan"].mode, Some(AgentMode::Primary));
    assert_eq!(config.agent["plan"].disable, Some(false));
    assert_eq!(config.agents.concurrency(), 3);
    assert_eq!(config.theme_mode, Some(ThemeMode::Dark));
    assert_eq!(config.shell.as_deref(), Some("/bin/zsh"));
    assert_eq!(config.command["ship"].template, "release $ARGUMENTS");
    assert_eq!(config.command["ship"].agent.as_deref(), Some("build"));
    assert!(config.command["ship"].description.is_none());
    assert!(matches!(config.mcp["fs"], McpServer::Local(_)));
    assert_eq!(config.hooks["Stop"].len(), 1);
    assert_eq!(config.provider["local-llama"].dialect, Some(Dialect::OpenaiChatCompletions));
}

/// The key is a scalar like `model`, and merges like one: a tier that
/// says nothing leaves the tier below it alone, and a closer one replaces.
#[test]
fn a_closer_tier_decides_the_default_provider_only_when_it_names_one() {
    let mut merged = parse(r#"default_provider = "anthropic""#).expect("it parses");
    merged.merge(parse(r#"model = "openai/gpt-5.6""#).expect("it parses"));
    assert_eq!(merged.default_provider.as_deref(), Some("anthropic"));

    merged.merge(parse(r#"default_provider = "openai""#).expect("it parses"));
    assert_eq!(merged.default_provider.as_deref(), Some("openai"));
}

/// The seeded effort is a scalar too, and merges like every other one —
/// which is what lets a global file set the house default and one project
/// run hotter without restating anything else.
#[test]
fn a_closer_tier_decides_the_effort_only_when_it_names_one() {
    let mut merged = parse(r#"effort = "high""#).expect("it parses");
    assert_eq!(merged.effort.as_deref(), Some("high"));

    merged.merge(parse(r#"model = "openai/gpt-5.6""#).expect("it parses"));
    assert_eq!(merged.effort.as_deref(), Some("high"));

    merged.merge(parse(r#"effort = "max""#).expect("it parses"));
    assert_eq!(merged.effort.as_deref(), Some("max"));
}

/// Nothing about an effort is checked here: which names exist depends on
/// the model the other tiers settle on, so the loader carries whatever was
/// written and adoption decides. What is still refused is a *sibling* the
/// struct has no field for — the discipline every key here holds.
#[test]
fn an_effort_name_is_carried_unvalidated_and_its_neighbours_still_are_not() {
    let config = parse(r#"effort = "wildly-not-an-effort""#).expect("it parses");
    assert_eq!(config.effort.as_deref(), Some("wildly-not-an-effort"));

    let error = parse(r#"efort = "high""#).expect_err("a typo is refused");
    assert!(format!("{error}").contains("efort"), "got {error}");
}

#[test]
fn a_model_string_splits_on_its_first_slash() {
    let cases = [
        ("anthropic/claude-sonnet-5", Some("anthropic"), "claude-sonnet-5"),
        ("openrouter/anthropic/claude-3", Some("openrouter"), "anthropic/claude-3"),
        ("claude-sonnet-5", None, "claude-sonnet-5"),
    ];

    for (spelled, provider, model) in cases {
        assert_eq!(split_model(spelled), (provider, model), "splitting {spelled}");
    }
}

/// The binding rule both config model keys ride, in every direction it has
/// (bead `s4w`): a prefix naming the selected provider applies, a bare
/// spec applies to whoever is running, and a prefix naming somebody else
/// yields nothing at all — never the stripped tail, which is what reached
/// a live openai request as a bare `claude-x` and came back a 400.
#[test]
fn a_prefixed_model_binds_to_the_provider_it_names_and_to_no_other() {
    const KEY: &str = "the config's `model` key";

    assert_eq!(
        model_bound_to("anthropic/claude-sonnet-5", "anthropic", KEY),
        Some("claude-sonnet-5")
    );
    assert_eq!(
        model_bound_to("cursor/claude-x", "openai", KEY),
        None,
        "the prefix names cursor, so openai is not asked for its tail"
    );
    assert_eq!(
        model_bound_to("claude-sonnet-5", "openai", KEY),
        Some("claude-sonnet-5"),
        "a bare spec claims no provider, so it applies to whichever is running"
    );

    // Only the first slash separates, so a gateway's own two-part model id
    // survives the rule intact.
    assert_eq!(
        model_bound_to("openrouter/anthropic/claude-3", "openrouter", KEY),
        Some("anthropic/claude-3")
    );

    // A config-declared endpoint is compared exactly as a builtin is:
    // there is no shipped list here to fall out of date, so an id this
    // build has never heard of binds to itself and to nothing else.
    assert_eq!(
        model_bound_to("local-llama/tiny-instruct", "local-llama", KEY),
        Some("tiny-instruct")
    );
    assert_eq!(model_bound_to("local-llama/tiny-instruct", "anthropic", KEY), None);
    assert_eq!(
        model_bound_to("anthropic/claude-sonnet-5", "local-llama", KEY),
        None,
        "a builtin's spec does not travel to a config's endpoint either"
    );
}

#[test]
fn a_directory_offers_the_one_name_there_is() {
    let directory = temporary();
    plant(&directory.path().join("ganja.toml"), "");

    let found = existing(directory.path()).expect("a directory holding the config is no error");
    assert_eq!(found.len(), 1);
    assert!(found[0].ends_with("ganja.toml"), "{found:?}");
}

/// Both legacy names, one at a time, and each refused by its own path — the
/// message has to name the file somebody has to convert, not the format in
/// general, because the walk can find more than one.
#[test]
fn a_legacy_name_in_a_directory_is_refused_by_path() {
    for name in super::LEGACY_FILES {
        let directory = temporary();
        let path = directory.path().join(name);
        plant(&path, "{}");

        let error = existing(directory.path()).expect_err("the old format is not read");
        let ConfigError::Legacy { path: named } = &error else {
            panic!("expected a legacy refusal, got {error:?}");
        };
        assert_eq!(named, &path);
        assert!(
            error.to_string().contains("ganja config migrate"),
            "the refusal says what to run: {error}"
        );
    }
}

/// And a `ganja.toml` beside it does **not** settle it. Reading the new file
/// and silently skipping the old one is the ignored-setting failure this whole
/// key set exists to prevent, so a half-migrated directory is refused until
/// somebody removes what it no longer reads.
#[test]
fn a_legacy_file_is_refused_even_with_the_config_beside_it() {
    let directory = temporary();
    let legacy = directory.path().join("ganja.jsonc");
    plant(&legacy, r#"model = "anthropic/claude-sonnet-5""#);
    plant(&directory.path().join("ganja.toml"), r#"model = "openai/gpt-5.6""#);

    let error = existing(directory.path()).expect_err("the old file is still there to answer for");

    assert!(matches!(&error, ConfigError::Legacy { path } if path == &legacy));
}

/// Every ancestor up to the project root contributes, outermost first, so
/// that the closest directory has the last word.
#[test]
fn the_project_walk_stacks_from_the_root_down_to_the_working_directory() {
    let directory = temporary();
    let root = directory.path().join("api");
    let nested = root.join("crates").join("core");
    fs::create_dir_all(&nested).expect("the fixture tree is creatable");
    fs::create_dir(root.join(".git")).expect("the fixture repository is creatable");
    plant(&root.join("ganja.toml"), "");
    plant(&nested.join("ganja.toml"), "");

    let found = project_files(&nested).expect("the walk finds only the one name");
    let names: Vec<String> = found
        .iter()
        .map(|path| {
            let parent = path.parent().expect("a config file has a directory");
            format!(
                "{}/{}",
                parent.file_name().unwrap_or_default().to_string_lossy(),
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        })
        .collect();

    assert_eq!(
        names,
        vec!["api/ganja.toml", "core/ganja.toml"],
        "the root's first, so the closest has the last word"
    );
}

#[test]
fn the_walk_stops_at_the_project_root() {
    let directory = temporary();
    let root = directory.path().join("api");
    fs::create_dir_all(&root).expect("the fixture tree is creatable");
    fs::create_dir(root.join(".git")).expect("the fixture repository is creatable");
    plant(&directory.path().join("ganja.toml"), "");
    plant(&root.join("ganja.toml"), "");

    let found = project_files(&root).expect("the walk parses");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with(fs::canonicalize(&root).expect("the fixture exists")));
}

/// A closer file wins the keys it names and leaves the rest alone, which is
/// the whole point of stacking them.
#[test]
fn the_closest_project_file_wins() {
    let directory = temporary();
    let root = directory.path().join("api");
    let nested = root.join("crates");
    fs::create_dir_all(&nested).expect("the fixture tree is creatable");
    fs::create_dir(root.join(".git")).expect("the fixture repository is creatable");
    plant(&root.join("ganja.toml"), "model = \"anthropic/claude-sonnet-5\"\ntheme = \"gruvbox\"\n");
    plant(&nested.join("ganja.toml"), r#"model = "openai/gpt-5.6""#);

    // The project tier alone, so the machine running the suite cannot
    // contribute a global config of its own. Which tiers stack in which
    // order is `tests/config.rs`'s to prove, where the environment that
    // decides it can be set.
    let config =
        merge_files(&project_files(&nested).expect("the walk parses")).expect("both tiers parse");

    assert_eq!(config.model.as_deref(), Some("openai/gpt-5.6"));
    assert_eq!(config.theme.as_deref(), Some("gruvbox"));
}

/// The refusal is not a property of the directory the walk starts in: an
/// ancestor holding a legacy file refuses the load from wherever the session
/// was launched, which is the only way a file somebody forgot about gets
/// mentioned at all.
#[test]
fn a_legacy_file_anywhere_in_the_walk_refuses_the_load() {
    let directory = temporary();
    let root = directory.path().join("api");
    let nested = root.join("crates");
    fs::create_dir_all(&nested).expect("the fixture tree is creatable");
    fs::create_dir(root.join(".git")).expect("the fixture repository is creatable");
    let legacy = root.join("ganja.jsonc");
    plant(&legacy, r#"model = "anthropic/claude-sonnet-5""#);
    plant(&nested.join("ganja.toml"), r#"model = "openai/gpt-5.6""#);

    let error = project_files(&nested).expect_err("an ancestor's old file is still its author's");

    // The walk canonicalises what it starts from, so the path the refusal
    // carries is the canonical one — which is the path somebody would have to
    // go and convert.
    let named = fs::canonicalize(&legacy).expect("the fixture exists");
    assert!(matches!(&error, ConfigError::Legacy { path } if path == &named));
}

/// The config home's two *discovered* candidates, ruled on by what is
/// there. The environment tier above them needs the environment and is
/// pinned in `tests/skills.rs`, which owns that binary's variables; these
/// three need only two directories, so they say the same thing on every
/// machine.
#[test]
fn the_xdg_home_answers_whenever_it_is_there() {
    let directory = temporary();
    let xdg = directory.path().join("config").join("ganja");
    let dotted = directory.path().join(".ganja");
    fs::create_dir_all(&xdg).expect("the fixture is creatable");

    assert_eq!(super::discovered(xdg.clone(), dotted.clone()), xdg);

    // And still, with the dotted one beside it: this is one home, not a
    // merge, and the higher tier is the one that answers.
    fs::create_dir_all(&dotted).expect("the fixture is creatable");
    assert_eq!(super::discovered(xdg.clone(), dotted), xdg);
}

#[test]
fn the_dotted_home_answers_only_when_the_xdg_one_is_absent() {
    let directory = temporary();
    let xdg = directory.path().join("config").join("ganja");
    let dotted = directory.path().join(".ganja");
    fs::create_dir_all(&dotted).expect("the fixture is creatable");

    assert_eq!(super::discovered(xdg.clone(), dotted.clone()), dotted);

    // A file where the directory would be is not a home either — the check
    // is `is_dir`, not "something is there".
    fs::create_dir_all(xdg.parent().expect("a parent")).expect("creatable");
    fs::write(&xdg, "not a directory").expect("writable");
    assert_eq!(super::discovered(xdg, dotted.clone()), dotted);
}

/// Nothing on disk: nothing to read either way, so what comes back is the
/// one whoever writes next should create. See [`super::config_home`] for
/// why that is the XDG path and not the dotted one.
#[test]
fn with_neither_on_disk_the_answer_is_the_one_a_writer_should_create() {
    let directory = temporary();
    let xdg = directory.path().join("config").join("ganja");
    let dotted = directory.path().join(".ganja");

    assert_eq!(super::discovered(xdg.clone(), dotted), xdg);
}

/// A `ResponsesOptions` table setting every field but `model`, which
/// `set_keys_reports_every_key_a_fully_populated_table_sets` proves.
const EVERY_FIELD: &str = r#"
service_tier = "flex"
reasoning = { context = "all_turns", summary = "concise", mode = "pro" }
text = { verbosity = "low" }
parallel_tool_calls = false
stream_options = { include_obfuscation = true }
tool_choice = "required"
custom_tools = ["bash"]
server_tools = [{ type = "web_search" }]
include = ["web_search_call.action.sources"]
context_management = [{ type = "compaction", compact_threshold = 1000 }]
client_metadata = { origin = "ganja" }
access_programs = { program = "research" }
max_output_tokens = 100
max_tool_calls = 3
prompt_cache_key = "k"
prompt_cache_retention = "24h"
prompt_cache_options = { mode = "explicit", ttl = "1h", comparison_response_id = "r" }
temperature = 0.2
top_p = 0.9
top_logprobs = 2
truncation = "auto"
safety_identifier = "s"
user = "u"
metadata = { team = "core" }
moderation = { model = "omni", policy = "strict" }
"#;

/// **D563.** `ResponsesOptions::set_keys` is a hand-written list, so a field
/// added to the struct and forgotten there would be a key that skips the
/// per-id gate. Pinned from both ends: the fixture sets every field the struct
/// serializes (so a new field without a fixture line fails here), and the keys
/// it reports are both ids' vocabulary together, both ways (so a field without
/// a `set_keys` line fails here too) — the union, since the 2026-09-17 probe
/// left a key on each id's list alone.
#[test]
fn set_keys_reports_every_key_a_fully_populated_table_sets() {
    let every: super::ResponsesOptions = toml::from_str(EVERY_FIELD).expect("the fixture decodes");

    let serde_json::Value::Object(fields) = serde_json::to_value(&every).expect("it serializes")
    else {
        panic!("a struct serializes as an object");
    };
    for (field, value) in &fields {
        if field == "model" {
            continue;
        }
        let unset = match value {
            serde_json::Value::Null => true,
            serde_json::Value::Array(items) => items.is_empty(),
            serde_json::Value::Object(members) => members.is_empty(),
            _ => false,
        };
        assert!(!unset, "the fixture leaves `{field}` unset, so this pin cannot see it");
    }

    let reported: std::collections::BTreeSet<&str> = every.set_keys().into_iter().collect();
    let accepted: std::collections::BTreeSet<&str> =
        crate::provider::responses::options::PLATFORM_ACCEPTED
            .iter()
            .chain(crate::provider::responses::options::SEAT_ACCEPTED)
            .copied()
            .collect();
    assert_eq!(
        reported.difference(&accepted).collect::<Vec<_>>(),
        Vec::<&&str>::new(),
        "set_keys reports a key neither id's list names"
    );
    assert_eq!(
        accepted.difference(&reported).collect::<Vec<_>>(),
        Vec::<&&str>::new(),
        "a key a fully populated table sets is missing from set_keys"
    );
}

/// **D563.** `ResponsesOptions::for_model` is the third hand-written list over
/// the struct, beside `set_keys` and the wire's `body()`, and a field it
/// forgets is a per-model entry that loads and is silently ignored. Pinned from
/// both sides with the fixture `set_keys` is pinned by: a per-model entry
/// setting every field overlays all of them onto an empty table, and an empty
/// per-model entry leaves a table setting every field as it was.
#[test]
fn for_model_carries_every_field_a_fully_populated_entry_sets() {
    let every: super::ResponsesOptions = toml::from_str(EVERY_FIELD).expect("the fixture decodes");

    let entry: super::ResponsesOptions = toml::from_str(&format!("[model.m]\n{EVERY_FIELD}"))
        .expect("the fixture decodes as a per-model entry");
    assert_eq!(entry.for_model("m"), every, "a field a per-model entry sets is not overlaid");

    let wide: super::ResponsesOptions = toml::from_str(&format!("{EVERY_FIELD}\n[model.m]\n"))
        .expect("the fixture decodes beside an empty per-model entry");
    assert_eq!(wide.for_model("m"), every, "an empty per-model entry moved a provider-wide field");
}
