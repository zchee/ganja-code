//! The peer envelope, at the engine's two doors (**D532**).
//!
//! What this pins is the pair of things a wire field is only worth having if
//! both are true: that what this session **puts on the wire** is what it
//! actually is — the class it enforces, the socket it answers on, the route a
//! message has crossed — and that what it **reads off the wire** reaches the
//! admission gate as a real input rather than a stub.
//!
//! The far side of every send here is `peer_support`'s own socket stub rather
//! than a real `ganja-serve`: `ganja-core` may not link an HTTP server, and
//! what these tests assert is the bytes this side wrote, which a stub records
//! better than a router does. The far side of every *receive* is
//! [`Engine::receive_peer_envelope`] driven directly, which is exactly what
//! `ganja-serve`'s socket route will call.
//!
//! Every root is handed in and nothing here mutates the environment, so this
//! binary may hold more than one test (the `teammate_engine.rs` rule).

#![cfg(unix)]

use std::{sync::Arc, time::Duration};

use futures::stream::BoxStream;
use ganja_core::{
    Engine, Incoming, NotReceived,
    config::{DialogExpiry, InboundPolicy},
    engine::{PeerEnvelope, SenderMode},
    permission::Permissions,
    protocol::{Command, Event, HoldCause, PermissionMode},
    provider::{ChatRequest, FakeProvider},
    teammate::TeammateRegistry,
    tool::Registry,
};
use ganja_protocol::PolicySource;
use ganja_testkit::{ScriptedProvider, team, tool_call};

mod peer_support;

use peer_support::{FAR_LEAD, FAR_TEAM, FarSide};

/// What a scripted turn's `send_message` call carries.
const TEXT: &str = "the parser lane is green, zarquon";

/// The stem the sending session claims as its own bound socket.
const OWN_STEM: &str = "0198beef";

/// One session that leads a team and answers a scripted provider — every
/// sender in this file.
struct Sender {
    engine: Arc<Engine>,
    provider: Arc<ScriptedProvider>,
    requests: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
    _home: tempfile::TempDir,
}

impl Sender {
    /// A sender whose turns each make one `send_message` call and then stop:
    /// the step is pushed per turn, and the follow-up request the loop makes
    /// once the tool has answered falls off the end of the script into the
    /// double's own bare completion.
    fn new() -> Self {
        Self::seeded(false)
    }

