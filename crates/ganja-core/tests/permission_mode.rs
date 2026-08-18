//! When a permission mode bites, and what it may never overturn (**D-15**,
//! **D496**, **AC-19**).
//!
//! Three claims about `Command::SetPermissionMode`, which is the one command
//! this engine takes **while a turn streams**: it is accepted and announced at
//! once, it changes nothing about the turn that is already running, it bites at
//! the next turn's start — and a `Bypass` that bit answers dialogs without
//! repealing a single rule.
//!
//! Every test hands in its own rules, its own registry and its own scripted
//! provider, and none of them persists anything: `Engine::new` has no store,
//! `Permissions::default` has nowhere to write an answer, and the one test that
//! needs a store and a mailbox roots both under a temporary directory it owns.
//! So nothing here reads the environment for a path and nothing mutates it,
//! which is why this binary may hold more than one test.

use std::{sync::Arc, time::Duration};

use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Engine,
    permission::{Action, Permissions, Rule},
    protocol::{
        Command, Event, PermissionId, PermissionMode, PermissionReply,
        team::{Frame, ModeSetRequest},
    },
    tool::Registry,
};
use ganja_team::LEAD;
use ganja_testkit::{RecorderTool, RunnerHarness, ScriptedProvider, drain, says, tool_call};
use serde_json::json;

/// The tool the rules ask about. Any name outside the permission crate's
/// ask-by-default list would be *allowed* by default, so the rule below is what
/// makes it a gated call — which is the point: what is being tested is the
/// posture, not which tools ganja happens to gate.
const ASKS: &str = "gated";

/// The tool a rule refuses outright, for the anti-laundering claim.
const DENIED: &str = "refused";

/// How long one event is waited for before the fixture is called wedged.
/// Generous against a loaded machine, and reached only when delivery is broken.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long the stream is watched for an event that must not arrive. The turn
/// is sitting in its dialog for the whole of it, so anything the engine was
/// going to answer with would already have been queued.
const NOTHING_ARRIVES: Duration = Duration::from_millis(250);

/// An engine over `scripts`, offering `tools`, judged by `rules`.
///
/// `Engine::new` rather than `Engine::persistent`: what a mode decides is who
/// answers a dialog, and none of that reaches a store.
fn engine(
    scripts: Vec<Vec<ganja_core::provider::ProviderEvent>>,
    tools: Vec<Arc<dyn ganja_core::tool::Tool>>,
    rules: Vec<Rule>,
) -> Engine {
    let (provider, _requests) = ScriptedProvider::new(scripts);
    let mut permissions = Permissions::default();
    permissions.set_baseline(rules);

    Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(tools)),
        permissions,
    )
}

/// A rule about one tool's every call.
fn rule(tool: &str, action: Action) -> Rule {
    Rule {
        permission: tool.to_owned(),
        pattern: "*".to_owned(),
        action,
    }
}

