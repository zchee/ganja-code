use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

use super::{EVENTS, HookEvent, Hooks, Outcome, Payload, Source, Trigger, envelope};
use crate::config::{HookCommand, HookHandler, HookMatcher};

/// A `hooks` block as a config file would have described it.
fn configured(
    event: HookEvent,
    matcher: Option<&str>,
    commands: &[&str],
) -> BTreeMap<String, Vec<HookMatcher>> {
    let mut config = BTreeMap::new();
    config.insert(
        event.name().to_owned(),
        vec![HookMatcher {
            matcher: matcher.map(str::to_owned),
            hooks: commands
                .iter()
                .map(|command| {
                    HookHandler::Command(HookCommand {
                        command: (*command).to_owned(),
                        timeout: None,
                    })
                })
                .collect(),
        }],
    );

    config
}

/// The compiled registry that block describes.
fn built(
    event: HookEvent,
    matcher: Option<&str>,
    commands: &[&str],
    cwd: &Path,
) -> std::sync::Arc<Hooks> {
    Hooks::new(&configured(event, matcher, commands), cwd).expect("the block describes hooks")
}

#[test]
fn every_event_round_trips_through_its_name() {
    for event in EVENTS {
        assert_eq!(HookEvent::from_name(event.name()), Some(event));
    }
    assert_eq!(HookEvent::from_name("PreToolUseX"), None);
    assert_eq!(HookEvent::from_name("pretooluse"), None);
}

/// The envelope every event puts on a hook's standard input, pinned field
/// by field. **No `transcript_path`** anywhere in it (D457).
#[test]
fn the_stdin_envelope_is_the_documented_one_for_every_event() {
    let cwd = Path::new("/tmp/project");
    let cases = [
        (
            Payload::PreToolUse {
                tool_name: "edit".to_owned(),
                tool_input: json!({ "file_path": "a.rs" }),
            },
            json!({
                "session_id": "ses_1",
                "cwd": "/tmp/project",
                "hook_event_name": "PreToolUse",
                "tool_name": "edit",
                "tool_input": { "file_path": "a.rs" },
            }),
        ),
        (
            Payload::PostToolUse {
                tool_name: "mcp__docs__search".to_owned(),
                tool_input: json!({ "query": "hooks" }),
                tool_response: json!({ "output": "found", "title": "search", "metadata": null }),
            },
            json!({
                "session_id": "ses_1",
                "cwd": "/tmp/project",
                "hook_event_name": "PostToolUse",
                "tool_name": "mcp__docs__search",
                "tool_input": { "query": "hooks" },
                "tool_response": { "output": "found", "title": "search", "metadata": null },
            }),
        ),
        (
            Payload::UserPromptSubmit { prompt: "ship it".to_owned() },
            json!({
                "session_id": "ses_1",
                "cwd": "/tmp/project",
                "hook_event_name": "UserPromptSubmit",
                "prompt": "ship it",
            }),
        ),
        (
            Payload::Notification { message: "ganja needs your permission to use bash".to_owned() },
            json!({
                "session_id": "ses_1",
                "cwd": "/tmp/project",
                "hook_event_name": "Notification",
                "message": "ganja needs your permission to use bash",
            }),
        ),
        (
            Payload::Stop { stop_hook_active: false },
            json!({
                "session_id": "ses_1",
                "cwd": "/tmp/project",
                "hook_event_name": "Stop",
                "stop_hook_active": false,
            }),
        ),
        (
            Payload::SubagentStop {
                stop_hook_active: false,
                agent: "explore".to_owned(),
                outcome: "completed".to_owned(),
            },
            json!({
                "session_id": "ses_1",
                "cwd": "/tmp/project",
                "hook_event_name": "SubagentStop",
                "stop_hook_active": false,
                "agent": "explore",
                "outcome": "completed",
            }),
        ),
        (
            Payload::SessionStart { source: Source::Resume },
            json!({
                "session_id": "ses_1",
                "cwd": "/tmp/project",
                "hook_event_name": "SessionStart",
                "source": "resume",
            }),
        ),
        (
            Payload::SessionEnd { reason: "exit".to_owned() },
            json!({
                "session_id": "ses_1",
                "cwd": "/tmp/project",
                "hook_event_name": "SessionEnd",
                "reason": "exit",
            }),
        ),
        (
            Payload::PreCompact { trigger: Trigger::Auto },
            json!({
                "session_id": "ses_1",
                "cwd": "/tmp/project",
                "hook_event_name": "PreCompact",
                "trigger": "auto",
            }),
        ),
    ];

    for (payload, expected) in cases {
        let built = envelope("ses_1", cwd, &payload);
        assert_eq!(built, expected, "{:?}", payload.event());
        assert!(
            built.get("transcript_path").is_none(),
            "D457: there is no JSONL transcript to name"
        );
    }
}

