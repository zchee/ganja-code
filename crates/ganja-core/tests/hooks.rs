//! Hooks against a real engine: what they can refuse, what they can add, and
//! what they must never be able to break.
//!
//! Acceptance criterion 3 of `.omc/plans/2026-08-11-claude-runtime-port.md`,
//! end to end. The runner's own contract — exit codes, matcher compilation, the
//! stdin envelope field by field, the timeout kill — is pinned beside the code
//! in `src/hook.rs`; what is proved here is the half that only exists once a
//! turn is running: that a refusal reaches the model through the same
//! `fail_call` shape a denied rule does, that an approval really skips the
//! dialog, that a prompt can be refused before the model hears it, and that
//! none of it can cost a turn.
//!
//! Unix-gated for `background_jobs.rs`'s reason: every hook here is a POSIX
//! shell one-liner. A Windows twin is a follow-up, not a translation of this
//! file.

#![cfg(unix)]

use std::{collections::BTreeMap, path::Path, sync::Arc};

use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Engine, EngineError, Storage,
    config::{HookCommand, HookHandler, HookMatcher},
    hook::{HookEvent, Hooks},
    permission::Permissions,
    protocol::{Command, Event, FinishReason, Message, PartBody, ToolState},
    provider::{ChatRequest, ProviderEvent},
    tool::Registry,
};
use ganja_testkit::{RecorderTool, ScriptedProvider, drain, says};
use serde_json::Value;

/// A `hooks` block for one event, with one command handler.
fn block(
    event: HookEvent,
    matcher: Option<&str>,
    command: &str,
    timeout: Option<u64>,
) -> BTreeMap<String, Vec<HookMatcher>> {
    let mut config = BTreeMap::new();
    config.insert(
        event.name().to_owned(),
        vec![HookMatcher {
            matcher: matcher.map(str::to_owned),
            hooks: vec![HookHandler::Command(HookCommand {
                command: command.to_owned(),
                timeout,
            })],
        }],
    );

    config
}

/// A hook command that appends the whole envelope it was handed to `ledger`,
/// one JSON object per line.
///
/// Reading the *envelope* back rather than a marker word is what lets one
/// fixture answer every observational question a test has — which event fired,
/// for which session, with which trigger or source — and lets a failure show
/// the whole thing rather than the one field somebody thought to echo.
fn records_into(ledger: &Path) -> String {
    format!("{{ cat; echo; }} >> {}", ledger.display())
}

/// Every envelope written to `ledger`, in the order the hooks wrote them.
fn recorded(ledger: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(ledger) else {
        return Vec::new();
    };

    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a hook wrote one envelope per line"))
        .collect()
}

