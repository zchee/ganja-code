//! The admission gate's socket door, at the engine boundary (**D523**,
//! **D524**): what [`Engine::receive_peer_message`] answers under each
//! policy, what the hold buffer does with what it parks, and how a hold ends
//! — release, deny, re-evaluation, expiry, shutdown.
//!
//! The wire above this seam is `ganja-serve`'s and is pinned there; what
//! these tests pin is that the **engine's** answer already carries the whole
//! contract — byte-identical accept and refuse, a held receipt naming its
//! cause, and a buffer no model ever reads from — so the route has nothing
//! to compute.
//!
//! Every root is handed in and nothing here mutates the environment, so this
//! binary may hold more than one test (the `teammate_engine.rs` rule).

use std::{sync::Arc, time::Duration};

use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Engine, Incoming, NotReceived,
    config::{DialogExpiry, InboundPolicy},
    permission::Permissions,
    protocol::{Command, Event, HeldDecision, HeldId, HeldOutcome, HoldCause, PermissionMode},
    provider::FakeProvider,
    teammate::{TeammateRegistry, inbound::ResolvedInbound, lead_inbox::LeadInbox},
    tool::Registry,
};
use ganja_protocol::PolicySource;
use ganja_team::mailbox;
use ganja_testkit::{flooded_inbox, team};

/// How long an event or a shutdown is given on a loaded machine.
const EVENTUALLY: Duration = Duration::from_secs(10);

/// The reference's shutdown bound, as the engine spells it: the flush waits
/// this long for a wedged fanout and then proceeds regardless.
const SHUTDOWN_BOUND: Duration = Duration::from_millis(750);

/// One lead engine over its own throwaway teams root.
struct Lead {
    engine: Arc<Engine>,
    registry: Arc<TeammateRegistry>,
    /// Held so the temporary directory outlives the engine that writes in it.
    _home: tempfile::TempDir,
}

impl Lead {
    /// An engine leading a team, with `policy` where a config would have set
    /// one and the seed where a launch line would have.
    fn new(policy: Option<(InboundPolicy, PolicySource)>, seeded: bool) -> Self {
        Self::with_expiry(policy, seeded, DialogExpiry::default())
    }