async fn prompt(engine: &Engine, text: &str) {
    engine
        .send(Command::SendPrompt {
            text: text.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
}

/// Reads until `wanted` answers, collecting everything read on the way into
/// `seen` so a later assertion about the turn is not missing what this took.
async fn until<T>(
    events: &mut BoxStream<'static, Event>,
    seen: &mut Vec<Event>,
    wanted: impl Fn(&Event) -> Option<T>,
) -> T {
    loop {
        let event = tokio::time::timeout(PATIENCE, events.next())
            .await
            .expect("the engine should have said something by now")
            .expect("the stream should outlive the turn");
        let found = wanted(&event);
        seen.push(event);
        if let Some(found) = found {
            return found;
        }
    }
}

/// Collects for `NOTHING_ARRIVES`, which is how long the absence is asserted
/// for: the loop never returns on its own, so the timeout always elapses and
/// what this proves is that nothing matching `forbidden` arrived inside it.
async fn quiet(
    events: &mut BoxStream<'static, Event>,
    seen: &mut Vec<Event>,
    forbidden: impl Fn(&Event) -> bool,
    what: &str,
) {
    let watching = async {
        loop {
            let Some(event) = events.next().await else {
                return;
            };
            assert!(
                !forbidden(&event),
                "{what}, and it should not have: {event:?}"
            );
            seen.push(event);
        }
    };
    let _ = tokio::time::timeout(NOTHING_ARRIVES, watching).await;
}

fn requested(event: &Event) -> Option<PermissionId> {
    match event {
        Event::PermissionRequested { id, .. } => Some(id.clone()),
        _ => None,
    }
}

/// D496's whole discipline, told as one turn that keeps its posture and one
/// that begins under the new one.
///
/// The two halves need each other. That a mode set mid-turn changes nothing is
/// only interesting if the mode does something at all, and that a bypassed turn
/// runs a gated call unasked is only interesting if the same call asked a
/// moment earlier — so the same tool, under the same rule, is called twice and
/// answered by two different parties.
#[tokio::test]
async fn a_mode_set_does_not_change_the_running_turn_and_bites_at_the_next_one() {
    let (tool, calls) = RecorderTool::new(ASKS, "the gated call", "it ran");
    let engine = engine(
        vec![
            tool_call(ASKS, json!({})),
            says("the first turn is done"),
            tool_call(ASKS, json!({})),
            says("the second turn is done"),
        ],
        vec![tool],
        vec![rule(ASKS, Action::Ask)],
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    let mut seen = Vec::new();

    // The turn is now sitting in a dialog nobody has answered.
    prompt(&engine, "the first turn").await;
    let waiting = until(&mut events, &mut seen, requested).await;

    // Accepted while that turn streams, where a switch of agent, model or
    // effort would be refused as `Busy`: what sends this may be a lead
    // answering a teammate mid-turn.
    engine
        .send(Command::SetPermissionMode {
            mode: PermissionMode::Bypass,
        })
        .await
        .expect("a permission mode is taken while a turn streams");
    assert_eq!(
        engine.permission_mode(),
        PermissionMode::Bypass,
        "the engine holds the new posture from the moment it takes it"
    );

    // And it changes nothing about the turn in flight: the dialog raised under
    // `Ask` is still owed a person's answer.
    quiet(
        &mut events,
        &mut seen,
        |event| matches!(event, Event::PermissionReplied { .. }),
        "the running turn's dialog was answered by the engine",
    )
    .await;
    assert!(
        seen.iter().any(|event| matches!(
            event,
            Event::PermissionModeChanged {
                mode: PermissionMode::Bypass,
                ..
            }
        )),
        "the acceptance is announced when it happens, not when it bites: {seen:?}"
    );
    assert!(
        calls
            .lock()
            .expect("the call log is never poisoned")
            .is_empty(),
        "the gated call has not run: it is still waiting on an answer"
    );

    engine
        .send(Command::ReplyPermission {
            id: waiting,
            reply: PermissionReply::Once,
        })
        .await
        .expect("the dialog this turn raised is answerable");
    drain(&mut events).await;
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "the answered call ran, and the first turn finished"
    );

    // The next turn begins under the new posture. Nothing in this test answers
    // anything from here on: the reply below is the engine's own.
    prompt(&engine, "the second turn").await;
    // Under a timeout, because the way this claim fails is by *waiting*: a
    // posture that never bit leaves the turn sitting in a dialog nobody in
    // this test is going to answer, and a hang reads worse than a failure.
    let second = tokio::time::timeout(PATIENCE, drain(&mut events))
        .await
        .expect("a bypassed turn answers its own dialog instead of waiting for a person");

    assert!(
        second
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "a bypass answers the dialog rather than hiding it: {second:?}"
    );
    assert!(
        second.iter().any(|event| matches!(
            event,
            Event::PermissionReplied {
                reply: PermissionReply::Once,
                ..
            }
        )),
        "and it answers `once`, which remembers nothing: {second:?}"
    );
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        2,
        "the second call ran without anybody being asked"
    );
}

/// A bypass answers dialogs; it does not repeal rules.
///
/// The anti-laundering rule, and the reason it holds is structural rather than
/// careful: a denied call never raises a request at all — the gate fails it
/// where it stands — so the posture that answers requests has nothing to
/// answer. What the model gets is the refusal, which is information it reads
/// and carries on from.
#[tokio::test]
async fn a_bypassed_turn_does_not_launder_a_denied_call() {
    let (tool, calls) = RecorderTool::new(DENIED, "the refused call", "it ran");
    let engine = engine(
        vec![tool_call(DENIED, json!({})), says("the turn is done")],
        vec![tool],
        vec![rule(DENIED, Action::Deny)],
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SetPermissionMode {
            mode: PermissionMode::Bypass,
        })
        .await
        .expect("an idle engine takes a permission mode too");
    prompt(&engine, "run the refused call").await;
    let seen = drain(&mut events).await;

    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "a denied call raises no dialog, so a bypass has nothing to answer: {seen:?}"
    );
    assert!(
        calls
            .lock()
            .expect("the call log is never poisoned")
            .is_empty(),
        "and the call itself never ran"
    );
}

/// The lead may set a teammate's mode; a peer claiming to may not (§7-2).
///
/// Told with two frames that ask for *different* postures, because the second
/// claim only means something against a posture it would have moved: the peer's
/// `default` would put this teammate back on `Ask` if the runner obeyed it.
/// Both directions are driven through one `Runner::tick`, which is that loop's
/// contract made testable.
#[tokio::test]
async fn a_lead_frame_maps_to_the_command_and_a_peer_frame_does_not() {
    // The birth queue is a lossless lane, so somebody has to read it; this test
    // reads it itself, because the announcement it asserts on arrives there.
    let mut harness = RunnerHarness::new(false).await;
    let mut events = harness
        .events
        .take()
        .expect("an undrained harness hands the birth queue to the test");
    let arrives = |from: &str, mode: &str| {
        harness.arrives(
            from,
            &Frame::ModeSetRequest(ModeSetRequest {
                mode: mode.to_owned(),
                from: from.to_owned(),
            }),
        );
    };

    arrives(LEAD, "bypassPermissions");
    let tick = harness.runner.tick().await;

    assert_eq!(tick.applied, ["mode_set_request"], "{tick:?}");
    assert_eq!(
        harness.teammate.engine().permission_mode(),
        PermissionMode::Bypass,
        "the lead's frame reached the engine as the command it maps to"
    );
    let mut seen = Vec::new();
    let announced = until(&mut events, &mut seen, |event| match event {
        Event::PermissionModeChanged { mode, .. } => Some(*mode),
        _ => None,
    })
    .await;
    assert_eq!(announced, PermissionMode::Bypass);

    // The same frame, from somebody who is not the lead: dropped by name, and
    // the posture it asked for is not the posture this teammate is left in.
    arrives("w2", "default");
    let tick = harness.runner.tick().await;

    assert_eq!(tick.dropped, ["mode_set_request"], "{tick:?}");
    assert!(tick.applied.is_empty(), "{tick:?}");
    assert_eq!(
        harness.teammate.engine().permission_mode(),
        PermissionMode::Bypass,
        "a peer's frame changed nothing, so the lead's posture still stands"
    );
}
