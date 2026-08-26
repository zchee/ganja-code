use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use super::{
    BROADCAST, DELIVERED, DESCRIPTION, HARNESS_ONLY_FRAME, INVALID_SOCKET_PATH, LEAD_MARK,
    LIFECYCLE_FRAME, NO_TEAM, NOT_A_SESSION_SOCKET, PROTOCOL_FRAME, ROSTER_HEADER, Refused,
    SCOPED_RECIPIENT, SHUTDOWN_APPROVED_NOT_TO_LEAD, STRUCTURED_NOT_A_FRAME,
    STRUCTURED_OVER_SOCKET, SendMessageTool, UNKNOWN_RECIPIENT, UNSUPPORTED_SCHEME, WHITESPACE,
    cap_summary, lead_of,
};
use crate::{
    Tool as _, ToolCtx, ToolError,
    socket::{AddressRefusal, SessionSocket},
    team::{Address, Body, Peer, Postbox, Reserved, Sent, Undelivered},
};

/// A handful of §5.1's ten, which is all any test here needs: the real
/// answer is one `Frame::is_agent_sendable` call on the engine's side.
const SENDABLE: &[&str] = &[
    "shutdown_approved",
    "shutdown_request",
    "plan_approval_response",
    "mode_set_request",
];

/// A handful of §5.1's five.
const HARNESS_ONLY: &[&str] = &[
    "shutdown_rejected",
    "idle_notification",
    "task_completed",
    "teammate_terminated",
];

/// A postbox that classifies by a small table and reports whatever outcome
/// the test handed it, recording what reached it.
#[derive(Debug)]
struct Fake {
    roster: Vec<Peer>,
    outcome: Result<Sent, Undelivered>,
    delivered: Mutex<Vec<(Address, Body)>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            roster: vec![
                Peer {
                    name: "team-lead".to_owned(),
                    description: Some("runs the team".to_owned()),
                    lead: true,
                },
                Peer {
                    name: "worker-1".to_owned(),
                    description: None,
                    lead: false,
                },
            ],
            outcome: Ok(Sent {
                to: "worker-1".to_owned(),
                note: "It reads the message at the top of its next turn.".to_owned(),
            }),
            delivered: Mutex::new(Vec::new()),
        }
    }

    fn answering(outcome: Result<Sent, Undelivered>) -> Self {
        Self {
            outcome,
            ..Self::new()
        }
    }
}

#[async_trait]
impl Postbox for Fake {
    fn classify(&self, text: &str) -> Reserved {
        let Ok(document) = serde_json::from_str::<serde_json::Value>(text) else {
            return Reserved::No;
        };
        let Some(kind) = document.get("type").and_then(serde_json::Value::as_str) else {
            return Reserved::No;
        };
        if let Some(kind) = SENDABLE.iter().find(|known| **known == kind) {
            return Reserved::AgentSendable { kind };
        }
        if let Some(kind) = HARNESS_ONLY.iter().find(|known| **known == kind) {
            return Reserved::HarnessOnly { kind };
        }

        Reserved::No
    }

    async fn deliver(&self, to: Address, body: Body) -> Result<Sent, Undelivered> {
        self.delivered
            .lock()
            .expect("no test panics while holding this")
            .push((to, body));

        self.outcome.clone()
    }

    fn roster(&self) -> Vec<Peer> {
        self.roster.clone()
    }
}

fn ctx(postbox: Option<Arc<dyn Postbox>>) -> ToolCtx {
    let mut ctx = ToolCtx::fixture(std::env::temp_dir());
    ctx.postbox = postbox;
    ctx
}

