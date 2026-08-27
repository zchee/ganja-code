//! Held-settlement receipts, at the engine's two ends (**D534**).
//!
//! The claim under test is a negative one, and it is the reason the whole
//! design is airtight rather than merely stated: **silence on this channel
//! means exactly one thing — still held.** So most of what is asserted here
//! is what does *not* happen. An accept posts nothing, a refuse posts
//! nothing, a guard drop posts nothing, a capacity eviction posts nothing and
//! the shutdown drain posts nothing; only a person's approve, a person's deny
//! and the review clock ever put anything on the wire.
//!
//! The spy is `peer_support`'s socket stub standing in for the *sender's* own
//! socket: every settlement that would be reported lands on it as a real
//! `POST /peer/receipt`, and every one that must not simply never arrives.
//!
//! Every root is handed in and nothing here mutates the environment, so this
//! binary may hold more than one test (the `teammate_engine.rs` rule).

#![cfg(unix)]

use std::{sync::Arc, time::Duration};

use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Engine, Incoming, ReceiptStatus, SocketReceipt,
    config::{DialogExpiry, InboundPolicy},
    engine::PeerEnvelope,
    permission::Permissions,
    protocol::{Command, Event, HeldDecision, HeldOutcome, HoldCause},
    provider::{ChatRequest, FakeProvider},
    teammate::{TeammateRegistry, receipts},
    tool::Registry,
};
use ganja_protocol::{PeerMessageId, PeerReceiptStatus, PolicySource};
use ganja_testkit::{ScriptedProvider, team, tool_call};

mod peer_support;

use peer_support::{Answer, FAR_LEAD, FarSide};

/// What every message here carries, appearing nowhere else.
const TEXT: &str = "the parser lane is green, zarquon";

/// How long a settlement that must arrive is given.
const EVENTUALLY: Duration = Duration::from_secs(10);

/// How long a settlement that must **not** arrive is given to prove it did
/// not. Long enough for a real post to have completed twice over — the
/// receipt client's own deadline is two seconds — so a silence asserted here
/// is a silence rather than a race won.
const GRACE: Duration = Duration::from_secs(3);

/// The one hold cause the far side of a send answers with in this file.
const HELD_MODE_MISMATCH: &str = r#"{"kind":"mode_mismatch"}"#;

/// One session that leads a team and answers a scripted provider.
struct Sender {
    engine: Arc<Engine>,
    provider: Arc<ScriptedProvider>,
    requests: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
    _home: tempfile::TempDir,
}

impl Sender {
    fn new() -> Self {
        let (provider, requests) = ScriptedProvider::new(Vec::new());
        let home = ganja_testkit::temp_dir();
        let (_root, _team, registry, _door) = team(home.path());
        let engine = Engine::new(
            Arc::clone(&provider) as Arc<dyn ganja_core::provider::Provider>,
            "recorder-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        )
        .with_teammates(Arc::clone(&registry));

        Self {
            engine: Arc::new(engine),
            provider,
            requests,
            _home: home,
        }
    }

    async fn events(&self) -> BoxStream<'static, Event> {
        self.engine
            .subscribe()
            .await
            .expect("the first subscriber wins")
    }

    /// One turn whose only act is a `send_message` at `address`.
    async fn send_to(&self, events: &mut BoxStream<'static, Event>, address: &str) {
        self.provider.push(tool_call(
            "send_message",
            serde_json::json!({"to": address, "message": TEXT}),
        ));
        self.engine
            .send(Command::SendPrompt {
                text: "say it".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                peers: Vec::new(),
                session_mentions: Vec::new(),
            })
            .await
            .expect("a prompt starts a turn");
        ganja_testkit::drain(events).await;
    }

    /// One plain turn, for reading back what the last one queued.
    async fn plain_turn(&self, events: &mut BoxStream<'static, Event>) {
        self.engine
            .send(Command::SendPrompt {
                text: "and now".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                peers: Vec::new(),
                session_mentions: Vec::new(),
            })
            .await
            .expect("a prompt starts a turn");
        ganja_testkit::drain(events).await;
    }