    /// The same, started under the D479 bypass trio — a construction-time
    /// seed, so it is a second engine rather than a switch.
    fn seeded(seeded: bool) -> Self {
        let (provider, requests) = ScriptedProvider::new(Vec::new());
        let home = ganja_testkit::temp_dir();
        let (_root, _team, registry, _door) = team(home.path());
        let engine = Engine::new(
            Arc::clone(&provider) as Arc<dyn ganja_core::provider::Provider>,
            "recorder-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        )
        .with_inbound_bypass(seeded)
        .with_teammates(Arc::clone(&registry));

        Self {
            engine: Arc::new(engine),
            provider,
            requests,
            _home: home,
        }
    }

    /// Queues the step that makes the next turn call `send_message` at
    /// `address`.
    fn will_send_to(&self, address: &str, text: &str) {
        self.provider.push(tool_call(
            "send_message",
            serde_json::json!({"to": address, "message": text}),
        ));
    }

    /// Takes one turn and drains it.
    async fn turn(&self, events: &mut BoxStream<'static, Event>) {
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

    async fn events(&self) -> BoxStream<'static, Event> {
        self.engine
            .subscribe()
            .await
            .expect("the first subscriber wins")
    }
}

/// One send, end to end, answering with what the far side is holding.
async fn send_once(sender: &Sender, far: &FarSide, text: &str) {
    let mut events = sender.events().await;
    sender.will_send_to(&far.address(), text);
    sender.turn(&mut events).await;
}

/// One engine leading a team, for the receiving half.
struct Receiver {
    engine: Arc<Engine>,
    registry: Arc<TeammateRegistry>,
    _home: tempfile::TempDir,
}

impl Receiver {
    fn new(policy: Option<(InboundPolicy, PolicySource)>, seeded: bool) -> Self {
        let home = ganja_testkit::temp_dir();
        let (_root, _team, registry, _door) = team(home.path());
        let engine = Engine::new(
            Arc::new(FakeProvider::new("on it", Duration::ZERO)),
            "fake/model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        )
        .with_inbound_policy(policy, DialogExpiry::default())
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
        ganja_team::mailbox::read(&self.registry.lead_inbox())
            .expect("the lead's inbox reads")
            .valid
    }
}

/// One arrival as the socket route hands it in.
fn incoming(text: &str) -> Incoming {
    Incoming {
        from: "team-lead@session-far".to_owned(),
        to: "team-lead".to_owned(),
        text: text.to_owned(),
        summary: None,
    }
}

/// An envelope carrying only an asserted class.
fn asserting(mode: Option<SenderMode>) -> PeerEnvelope {
    PeerEnvelope {
        from_mode: mode,
        ..PeerEnvelope::none()
    }
}

// ---------------------------------------------------------------------
// AC-3 — one class, both directions
// ---------------------------------------------------------------------

/// **AC-3**: over the whole `(PermissionMode, seeded_trio)` cross product,
/// the class this session **asserts on the wire** and the class it
/// **enforces at its own door** are the same word.
///
/// Asserted through the real send rather than by calling the classifier
/// twice: the point of the structural claim is that no copy of the class
/// exists to go stale, and only reading the bytes proves that.
#[tokio::test]
async fn the_class_this_session_asserts_equals_the_class_it_enforces() {
    for mode in [PermissionMode::Ask, PermissionMode::Bypass] {
        for seeded in [false, true] {
            let far = FarSide::accepting();
            let sender = Sender::seeded(seeded);
            if mode == PermissionMode::Bypass {
                sender
                    .engine
                    .send(Command::SetPermissionMode { mode })
                    .await
                    .expect("a mode switch is never refused");
            }
            let expected = sender
                .engine
                .receiver_class()
                .expect("the receiver class is total");

            send_once(&sender, &far, TEXT).await;

            let asserted = far.message()["from_mode"]
                .as_str()
                .expect("every send asserts a class")
                .to_owned();
            let enforced = match expected {
                ganja_core::teammate::inbound::ReceiverClass::Prompting => "prompting",
                ganja_core::teammate::inbound::ReceiverClass::Bypass => "bypass",
            };
            assert_eq!(
                asserted, enforced,
                "mode {mode:?}, seeded {seeded}: the wire and the door must name one class"
            );
        }
    }
}

/// **AC-3**, the seeded half, which cannot be reached by a mode switch: the
/// D479 trio is decided at assembly, so a session started under it asserts
/// `bypass` from its first send whatever its permission mode says.
#[tokio::test]
async fn a_session_seeded_bypass_asserts_bypass_without_switching_anything() {
    let far = FarSide::accepting();
    let sender = Sender::seeded(true);

    assert_eq!(
        sender.engine.permission_mode(),
        PermissionMode::Ask,
        "the trio answers dialogs; it does not move the mode"
    );

    send_once(&sender, &far, TEXT).await;

    assert_eq!(
        far.message()["from_mode"],
        "bypass",
        "a seeded session asserts what it enforces, not what its mode says"
    );
}

// ---------------------------------------------------------------------
// AC-4 — the never-loosen composition, through real bodies
// ---------------------------------------------------------------------

/// **AC-4**: `from_mode` drives the matrix through the never-loosen
/// composition, at the engine's own door, with real bodies.
///
/// The one row whose **outcome** moves is `(prompting receiver, bypass
/// sender)`; `(bypass, bypass)` stays held `no_mode_asserted`, which is this
/// build's recorded divergence from the reference; and an explicit
/// `cross_session_inbound` value wins over every row of it.
#[tokio::test]
async fn from_mode_reaches_the_matrix_and_can_only_tighten() {
    // (seeded receiver, asserted sender, expected hold cause or None for a
    // delivery) — the rows a production call site can now reach.
    let rows: Vec<(bool, Option<SenderMode>, Option<HoldCause>)> = vec![
        (false, Some(SenderMode::Prompting), None),
        (
            false,
            Some(SenderMode::Bypass),
            Some(HoldCause::ModeMismatch),
        ),
        (
            true,
            Some(SenderMode::Bypass),
            Some(HoldCause::NoModeAsserted),
        ),
        (
            true,
            Some(SenderMode::Prompting),
            Some(HoldCause::ModeMismatch),
        ),
        (false, None, None),
        (true, None, Some(HoldCause::NoModeAsserted)),
    ];

    for (seeded, sender, expected) in rows {
        let receiver = Receiver::new(None, seeded);
        let received = receiver
            .engine
            .receive_peer_envelope(incoming("how far along is the parser"), asserting(sender))
            .await
            .expect("every row still answers success");

        match expected {
            None => {
                assert!(
                    received.held.is_none(),
                    "seeded {seeded}, sender {sender:?}: expected a delivery, got {received:?}"
                );
                assert_eq!(
                    receiver.inbox().len(),
                    1,
                    "seeded {seeded}, sender {sender:?}: an accepted message is in the inbox"
                );
            }
            Some(cause) => {
                let held = received.held.as_ref().unwrap_or_else(|| {
                    panic!("seeded {seeded}, sender {sender:?}: expected a hold")
                });
                assert_eq!(held.cause, cause, "seeded {seeded}, sender {sender:?}");
                assert!(
                    receiver.inbox().is_empty(),
                    "seeded {seeded}, sender {sender:?}: a held message touches no inbox"
                );
            }
        }
    }
}

/// **AC-4**'s last clause: an explicit `cross_session_inbound` outranks every
/// row of the matrix, in both directions — a configured `accept` delivers a
/// body the parity default would have held, and a configured `refuse` drops
/// one it would have accepted.
#[tokio::test]
async fn an_explicit_policy_wins_over_every_matrix_row() {
    let accepting = Receiver::new(Some((InboundPolicy::Accept, PolicySource::Global)), true);
    let received = accepting
        .engine
        .receive_peer_envelope(
            incoming("the parity default would have held this"),
            asserting(Some(SenderMode::Bypass)),
        )
        .await
        .expect("an explicit accept answers success");
    assert!(received.held.is_none(), "an explicit accept never holds");
    assert_eq!(accepting.inbox().len(), 1, "and it really delivers");

    let refusing = Receiver::new(Some((InboundPolicy::Refuse, PolicySource::Global)), false);
    let received = refusing
        .engine
        .receive_peer_envelope(
            incoming("the parity default would have accepted this"),
            asserting(Some(SenderMode::Prompting)),
        )
        .await
        .expect("an explicit refuse still answers success");
    assert!(
        received.held.is_none(),
        "a refuse answers byte-identically to an accept, this field included"
    );
    assert!(refusing.inbox().is_empty(), "and it delivers nothing");
}

// AC-4's `self_sent` row is deliberately **not** driven here: every
// production call site passes `self_sent = false`, because the ancestry walk
// that would answer it has no producer in this build (`teammate::inbound`'s
// own module doc names the gap). Its rows stay `decide_unset`'s unit tests',
// where the argument can be supplied.

// ---------------------------------------------------------------------
// AC-5 / AC-7 — hop emission
// ---------------------------------------------------------------------

/// **AC-5**: a bound session's send carries `[own stem]`; after admitting a
/// message whose chain is `C`, the next send carries `C + [own stem]`; and
/// **AC-7**, `NewSession` clears the inherited chain through the same door
/// that clears the pin map.
#[tokio::test]
async fn a_send_carries_this_sessions_marker_after_whatever_it_admitted() {
    let far = FarSide::accepting();
    let sender = Sender::new();
    sender
        .engine
        .set_peer_address(Some(&far.directory().join(format!("{OWN_STEM}.sock"))));

    let mut events = sender.events().await;
    sender.will_send_to(&far.address(), TEXT);
    sender.turn(&mut events).await;
    assert_eq!(
        far.taken_on(&format!("/team/{FAR_LEAD}/message"))[0].body["hop_chain"],
        serde_json::json!([OWN_STEM]),
        "a bound sender that has admitted nothing carries its own marker alone"
    );

    // The same engine now admits a message carrying a chain of its own.
    sender
        .engine
        .receive_peer_envelope(
            incoming("forward this along"),
            PeerEnvelope {
                hop_chain: vec!["0198aaaa".to_owned(), "0198bbbb".to_owned()],
                ..PeerEnvelope::none()
            },
        )
        .await
        .expect("an unset policy at a prompting receiver accepts");

    sender.will_send_to(&far.address(), TEXT);
    sender.turn(&mut events).await;
    assert_eq!(
        far.taken_on(&format!("/team/{FAR_LEAD}/message"))[1].body["hop_chain"],
        serde_json::json!(["0198aaaa", "0198bbbb", OWN_STEM]),
        "a forward carries what it inherited, then this session"
    );

    // AC-7: the same door that clears the pin map.
    sender
        .engine
        .send(Command::NewSession)
        .await
        .expect("a new session is never refused");
    sender.will_send_to(&far.address(), TEXT);
    sender.turn(&mut events).await;
    assert_eq!(
        far.taken_on(&format!("/team/{FAR_LEAD}/message"))[2].body["hop_chain"],
        serde_json::json!([OWN_STEM]),
        "a new conversation has inherited nothing"
    );
}

/// **AC-5**, the two remaining clauses as this door can reach them: a chain
/// grows **oldest-first with this session last**, and an **unbound** sender
/// appends no marker and names nowhere to answer.
///
/// The 33-entry truncation is not driven from here on purpose, and the reason
/// is a fact about the two caps rather than a gap: the receiver's own chain
/// check drops anything past 28 entries, so the most a chain can *inherit*
/// through an admission is 28, and 28 plus this session's own marker is 29 —
/// four short of the sender cap. The truncation arithmetic is therefore
/// pinned where it can be reached at all, over the facts value itself, in
/// `engine_tests.rs`.
#[tokio::test]
async fn a_chain_grows_oldest_first_and_an_unbound_sender_appends_nothing() {
    let far = FarSide::accepting();
    let sender = Sender::new();

    let inherited: Vec<String> = (0..28).map(|index| format!("0198c{index:03}")).collect();
    sender
        .engine
        .set_peer_address(Some(&far.directory().join(format!("{OWN_STEM}.sock"))));
    sender
        .engine
        .receive_peer_envelope(
            incoming("a long way round"),
            PeerEnvelope {
                hop_chain: inherited.clone(),
                ..PeerEnvelope::none()
            },
        )
        .await
        .expect("a 28-entry chain is the most the receiver's own check admits");

    let mut events = sender.events().await;
    sender.will_send_to(&far.address(), TEXT);
    sender.turn(&mut events).await;

    let carried = far.taken_on(&format!("/team/{FAR_LEAD}/message"))[0].body["hop_chain"]
        .as_array()
        .expect("a chain is an array")
        .clone();
    assert_eq!(carried.len(), 29, "everything inherited, plus this session");
    assert_eq!(
        carried[0], inherited[0],
        "the oldest entry stays at the front"
    );
    assert_eq!(carried[28], OWN_STEM, "and this session is the newest");

    // Unbound: the address cell cleared, so no marker and no reply address.
    sender.engine.set_peer_address(None);
    sender.will_send_to(&far.address(), TEXT);
    sender.turn(&mut events).await;

    let second = &far.taken_on(&format!("/team/{FAR_LEAD}/message"))[1].body;
    assert_eq!(
        second["hop_chain"].as_array().map(Vec::len),
        Some(28),
        "an unbound sender forwards what it inherited and appends nothing"
    );
    assert!(
        !second["hop_chain"]
            .as_array()
            .expect("a chain is an array")
            .contains(&serde_json::json!(OWN_STEM)),
        "and its own marker is not in it: {second}"
    );
    assert!(
        second.get("reply_to").is_none(),
        "nor does it name anywhere to answer: {second}"
    );
}

// ---------------------------------------------------------------------
// AC-6 — hop enforcement, end to end
// ---------------------------------------------------------------------

/// **AC-6**: the two hop guards stop being stubs. A chain carrying this
/// session's own marker past the own-marker cap is dropped `HopLoop`; a chain
/// past the receiver's chain cap is dropped `HopRunaway`; and the sender's
/// answer is **byte-identical to an accept's** in both cases, which is
/// D523's uniform-answer rule extended to the drops this landing makes
/// reachable.
#[tokio::test]
async fn a_looping_or_runaway_chain_is_dropped_and_the_sender_cannot_tell() {
    let receiver = Receiver::new(None, false);
    let socket = std::env::temp_dir().join(format!("{OWN_STEM}.sock"));
    receiver.engine.set_peer_address(Some(&socket));

    let accepted = receiver
        .engine
        .receive_peer_envelope(incoming("an ordinary arrival"), PeerEnvelope::none())
        .await
        .expect("an unset policy at a prompting receiver accepts");
    assert_eq!(receiver.inbox().len(), 1, "the ordinary one landed");

    // Eleven of this session's own marker: one past the ten the loop check
    // admits.
    let looping = receiver
        .engine
        .receive_peer_envelope(
            incoming("round and round"),
            PeerEnvelope {
                hop_chain: vec![OWN_STEM.to_owned(); 11],
                ..PeerEnvelope::none()
            },
        )
        .await
        .expect("a dropped message still answers success");

    // Twenty-nine entries: one past the twenty-eight the chain check admits.
    let runaway = receiver
        .engine
        .receive_peer_envelope(
            incoming("forwarded to death"),
            PeerEnvelope {
                hop_chain: (0..29).map(|index| format!("0198d{index:03}")).collect(),
                ..PeerEnvelope::none()
            },
        )
        .await
        .expect("a dropped message still answers success");

    assert_eq!(
        receiver.inbox().len(),
        1,
        "neither drop reached the lead's inbox"
    );
    assert!(
        receiver.engine.held_messages().is_empty(),
        "a guard drop is a drop, not a hold"
    );
    assert_eq!(
        (accepted.sent.to.as_str(), accepted.sent.note.as_str()),
        (looping.sent.to.as_str(), looping.sent.note.as_str()),
        "a loop drop must be byte-indistinguishable from an accept"
    );
    assert_eq!(
        (accepted.sent.to.as_str(), accepted.sent.note.as_str()),
        (runaway.sent.to.as_str(), runaway.sent.note.as_str()),
        "a runaway drop must be byte-indistinguishable from an accept"
    );
    assert_eq!(
        (
            accepted.held.as_ref(),
            looping.held.as_ref(),
            runaway.held.as_ref()
        ),
        (None, None, None),
        "and none of the three names a hold"
    );
}

// ---------------------------------------------------------------------
// AC-8 — the reply address across the bind lifecycle
// ---------------------------------------------------------------------

/// **AC-8**, the engine cell's half: `reply_to` is on the wire exactly when
/// this session has an address recorded — present after a bind, absent
/// before one, absent after teardown, and absent after an observed rebind
/// whose new bind was refused (which the frontend spells as a clear that no
/// set follows). Read off **the far side's own body**, never off the cell.
#[tokio::test]
async fn reply_to_is_on_the_wire_exactly_while_a_socket_is_bound() {
    let far = FarSide::accepting();
    let sender = Sender::new();
    let mut events = sender.events().await;
    let route = format!("/team/{FAR_LEAD}/message");
    let socket = far.directory().join(format!("{OWN_STEM}.sock"));

    // Before any bind.
    sender.will_send_to(&far.address(), TEXT);
    sender.turn(&mut events).await;
    assert!(
        far.taken_on(&route)[0].body.get("reply_to").is_none(),
        "an unbound session names nowhere to answer"
    );

    // After `Synced::Bound`.
    sender.engine.set_peer_address(Some(&socket));
    sender.will_send_to(&far.address(), TEXT);
    sender.turn(&mut events).await;
    assert_eq!(
        far.taken_on(&route)[1].body["reply_to"],
        serde_json::json!(format!("uds:{}", socket.display())),
        "a bound session names its own socket, in the `uds:` spelling"
    );

    // Teardown.
    sender.engine.set_peer_address(None);
    sender.will_send_to(&far.address(), TEXT);
    sender.turn(&mut events).await;
    assert!(
        far.taken_on(&route)[2].body.get("reply_to").is_none(),
        "teardown takes the address with it"
    );

    // An observed rebind whose new bind was refused: the frontend removes the
    // old advertisement before the new bind's outcome exists, and no set
    // follows a refusal.
    sender.engine.set_peer_address(Some(&socket));
    sender.engine.set_peer_address(None);
    sender.will_send_to(&far.address(), TEXT);
    sender.turn(&mut events).await;
    assert!(
        far.taken_on(&route)[3].body.get("reply_to").is_none(),
        "a refused rebind leaves no stale advertisement"
    );
}

// ---------------------------------------------------------------------
// AC-14 — the sender takes no branch on what the far side is
// ---------------------------------------------------------------------

/// **AC-14**, narrowed to the two receiver kinds this build can actually
/// produce (the solo receiving surface is not built): a **team-led** receiver
/// and a **zero-teammate** one are indistinguishable from the sending side.
///
/// The pair is spelled the only way a receiver's shape reaches this wire at
/// all — the roster its `GET /team` answers with, which a sender reads for
/// the lead's name and the team's and never for who is in it — and the two
/// answers are asserted to really differ before the traces are compared, so
/// a stub that stopped varying could not quietly leave this passing on
/// nothing.
///
/// Asserted by driving both through one parameterized send and comparing the
/// **sender-side trace** — the routes it drove, in order, and the body it
/// wrote, minus the one field minted fresh per send — rather than by diffing
/// `deliver_over_socket` against its old bytes.
#[tokio::test]
async fn the_sender_takes_no_branch_on_what_the_far_side_is() {
    let mut rosters = Vec::new();
    let mut traces = Vec::new();
    // A lead with nobody beside it, then a lead with one teammate.
    for teammates in [0usize, 1] {
        let far = FarSide::leading(teammates);
        let sender = Sender::new();
        send_once(&sender, &far, TEXT).await;

        rosters.push(far.team_answer().to_owned());
        let taken = far.taken();
        traces.push((
            taken
                .iter()
                .map(|one| one.route.clone())
                .collect::<Vec<_>>(),
            {
                // Everything a branch could reach is compared by value —
                // which fields are there at all, the identity, the text, the
                // asserted class, the chain, the reply address. Only
                // `message_id` is dropped, and only because it names this
                // send rather than anything about the far side.
                let mut body = taken[1].body.clone();
                body.as_object_mut()
                    .expect("a message body is an object")
                    .remove("message_id")
                    .expect("every send mints one");
                body
            },
        ));
    }

    assert_ne!(
        rosters[0], rosters[1],
        "the two peers really are two receiver kinds, or the comparison below \
         is one setup compared against itself"
    );
    assert_eq!(
        traces[0], traces[1],
        "the crossing drives the same routes and writes the same body whatever answers"
    );
    assert_eq!(
        traces[0].0,
        vec!["/team".to_owned(), format!("/team/{FAR_LEAD}/message")],
        "and it is still the same two requests it always was"
    );
}

/// **AC-14**'s composition half: `Sent.to` still composes the far side's own
/// answer with the team it named, whatever the far side is.
#[tokio::test]
async fn the_far_sides_answer_still_composes_into_the_senders_own_note() {
    let far = FarSide::accepting();
    let sender = Sender::new();
    let mut events = sender.events().await;
    sender.will_send_to(&far.address(), TEXT);
    sender.turn(&mut events).await;

    // The tool's result travels in the next request as a tool part rather
    // than as text, so the whole recorded request is what is searched — the
    // question is whether the far side's own words got that far at all.
    let asked = format!(
        "{:?}",
        sender
            .requests
            .lock()
            .expect("the request log is never poisoned")
            .last()
            .expect("the send's result reached a following request")
            .messages
    );

    assert!(
        asked.contains(&format!("{FAR_LEAD}@{FAR_TEAM}")),
        "the tool's own result names the far side in that session's terms: {asked}"
    );
}

// ---------------------------------------------------------------------
// AC-10 — nothing on the new paths logs a body
// ---------------------------------------------------------------------

/// **AC-10**: no new-path log line carries a message body, a `reply_to` path
/// or a hop chain's contents — presence and counts only.
///
/// Its own binary would be cleaner still, but the capture is per-test here
/// because a `tracing` subscriber is process-wide: this test installs one and
/// is the only test in this file that reads it back.
#[tokio::test]
async fn no_new_path_logs_a_body_a_reply_address_or_a_chain() {
    const SECRET_BODY: &str = "xyzzy-the-body-nobody-may-log";
    const SECRET_STEM: &str = "0198f00d";

    let capture = ganja_testkit::LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let receiver = Receiver::new(Some((InboundPolicy::Refuse, PolicySource::Global)), false);
    let socket = std::env::temp_dir().join("0198beef.sock");
    receiver.engine.set_peer_address(Some(&socket));

    // A refuse traces its typed reason; a guard drop traces its own; and a
    // failed `reply_to` vetting traces the refusal. None may carry bytes.
    receiver
        .engine
        .receive_peer_envelope(
            incoming(SECRET_BODY),
            PeerEnvelope {
                hop_chain: vec![SECRET_STEM.to_owned(); 3],
                reply_to: Some(format!("uds:/nowhere/{SECRET_STEM}.sock")),
                ..PeerEnvelope::none()
            },
        )
        .await
        .expect("a refuse still answers success");

    let logged = capture.logged();
    assert!(
        !logged.contains(SECRET_BODY),
        "a body reached a log line: {logged}"
    );
    assert!(
        !logged.contains(SECRET_STEM),
        "a chain's contents or a reply path reached a log line: {logged}"
    );
    assert!(
        !logged.contains("/nowhere/"),
        "a reply address's path reached a log line: {logged}"
    );
}

// ---------------------------------------------------------------------
// The ladder still answers before any of this
// ---------------------------------------------------------------------

/// A session that leads no team still refuses before an envelope is looked
/// at: the structural refusal predates every rung and every field.
#[tokio::test]
async fn a_session_with_no_team_refuses_before_the_envelope_is_read() {
    let engine = Engine::new(
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        "fake/model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );

    let refused = engine
        .receive_peer_envelope(
            incoming("anybody there"),
            PeerEnvelope {
                from_mode: Some(SenderMode::Bypass),
                hop_chain: vec!["0198aaaa".to_owned()],
                ..PeerEnvelope::none()
            },
        )
        .await
        .expect_err("a session with no team receives nothing");

    assert!(matches!(refused, NotReceived::NoTeam), "got {refused:?}");
}
