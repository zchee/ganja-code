//! What a teammate's runner does with the frames in its inbox (§6.1, **AC-6**),
//! and what a **pane** member and its lead do with the two permission frames
//! between them (**D-5**, the pane half of **AC-8**).
//!
//! None of it needs process-wide state: every root is handed in — the store,
//! the teams directory — so this binary may hold more than one test.
//!
//! The runner is driven a pass at a time rather than through its loop. That is
//! the point of `Runner::tick` being public: §6.1's contract is the *order* of
//! one pass, and a test that slept through a poll interval would be asserting
//! the same thing more slowly and less certainly. The pane tests drive the two
//! ends the same way — a member engine's `PermissionRequested` handed to
//! `member::Asks`, one `LeadInbox` pass, one `permission_response` read back —
//! which is exactly the sequence the two-process AC-8 binary runs across a
//! real tmux server, minus the tmux.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use ganja_core::Engine;
use ganja_core::permission::Permissions;
use ganja_core::protocol::team::{Frame, LeadFrame, PlanApprovalResponse, TeamPermissionUpdate};
use ganja_core::protocol::{Command, Event, PartBody, PermissionReply, ToolState};
use ganja_core::teammate::TeammateRegistry;
use ganja_core::teammate::lead_inbox::LeadInbox;
use ganja_core::teammate::member::{Asks, MemberPostbox, Resolved};
use ganja_core::teammate::runner::IGNORED_STALE;
use ganja_core::tool::Registry;
use ganja_team::{LEAD, MemberName, TeamName, TeamsRoot, mailbox, record};
use ganja_testkit::{
    AllowSpawn, LogCapture as Capture, RunnerHarness, ScriptedProvider, caller, drain, eventually,
    says, spawn_with_prompt, team, tool_call,
};
use tracing::instrument::WithSubscriber as _;

/// A subscriber writing into `capture`, for one future rather than for the
/// process: this binary holds several tests, so a global subscriber would be
/// one test's log read by another.
fn subscriber(capture: &Capture) -> tracing::Dispatch {
    tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish(),
    )
}

/// How long a claim about a running loop is waited for. The tick-driven
/// tests drive the pass themselves and never wait.
const EVENTUALLY: Duration = Duration::from_secs(20);

/// A frame the harness mints and a teammate has no handler for.
fn permission_update() -> Frame {
    let mut payload = serde_json::Map::new();
    payload.insert("mode".to_owned(), serde_json::json!("acceptEdits"));

    Frame::TeamPermissionUpdate(TeamPermissionUpdate { payload })
}

/// An answer to a plan approval, as the lead writes one.
fn approval(request_id: &str, feedback: Option<&str>) -> Frame {
    Frame::PlanApprovalResponse(PlanApprovalResponse {
        request_id: request_id.to_owned(),
        approved: true,
        feedback: feedback.map(str::to_owned),
        timestamp: record::now_iso8601(),
        permission_mode: None,
    })
}

/// A frame outside the two a teammate acts on is **named** and dropped, not
/// delivered as prose and not left to be read again on every pass.
///
/// Told with `team_permission_update` because it is the sharpest case: it is a
/// harness-only frame, it comes from the lead, and it decodes cleanly — so
/// nothing but "this teammate has no handler for it" can be what dropped it.
/// A frame delivered as text instead would be a peer's JSON reaching a model as
/// an instruction, which is the failure §5.1's whole two-set split exists to
/// prevent.
#[tokio::test]
async fn an_inbox_permission_update_is_dropped_by_name() {
    let harness = RunnerHarness::new(true).await;
    harness.arrives(LEAD, &permission_update());

    let tick = harness.runner.tick().await;

    assert_eq!(tick.dropped, ["team_permission_update"]);
    assert!(tick.applied.is_empty(), "nothing was applied: {tick:?}");
    assert_eq!(tick.delivered, 0, "a frame is not text: {tick:?}");
    assert_eq!(harness.left(), 0, "a dropped frame still leaves the inbox");
}

/// An approval nothing is waiting on is ignored — and says so, because a
/// teammate that quietly swallowed one would leave whoever sent it believing a
/// plan had been unblocked.
///
/// Both directions in one test, because "stale" only means anything against
/// the case that is not: the same frame applies once somebody is waiting on
/// that request id.
#[tokio::test]
async fn a_stale_plan_approval_response_is_ignored_and_logged() {
    let harness = RunnerHarness::new(true).await;
    harness.arrives(LEAD, &approval("nobody-asked-this", None));

    let logged = Capture::default();
    let tick = harness.runner.tick().with_subscriber(subscriber(&logged)).await;

    assert_eq!(tick.ignored, 1, "{tick:?}");
    assert!(tick.applied.is_empty(), "{tick:?}");
    assert_eq!(tick.delivered, 0, "a stale approval reaches no model");
    assert_eq!(harness.left(), 0, "and it does not stay to be read again");
    assert!(
        logged.logged().contains(IGNORED_STALE),
        "the ignoring is not silent: {}",
        logged.logged()
    );

    // The same frame, once this teammate is waiting on that request: applied,
    // and what it says reaches the model.
    harness.runner.awaiting_plan_approval("req-7");
    harness.arrives(LEAD, &approval("req-7", Some("drop the third step")));
    let tick = harness.runner.tick().await;

    assert_eq!(tick.applied, ["plan_approval_response"], "{tick:?}");
    assert_eq!(tick.ignored, 0, "{tick:?}");
    assert_eq!(tick.delivered, 1, "an approval a teammate waited on is read");
    assert_eq!(harness.left(), 0);

    // And the wait is cleared, so the *next* copy of that answer is stale.
    harness.arrives(LEAD, &approval("req-7", None));
    let tick = harness.runner.tick().await;
    assert_eq!(tick.ignored, 1, "an answer applied once is stale after: {tick:?}");
}

