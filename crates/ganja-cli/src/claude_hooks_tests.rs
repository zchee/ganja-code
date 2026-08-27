//! What a settings file's hooks become, and what is reported instead.
//!
//! Which settings file is read and which `ganja.toml` is written are settled
//! in `tests/import_claude_hooks.rs`, through the built binary. What is
//! settled here is the extraction and the merge: the rows, the refusals, and
//! the fact that an append lands after what was already there.

use std::path::Path;

use super::*;

/// The path every fixture is attributed to, so a failure names something.
const FIXTURE: &str = "settings.json";

/// The target every merge fixture writes into.
const TARGET: &str = "ganja.toml";

/// Extracts `text`, hands back the groups and the report.
fn collected(text: &str) -> (BTreeMap<&'static str, Vec<Group>>, Report) {
    let directory = ganja_testkit::temp_dir();
    let path = directory.path().join(FIXTURE);
    fs::write(&path, text).expect("the fixture is writable");

    let mut groups = BTreeMap::new();
    let mut report = Report::default();
    collect(&path, &mut groups, &mut report).expect("the fixture is read");

    (groups, report)
}

/// The reason a row gives for `key`, or nothing when there is no such row.
///
/// Rows name the file they came from, since the project tier reads two and
/// both have a `hooks.Stop[0]`; every fixture here is [`FIXTURE`], so the
/// prefix is added rather than spelled at each call.
fn why(report: &Report, key: &str) -> Option<String> {
    let key = format!("{FIXTURE}:{key}");

    report
        .skipped
        .iter()
        .find(|(left, _)| *left == key)
        .map(|(_, reason)| reason.clone())
}

/// Merges `text`'s hooks into `target`, and hands back what would be written.
fn merged(target: &str, text: &str) -> String {
    let (groups, _) = collected(text);

    merge(Path::new(TARGET), target, &groups)
        .expect("the target takes the append")
        .to_string()
}

/// The ordinary case, end to end: a group ganja fires travels whole, and the
/// deadline is not converted on the way — both sides count seconds, and a
/// conversion here would surface months later as a hook killed early.
#[test]
fn a_group_for_an_event_this_build_fires_travels_whole() {
    let rendered = merged(
        "",
        r#"{
             "hooks": {
               "PreToolUse": [
                 {
                   "matcher": "Bash",
                   "hooks": [{ "type": "command", "command": "./check.sh", "timeout": 5 }]
                 }
               ]
             }
           }"#,
    );

    let config = toml_edit::de::from_str::<Config>(&rendered)
        .unwrap_or_else(|error| panic!("the merged document loads: {error}\n{rendered}"));
    let groups = &config.hooks["PreToolUse"];
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].matcher.as_deref(), Some("Bash"));

    let ganja_core::config::HookHandler::Command(handler) = &groups[0].hooks[0];
    assert_eq!(handler.command, "./check.sh");
    assert_eq!(
        handler.timeout,
        Some(5),
        "seconds on both sides, unconverted"
    );
}

/// A group with no matcher matches everything, on both sides, so the absent
/// key stays absent rather than becoming an empty pattern.
#[test]
fn a_group_with_no_matcher_keeps_none() {
    let rendered = merged(
        "",
        r#"{ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "./x.sh" }] }] } }"#,
    );

    assert!(!rendered.contains("matcher"), "{rendered}");
    let config = toml_edit::de::from_str::<Config>(&rendered).expect("it loads");
    assert_eq!(config.hooks["Stop"][0].matcher, None);
}

/// Appending, not replacing: two tiers naming one event both fire, which is
/// the config system's own answer for what a second group means — so an import
/// lands *after* what the file already said rather than on top of it.
#[test]
fn groups_are_appended_after_the_ones_already_there() {
    let rendered = merged(
        "[[hooks.PreToolUse]]\nmatcher = \"Edit\"\n\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"./first.sh\"\n",
        r#"{
             "hooks": {
               "PreToolUse": [
                 { "matcher": "Bash", "hooks": [{ "type": "command", "command": "./second.sh" }] }
               ]
             }
           }"#,
    );

    let config = toml_edit::de::from_str::<Config>(&rendered).expect("it loads");
    let matchers: Vec<_> = config.hooks["PreToolUse"]
        .iter()
        .map(|group| group.matcher.clone())
        .collect();
    assert_eq!(
        matchers,
        [Some("Edit".to_owned()), Some("Bash".to_owned())],
        "the one that was there is still first:\n{rendered}"
    );
}