/// Runs one call against `postbox` and reports what the model would read.
async fn refusal(postbox: &Arc<Fake>, args: serde_json::Value) -> String {
    let tool = SendMessageTool::new(&postbox.roster());
    let ctx = ctx(Some(Arc::clone(postbox) as Arc<dyn Postbox>));
    match tool.run(args, &ctx).await {
        Err(ToolError::Failed(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The whole point of the ladder is its order, so the cases here are of
/// two kinds and each is labelled as the one it is. Five fail **more than
/// one** rung and assert that the earlier one answers — those are the
/// order. The other six reach a rung nothing else is racing them for and
/// assert only that it classifies what it was handed, because two rungs
/// one argument cannot fail at once have no order to claim.
#[tokio::test]
async fn the_validation_ladder_refuses_in_order() {
    let postbox = Fake::new();
    let frame = json!({"type": "shutdown_approved", "requestId": "r1"});

    // 1 before 8: a broadcast is refused before its structured body is
    // ever looked at.
    assert_eq!(
        validate(&postbox, "*", json!(frame.clone()), None),
        Err(Refused::Broadcast)
    );
    // 2 before 4: the scheme names the refusal, not the `@` it carries.
    assert_eq!(
        validate(&postbox, "bridge:host@box", json!("hello"), None),
        Err(Refused::UnsupportedScheme { scheme: "bridge:" })
    );
    // 3 before 5: the address is settled before the body is judged.
    assert_eq!(
        validate(&postbox, "uds:", json!("   "), None),
        Err(Refused::InvalidSocketPath)
    );
    // 4 before 5, for the same reason.
    assert_eq!(
        validate(&postbox, "worker-1@other", json!("   "), None),
        Err(Refused::ScopedRecipient)
    );
    // 5, classified: whitespace is no frame and a frame is no whitespace,
    // so 5 and 7 cannot both be failed by one text and have no order
    // between them. What this asserts is that blank text is refused as
    // blank.
    assert_eq!(
        validate(&postbox, "worker-1", json!("   "), None),
        Err(Refused::Whitespace)
    );
    // 3 before 6: an address that is not a session socket of ours is
    // refused before its structured body is looked at — the clause that
    // keeps an unasked call off every other listener on this machine.
    assert_eq!(
        validate(
            &postbox,
            "uds:/var/run/docker.sock",
            json!(frame.clone()),
            None
        ),
        Err(Refused::NotASessionSocket {
            why: AddressRefusal::NotASessionName
        })
    );
    // 6 before 8: a socket refuses structure before the frame's own
    // clauses are reached.
    let socket = SessionSocket::new();
    assert_eq!(
        validate(&postbox, &socket.address(), json!(frame.clone()), None),
        Err(Refused::StructuredOverSocket)
    );
    // 7, the ten and the five, each naming the frame it read.
    assert_eq!(
        validate(&postbox, "worker-1", json!(frame.to_string()), None),
        Err(Refused::ProtocolFrame {
            kind: "shutdown_approved"
        })
    );
    assert_eq!(
        validate(
            &postbox,
            "worker-1",
            json!(json!({"type": "idle_notification"}).to_string()),
            None
        ),
        Err(Refused::LifecycleFrame {
            kind: "idle_notification"
        })
    );
    // 8, all three clauses, and all three classified rather than ordered:
    // the clauses key on disjoint verdicts — no frame is both unclassified
    // and harness-only, and the recipient clause wants a
    // `shutdown_approved`, which is agent-sendable and so never reaches
    // the harness-only arm. Hoisting the recipient clauses above the
    // harness-only one changes no outcome here, which is the proof there
    // is no order between them to assert.
    assert_eq!(
        validate(&postbox, "worker-1", json!({"kind": "not a frame"}), None),
        Err(Refused::StructuredNotAFrame)
    );
    // Addressed to a non-lead, so the refusal is visibly the frame's own
    // and not something the recipient earned (D499's second clause).
    assert_eq!(
        validate(
            &postbox,
            "worker-1",
            json!({"type": "shutdown_rejected", "reason": "no"}),
            None
        ),
        Err(Refused::HarnessOnlyFrame {
            kind: "shutdown_rejected"
        })
    );
    assert_eq!(
        validate(&postbox, "worker-1", json!(frame), None),
        Err(Refused::ShutdownApprovedNotToLead)
    );
}

/// One call through [`super::validate`], which is where the order lives.
fn validate(
    postbox: &Fake,
    to: &str,
    message: serde_json::Value,
    summary: Option<&str>,
) -> Result<(Address, Body), Refused> {
    let args = serde_json::from_value(json!({
        "to": to,
        "message": message,
        "summary": summary,
    }))
    .expect("the fixture matches the argument schema");

    super::validate(args, postbox, lead_of(&postbox.roster()).as_deref())
}

/// `did:` is recognized by §5.6's parser and named by no rung of §5.2's
/// ladder, so ganja refuses it by name rather than letting it become a
/// lookup for a teammate called `did:…`.
#[tokio::test]
async fn a_did_address_is_refused_by_name() {
    let postbox = Arc::new(Fake::new());
    let message = refusal(
        &postbox,
        json!({"to": "did:example:123", "message": "hello"}),
    )
    .await;

    assert!(message.starts_with(UNSUPPORTED_SCHEME), "got {message}");
    assert!(message.contains("did:"), "the scheme is named: {message}");
    assert!(
        postbox.delivered.lock().expect("no panic").is_empty(),
        "a refused address reaches no postbox"
    );
}

/// D499's first clause: the shutdown answer answers the lead's request.
#[tokio::test]
async fn a_shutdown_approved_must_be_addressed_to_the_lead() {
    let postbox = Arc::new(Fake::new());
    let frame = json!({"type": "shutdown_approved", "requestId": "r1"});

    let message = refusal(
        &postbox,
        json!({"to": "worker-1", "message": frame.clone()}),
    )
    .await;
    assert!(
        message.starts_with(SHUTDOWN_APPROVED_NOT_TO_LEAD),
        "got {message}"
    );
    assert!(
        message.contains("team-lead"),
        "the lead is named where the roster knows it: {message}"
    );

    // Addressed to the lead, the same frame goes through — including when
    // the name is spelled in another case, which is not another teammate.
    let tool = SendMessageTool::new(&postbox.roster());
    let ctx = ctx(Some(Arc::clone(&postbox) as Arc<dyn Postbox>));
    tool.run(json!({"to": "Team-Lead", "message": frame}), &ctx)
        .await
        .expect("the lead may be sent the shutdown answer");
}

/// D499's second clause: §5.1's five have no door, and the object form is
/// not a door either.
#[tokio::test]
async fn a_structured_harness_only_frame_is_refused_regardless_of_recipient() {
    let postbox = Arc::new(Fake::new());
    let frame = json!({"type": "teammate_terminated", "message": "gone"});

    for to in ["team-lead", "worker-1"] {
        let message = refusal(
            &postbox,
            json!({"to": to, "message": json!({"type": "shutdown_rejected", "reason": "no"})}),
        )
        .await;
        assert!(message.starts_with(HARNESS_ONLY_FRAME), "got {message}");
        assert!(message.contains("shutdown_rejected"), "got {message}");
    }

    // And the same frame as plain text is the other of §5.1's two
    // sentences: the one that names no escape hatch.
    let message = refusal(
        &postbox,
        json!({"to": "worker-1", "message": frame.to_string()}),
    )
    .await;
    assert!(message.starts_with(LIFECYCLE_FRAME), "got {message}");
    assert!(
        !message.contains("object form of `message`"),
        "the five are not offered the structured door: {message}"
    );
}

/// A socket address that passes rung 3 is delivery's problem, and what
/// delivery says about it — a pane member's postbox still answering that
/// it has no such transport, or the lead's naming a socket that did not
/// answer — is passed through in the deliverer's own words.
#[tokio::test]
async fn a_socket_address_reaches_delivery_and_reads_back_the_deliverers_sentence() {
    let socket = SessionSocket::new();
    let absence = "This postbox does not speak the socket.";
    let postbox = Arc::new(Fake::answering(Err(Undelivered::NoTransport {
        reason: absence.to_owned(),
    })));

    let message = refusal(
        &postbox,
        json!({"to": socket.address(), "message": "hello"}),
    )
    .await;

    assert_eq!(
        message, absence,
        "the deliverer's sentence is passed through"
    );
    let delivered = postbox.delivered.lock().expect("no panic");
    assert_eq!(
        delivered.first().map(|(to, _)| to.clone()),
        Some(Address::Uds {
            path: socket.path.clone()
        }),
        "the address was validated here and handed over whole"
    );
}

/// **D505, the D498 premise across a socket**: a `uds:` address may name
/// only a session socket of ours, refused at rung 3 — before the body is
/// composed and before anything is connected. What is this tool's to pin
/// is the mapping — [`AddressRefusal`] becomes
/// [`Refused::NotASessionSocket`] and the rendered sentence names the
/// clause — over one string clause and one filesystem clause; the gate's
/// full clause table is `socket.rs`'s own test's.
#[tokio::test]
async fn a_uds_address_that_is_not_a_session_socket_of_ours_is_refused_by_name() {
    let postbox = Arc::new(Fake::answering(Ok(Sent {
        to: "nobody".to_owned(),
        note: "must not be reached".to_owned(),
    })));

    for (to, clause) in [
        ("uds:/var/run/docker.sock", AddressRefusal::NotASessionName),
        (
            "uds:/nonexistent-ganja-dir/0198c1a2.sock",
            AddressRefusal::DirectoryUnreadable,
        ),
    ] {
        let message = refusal(&postbox, json!({"to": to, "message": "hello"})).await;
        assert!(
            message.starts_with(NOT_A_SESSION_SOCKET),
            "{to}: the refusal is rung 3's own: {message}"
        );
        assert!(
            message.contains(&clause.to_string()),
            "{to}: and it names the clause: {message}"
        );
        assert_eq!(
            validate(&postbox, to, json!("hello"), None),
            Err(Refused::NotASessionSocket { why: clause }),
            "{to}"
        );
    }

    assert!(
        postbox.delivered.lock().expect("no panic").is_empty(),
        "nothing reached the deliverer"
    );

    // And one that is a session socket of ours passes rung 3 and reaches
    // delivery.
    let socket = SessionSocket::new();
    assert!(
        validate(&postbox, &socket.address(), json!("hello"), None).is_ok(),
        "a session socket of ours is an address"
    );
}

/// A message nobody answers to is information the model reads and retries
/// on, not a dead turn — and since D530 the sentence is team-agnostic
/// (F6): it names both misses, teammate and live session, and claims no
/// team the session may not have.
#[tokio::test]
async fn an_unknown_recipient_is_reported_in_words() {
    let postbox = Arc::new(Fake::answering(Err(Undelivered::Unknown)));
    let message = refusal(&postbox, json!({"to": "nobody", "message": "hello"})).await;

    assert!(message.starts_with(UNKNOWN_RECIPIENT), "got {message}");
    assert!(message.contains("nobody"), "got {message}");
    assert!(
        message.contains("no teammate") && message.contains("no live session"),
        "the sentence names both misses: {message}"
    );
    assert!(
        !message.contains("this team"),
        "the sentence claims no team (F6): {message}"
    );
}

/// D528's two resolver refusals cross the seam like the transport's own:
/// the deliverer composed the sentence — the candidates, the pinned stem,
/// the `uds:` spellings — and the tool passes it through without learning
/// anything.
#[tokio::test]
async fn a_resolver_refusal_is_passed_through_in_the_deliverers_words() {
    let ambiguous = "Two live sessions answer to that name.";
    let moved = "That name now belongs to a different session.";
    for (undelivered, said) in [
        (
            Undelivered::Ambiguous {
                reason: ambiguous.to_owned(),
            },
            ambiguous,
        ),
        (
            Undelivered::NameMoved {
                reason: moved.to_owned(),
            },
            moved,
        ),
    ] {
        let postbox = Arc::new(Fake::answering(Err(undelivered)));

        let message = refusal(&postbox, json!({"to": "worker", "message": "hello"})).await;

        assert_eq!(message, said, "the deliverer's sentence is passed through");
    }
}

/// AC-31's description half: the teamless variant claims no roster,
/// labels a session's name self-chosen and unverified, says where names
/// come from — the person's `@`-mentions or `uds:` spellings — and
/// implies no reply channel anywhere (D530's asymmetry rule).
#[test]
fn the_teamless_description_claims_no_roster_and_names_no_reply_channel() {
    let tool = SendMessageTool::teamless();
    let described = tool.description();

    assert!(
        !described.contains(ROSTER_HEADER),
        "no roster is claimed: {described}"
    );
    assert!(
        described.contains("chose for itself") && described.contains("nothing verifies"),
        "a registry name is labeled self-asserted: {described}"
    );
    assert!(
        described.contains("@-mention") && described.contains("uds:"),
        "the two ways a name arrives are named: {described}"
    );
    // The no-reply-channel rule, as absence: no word of hearing back
    // appears. The roster's absence is asserted on the rendered header
    // above rather than on the word, because the description's own
    // denial — "there is no teammate roster" — legitimately carries it.
    let lowered = described.to_lowercase();
    for claim in ["reply", "back"] {
        assert!(
            !lowered.contains(claim),
            "no {claim:?} in the teamless description: {described}"
        );
    }

    // Distinct from a team of one, whose empty roster is listed as such.
    let team_of_one = SendMessageTool::new(&[]);
    assert!(team_of_one.description().contains(super::NO_PEERS));
    assert_ne!(described, team_of_one.description());
}

/// The delivered path: the body crosses whole, and the model reads what
/// became of it.
#[tokio::test]
async fn a_delivered_message_reports_what_became_of_it() {
    let postbox = Arc::new(Fake::new());
    let tool = SendMessageTool::new(&postbox.roster());
    let ctx = ctx(Some(Arc::clone(&postbox) as Arc<dyn Postbox>));

    let output = tool
        .run(
            json!({"to": "worker-1", "message": "start on the parser", "summary": "kickoff"}),
            &ctx,
        )
        .await
        .expect("a plain message to a teammate sends");

    assert!(output.output.starts_with(DELIVERED), "got {output:?}");
    assert_eq!(output.metadata["structured"], json!(false));
    let delivered = postbox.delivered.lock().expect("no panic");
    assert_eq!(
        delivered.first().map(|(_, body)| body.clone()),
        Some(Body::Text {
            text: "start on the parser".to_owned(),
            summary: Some("kickoff".to_owned()),
        })
    );
}

/// AC-21: the wording is ganja's and may improve, but every refusal is
/// rendered out of a constant a reviewer can find. The match below is
/// exhaustive on purpose — a rung added without a constant does not
/// compile here.
#[test]
fn every_refusal_is_a_declared_constant() {
    fn declared(refused: Refused) -> &'static str {
        match refused {
            Refused::NoTeam => NO_TEAM,
            Refused::Broadcast => BROADCAST,
            Refused::UnsupportedScheme { .. } => UNSUPPORTED_SCHEME,
            Refused::InvalidSocketPath => INVALID_SOCKET_PATH,
            Refused::NotASessionSocket { .. } => NOT_A_SESSION_SOCKET,
            Refused::ScopedRecipient => SCOPED_RECIPIENT,
            Refused::Whitespace => WHITESPACE,
            Refused::StructuredOverSocket => STRUCTURED_OVER_SOCKET,
            Refused::ProtocolFrame { .. } => PROTOCOL_FRAME,
            Refused::LifecycleFrame { .. } => LIFECYCLE_FRAME,
            Refused::StructuredNotAFrame => STRUCTURED_NOT_A_FRAME,
            Refused::HarnessOnlyFrame { .. } => HARNESS_ONLY_FRAME,
            Refused::ShutdownApprovedNotToLead => SHUTDOWN_APPROVED_NOT_TO_LEAD,
        }
    }

    let every = [
        Refused::NoTeam,
        Refused::Broadcast,
        Refused::UnsupportedScheme { scheme: "did:" },
        Refused::InvalidSocketPath,
        Refused::NotASessionSocket {
            why: AddressRefusal::NotASessionName,
        },
        Refused::ScopedRecipient,
        Refused::Whitespace,
        Refused::StructuredOverSocket,
        Refused::ProtocolFrame {
            kind: "mode_set_request",
        },
        Refused::LifecycleFrame {
            kind: "task_completed",
        },
        Refused::StructuredNotAFrame,
        Refused::HarnessOnlyFrame {
            kind: "shutdown_rejected",
        },
        Refused::ShutdownApprovedNotToLead,
    ];

    // The count moves with the ladder, and moving it is the moment to ask
    // whether the new rung earned its place.
    assert_eq!(every.len(), 13, "every kind the ladder can produce");
    for refused in every {
        let sentence = refused.sentence("worker-1", Some("team-lead"));
        assert!(
            sentence.contains(declared(refused)),
            "{refused:?} renders through its constant: {sentence}"
        );
    }
}

/// The tool offered without a team behind it still answers in words.
#[tokio::test]
async fn a_call_without_a_team_is_refused_readably() {
    let tool = SendMessageTool::new(&[]);
    let message = match tool
        .run(json!({"to": "worker-1", "message": "hello"}), &ctx(None))
        .await
    {
        Err(ToolError::Failed(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    };

    assert_eq!(message, NO_TEAM);
}

/// The roster is what makes a `to` argument answerable, so it is in the
/// description, in name order, with the lead marked.
#[test]
fn the_description_lists_the_team_with_its_lead_marked() {
    let tool = SendMessageTool::new(&Fake::new().roster);
    let described = tool.description();

    assert!(described.starts_with(DESCRIPTION), "got {described}");
    let (_, listed) = described
        .split_once(ROSTER_HEADER)
        .expect("the roster header is appended");
    let roster: Vec<&str> = listed
        .lines()
        .filter(|line| line.starts_with("- "))
        .collect();
    assert_eq!(
        roster,
        vec![
            &format!("- team-lead: runs the team ({LEAD_MARK})")[..],
            "- worker-1: a teammate of this session",
        ]
    );
}

/// §5.3's cap, applied before anything crosses the seam.
#[test]
fn a_summary_is_capped_before_it_crosses_the_seam() {
    assert_eq!(cap_summary(None), None);
    assert_eq!(cap_summary(Some("  ".to_owned())), None);
    assert_eq!(
        cap_summary(Some("あ".repeat(300))).map(|summary| summary.chars().count()),
        Some(200),
        "counted in characters, so a multi-byte summary is not cut mid-character"
    );
}