    fn with_expiry(
        policy: Option<(InboundPolicy, PolicySource)>,
        seeded: bool,
        expiry: DialogExpiry,
    ) -> Self {
        let home = tempfile::tempdir().expect("a temporary home");
        let (_root, _team, registry, _door) = team(home.path());
        let engine = Engine::new(
            Arc::new(FakeProvider::new("on it", Duration::ZERO)),
            "fake/model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        )
        .with_inbound_policy(policy, expiry)
        .with_inbound_bypass(seeded)
        .with_teammates(Arc::clone(&registry));

        Self {
            engine: Arc::new(engine),
            registry,
            _home: home,
        }
    }

    /// What the lead's inbox holds right now.
    fn inbox(&self) -> Vec<ganja_team::MailboxMessage> {
        mailbox::read(&self.registry.lead_inbox())
            .expect("the lead's inbox reads")
            .valid
    }

    /// The §6.2 pass over this lead's inbox, gated on this engine's own gate
    /// — the production wiring, in miniature.
    fn lead_inbox(&self) -> LeadInbox {
        let engine = Arc::clone(&self.engine);

        LeadInbox::reading(Arc::clone(&self.registry), None)
            .gated(Arc::clone(self.engine.inbound()), move || {
                engine.receiver_class()
            })
    }
}

/// One peer message as the socket route would hand it in.
fn incoming(text: &str) -> Incoming {
    Incoming {
        from: "team-lead@session-far".to_owned(),
        to: "team-lead".to_owned(),
        text: text.to_owned(),
        summary: None,
    }
}

/// Drains the stream until a hold settles, so a test asserts the outcome the
/// engine announced rather than the one it hoped for.
async fn until_settled(events: &mut BoxStream<'static, Event>) -> (HeldId, HeldOutcome) {
    loop {
        let event = tokio::time::timeout(EVENTUALLY, events.next())
            .await
            .expect("a settlement event before the deadline")
            .expect("the event stream stays open");
        if let Event::PeerHoldSettled { id, outcome, .. } = event {
            return (id, outcome);
        }
    }
}

// AC-8, the engine half: what a sender reads back must not say which policy
// answered it.
#[tokio::test]
async fn a_refused_message_answers_byte_identically_to_an_accepted_one() {
    let accepting = Lead::new(None, false);
    let refusing = Lead::new(Some((InboundPolicy::Refuse, PolicySource::Global)), false);

    let accepted = accepting
        .engine
        .receive_peer_message(incoming("how far along is the parser"))
        .await
        .expect("an unset policy at a prompting receiver accepts");
    let refused = refusing
        .engine
        .receive_peer_message(incoming("how far along is the parser"))
        .await
        .expect("an explicit refuse still answers success");

    assert_eq!(
        (accepted.to.as_str(), accepted.note.as_str()),
        (refused.to.as_str(), refused.note.as_str()),
        "refuse must be byte-indistinguishable from accept"
    );
    assert_eq!(
        accepting.inbox().len(),
        1,
        "the accepted message is in the lead's inbox"
    );
    assert!(
        refusing.inbox().is_empty(),
        "the refused message wrote nothing"
    );
    assert!(
        refusing.engine.held_messages().is_empty(),
        "a refuse is a drop, not a hold"
    );
}

// D526: past the inbox's ceiling an *accepted* message still cannot land.
// The write refuses by name and the door surfaces it on the arm the serve
// crate already maps to 500 (`routes.rs`: `NotReceived::Failed` →
// `ApiError::Internal`) — infrastructure, never a new admission outcome —
// and the file is byte-identical after, because a ceiling refusal writes
// nothing: an unread backlog is not reshaped by the flood that failed to
// join it.
#[tokio::test]
async fn a_full_inbox_refuses_the_accepted_write_on_the_failure_arm_and_changes_nothing() {
    let lead = Lead::new(None, false);
    let inbox = lead.registry.lead_inbox();
    let planted = flooded_inbox(&inbox);

    let refused = lead
        .engine
        .receive_peer_message(incoming("one more onto the pile"))
        .await
        .expect_err("a full inbox refuses the write");

    let NotReceived::Failed { reason } = refused else {
        panic!("a ceiling refusal rides the write-failure arm, got {refused:?}");
    };
    assert!(
        reason.contains("could not be written") && reason.contains("past its ceiling"),
        "the failure names the write and the ceiling, not the policy: {reason}"
    );
    assert!(
        !reason.contains("xxxx"),
        "a refusal carries counts, never a body"
    );
    assert_eq!(
        std::fs::read_to_string(&inbox).expect("the inbox is readable"),
        planted,
        "a refused append leaves the file byte-identical"
    );
}

// AC-9: a hold is announced with its cause, and parks the message where no
// inbox — and so no model — can see it.
#[tokio::test]
async fn a_held_message_names_its_cause_and_writes_nothing() {
    let lead = Lead::new(Some((InboundPolicy::Hold, PolicySource::Global)), false);

    let sent = lead
        .engine
        .receive_peer_message(incoming("please review this"))
        .await
        .expect("a hold still answers success");

    assert!(
        sent.note.contains("held") && sent.note.contains("explicit hold policy"),
        "the note names held and the cause: {:?}",
        sent.note
    );
    assert!(lead.inbox().is_empty(), "a held message touches no inbox");
    let held = lead.engine.held_messages();
    assert_eq!(held.len(), 1, "the entry is in the held list");
    assert_eq!(
        held[0].cause,
        HoldCause::Explicit {
            source: PolicySource::Global
        }
    );
    assert_eq!(held[0].from, "team-lead@session-far");
    assert_eq!(
        held[0].expires_in, None,
        "an explicit hold installs no timer (the expiry re-check)"
    );
}

// AC-7's seed integration: the D479 trio reaches the classifier through
// `Engine::with_inbound_bypass`, and nothing else about the session moves.
#[tokio::test]
async fn the_bypass_seed_turns_an_unset_policy_accept_into_a_hold() {
    let unseeded = Lead::new(None, false);
    let seeded = Lead::new(None, true);

    unseeded
        .engine
        .receive_peer_message(incoming("hello"))
        .await
        .expect("a prompting receiver accepts");
    assert_eq!(unseeded.inbox().len(), 1);

    let sent = seeded
        .engine
        .receive_peer_message(incoming("hello"))
        .await
        .expect("a bypass-classed receiver holds, and says so");
    assert!(
        sent.note.contains("no sender mode asserted"),
        "the parity cause is named: {:?}",
        sent.note
    );
    assert!(seeded.inbox().is_empty());
    let held = seeded.engine.held_messages();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].cause, HoldCause::NoModeAsserted);
    assert!(
        held[0].expires_in.is_some(),
        "a parity hold carries the review deadline"
    );
}