/// The target is a file somebody wrote by hand, so the edit keeps their
/// comments and the position of everything it did not touch — `ganja mcp add`'s
/// contract (D483), which this shares because it edits the same file.
#[test]
fn the_target_keeps_its_comments_and_its_other_keys() {
    let rendered = merged(
        "# The model this checkout uses.\nmodel = \"anthropic/claude-sonnet-5\"\n\n[permission]\nedit = \"ask\"\n",
        r#"{ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "./x.sh" }] }] } }"#,
    );

    assert!(
        rendered.starts_with("# The model this checkout uses.\nmodel = "),
        "the comment and its key are where they were:\n{rendered}"
    );
    assert!(rendered.contains("[permission]"), "{rendered}");
    assert!(rendered.contains("[[hooks.Stop]]"), "{rendered}");
}

/// A file that spelled its list inline is a legal file, so it is appended to
/// in its own spelling rather than refused or rewritten into another one.
#[test]
fn a_list_spelled_inline_is_appended_to_inline() {
    let rendered = merged(
        "hooks.Stop = [{ hooks = [{ type = \"command\", command = \"./first.sh\" }] }]\n",
        r#"{ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "./second.sh" }] }] } }"#,
    );

    let config = toml_edit::de::from_str::<Config>(&rendered)
        .unwrap_or_else(|error| panic!("the merged document loads: {error}\n{rendered}"));
    assert_eq!(config.hooks["Stop"].len(), 2, "{rendered}");
    assert!(
        !rendered.contains("[[hooks.Stop]]"),
        "the file's own spelling is kept:\n{rendered}"
    );
}

/// An event this build fires nothing for is a row and not an error: the run is
/// worth having for the events that do map, and a person cannot fix what they
/// were not told about.
#[test]
fn an_event_this_build_does_not_fire_is_reported_by_name() {
    let (groups, report) = collected(
        r#"{
             "hooks": {
               "PreCompact": [{ "hooks": [{ "type": "command", "command": "./a.sh" }] }],
               "PreResponse": [{ "hooks": [{ "type": "command", "command": "./b.sh" }] }]
             }
           }"#,
    );

    assert_eq!(why(&report, "hooks.PreResponse").as_deref(), Some("unrun"));
    assert!(
        groups.contains_key("PreCompact") && !groups.contains_key("PreResponse"),
        "the event this build fires still travels"
    );
}

/// The same for a handler kind: `http`, `prompt` and `agent` are Claude's and
/// not this build's, and each is one row rather than a silent absence.
#[test]
fn a_handler_this_build_cannot_run_is_reported_and_the_rest_of_the_group_travels() {
    let (groups, report) = collected(
        r#"{
             "hooks": {
               "Stop": [
                 {
                   "hooks": [
                     { "type": "prompt", "prompt": "summarise" },
                     { "type": "command", "command": "./x.sh" }
                   ]
                 }
               ]
             }
           }"#,
    );

    assert_eq!(
        why(&report, "hooks.Stop[0].hooks[0]").as_deref(),
        Some("unsupported")
    );
    assert_eq!(
        groups["Stop"][0].handlers.len(),
        1,
        "the command handler beside it still travels"
    );
}

/// A field neither side's shape names is reported by name, so a Claude release
/// that adds one degrades to a visible row rather than a silent drop.
#[test]
fn a_field_neither_side_names_is_reported_by_name() {
    let (groups, report) = collected(
        r#"{
             "hooks": {
               "Stop": [
                 {
                   "description": "why this exists",
                   "hooks": [{ "type": "command", "command": "./x.sh", "retries": 3 }]
                 }
               ]
             }
           }"#,
    );

    assert_eq!(
        why(&report, "hooks.Stop[0].description").as_deref(),
        Some("unknown")
    );
    assert_eq!(
        why(&report, "hooks.Stop[0].hooks[0].retries").as_deref(),
        Some("unknown")
    );
    assert_eq!(groups["Stop"].len(), 1, "the group itself still travels");
}

/// The refusal the loader would make at the next launch, made here instead: a
/// handler with nothing to run makes the *whole file* unreadable, so writing
/// one would stop a session over a hook nobody wanted.
#[test]
fn a_group_this_build_would_refuse_to_load_is_left_out() {
    let (groups, report) = collected(
        r#"{
             "hooks": {
               "Stop": [
                 { "hooks": [{ "type": "command", "command": "   " }] },
                 { "hooks": [{ "type": "command", "command": "./x.sh" }] }
               ]
             }
           }"#,
    );

    assert_eq!(why(&report, "hooks.Stop[0]").as_deref(), Some("refused"));
    assert_eq!(
        report.warnings,
        [format!(
            "{FIXTURE}:hooks.Stop[0] was left out — a command handler with no command"
        )],
        "the word in the column is not the reason; the sentence beside it is"
    );
    assert_eq!(
        groups["Stop"].len(),
        1,
        "the group beside it is still imported"
    );
}