/// Acceptance criterion 3's first clause: a matcher of `edit|write` fires
/// for `edit` and not for `read`. Proved with a hook that *speaks*, since a
/// hook that ran silently and one that never ran are the same empty
/// outcome.
#[tokio::test]
async fn a_matcher_fires_for_the_tools_it_names_and_no_others() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let hooks =
        built(HookEvent::PreToolUse, Some("edit|write"), &["echo refused >&2; exit 2"], dir.path());

    for (tool, blocked) in [("edit", true), ("write", true), ("read", false)] {
        let outcome = hooks
            .fire(
                "ses_1",
                &Payload::PreToolUse { tool_name: tool.to_owned(), tool_input: json!({}) },
            )
            .await;
        assert_eq!(
            outcome.blocked.as_deref() == Some("refused"),
            blocked,
            "{tool} produced {outcome:?}"
        );
    }
}

#[tokio::test]
async fn an_exit_two_blocks_with_its_stderr_and_a_clean_exit_does_not() {
    let dir = tempfile::tempdir().expect("a scratch directory");

    let refusing =
        built(HookEvent::PreToolUse, None, &["echo 'not that file' >&2; exit 2"], dir.path());
    let outcome = refusing
        .fire("ses_1", &Payload::PreToolUse { tool_name: "edit".to_owned(), tool_input: json!({}) })
        .await;
    assert_eq!(outcome.blocked.as_deref(), Some("not that file"));
    assert!(!outcome.allowed);

    let passing = built(HookEvent::PreToolUse, None, &["exit 0"], dir.path());
    let outcome = passing
        .fire("ses_1", &Payload::PreToolUse { tool_name: "edit".to_owned(), tool_input: json!({}) })
        .await;
    assert_eq!(outcome.blocked, None);
}

/// Exit 2 where blocking would mean nothing is reported, never silently
/// swallowed — and never carried back as a refusal the caller cannot act
/// on.
#[tokio::test]
async fn an_exit_two_on_a_non_blocking_event_becomes_a_notice() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let hooks = built(HookEvent::Stop, None, &["echo 'keep going' >&2; exit 2"], dir.path());

    let outcome = hooks.fire("ses_1", &Payload::Stop { stop_hook_active: false }).await;

    assert_eq!(outcome.blocked, None);
    assert_eq!(outcome.notices, vec!["keep going".to_owned()]);
}

#[tokio::test]
async fn a_permission_decision_allows_or_denies_by_name() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let call = Payload::PreToolUse {
        tool_name: "bash".to_owned(),
        tool_input: json!({ "command": "ls" }),
    };

    let allowing = built(
        HookEvent::PreToolUse,
        None,
        &[
            r#"echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}'"#,
        ],
        dir.path(),
    );
    let outcome = allowing.fire("ses_1", &call).await;
    assert!(outcome.allowed);
    assert_eq!(outcome.blocked, None);

    let denying = built(
        HookEvent::PreToolUse,
        None,
        &[
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"no shells today"}}'"#,
        ],
        dir.path(),
    );
    let outcome = denying.fire("ses_1", &call).await;
    assert_eq!(outcome.blocked.as_deref(), Some("no shells today"));

    let asking = built(
        HookEvent::PreToolUse,
        None,
        &[r#"echo '{"hookSpecificOutput":{"permissionDecision":"ask"}}'"#],
        dir.path(),
    );
    let outcome = asking.fire("ses_1", &call).await;
    assert_eq!(outcome, Outcome::default(), "ask is the flow with no hook at all");
}