// AC-14, the socket half: a release re-checks the policy of the moment, and
// an approval cannot override a refuse that arrived after the hold.
#[tokio::test]
async fn a_release_recheck_turns_into_a_deny_under_a_policy_now_refusing() {
    let lead = Lead::new(Some((InboundPolicy::Hold, PolicySource::Global)), false);
    let mut events = lead.engine.subscribe().await.expect("a subscription");

    lead.engine
        .receive_peer_message(incoming("let me in"))
        .await
        .expect("held");
    let id = lead.engine.held_messages()[0].id.clone();

    lead.engine
        .inbound()
        .replace_policy(ResolvedInbound::new(Some((
            InboundPolicy::Refuse,
            PolicySource::Global,
        ))));
    lead.engine
        .send(Command::SettleHeld {
            id: id.clone(),
            decision: HeldDecision::Release,
        })
        .await
        .expect("a settle is accepted");

    let (settled, outcome) = until_settled(&mut events).await;
    assert_eq!(settled, id);
    assert_eq!(
        outcome,
        HeldOutcome::Denied,
        "the approval was overridden by the policy re-check"
    );
    assert!(lead.inbox().is_empty(), "nothing was written");
    assert!(lead.engine.held_messages().is_empty());
}

// AC-14's other arm, with AC-18 riding it: while held nothing reaches inbox
// or pass, and a release delivers exactly once through the admitted set.
#[tokio::test]
async fn a_held_peer_message_never_reaches_the_model_until_released() {
    let lead = Lead::new(None, true);
    let mut events = lead.engine.subscribe().await.expect("a subscription");
    let inbox = lead.lead_inbox();

    lead.engine
        .receive_peer_message(incoming("the numbers you asked for"))
        .await
        .expect("held under the bypass seed");

    assert!(lead.inbox().is_empty(), "no inbox entry exists while held");
    let pass = inbox.poll().await;
    assert!(
        pass.messages.is_empty(),
        "no delivery is handed to the frontend while held: {pass:?}"
    );
    assert_eq!(lead.engine.held_messages().len(), 1);

    let id = lead.engine.held_messages()[0].id.clone();
    lead.engine
        .send(Command::SettleHeld {
            id: id.clone(),
            decision: HeldDecision::Release,
        })
        .await
        .expect("a settle is accepted");
    let (settled, outcome) = until_settled(&mut events).await;
    assert_eq!((settled, outcome), (id, HeldOutcome::Delivered));

    let written = lead.inbox();
    assert_eq!(written.len(), 1, "the release wrote the message");
    assert_eq!(written[0].from, "team-lead@session-far");
    assert_eq!(written[0].text, "the numbers you asked for");

    let pass = inbox.poll().await;
    assert_eq!(
        pass.messages.len(),
        1,
        "the released message is delivered exactly once: {pass:?}"
    );
    assert_eq!(pass.messages[0].body, "the numbers you asked for");
    inbox.delivered(&pass.messages).await;
    let pass = inbox.poll().await;
    assert!(
        pass.messages.is_empty(),
        "a delivered release is not re-offered: {pass:?}"
    );
}

// AC-16: a mode change re-evaluates what is held — the parity hold releases
// once the receiver prompts again.
#[tokio::test]
async fn a_mode_change_releases_the_hold_it_now_accepts() {
    let lead = Lead::new(None, false);
    let mut events = lead.engine.subscribe().await.expect("a subscription");

    lead.engine
        .send(Command::SetPermissionMode {
            mode: PermissionMode::Bypass,
        })
        .await
        .expect("the mode is taken");
    lead.engine
        .receive_peer_message(incoming("parked at a bypassed receiver"))
        .await
        .expect("held");
    assert_eq!(lead.engine.held_messages().len(), 1);
    assert!(lead.inbox().is_empty());

    lead.engine
        .send(Command::SetPermissionMode {
            mode: PermissionMode::Ask,
        })
        .await
        .expect("the mode is taken");

    let (_, outcome) = until_settled(&mut events).await;
    assert_eq!(
        outcome,
        HeldOutcome::Delivered,
        "a prompting receiver accepts what the bypass held"
    );
    assert!(lead.engine.held_messages().is_empty());
    assert_eq!(
        lead.inbox().len(),
        1,
        "the released message reached the inbox"
    );
}