    /// The text of the **newest user message** in the last request — the
    /// bytes this turn's own intake added, rather than the whole transcript,
    /// which of course still carries what earlier intakes added.
    fn last_intake(&self) -> String {
        self.requests
            .lock()
            .expect("the request log is never poisoned")
            .last()
            .expect("at least one request")
            .messages
            .iter()
            .rev()
            .find(|message| message.role == ganja_core::protocol::Role::User)
            .expect("every request carries a user message")
            .parts
            .iter()
            .filter_map(ganja_core::protocol::Part::as_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One engine leading a team, for the receiving half.
struct Receiver {
    engine: Arc<Engine>,
    registry: Arc<TeammateRegistry>,
    _home: tempfile::TempDir,
}

impl Receiver {
    fn new(policy: Option<(InboundPolicy, PolicySource)>, seeded: bool) -> Self {
        Self::with_expiry(policy, seeded, DialogExpiry::default())
    }

    fn with_expiry(
        policy: Option<(InboundPolicy, PolicySource)>,
        seeded: bool,
        expiry: DialogExpiry,
    ) -> Self {
        let home = ganja_testkit::temp_dir();
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

    /// Takes one arrival in, answering `reply_to` where one is given.
    async fn takes(&self, text: &str, reply_to: Option<&FarSide>) -> ganja_core::engine::Received {
        self.engine
            .receive_peer_envelope(
                Incoming {
                    from: "team-lead@session-far".to_owned(),
                    to: "team-lead".to_owned(),
                    text: text.to_owned(),
                    summary: None,
                },
                PeerEnvelope {
                    message_id: Some(PeerMessageId::ascending()),
                    reply_to: reply_to.map(FarSide::address),
                    ..PeerEnvelope::none()
                },
            )
            .await
            .expect("every arm still answers success")
    }

    fn inbox(&self) -> Vec<ganja_team::MailboxMessage> {
        ganja_team::mailbox::read(&self.registry.lead_inbox())
            .expect("the lead's inbox reads")
            .valid
    }
}

/// Waits until the spy has taken at least `count` receipts, on the test's own
/// runtime — never `std::thread::sleep`, which would stop the very tasks the
/// settlement runs on.
async fn receipts_reach(spy: &FarSide, count: usize) -> Vec<peer_support::Taken> {
    let deadline = tokio::time::Instant::now() + EVENTUALLY;
    loop {
        let taken = spy.taken_on("/peer/receipt");
        if taken.len() >= count {
            return taken;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} receipts of {count} arrived before the deadline",
            taken.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until the gate has announced a hold, which is also the moment its
/// deadline timer exists.
async fn wait_for_held(events: &mut BoxStream<'static, Event>) {
    loop {
        if let Event::PeerHeld { .. } = events.next().await.expect("the stream stays open") {
            return;
        }
    }
}

/// Gives anything in flight time to land and answers what did.
async fn receipts_after_grace(spy: &FarSide) -> Vec<peer_support::Taken> {
    tokio::time::sleep(GRACE).await;

    spy.taken_on("/peer/receipt")
}

// ---------------------------------------------------------------------
// AC-32, AC-46, AC-28 — what registers, and what stays silent
// ---------------------------------------------------------------------

/// **AC-32**: registration is **held-and-reply-capable only**, as a
/// four-case table over one spy'd registry.
///
/// The spy is the engine's own settlement channel: an id that was registered
/// is settled by [`Engine::apply_receipt`] and announced as
/// [`Event::PeerReceipt`]; one that was not is dropped in silence. So
/// "registered" is asked by settling and watching, which is the only
/// observable the design offers on purpose.
#[tokio::test]
async fn only_a_held_and_reply_capable_send_registers_an_outstanding_id() {
    // (bound, what the far side answers, whether an id should register)
    let cases = [
        (false, Answer::Held(HELD_MODE_MISMATCH), false),
        (true, Answer::Accepted, false),
        (true, Answer::Held(HELD_MODE_MISMATCH), true),
    ];

    for (bound, answer, expected) in cases {
        let far = FarSide::answering(vec![answer.clone()]);
        let own = FarSide::accepting();
        let sender = Sender::new();
        if bound {
            sender.engine.set_peer_address(Some(own.path()));
        }
        let mut events = sender.events().await;
        sender.send_to(&mut events, &far.address()).await;

        let id = far.message()["message_id"]
            .as_str()
            .expect("every send mints an id")
            .to_owned();
        sender
            .engine
            .apply_receipt(SocketReceipt {
                message_id: PeerMessageId::from(id),
                status: ReceiptStatus::Delivered,
            })
            .await;

        let settled = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                match events.next().await {
                    Some(Event::PeerReceipt { .. }) => return true,
                    Some(_) => {}
                    None => return false,
                }
            }
        })
        .await
        .unwrap_or(false);

        assert_eq!(
            settled, expected,
            "bound {bound}, answer {answer:?}: registration is held-and-reply-capable only"
        );
    }
}

/// **AC-32**'s companion clause, at the receiver: an accept's answer and a
/// refuse's answer are **byte-identical to each other**, with the new `held`
/// field present in neither — so the field that makes registration possible
/// cannot have reopened the enumeration channel.
#[tokio::test]
async fn an_accept_and_a_refuse_still_answer_byte_identically() {
    let accepting = Receiver::new(None, false);
    let refusing = Receiver::new(Some((InboundPolicy::Refuse, PolicySource::Global)), false);

    let accepted = accepting.takes(TEXT, None).await;
    let refused = refusing.takes(TEXT, None).await;

    let wire = |received: &ganja_core::engine::Received| {
        serde_json::to_string(&ganja_core::SocketDelivered {
            to: received.sent.to.clone(),
            note: received.sent.note.clone(),
            held: received.held.clone(),
        })
        .expect("an answer serializes")
    };

    assert_eq!(
        wire(&accepted),
        wire(&refused),
        "the two answers must stay one string"
    );
    assert!(
        !wire(&accepted).contains("held"),
        "and neither carries the hold field: {}",
        wire(&accepted)
    );
    assert_eq!(accepting.inbox().len(), 1, "the accepted one landed");
    assert!(refusing.inbox().is_empty(), "the refused one did not");
}

/// **AC-46** and **AC-28** on one spy: a message the gate **accepts** posts
/// no receipt of any kind, and neither does one it **refuses** or one a
/// guard drops. Together with AC-32 this is the structural form of D523's
/// uniform answer — no outcome except a held one ever emits a receipt, so no
/// receipt's presence or absence can tell an accept from a refuse.
#[tokio::test]
async fn an_accept_a_refuse_and_a_guard_drop_all_post_nothing() {
    let spy = FarSide::accepting();

    let accepting = Receiver::new(None, false);
    accepting.takes("the accepted one", Some(&spy)).await;

    let refusing = Receiver::new(Some((InboundPolicy::Refuse, PolicySource::Global)), false);
    refusing.takes("the refused one", Some(&spy)).await;

    // A guard drop: the same body twice inside the 30 s dedup window.
    let deduping = Receiver::new(None, false);
    deduping.takes("the duplicated one", Some(&spy)).await;
    deduping.takes("the duplicated one", Some(&spy)).await;
    assert_eq!(
        deduping.inbox().len(),
        1,
        "the second arrival really was dropped"
    );

    assert!(
        receipts_after_grace(&spy).await.is_empty(),
        "only a held entry's settlement may ever post"
    );
}

// ---------------------------------------------------------------------
// AC-29 — held, then approved, denied, or run out
// ---------------------------------------------------------------------

/// **AC-29**: the sender learns `held` **and its cause synchronously**, in
/// the answer to the POST itself, and then reads a `Delivered` receipt once
/// somebody approves. A denial yields `Denied`.
#[tokio::test]
async fn a_hold_answers_its_cause_at_once_and_its_settlement_follows() {
    for (decision, expected) in [
        (HeldDecision::Release, ReceiptStatus::Delivered),
        (HeldDecision::Deny, ReceiptStatus::Denied),
    ] {
        let spy = FarSide::accepting();
        let receiver = Receiver::new(Some((InboundPolicy::Hold, PolicySource::Global)), false);

        let held = receiver.takes(TEXT, Some(&spy)).await;
        assert_eq!(
            held.held.as_ref().map(|held| held.cause),
            Some(HoldCause::Explicit {
                source: PolicySource::Global
            }),
            "the typed cause rides the very answer that was held"
        );
        assert!(
            held.sent.note.contains("held"),
            "and so does the note, in prose: {:?}",
            held.sent.note
        );

        let id = receiver.engine.held_messages()[0].id.clone();
        receiver
            .engine
            .send(Command::SettleHeld { id, decision })
            .await
            .expect("a settle is never refused");

        let taken = receipts_reach(&spy, 1).await;
        assert_eq!(taken.len(), 1, "exactly one settlement crossed");
        assert_eq!(
            taken[0].body["status"],
            serde_json::json!(match expected {
                ReceiptStatus::Delivered => "delivered",
                ReceiptStatus::Denied => "denied",
                ReceiptStatus::Expired => "expired",
            }),
            "{decision:?} settles as {expected:?}"
        );
    }
}

/// **AC-29**'s model-facing half: the sender's model reads **one**
/// `<peer_receipt>` part per batch, byte-identical to the one rendering
/// function — never a second copy of its words.
#[tokio::test]
async fn the_model_reads_one_peer_receipt_part_per_batch() {
    let far = FarSide::answering(vec![
        Answer::Held(HELD_MODE_MISMATCH),
        Answer::Held(HELD_MODE_MISMATCH),
    ]);
    let own = FarSide::accepting();
    let sender = Sender::new();
    sender.engine.set_peer_address(Some(own.path()));
    let mut events = sender.events().await;

    sender.send_to(&mut events, &far.address()).await;
    sender.send_to(&mut events, &far.address()).await;

    let route = format!("/team/{FAR_LEAD}/message");
    let sent = far.taken_on(&route);
    let mut expected = Vec::new();
    for (index, status) in [
        (0usize, PeerReceiptStatus::Delivered),
        (1, PeerReceiptStatus::Denied),
    ] {
        let id = sent[index].body["message_id"]
            .as_str()
            .expect("every send mints an id")
            .to_owned();
        sender
            .engine
            .apply_receipt(SocketReceipt {
                message_id: PeerMessageId::from(id.clone()),
                status: match status {
                    PeerReceiptStatus::Delivered => ReceiptStatus::Delivered,
                    PeerReceiptStatus::Denied => ReceiptStatus::Denied,
                    PeerReceiptStatus::Expired => ReceiptStatus::Expired,
                },
            })
            .await;
        expected.push(receipts::Settled {
            id: PeerMessageId::from(id),
            to: format!("{FAR_LEAD}@{}", peer_support::FAR_TEAM),
            status,
        });
    }

    sender.plain_turn(&mut events).await;

    let asked = sender.last_intake();
    let rendered = receipts::rendered(&expected);
    assert!(
        asked.contains(&rendered),
        "the batch reaches the model as the one rendering, whole:\n{rendered}\n\nin\n{asked}"
    );
    assert_eq!(
        asked.matches("<peer_receipt>").count(),
        1,
        "one part per batch, not one per settlement: {asked}"
    );

    // And the batch is drained: a second turn carries nothing.
    sender.plain_turn(&mut events).await;
    assert!(
        !sender.last_intake().contains("<peer_receipt>"),
        "a settlement is news once"
    );
}

// ---------------------------------------------------------------------
// AC-31 — a receipt grants nothing
// ---------------------------------------------------------------------

/// **AC-31**: applying a receipt performs no delivery, raises no permission
/// dialog, enqueues no turn and touches no inbox — it announces one fact and
/// stops.
#[tokio::test]
async fn a_receipt_grants_nothing() {
    let far = FarSide::answering(vec![Answer::Held(HELD_MODE_MISMATCH)]);
    let own = FarSide::accepting();
    let sender = Sender::new();
    sender.engine.set_peer_address(Some(own.path()));
    let mut events = sender.events().await;
    sender.send_to(&mut events, &far.address()).await;

    let id = far.message()["message_id"]
        .as_str()
        .expect("every send mints an id")
        .to_owned();
    let before = far.taken().len();

    sender
        .engine
        .apply_receipt(SocketReceipt {
            message_id: PeerMessageId::from(id),
            status: ReceiptStatus::Denied,
        })
        .await;

    // The one thing it does.
    let announced = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            match events.next().await {
                Some(Event::PeerReceipt { status, .. }) => return Some(status),
                Some(Event::PermissionRequested { .. } | Event::MessageStarted { .. }) => {
                    panic!("a receipt raised a dialog or started a turn")
                }
                Some(_) => {}
                None => return None,
            }
        }
    })
    .await
    .expect("the announcement is prompt");
    assert_eq!(announced, Some(PeerReceiptStatus::Denied));