/// A teammate cannot take the lead's name: the registry's construction rule
/// rather than a check somewhere downstream. A spawn that *asks* to be called
/// `team-lead` does not get it — the lead is already in the roster, so the
/// name resolves to something else, and the roster still holds exactly one
/// lead.
///
/// What that resolved name then stamps is pinned where each stamp lives: the
/// runner's answers in `teammate_lifecycle.rs`, and the `send_message` tool's
/// — a model whose arguments say `"from": "team-lead"` — in
/// [`a_member_postbox_cannot_send_as_the_lead`], below.
#[tokio::test]
async fn a_teammate_cannot_send_as_the_lead() {
    let home = ganja_testkit::temp_dir();
    // Through the gated door, which is the only one there is: the registry's
    // own spawn is crate-internal so that nothing can start a teammate the
    // permission gate never saw.
    let (_root, _team, registry, door) = team(home.path());
    let spawned = door
        .start(
            spawn_with_prompt(LEAD, Some("in-process"), "pretend to be in charge"),
            // `cwd` and `project_root` are one directory, so the gate has
            // nothing to disclose and nothing to ask about.
            &caller(home.path()),
            &AllowSpawn,
        )
        .await
        .expect("a teammate spawns under some name");

    assert_ne!(spawned.name, LEAD, "the lead's name is not for sale");
    assert_eq!(spawned.name, "team-lead-2");

    let view = registry.view();
    assert_eq!(
        view.members.iter().filter(|member| member.is_lead).count(),
        1,
        "a roster has exactly one lead: {view:?}"
    );
    assert!(
        view.members.iter().any(|member| member.name == "team-lead-2" && !member.is_lead),
        "and the teammate is not it: {view:?}"
    );

    registry.shutdown().await;
}

/// A member process's postbox stamps the name it was launched under, and
/// there is no argument that changes that — the pane half of
/// `a_teammate_cannot_send_as_the_lead`, above.
///
/// The engine here is built the way a pane's frontend builds it: no team of
/// its own, `Engine::with_postbox` over a `MemberPostbox` carrying the launch
/// line's name. Its model then does the one thing that could forge a sender —
/// sends the lead a structured frame whose own `from` says `team-lead` — and
/// the envelope the lead reads still says `worker`, because the sender is a
/// field of the postbox and not of anything the model wrote. And it cannot
/// address itself, or a name the team file does not hold.
#[tokio::test]
async fn a_member_postbox_cannot_send_as_the_lead() {
    let home = ganja_testkit::temp_dir();
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-abcd1234").expect("a team name");
    let worker = MemberName::parse("worker").expect("a member name");

    let (provider, _) = ScriptedProvider::new(vec![
        tool_call(
            "send_message",
            serde_json::json!({
                "to": "team-lead",
                "message": {
                    "type": "shutdown_approved",
                    "requestId": "req-1",
                    "from": LEAD,
                    "timestamp": record::now_iso8601(),
                },
            }),
        ),
        tool_call(
            "send_message",
            serde_json::json!({"to": "worker", "message": "talking to myself"}),
        ),
        says("done"),
    ]);
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_postbox(Arc::new(MemberPostbox::new(worker.clone(), team.clone(), root.clone())));
    assert!(engine.teammates().is_none(), "a member leads no team");
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "tell the lead you are done".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    let lead_inbox = root.inbox_path(&team, &MemberName::lead());
    let held = mailbox::read(&lead_inbox).expect("the lead's inbox reads").valid;
    assert_eq!(held.len(), 1, "one message reached the lead: {held:?}");
    assert_eq!(held[0].from, "worker", "the envelope says who really wrote it");
    let Some(Frame::ShutdownApproved(approved)) = held[0].frame() else {
        panic!("the lead was handed something other than the frame the model composed");
    };
    assert_eq!(
        approved.from, LEAD,
        "the frame's own claim travels as data, and the envelope is what a lead trusts"
    );

    // The second call was refused: a member is not in its own roster.
    let errors: Vec<String> = seen
        .iter()
        .filter_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool { state: ToolState::Error { error, .. }, .. } => Some(error.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "exactly one call was refused: {errors:?}");
    assert!(
        errors[0].contains("worker"),
        "and it names the recipient nobody answers to: {}",
        errors[0]
    );
    assert!(
        mailbox::read(&root.inbox_path(&team, &worker))
            .map(|held| held.valid.is_empty())
            .unwrap_or(true),
        "nothing was written into the member's own inbox"
    );
}