/// Waits until `ledger` holds `count` envelopes and hands them back.
///
/// The synchronization every observational assertion here needs: a turn's
/// `Stop` hook runs *after* the finish event a drain returns on and before the
/// slot is released, so reading the file the instant a drain returns is reading
/// it one hook too early. Polling the file rather than the slot keeps the test
/// honest about what it is waiting for — the hook's own side effect.
async fn awaited(ledger: &Path, count: usize) -> Vec<Value> {
    for _ in 0..500 {
        let written = recorded(ledger);
        if written.len() >= count {
            return written;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    panic!(
        "waited for {count} hook envelopes and saw {:?}",
        events_recorded(ledger)
    );
}

/// The `hook_event_name` of every envelope in `ledger`.
fn events_recorded(ledger: &Path) -> Vec<String> {
    recorded(ledger)
        .into_iter()
        .map(|envelope| {
            envelope["hook_event_name"]
                .as_str()
                .unwrap_or("?")
                .to_owned()
        })
        .collect()
}

/// A tool-call script fragment, arguments in one piece.
fn call(id: &str, tool: &str, args: Value) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallStart {
            id: id.to_owned(),
            name: tool.to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            id: id.to_owned(),
            json: args.to_string(),
        },
        ProviderEvent::ToolCallEnd { id: id.to_owned() },
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

fn prompt(text: &str) -> Command {
    Command::SendPrompt {
        text: text.to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
    }
}

/// The text of the last user message of `request` — where a reminder rides.
fn last_user_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == ganja_protocol::Role::User)
        .map(|message| {
            message
                .parts
                .iter()
                .filter_map(ganja_protocol::Part::as_text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Every tool part that ended in an error, as `(tool, message)`.
fn errors(seen: &[Event]) -> Vec<(String, String)> {
    seen.iter()
        .filter_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    tool,
                    state: ToolState::Error { error, .. },
                    ..
                } => Some((tool.clone(), error.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Every completed tool part, as `(tool, output)`.
fn completions(seen: &[Event]) -> Vec<(String, String)> {
    seen.iter()
        .filter_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    tool,
                    state: ToolState::Completed { output, .. },
                    ..
                } => Some((tool.clone(), output.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Acceptance 3, first two clauses: the matcher selects the call, and an exit 2
/// refuses it through the shape a denied rule already uses — a tool part in
/// `ToolState::Error` carrying words the model can act on.
#[tokio::test]
async fn a_matcher_refuses_the_calls_it_names_and_lets_the_others_through() {
    let dir = ganja_testkit::temp_dir();
    let (edit, edits) = RecorderTool::new("edit", "edit ran", "edited");
    let (read, reads) = RecorderTool::new("read", "read ran", "the file");
    let (provider, _requests) = ScriptedProvider::new(vec![
        call("call_1", "edit", serde_json::json!({ "path": "a.rs" })),
        call("call_2", "read", serde_json::json!({ "path": "a.rs" })),
        says("done"),
    ]);
    let hooks = Hooks::new(
        &block(
            HookEvent::PreToolUse,
            Some("edit|write"),
            "echo 'that file is generated' >&2; exit 2",
            None,
        ),
        dir.path(),
    )
    .expect("the block describes hooks");

    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![edit, read])),
        Permissions::default(),
    )
    .with_hooks(hooks);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt("edit it"))
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    assert!(
        edits
            .lock()
            .expect("the call log is never poisoned")
            .is_empty(),
        "the refused call must never have run"
    );
    assert_eq!(
        reads.lock().expect("the call log is never poisoned").len(),
        1,
        "and the call the matcher does not name runs untouched"
    );
    let errors = errors(&seen);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].0, "edit");
    assert!(
        errors[0].1.contains("PreToolUse hook") && errors[0].1.contains("that file is generated"),
        "the model reads the hook's own stderr: {}",
        errors[0].1
    );
    assert!(
        matches!(
            seen.last(),
            Some(Event::MessageFinished {
                reason: FinishReason::Completed,
                ..
            })
        ),
        "a refused call is information, not the end of the turn: {:?}",
        seen.last()
    );
}

/// Acceptance 3: `permissionDecision: "allow"` skips the dialog for a call the
/// rules would have asked about — and nothing else about the session changes.
#[tokio::test]
async fn an_allow_decision_skips_the_dialog_for_an_ask_gated_call() {
    let dir = ganja_testkit::temp_dir();
    let (shell, calls) = RecorderTool::new("shell", "shell ran", "output");
    let (provider, _requests) = ScriptedProvider::new(vec![
        call("call_1", "shell", serde_json::json!({ "command": "ls" })),
        says("done"),
    ]);
    let hooks = Hooks::new(
        &block(
            HookEvent::PreToolUse,
            None,
            r#"echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","permissionDecisionReason":"the script approved it"}}'"#,
            None,
        ),
        dir.path(),
    )
    .expect("the block describes hooks");

    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![shell])),
        Permissions::default(),
    )
    .with_hooks(hooks);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt("run it"))
        .await
        .expect("an idle engine accepts a prompt");
    // Nothing answers a dialog here: if one were raised, this drain would hang
    // rather than return, which is the strongest form the assertion has.
    let seen = drain(&mut events).await;

    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "the hook answered, so nobody is asked: {seen:#?}"
    );
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "and the call really ran"
    );
}