    assert_eq!(
        far.taken().len(),
        before,
        "and it sent nothing anywhere: {:?}",
        far.taken()
    );
    assert!(
        sender.engine.held_messages().is_empty(),
        "nor did it park anything for review"
    );
}

// ---------------------------------------------------------------------
// AC-51 — only the review clock emits `Expired`
// ---------------------------------------------------------------------

/// **AC-51**, the two silent halves: filling the hold buffer past its cap
/// evicts the oldest entry and settles it internally, and draining the whole
/// buffer through `shutdown_settle` settles every entry — and the receipt
/// spy sees **nothing** in either case.
///
/// This is what makes "`Expired` means the review window ran out" a property
/// of the code rather than a sentence in a doc: a capacity eviction is
/// attacker-paced and a shutdown drain is a hundred settlements at once, and
/// neither is a person's decision.
#[tokio::test]
async fn a_capacity_eviction_and_the_shutdown_drain_are_both_silent() {
    let spy = FarSide::accepting();
    let receiver = Receiver::new(Some((InboundPolicy::Hold, PolicySource::Global)), false);

    // Past `HELD_CAP` (100), so the oldest entries are evicted with no person
    // anywhere near the decision. Each carries a real, vetted `reply_to`, so
    // an association exists for every one of them.
    for index in 0..110 {
        receiver.takes(&format!("{TEXT} {index}"), Some(&spy)).await;
    }
    assert_eq!(
        receiver.engine.held_messages().len(),
        100,
        "the buffer holds its cap, oldest evicted"
    );
    assert!(
        receipts_after_grace(&spy).await.is_empty(),
        "an eviction settles silently: the sender's last known state stays held"
    );

    receiver.engine.shutdown_settle().await;
    assert!(
        receiver.engine.held_messages().is_empty(),
        "the drain really settled everything"
    );
    assert!(
        receipts_after_grace(&spy).await.is_empty(),
        "and it, too, posted nothing"
    );
}