/// The other half of `check_hooks`, judged by the same engine the loader
/// judges it with: a matcher that does not compile is a group that matches
/// nothing forever, and a file holding one does not load at all — so the
/// group is left out and the reason is a sentence, not a word.
#[test]
fn a_matcher_that_is_not_a_regular_expression_is_left_out() {
    let (groups, report) = collected(
        r#"{
             "hooks": {
               "Stop": [
                 { "matcher": "Edit(", "hooks": [{ "type": "command", "command": "./a.sh" }] },
                 { "matcher": "Edit|Write", "hooks": [{ "type": "command", "command": "./b.sh" }] }
               ]
             }
           }"#,
    );

    assert_eq!(why(&report, "hooks.Stop[0]").as_deref(), Some("refused"));
    assert!(
        report.warnings[0].contains("not a regular expression"),
        "the sentence says which refusal it was: {:?}",
        report.warnings
    );
    assert_eq!(
        groups["Stop"].len(),
        1,
        "the group with a matcher that compiles is still imported"
    );
}

/// An empty matcher is not a pattern that failed to compile — it means
/// "everything", on both sides — so it is not put to the engine, exactly as
/// `check_hooks` does not put it.
#[test]
fn an_empty_matcher_is_not_a_matcher_that_failed_to_compile() {
    let (groups, report) = collected(
        r#"{ "hooks": { "Stop": [{ "matcher": "", "hooks": [{ "type": "command", "command": "./x.sh" }] }] } }"#,
    );

    assert_eq!(why(&report, "hooks.Stop[0]"), None);
    assert_eq!(groups["Stop"].len(), 1);
}

/// A group whose every handler was left out has nothing left to run, and
/// writing an empty group would be writing noise.
#[test]
fn a_group_with_nothing_left_to_run_is_reported_empty() {
    let (groups, report) = collected(
        r#"{ "hooks": { "Stop": [{ "hooks": [{ "type": "http", "url": "https://x.test" }] }] } }"#,
    );

    assert_eq!(why(&report, "hooks.Stop[0]").as_deref(), Some("empty"));
    assert!(groups.is_empty());
}

/// Every other key of a settings file is a row, because somebody who ran this
/// expecting their permissions to come across should be told in the same
/// breath that they did not.
#[test]
fn every_key_this_command_does_not_read_is_a_row() {
    let (_, report) = collected(
        r#"{
             "model": "claude-sonnet-4-5",
             "permissions": { "allow": ["Bash(git status)"] },
             "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "./x.sh" }] }] }
           }"#,
    );

    assert_eq!(why(&report, "model").as_deref(), Some("unread"));
    assert_eq!(why(&report, "permissions").as_deref(), Some("unread"));
    assert_eq!(why(&report, "hooks"), None, "hooks is what this reads");
}

/// A value that is not the shape its key takes is reported where it was
/// written, and never guessed at.
#[test]
fn a_value_of_the_wrong_shape_is_reported_where_it_was_written() {
    let (groups, report) = collected(
        r#"{
             "hooks": {
               "Stop": [
                 { "matcher": 3, "hooks": [{ "type": "command", "command": "./x.sh" }] },
                 { "hooks": [{ "type": "command", "command": "./y.sh", "timeout": "soon" }] }
               ]
             }
           }"#,
    );

    assert_eq!(
        why(&report, "hooks.Stop[0].matcher").as_deref(),
        Some("malformed")
    );
    assert_eq!(
        why(&report, "hooks.Stop[1].hooks[0].timeout").as_deref(),
        Some("malformed")
    );
    assert!(
        groups.is_empty(),
        "neither group is guessed at: one lost its matcher, the other its only handler"
    );
}