/// Without the hook, the same call raises a dialog — so the test above is about
/// the hook and not about a tool that was never gated.
#[tokio::test]
async fn the_same_call_without_a_hook_still_asks() {
    let (shell, _calls) = RecorderTool::new("shell", "shell ran", "output");
    let (provider, _requests) = ScriptedProvider::new(vec![
        call("call_1", "shell", serde_json::json!({ "command": "ls" })),
        says("done"),
    ]);

    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![shell])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt("run it"))
        .await
        .expect("an idle engine accepts a prompt");

    let asked = loop {
        match events.next().await {
            Some(Event::PermissionRequested { .. }) => break true,
            Some(Event::MessageFinished { .. }) | None => break false,
            Some(_) => {}
        }
    };
    assert!(asked, "the shell tool is ask-gated by default");
}

/// Acceptance 3: a `UserPromptSubmit` hook's stdout reaches the model as a
/// reminder on the request that prompt produced.
#[tokio::test]
async fn a_user_prompt_submit_hooks_stdout_reaches_the_model() {
    let dir = ganja_testkit::temp_dir();
    let (provider, requests) = ScriptedProvider::new(vec![says("noted")]);
    let hooks = Hooks::new(
        &block(
            HookEvent::UserPromptSubmit,
            None,
            "echo 'the build is red on main'",
            None,
        ),
        dir.path(),
    )
    .expect("the block describes hooks");

    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_hooks(hooks);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt("what is going on"))
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let text = last_user_text(&requests[0]);
    assert!(
        text.contains("what is going on") && text.contains("the build is red on main"),
        "the hook's line rides the prompt it was fired for: {text}"
    );
}

/// Acceptance 3: an exit 2 there refuses the prompt with a typed error, the
/// model is never asked, and the engine stays idle.
#[tokio::test]
async fn a_user_prompt_submit_hook_that_exits_two_refuses_the_prompt() {
    let dir = ganja_testkit::temp_dir();
    let (provider, requests) = ScriptedProvider::new(vec![says("never asked")]);
    let hooks = Hooks::new(
        &block(
            HookEvent::UserPromptSubmit,
            None,
            "echo 'not while the release is out' >&2; exit 2",
            None,
        ),
        dir.path(),
    )
    .expect("the block describes hooks");

    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_hooks(hooks);

    let refused = engine
        .send(prompt("ship it"))
        .await
        .expect_err("the hook refused the prompt");
    let EngineError::HookRefused { event, reason } = &refused else {
        panic!("expected a typed hook refusal, got {refused:?}");
    };
    assert_eq!(*event, "UserPromptSubmit");
    assert_eq!(reason, "not while the release is out");
    assert!(
        requests
            .lock()
            .expect("the request log is never poisoned")
            .is_empty(),
        "the model never heard the prompt"
    );

    // And the slot is free: the refusal ended nothing, because nothing started.
    let refused_again = engine.send(prompt("ship it anyway")).await;
    assert!(
        matches!(refused_again, Err(EngineError::HookRefused { .. })),
        "a refused prompt leaves an idle engine, not a busy one: {refused_again:?}"
    );
}