/// **AC-51**'s emitting half: the `dialog_expiry` timer **does** post
/// `Expired` for a reply-capable sender.
///
/// Paused time reaches the deadline, and real time is resumed before the post
/// is awaited — the receipt client has its own two-second deadline, and a
/// paused clock would auto-advance straight through it.
#[tokio::test(start_paused = true)]
async fn the_review_clock_posts_expired() {
    let spy = FarSide::accepting();
    let receiver = Receiver::with_expiry(None, true, DialogExpiry::OneMinute);
    let mut events = receiver
        .engine
        .subscribe()
        .await
        .expect("the first subscriber wins");

    let held = receiver.takes(TEXT, Some(&spy)).await;
    assert_eq!(
        held.held.as_ref().map(|held| held.cause),
        Some(HoldCause::NoModeAsserted),
        "a bypass-classed receiver holds an unasserted arrival"
    );

    // The deadline timer is spawned by the gate's own drain when it takes the
    // `Held` transition, and that transition is published as `PeerHeld`
    // **after** the timer exists — so waiting for the event is what makes
    // advancing the clock deterministic rather than a race against a sleep
    // that has not started yet.
    wait_for_held(&mut events).await;
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::resume();

    let outcome = loop {
        match events.next().await.expect("the event stream stays open") {
            Event::PeerHoldSettled { outcome, .. } => break outcome,
            _ => continue,
        }
    };
    assert_eq!(outcome, HeldOutcome::Expired, "the clock ran out");

    let taken = receipts_reach(&spy, 1).await;
    assert_eq!(taken[0].body["status"], serde_json::json!("expired"));
}