/// The mapped section names where each group lands and what each of its
/// handlers runs, which is the half of the table a person checks before
/// letting it write.
///
/// The command rows are not decoration: a hook runs with the user's own
/// authority and crosses no permission dialog, so the command line is the
/// whole of what there is to review, and the group row alone would name a
/// destination without naming the payload.
#[test]
fn a_group_that_travels_is_a_row_naming_where_it_lands_and_a_row_per_command() {
    let (_, report) = collected(
        r#"{ "hooks": { "Stop": [{ "hooks": [
             { "type": "command", "command": "./x.sh" },
             { "type": "command", "command": "./y.sh --now" }
           ] }] } }"#,
    );

    assert_eq!(
        report.mapped,
        [
            (
                format!("{FIXTURE}:hooks.Stop[0]"),
                "[[hooks.Stop]]".to_owned()
            ),
            (
                format!("{FIXTURE}:hooks.Stop[0].hooks[0]"),
                "./x.sh".to_owned()
            ),
            (
                format!("{FIXTURE}:hooks.Stop[0].hooks[1]"),
                "./y.sh --now".to_owned()
            ),
        ]
    );
}

/// Both sections of the table index the settings file's own array.
///
/// The mapped rows are written from the handlers that survived and the skipped
/// rows from the source array as it is walked, so numbering the mapped ones by
/// their position among the survivors would put two different `settings.json`
/// entries under one path in one table — a left column somebody cannot go and
/// look at. This fixture is that collision exactly: the third entry is the
/// second survivor, so a filtered index would have written `hooks[1]` for
/// `./two.sh` — the path the skipped section already uses for the handler
/// this build cannot run.
#[test]
fn the_mapped_and_skipped_rows_number_the_same_array() {
    let (_, report) = collected(
        r#"{ "hooks": { "Stop": [{ "hooks": [
             { "type": "command", "command": "./zero.sh" },
             { "type": "prompt", "prompt": "summarise" },
             { "type": "command", "command": "./two.sh" },
             { "type": "command", "command": "./three.sh", "timeout": -1 }
           ] }] } }"#,
    );

    let at = |index: usize| format!("{FIXTURE}:hooks.Stop[0].hooks[{index}]");
    assert_eq!(
        report
            .mapped
            .iter()
            .filter(|(key, _)| key.contains(".hooks["))
            .cloned()
            .collect::<Vec<_>>(),
        [
            (at(0), "./zero.sh".to_owned()),
            (at(2), "./two.sh".to_owned()),
        ]
    );

    // The two that fell out, at the positions they actually sit at.
    assert_eq!(
        why(&report, "hooks.Stop[0].hooks[1]").as_deref(),
        Some("unsupported")
    );
    assert_eq!(
        why(&report, "hooks.Stop[0].hooks[3].timeout").as_deref(),
        Some("malformed")
    );

    // And the claim that makes the two lists one table: no path is used by
    // both sections, which is what a filtered index would have broken.
    for (key, _) in &report.mapped {
        assert!(
            !report.skipped.iter().any(|(skipped, _)| skipped == key),
            "{key} is in both sections"
        );
    }
}

/// A handler the group dropped leaves no command row, because the row is a
/// claim that this command line is about to be installed.
#[test]
fn a_handler_that_was_left_out_gets_no_command_row() {
    let (_, report) = collected(
        r#"{ "hooks": { "Stop": [{ "hooks": [
             { "type": "prompt", "prompt": "summarise" },
             { "type": "command", "command": "./y.sh" }
           ] }] } }"#,
    );

    let commands: Vec<&String> = report
        .mapped
        .iter()
        .filter(|(key, _)| key.contains(".hooks["))
        .map(|(_, value)| value)
        .collect();
    assert_eq!(commands, ["./y.sh"]);
    assert_eq!(
        why(&report, "hooks.Stop[0].hooks[0]").as_deref(),
        Some("unsupported")
    );
}

/// A settings file with no hooks at all is not an error — it is a run that
/// says so, having reported every key it did not read.
#[test]
fn a_settings_file_with_no_hooks_is_not_an_error() {
    let (groups, report) = collected(r#"{ "model": "claude-sonnet-4-5" }"#);

    assert!(groups.is_empty());
    assert_eq!(why(&report, "model").as_deref(), Some("unread"));
}

/// A target whose `hooks` is not a list of groups is refused rather than
/// replaced: whatever it is, it is not this command's to throw away.
#[test]
fn a_target_whose_hooks_are_not_groups_is_refused() {
    let (groups, _) = collected(
        r#"{ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "./x.sh" }] }] } }"#,
    );
    let error = merge(
        Path::new(TARGET),
        "[hooks]\nStop = \"every-one\"\n",
        &groups,
    )
    .expect_err("a string is not a list of groups");

    assert!(
        format!("{error}").contains("hooks.Stop"),
        "the refusal names what it found: {error}"
    );
}