/// Acceptance 3: `SessionStart` fires with `startup` when a session opens and
/// with `resume` only on a resume — which is the whole of what its matcher
/// selects between.
#[tokio::test]
async fn session_start_names_startup_and_resume_only_on_a_resume() {
    let dir = ganja_testkit::temp_dir();
    let ledger = dir.path().join("ledger");
    let storage = Storage::open(dir.path().join("storage"));
    let session = ganja_testkit::seed_session(&storage, 0);
    ganja_testkit::seed_message(&storage, &session, &Message::user("the objective"));

    let (provider, _requests) = ScriptedProvider::new(vec![says("hello")]);
    let hooks = Hooks::new(
        &block(HookEvent::SessionStart, None, &records_into(&ledger), None),
        dir.path(),
    )
    .expect("the block describes hooks");
    let engine = Engine::persistent(
        provider,
        ganja_core::catalog::default_model("anthropic").expect("the catalog has a default"),
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
    .with_hooks(hooks);

    engine.session_start().await;
    assert_eq!(
        recorded(&ledger)
            .iter()
            .map(|envelope| envelope["source"].as_str().unwrap_or("?").to_owned())
            .collect::<Vec<_>>(),
        vec!["startup".to_owned()]
    );

    engine.resume(&session).await.expect("the session loads");
    let sources: Vec<String> = recorded(&ledger)
        .iter()
        .map(|envelope| envelope["source"].as_str().unwrap_or("?").to_owned())
        .collect();
    assert_eq!(sources, vec!["startup".to_owned(), "resume".to_owned()]);
    // The resumed fire names the session that was resumed, not the one the
    // engine minted at birth.
    assert_eq!(
        recorded(&ledger)[1]["session_id"].as_str(),
        Some(session.as_str())
    );
}

/// **D460**: what a `SessionStart` hook said reaches the model once, on the
/// next turn that asks it, and does not repeat on every request afterwards.
#[tokio::test]
async fn session_start_context_rides_exactly_one_turn() {
    let dir = ganja_testkit::temp_dir();
    let (provider, requests) = ScriptedProvider::new(vec![says("one"), says("two")]);
    let hooks = Hooks::new(
        &block(
            HookEvent::SessionStart,
            None,
            "echo 'the migration is half applied'",
            None,
        ),
        dir.path(),
    )
    .expect("the block describes hooks");

    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_hooks(hooks);
    engine.session_start().await;

    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    for text in ["first", "second"] {
        engine
            .send(prompt(text))
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;
    }

    let requests = requests.lock().expect("the request log is never poisoned");
    assert!(
        last_user_text(&requests[0]).contains("the migration is half applied"),
        "the first turn that asks the model delivers it"
    );
    assert!(
        !last_user_text(&requests[1]).contains("the migration is half applied"),
        "and no turn after it repeats it"
    );
}

/// Acceptance 3: `PreCompact` fires on both trigger paths, naming which one.
#[tokio::test]
async fn pre_compact_names_the_trigger_that_asked_for_it() {
    let model = ganja_core::catalog::default_model("anthropic").expect("the catalog has a default");

    // Manual: somebody typed `/compact` on a session nowhere near full.
    let manual_dir = ganja_testkit::temp_dir();
    let manual_ledger = manual_dir.path().join("ledger");
    let storage = Storage::open(manual_dir.path().join("storage"));
    let session = ganja_testkit::seed_session(&storage, 0);
    ganja_testkit::seed_message(&storage, &session, &Message::user("the objective"));
    let (provider, _requests) = ScriptedProvider::new(vec![says("## Objective\n- the thing")]);
    let engine = Engine::persistent(
        provider,
        model,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
    .with_hooks(
        Hooks::new(
            &block(
                HookEvent::PreCompact,
                None,
                &records_into(&manual_ledger),
                None,
            ),
            manual_dir.path(),
        )
        .expect("the block describes hooks"),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the session loads");
    engine
        .send(Command::Compact)
        .await
        .expect("an idle engine accepts a compaction");
    drain(&mut events).await;

    assert_eq!(
        recorded(&manual_ledger)
            .iter()
            .map(|envelope| envelope["trigger"].as_str().unwrap_or("?").to_owned())
            .collect::<Vec<_>>(),
        vec!["manual".to_owned()]
    );

    // Automatic: a session whose stored fill level is past the threshold, on
    // the next turn that asks the model.
    let auto_dir = ganja_testkit::temp_dir();
    let auto_ledger = auto_dir.path().join("ledger");
    let storage = Storage::open(auto_dir.path().join("storage"));
    let session = ganja_testkit::seed_session(&storage, 10_000_000);
    ganja_testkit::seed_message(&storage, &session, &Message::user("the objective"));
    let (provider, _requests) =
        ScriptedProvider::new(vec![says("## Objective\n- the thing"), says("carrying on")]);
    let engine = Engine::persistent(
        provider,
        model,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
    .with_hooks(
        Hooks::new(
            &block(
                HookEvent::PreCompact,
                None,
                &records_into(&auto_ledger),
                None,
            ),
            auto_dir.path(),
        )
        .expect("the block describes hooks"),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the session loads");
    engine
        .send(prompt("next step please"))
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    assert_eq!(
        recorded(&auto_ledger)
            .iter()
            .map(|envelope| envelope["trigger"].as_str().unwrap_or("?").to_owned())
            .collect::<Vec<_>>(),
        vec!["auto".to_owned()]
    );
}

/// Acceptance 3, and pre-mortem #2: a hook that outruns its budget is killed
/// and reported, the call it was about still runs, and the turn ends clean. The
/// kill can never be read as approval — this one refuses nothing and approves
/// nothing, because it never wrote anything at all.
#[tokio::test]
async fn a_hook_that_outlives_its_timeout_costs_the_turn_nothing() {
    let dir = ganja_testkit::temp_dir();
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let (provider, _requests) = ScriptedProvider::new(vec![
        call("call_1", "lookup", serde_json::json!({ "key": "a" })),
        says("done"),
    ]);
    let hooks = Hooks::new(
        &block(HookEvent::PreToolUse, None, "sleep 30", Some(1)),
        dir.path(),
    )
    .expect("the block describes hooks");

    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    )
    .with_hooks(hooks);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt("look it up"))
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "the call the hook could not answer for still ran"
    );
    assert!(errors(&seen).is_empty(), "and nothing failed: {seen:#?}");
    assert!(matches!(
        seen.last(),
        Some(Event::MessageFinished {
            reason: FinishReason::Completed,
            ..
        })
    ));
}

/// A `PostToolUse` hook's `additionalContext` reaches the model with the
/// result of the call it was fired for.
#[tokio::test]
async fn a_post_tool_use_hooks_context_rides_the_calls_result() {
    let dir = ganja_testkit::temp_dir();
    // `read`, not `edit`: this test is about what a hook adds to a result, and
    // an ask-gated tool would put a dialog between the call and the assertion.
    let (tool, _calls) = RecorderTool::new("read", "read ran", "the first line");
    let (provider, _requests) = ScriptedProvider::new(vec![
        call("call_1", "read", serde_json::json!({ "path": "a.rs" })),
        says("done"),
    ]);
    let hooks = Hooks::new(
        &block(
            HookEvent::PostToolUse,
            Some("read"),
            r#"echo '{"hookSpecificOutput":{"additionalContext":"rustfmt reformatted it"}}'"#,
            None,
        ),
        dir.path(),
    )
    .expect("the block describes hooks");

    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    )
    .with_hooks(hooks);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt("edit it"))
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    let completions = completions(&seen);
    assert_eq!(completions.len(), 1, "{completions:?}");
    assert!(
        completions[0].1.starts_with("the first line")
            && completions[0].1.contains("rustfmt reformatted it"),
        "the hook's context follows the tool's own output: {}",
        completions[0].1
    );
}