// ---------------------------------------------------------------------
// AC-47 — the reflection, bounded
// ---------------------------------------------------------------------

/// **AC-47**: a `reply_to` naming a **third** session is a bounded
/// reflection. A message A sends B carrying C's socket, held at B and settled
/// by the review clock, reaches C as **at most one** connection attempt,
/// answered exactly as C answers an admitted receipt — and three outcomes
/// reach C not at all: an accept, a refuse, and a capacity eviction.
#[tokio::test(start_paused = true)]
async fn a_reply_to_naming_a_third_session_is_a_bounded_reflection() {
    let third = FarSide::accepting();

    // The three that must reach C not at all.
    let accepting = Receiver::new(None, false);
    accepting.takes("the accepted one", Some(&third)).await;
    let refusing = Receiver::new(Some((InboundPolicy::Refuse, PolicySource::Global)), false);
    refusing.takes("the refused one", Some(&third)).await;
    let flooded = Receiver::new(Some((InboundPolicy::Hold, PolicySource::Global)), false);
    for index in 0..110 {
        flooded.takes(&format!("flood {index}"), Some(&third)).await;
    }

    // And the one that must, exactly once.
    let holding = Receiver::with_expiry(None, true, DialogExpiry::OneMinute);
    let mut events = holding
        .engine
        .subscribe()
        .await
        .expect("the first subscriber wins");
    holding.takes(TEXT, Some(&third)).await;

    wait_for_held(&mut events).await;
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::resume();

    loop {
        if let Event::PeerHoldSettled { .. } = events.next().await.expect("the stream stays open") {
            break;
        }
    }

    let taken = receipts_reach(&third, 1).await;
    assert_eq!(
        taken.len(),
        1,
        "one settlement, one attempt — and never a retry: {taken:?}"
    );

    // C answered it exactly as it answers any receipt, and nothing followed.
    tokio::time::sleep(GRACE).await;
    assert_eq!(
        third.taken_on("/peer/receipt").len(),
        1,
        "an accept, a refuse and an eviction reach the third session not at all"
    );
}