#[tokio::test]
async fn plain_stdout_is_context_where_the_event_takes_it() {
    let dir = tempfile::tempdir().expect("a scratch directory");

    let prompt = built(HookEvent::UserPromptSubmit, None, &["echo 'the build is red'"], dir.path());
    let outcome =
        prompt.fire("ses_1", &Payload::UserPromptSubmit { prompt: "fix it".to_owned() }).await;
    assert_eq!(outcome.context, vec!["the build is red".to_owned()]);

    // The same words at an event that does not read them: not context, not
    // a failure either.
    let stop = built(HookEvent::Stop, None, &["echo 'the build is red'"], dir.path());
    let outcome = stop.fire("ses_1", &Payload::Stop { stop_hook_active: false }).await;
    assert_eq!(outcome, Outcome::default(), "{outcome:?}");
}

#[tokio::test]
async fn additional_context_reaches_the_caller() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let hooks = built(
        HookEvent::PostToolUse,
        Some("edit"),
        &[r#"echo '{"hookSpecificOutput":{"additionalContext":"reformatted"}}'"#],
        dir.path(),
    );

    let outcome = hooks
        .fire(
            "ses_1",
            &Payload::PostToolUse {
                tool_name: "edit".to_owned(),
                tool_input: json!({}),
                tool_response: json!({ "output": "done" }),
            },
        )
        .await;

    assert_eq!(outcome.context, vec!["reformatted".to_owned()]);
}

/// The hook writes its own standard input to a file, which is the only way
/// to prove the envelope reached the process rather than merely being
/// built.
#[tokio::test]
async fn the_envelope_reaches_the_process_on_standard_input() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let written = dir.path().join("envelope.json");
    let command = format!("cat > {}", written.display());
    let hooks = built(HookEvent::UserPromptSubmit, None, &[&command], dir.path());

    let outcome =
        hooks.fire("ses_7", &Payload::UserPromptSubmit { prompt: "hello".to_owned() }).await;
    assert!(outcome.notices.is_empty(), "{outcome:?}");

    let text = std::fs::read_to_string(&written).expect("the hook wrote its stdin");
    let echoed: Value = serde_json::from_str(&text).expect("what it wrote is the envelope");
    assert_eq!(echoed["session_id"], json!("ses_7"));
    assert_eq!(echoed["prompt"], json!("hello"));
    assert_eq!(echoed["hook_event_name"], json!("UserPromptSubmit"));
}

/// Pre-mortem #2: the kill maps to a non-blocking failure and a notice,
/// **never** to an allow.
#[tokio::test]
async fn a_hook_that_runs_too_long_is_killed_and_reported_without_blocking() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let mut config = configured(HookEvent::PreToolUse, None, &["sleep 30"]);
    for group in config.values_mut() {
        for entry in group {
            for handler in &mut entry.hooks {
                let HookHandler::Command(command) = handler;
                command.timeout = Some(1);
            }
        }
    }
    let hooks = Hooks::new(&config, dir.path()).expect("the block describes hooks");

    let outcome = hooks
        .fire("ses_1", &Payload::PreToolUse { tool_name: "edit".to_owned(), tool_input: json!({}) })
        .await;

    assert_eq!(outcome.blocked, None, "a killed hook refuses nothing");
    assert!(!outcome.allowed, "and above all approves nothing");
    assert_eq!(outcome.notices.len(), 1, "{outcome:?}");
    assert!(outcome.notices[0].contains("killed"), "{}", outcome.notices[0]);
}