/// `Stop` fires at the end of a turn the session ran, and `SessionEnd` when the
/// frontend closes the session — both naming the session they belong to.
#[tokio::test]
async fn stop_fires_at_the_turn_boundary_and_session_end_at_the_close() {
    let dir = ganja_testkit::temp_dir();
    let ledger = dir.path().join("ledger");
    let mut config = block(HookEvent::Stop, None, &records_into(&ledger), None);
    config.extend(block(
        HookEvent::SessionEnd,
        None,
        &records_into(&ledger),
        None,
    ));
    let (provider, _requests) = ScriptedProvider::new(vec![says("done")]);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_hooks(Hooks::new(&config, dir.path()).expect("the block describes hooks"));

    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt("hello"))
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    // The turn's own hook first — it runs inside the tail, after the finish
    // event this drain returned on — then the close.
    awaited(&ledger, 1).await;
    engine.session_end(ganja_core::hook::EXIT_REASON).await;

    let written = awaited(&ledger, 2).await;
    assert_eq!(
        events_recorded(&ledger),
        vec!["Stop".to_owned(), "SessionEnd".to_owned()],
        "one turn, then the close"
    );
    assert_eq!(written[1]["reason"].as_str(), Some("exit"));
    assert_eq!(written[0]["session_id"], written[1]["session_id"]);
}