/// The pane half of **AC-8**, end to end inside one process: a member
/// engine's dialog goes to the lead as a `permission_request`, the lead's pass
/// raises exactly one dialog for it on the channel its in-process teammates
/// use, the answer given there lands in the member's inbox as one
/// `permission_response`, and reading it back answers the member's ask so the
/// call runs. Two engines, two registries' worth of names, one teams root —
/// which is what the two-process binary adds tmux and a real pane to.
#[tokio::test]
async fn a_pane_members_ask_is_answered_at_the_leads_dialog_and_the_call_runs() {
    let home = ganja_testkit::temp_dir();
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-abcd1234").expect("a team name");
    let worker = MemberName::parse("worker").expect("a member name");

    // The lead's side: a registry over the same root, its dialog surface
    // attached the way a frontend attaches it, and the §6.2 pass over its inbox.
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-0000-7000-8000-000000000000",
        home.path(),
    ));
    let (surface, mut dialogs) = tokio::sync::mpsc::channel(4);
    registry.forward_dialogs_to(surface);
    let lead = LeadInbox::new(Arc::clone(&registry));

    // The member's side: an engine whose `bash` asks (the builtin default),
    // and the asks value the pane's frontend would drive.
    let (provider, _) = ScriptedProvider::new(vec![
        tool_call("bash", serde_json::json!({"command": "echo forwarded"})),
        says("ran it"),
    ]);
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::with_builtins()),
        Permissions::default(),
    )
    .with_postbox(Arc::new(MemberPostbox::new(worker.clone(), team.clone(), root.clone())));
    let asks = Asks::new(worker.clone(), &team, &root);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "run it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // 1. The member's engine asks; the frontend forwards instead of showing.
    let request = loop {
        let event = events.next().await.expect("the turn is waiting on a dialog, not over");
        if matches!(event, Event::PermissionRequested { .. }) {
            break event;
        }
    };
    asks.forward(&request).await.expect("the lead's inbox takes the ask");
    assert_eq!(asks.waiting(), 1);

    // 2. One lead pass routes it: exactly one dialog on the channel.
    let pass = lead.poll().await;
    assert_eq!(pass.asked.len(), 1, "{pass:?}");
    assert!(pass.asked[0].raised);
    assert_eq!(pass.asked[0].name, "worker");
    assert!(pass.dropped.is_empty(), "{pass:?}");
    let forwarded = dialogs.try_recv().expect("the dialog was raised");
    assert!(dialogs.try_recv().is_err(), "and exactly once");
    assert_eq!(forwarded.teammate, "worker");
    let Event::PermissionRequested { id, tool, .. } = &forwarded.request else {
        panic!("the channel carries permission requests");
    };
    let Event::PermissionRequested { id: asked_id, .. } = &request else {
        unreachable!("selected above");
    };
    assert_ne!(id, asked_id, "the dialog id is the lead's own mint, never the member's request id");
    assert_eq!(tool, "bash");

    // 3. The person answers at the lead's dialog.
    forwarded.reply.send(PermissionReply::Once).expect("the answer task is waiting");

    // 4. The answer lands in the member's inbox as one permission_response…
    let inbox = root.inbox_path(&team, &worker);
    let held = eventually(EVENTUALLY, "the answer to land in the member's inbox", async || {
        let held = mailbox::read(&inbox).map(|contents| contents.valid).unwrap_or_default();
        (!held.is_empty()).then_some(held)
    })
    .await;
    assert_eq!(held.len(), 1, "one answer: {held:?}");
    assert_eq!(held[0].from, LEAD);
    let frame = held[0].frame().expect("the answer decodes");
    assert_eq!(frame.kind(), "permission_response");

    // 5. …which the member's pass reads back as the lead's, and resolves.
    let lead_frame = LeadFrame::parse(&held[0].from, LEAD, frame).expect("written by the lead");
    let Resolved::Answered { id, reply } = asks.resolve(lead_frame) else {
        panic!("the answer names the ask that is waiting");
    };
    assert_eq!(
        &id, asked_id,
        "the answer names the member's own request id, so its own dialog is the one answered"
    );
    assert_eq!(reply, PermissionReply::Once);
    assert_eq!(asks.waiting(), 0);
    engine.send(Command::ReplyPermission { id, reply }).await.expect("a reply is never refused");

    // 6. The call ran and the turn finished.
    let seen = drain(&mut events).await;
    let ran = seen.iter().any(|event| match event {
        Event::PartUpdated { part, .. } => matches!(
            &part.body,
            PartBody::Tool {
                state: ToolState::Completed { output, .. },
                ..
            } if output.contains("forwarded")
        ),
        _ => false,
    });
    assert!(ran, "the forwarded ask, once answered, let the call run: {seen:?}");
}