#[tokio::test]
async fn a_hook_that_fails_is_a_notice_and_not_a_refusal() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let hooks =
        built(HookEvent::PreToolUse, None, &["echo 'no such thing' >&2; exit 7"], dir.path());

    let outcome = hooks
        .fire("ses_1", &Payload::PreToolUse { tool_name: "edit".to_owned(), tool_input: json!({}) })
        .await;

    assert_eq!(outcome.blocked, None);
    assert!(!outcome.allowed);
    assert_eq!(outcome.notices.len(), 1);
    assert!(
        outcome.notices[0].contains("exited with 7")
            && outcome.notices[0].contains("no such thing"),
        "{}",
        outcome.notices[0]
    );
}

/// Every matching hook runs, and the answers come back in configuration
/// order however the machine finished them.
#[tokio::test]
async fn matching_hooks_all_run_and_report_in_the_order_they_were_written() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let hooks = built(
        HookEvent::UserPromptSubmit,
        None,
        &["sleep 0.2; echo first", "echo second"],
        dir.path(),
    );

    let outcome = hooks.fire("ses_1", &Payload::UserPromptSubmit { prompt: "go".to_owned() }).await;

    assert_eq!(
        outcome.context,
        vec!["first".to_owned(), "second".to_owned()],
        "the slower one was written first, so it is reported first"
    );
}

/// The concurrency claim itself: two hooks that each sleep take about as
/// long as one of them, not twice as long.
#[tokio::test]
async fn matching_hooks_run_concurrently() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let hooks = built(
        HookEvent::UserPromptSubmit,
        None,
        &["sleep 0.4", "sleep 0.4", "sleep 0.4"],
        dir.path(),
    );

    let started = std::time::Instant::now();
    hooks.fire("ses_1", &Payload::UserPromptSubmit { prompt: "go".to_owned() }).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_millis(900),
        "three 400ms hooks in sequence would take 1.2s; this took {elapsed:?}"
    );
}

/// Hooks never fire for a hook. The pin is structural — nothing in this
/// module calls a fire site — and this is the observable half: a hook whose
/// own command would match the same matcher runs exactly once.
#[tokio::test]
async fn a_hook_does_not_fire_hooks_of_its_own() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ledger = dir.path().join("ledger");
    let command = format!("echo ran >> {}; sh -c 'echo nested'", ledger.display());
    let hooks = built(HookEvent::PreToolUse, None, &[&command], dir.path());

    hooks
        .fire(
            "ses_1",
            &Payload::PreToolUse {
                tool_name: "bash".to_owned(),
                tool_input: json!({ "command": "echo hi" }),
            },
        )
        .await;

    let written = std::fs::read_to_string(&ledger).expect("the hook ran");
    assert_eq!(
        written.lines().count(),
        1,
        "the hook's own shell must not fire another round of hooks"
    );
}

#[test]
fn a_block_with_no_handlers_configures_nothing() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    assert!(Hooks::new(&BTreeMap::new(), dir.path()).is_none());

    let mut empty = BTreeMap::new();
    empty.insert(
        HookEvent::Stop.name().to_owned(),
        vec![HookMatcher { matcher: None, hooks: Vec::new() }],
    );
    assert!(Hooks::new(&empty, dir.path()).is_none());
}

/// A matcher that would not compile matches nothing rather than
/// everything: the group asked to be narrow.
#[tokio::test]
async fn an_uncompilable_matcher_fires_for_nothing() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let ledger = dir.path().join("ledger");
    let command = format!("echo ran >> {}", ledger.display());
    let hooks = built(HookEvent::PreToolUse, Some("(unclosed"), &[&command], dir.path());

    hooks
        .fire("ses_1", &Payload::PreToolUse { tool_name: "edit".to_owned(), tool_input: json!({}) })
        .await;

    assert!(!ledger.exists(), "a broken matcher must not widen to everything");
}

#[test]
fn only_the_two_events_that_can_refuse_are_blocking() {
    let blocking: Vec<&str> =
        EVENTS.into_iter().filter(|event| event.blocking()).map(HookEvent::name).collect();

    assert_eq!(blocking, vec!["PreToolUse", "UserPromptSubmit"]);
}