/// A turn's `Stop` hook runs **before** the busy slot is released, which is
/// what stops the next turn from starting while it works — the ordering the
/// tail's own comment pins.
#[tokio::test]
async fn a_stop_hook_finishes_before_the_next_turn_can_start() {
    let dir = ganja_testkit::temp_dir();
    let ledger = dir.path().join("ledger");
    let command = format!("sleep 0.3; {}", records_into(&ledger));
    let (provider, _requests) = ScriptedProvider::new(vec![says("one"), says("two")]);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_hooks(
        Hooks::new(&block(HookEvent::Stop, None, &command, None), dir.path())
            .expect("the block describes hooks"),
    );

    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt("first"))
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    // The finish event has been seen; the slot may still be held by the hook,
    // so a prompt sent now either waits for it or is refused as busy — and
    // either way the hook's line is on disk before the second turn's is.
    let mut attempts = 0;
    while engine.send(prompt("second")).await.is_err() {
        attempts += 1;
        assert!(attempts < 100, "the slot never came back");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        events_recorded(&ledger),
        vec!["Stop".to_owned()],
        "the first turn's stop hook had run to completion before the second \
         turn was ever admitted"
    );

    let _ = drain(&mut events).await;
}

/// The engine subscribes nobody to a hook: a session with hooks configured
/// still produces exactly the event stream it would have without them.
#[tokio::test]
async fn hooks_add_nothing_to_the_event_stream() {
    let dir = ganja_testkit::temp_dir();
    let ledger = dir.path().join("ledger");
    let mut config = block(
        HookEvent::UserPromptSubmit,
        None,
        &records_into(&ledger),
        None,
    );
    config.extend(block(HookEvent::Stop, None, &records_into(&ledger), None));

    let with_hooks: Vec<String> = {
        let (provider, _requests) = ScriptedProvider::new(vec![says("done")]);
        let engine = Engine::new(
            provider,
            "scripted-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        )
        .with_hooks(Hooks::new(&config, dir.path()).expect("the block describes hooks"));
        shapes(&engine).await
    };
    let without: Vec<String> = {
        let (provider, _requests) = ScriptedProvider::new(vec![says("done")]);
        let engine = Engine::new(
            provider,
            "scripted-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        );
        shapes(&engine).await
    };

    assert_eq!(with_hooks, without);
    awaited(&ledger, 2).await;
    assert_eq!(
        events_recorded(&ledger),
        vec!["UserPromptSubmit".to_owned(), "Stop".to_owned()],
        "and the hooks really did fire"
    );
}

/// One prompt's worth of event names.
async fn shapes(engine: &Engine) -> Vec<String> {
    let mut events: BoxStream<'static, Event> =
        engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt("hello"))
        .await
        .expect("an idle engine accepts a prompt");

    drain(&mut events)
        .await
        .iter()
        .map(|event| match event {
            Event::MessageStarted { .. } => "started",
            Event::MessageFinished { .. } => "finished",
            Event::PartStarted { .. } => "part",
            Event::PartUpdated { .. } => "updated",
            _ => "other",
        })
        .map(str::to_owned)
        .collect()
}

/// `Notification` fires when the session starts waiting for a person, and does
/// not make them wait for it: the hook is started beside the dialog rather than
/// before it, so a notifier that is slow costs nobody a keystroke.
#[tokio::test]
async fn a_notification_hook_fires_when_a_dialog_asks_for_a_person() {
    let dir = ganja_testkit::temp_dir();
    let ledger = dir.path().join("ledger");
    let (shell, calls) = RecorderTool::new("shell", "shell ran", "output");
    let (provider, _requests) = ScriptedProvider::new(vec![
        call("call_1", "shell", serde_json::json!({ "command": "ls" })),
        says("done"),
    ]);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![shell])),
        Permissions::default(),
    )
    .with_hooks(
        Hooks::new(
            &block(HookEvent::Notification, None, &records_into(&ledger), None),
            dir.path(),
        )
        .expect("the block describes hooks"),
    );

    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt("run it"))
        .await
        .expect("an idle engine accepts a prompt");
    ganja_testkit::drain_answering(&engine, &mut events, ganja_protocol::PermissionReply::Once)
        .await;

    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "the dialog was answered and the call ran"
    );
    let written = awaited(&ledger, 1).await;
    assert_eq!(written[0]["hook_event_name"].as_str(), Some("Notification"));
    assert_eq!(
        written[0]["message"].as_str(),
        Some("ganja needs your permission to use shell"),
        "the message names what is being asked about"
    );
}