// AC-16's other arm: an entry whose verdict still holds stays held, original
// cause intact — re-evaluation never re-causes a standing hold.
#[tokio::test]
async fn a_mode_change_leaves_an_explicit_hold_standing() {
    let lead = Lead::new(Some((InboundPolicy::Hold, PolicySource::Global)), true);

    lead.engine
        .receive_peer_message(incoming("held on policy, not parity"))
        .await
        .expect("held");
    lead.engine
        .send(Command::SetPermissionMode {
            mode: PermissionMode::Ask,
        })
        .await
        .expect("the mode is taken");

    let held = lead.engine.held_messages();
    assert_eq!(held.len(), 1, "the explicit hold survived the mode change");
    assert_eq!(
        held[0].cause,
        HoldCause::Explicit {
            source: PolicySource::Global
        },
        "and keeps its original cause"
    );
    assert!(lead.inbox().is_empty(), "and still wrote nothing");
}

// AC-13's engine half: the parity deadline is the engine's own timer, firing
// with no frontend anywhere near it.
#[tokio::test(start_paused = true)]
async fn a_parity_hold_expires_at_its_deadline_without_any_frontend() {
    let lead = Lead::with_expiry(None, true, DialogExpiry::OneMinute);
    let mut events = lead.engine.subscribe().await.expect("a subscription");

    lead.engine
        .receive_peer_message(incoming("nobody will answer this"))
        .await
        .expect("held");
    assert_eq!(lead.engine.held_messages().len(), 1);

    // Paused time: the plain await is what advances the clock to the one
    // pending timer — the hold's own deadline. No wall-clock timeout wrapper
    // here, deliberately: under a paused clock that wrapper would itself be
    // the nearest timer, and auto-advance would fire it first.
    let outcome = loop {
        let event = events.next().await.expect("the event stream stays open");
        if let Event::PeerHoldSettled { outcome, .. } = event {
            break outcome;
        }
    };
    assert_eq!(outcome, HeldOutcome::Expired);
    assert!(lead.engine.held_messages().is_empty());
    assert!(lead.inbox().is_empty(), "an expiry never delivers");
}

// AC-15's unknown-id arm: a settle naming nothing is ignored, which is also
// how a person racing the expiry timer loses gracefully.
#[tokio::test]
async fn a_settle_naming_an_unknown_id_is_ignored_without_error() {
    let lead = Lead::new(Some((InboundPolicy::Hold, PolicySource::Global)), false);
    lead.engine
        .receive_peer_message(incoming("still here"))
        .await
        .expect("held");

    lead.engine
        .send(Command::SettleHeld {
            id: HeldId::ascending(),
            decision: HeldDecision::Deny,
        })
        .await
        .expect("an unknown id is ignored, not an error");

    assert_eq!(
        lead.engine.held_messages().len(),
        1,
        "the real hold is untouched"
    );
}

// AC-17: shutdown settles everything expired within the bounded flush, even
// with a lossless subscriber that never reads — the wedge the bound exists
// for.
#[tokio::test]
async fn shutdown_settles_every_hold_within_the_bound_despite_a_slow_subscriber() {
    let lead = Lead::new(Some((InboundPolicy::Hold, PolicySource::Global)), false);
    // A deliberately slow subscriber: it claims the lossless queue and never
    // polls it, so once the queue fills the gate's event drain blocks on the
    // fanout mid-publish.
    let events = lead.engine.subscribe().await.expect("a subscription");

    // Enough transitions to fill the lossless queue past its capacity: each
    // message holds (one event), and each hold past the buffer's cap of 100
    // also evicts (a second event).
    for n in 0..600 {
        lead.engine
            .receive_peer_message(incoming(&format!("flood {n}")))
            .await
            .expect("held");
    }
    assert_eq!(
        lead.engine.held_messages().len(),
        100,
        "the buffer holds its cap, oldest evicted"
    );

    let started = std::time::Instant::now();
    lead.engine.shutdown_settle().await;
    let elapsed = started.elapsed();

    assert!(
        lead.engine.held_messages().is_empty(),
        "every held entry settled expired"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the flush is bounded, not a hang: {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(700),
        "a wedged fanout spends the bound rather than skipping the flush: {elapsed:?}"
    );
    drop(events);

    // Idempotent, like its teardown siblings — and fast once nothing is held.
    let again = std::time::Instant::now();
    lead.engine.shutdown_settle().await;
    assert!(
        again.elapsed() < SHUTDOWN_BOUND + Duration::from_secs(2),
        "a second settle finds nothing and returns"
    );
}