/// **AC-30**'s vetting half at the engine's own door: a `reply_to` that
/// [`ganja_core::tool::socket::vet_address`] refuses is never kept, so its
/// settlement posts nothing at all — the honest silence, rather than an
/// association that looks answerable and is not.
#[tokio::test]
async fn a_reply_to_that_fails_vetting_is_never_kept() {
    let receiver = Receiver::new(Some((InboundPolicy::Hold, PolicySource::Global)), false);

    let held = receiver
        .engine
        .receive_peer_envelope(
            Incoming {
                from: "team-lead@session-far".to_owned(),
                to: "team-lead".to_owned(),
                text: TEXT.to_owned(),
                summary: None,
            },
            PeerEnvelope {
                message_id: Some(PeerMessageId::ascending()),
                // A path that is not a session socket of ours in any respect.
                reply_to: Some("uds:/etc/passwd".to_owned()),
                ..PeerEnvelope::none()
            },
        )
        .await
        .expect("a hold still answers success");
    assert!(held.held.is_some(), "it is held all the same");

    let id = receiver.engine.held_messages()[0].id.clone();
    receiver
        .engine
        .send(Command::SettleHeld {
            id,
            decision: HeldDecision::Release,
        })
        .await
        .expect("a settle is never refused");

    assert!(
        receiver.engine.held_messages().is_empty(),
        "the settlement itself is unaffected by where it could not be reported"
    );
}
