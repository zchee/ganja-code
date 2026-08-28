use std::sync::Arc;

use ganja_protocol::team::{Frame, LeadFrame, PermissionResponse, PermissionResponseBody};
use ganja_team::{
    LEAD, MailboxMessage, MemberName, MemberRecord, Spawn, Surface, TeamFile, TeamName, TeamsRoot,
    mailbox, record,
};
use serde_json::json;

use super::{
    Asks, IGNORED_STALE_ANSWER, MemberPostbox, REFUSED_AT_DIALOG, Resolved, Unforwarded, ask_of,
    dialog_of, reply_of, response_of,
};
use crate::protocol::{Event, PermissionId, PermissionReply, SessionId};
use crate::tool::team::{Address, Body, Peer, Postbox as _, Reserved, Undelivered};

/// A teams root under a throwaway home, and the names both sides use.
struct Team {
    _home: tempfile::TempDir,
    root: TeamsRoot,
    team: TeamName,
}

impl Team {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("a temporary home");
        let root = TeamsRoot::new(home.path().join("teams"));

        Self { _home: home, root, team: TeamName::parse("session-abcd1234").expect("a team name") }
    }

    /// Writes a team file naming the lead and `teammates`, the way a lead
    /// writes one.
    fn write_file(&self, teammates: &[&str]) {
        let mut file = TeamFile::new(&self.team, "lead-session", "/tmp", record::now_millis());
        for name in teammates {
            let member = MemberName::parse(name).expect("a member name");
            file.members.push(MemberRecord::teammate(
                &member,
                &self.team,
                Spawn {
                    agent_type: "general".to_owned(),
                    model: "recorder-model".to_owned(),
                    prompt: "hold the fort".to_owned(),
                    cwd: "/tmp".to_owned(),
                    color: "blue".to_owned(),
                    plan_mode_required: false,
                    surface: Surface::Pane { id: "%3".to_owned() },
                },
                record::now_millis(),
            ));
        }
        let path = self.root.config_path(&self.team);
        std::fs::create_dir_all(path.parent().expect("a team directory")).expect("mkdir");
        std::fs::write(&path, record::document(&file).expect("the file encodes"))
            .expect("the team file writes");
    }

    fn postbox(&self, name: &str) -> MemberPostbox {
        MemberPostbox::new(
            MemberName::parse(name).expect("a member name"),
            self.team.clone(),
            self.root.clone(),
        )
    }

    fn inbox(&self, name: &str) -> std::path::PathBuf {
        self.root.inbox_path(&self.team, &MemberName::parse(name).expect("a member name"))
    }

    fn held(&self, name: &str) -> Vec<MailboxMessage> {
        mailbox::read(&self.inbox(name)).map(|contents| contents.valid).unwrap_or_default()
    }
}

/// The lead is addressable from the first millisecond of a pane's life,
/// before the team file that will name the pane exists.
#[tokio::test]
async fn a_member_can_reach_its_lead_before_the_team_file_exists() {
    let team = Team::new();
    let postbox = team.postbox("worker");

    let roster = postbox.roster();
    assert_eq!(
        roster,
        [Peer { name: LEAD.to_owned(), description: Some(super::LEADS.to_owned()), lead: true }],
        "the lead, and only the lead, with no file to read"
    );

    let sent = postbox
        .deliver(
            Address::Local("Team-Lead".to_owned()),
            Body::Text {
                text: "the parser is done".to_owned(),
                summary: Some("parser".to_owned()),
            },
        )
        .await
        .expect("the lead's inbox takes it");
    assert_eq!(sent.to, LEAD, "the canonical spelling comes back");

    let held = team.held(LEAD);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].from, "worker");
    assert_eq!(held[0].text, "the parser is done");
    assert_eq!(held[0].summary.as_deref(), Some("parser"));
}

/// The roster is the file's, minus the reader, and a peer the file names
/// is reachable while one it does not is unknown.
#[tokio::test]
async fn a_members_roster_is_the_team_file_without_itself() {
    let team = Team::new();
    team.write_file(&["worker", "reviewer"]);
    let postbox = team.postbox("worker");

    let names: Vec<String> = postbox.roster().into_iter().map(|peer| peer.name).collect();
    assert_eq!(names, [LEAD, "reviewer"], "the lead first, then the peers");
    assert_eq!(postbox.roster().iter().filter(|peer| peer.lead).count(), 1, "exactly one lead");

    postbox
        .deliver(
            Address::Local("reviewer".to_owned()),
            Body::Text { text: "look at the parser".to_owned(), summary: None },
        )
        .await
        .expect("a named peer is reachable");
    assert_eq!(team.held("reviewer").len(), 1);

    assert_eq!(
        postbox
            .deliver(
                Address::Local("nobody".to_owned()),
                Body::Text { text: "hello?".to_owned(), summary: None },
            )
            .await,
        Err(Undelivered::Unknown),
        "a name the file does not hold is nobody"
    );
}

/// The sender is the postbox's, never the message's: a member built as
/// `worker` writes `worker`, whatever the body claims about itself.
#[tokio::test]
async fn a_member_postbox_stamps_the_name_it_was_built_with() {
    let team = Team::new();
    let postbox = team.postbox("worker");

    postbox
        .deliver(
            Address::Local(LEAD.to_owned()),
            Body::Frame(json!({
                "type": "shutdown_approved",
                "requestId": "req-1",
                "from": LEAD,
                "timestamp": record::now_iso8601(),
            })),
        )
        .await
        .expect("the frame is written");

    let held = team.held(LEAD);
    assert_eq!(held[0].from, "worker", "the envelope says who wrote it");
}

#[test]
fn a_member_postbox_classifies_with_the_protocols_own_lists() {
    let team = Team::new();
    let postbox = team.postbox("worker");

    assert_eq!(postbox.classify("just a message"), Reserved::No);
    assert_eq!(
        postbox.classify(r#"{"type":"shutdown_approved","requestId":"r1"}"#),
        Reserved::AgentSendable { kind: "shutdown_approved" }
    );
    assert_eq!(
        postbox.classify(r#"{"type":"idle_notification"}"#),
        Reserved::HarnessOnly { kind: "idle_notification" }
    );
}

#[tokio::test]
async fn a_uds_address_is_validated_but_has_no_transport_yet() {
    let team = Team::new();
    let outcome = team
        .postbox("worker")
        .deliver(
            Address::Uds { path: "/tmp/peer.sock".into() },
            Body::Text { text: "hello".to_owned(), summary: None },
        )
        .await;

    assert!(matches!(outcome, Err(Undelivered::NoTransport { .. })), "{outcome:?}");
}

/// One dialog as the member's engine publishes it.
fn ask(id: &str, directories: &[&str]) -> Event {
    Event::PermissionRequested {
        session_id: SessionId::from("member-session".to_owned()),
        id: PermissionId::from(id.to_owned()),
        call_id: "call-1".to_owned(),
        tool: "bash".to_owned(),
        title: "rm -rf build".to_owned(),
        args: json!({"command": "rm -rf build"}),
        directories: directories.iter().map(|d| (*d).to_owned()).collect(),
    }
}

/// The two halves of the dialect agree: what a member writes is what the
/// lead's dialog shows, directories included, and the answer comes back
/// as the reply the person gave.
#[test]
fn the_dialect_round_trips_a_dialog_and_its_answer() {
    let request = ask("req-1", &["/srv/other"]);
    let frame = ask_of("worker@session-abcd1234", &request).expect("an ask");
    assert_eq!(frame.request_id, "req-1");
    assert_eq!(frame.agent_id, "worker@session-abcd1234");
    assert_eq!(frame.tool_name, "bash");
    assert_eq!(frame.tool_use_id, "call-1");
    assert_eq!(frame.description, "rm -rf build");
    assert_eq!(frame.permission_suggestions.len(), 1, "one directory disclosure");

    let dialog = dialog_of(SessionId::from("lead-session".to_owned()), frame);
    let Event::PermissionRequested { id, tool, title, args, directories, .. } = &dialog else {
        panic!("a dialog is a permission request");
    };
    assert_ne!(
        id.as_str(),
        "req-1",
        "the dialog's id is the lead's own mint, never the member's string"
    );
    assert_eq!(tool, "bash");
    assert_eq!(title, "rm -rf build");
    assert_eq!(args, &json!({"command": "rm -rf build"}));
    assert_eq!(directories, &["/srv/other"]);

    // No directories, no suggestion at all: the common frame stays small.
    let plain = ask_of("worker@session-abcd1234", &ask("req-2", &[])).expect("an ask");
    assert!(plain.permission_suggestions.is_empty());

    for reply in [PermissionReply::Once, PermissionReply::Always, PermissionReply::Reject] {
        let response = response_of("req-1", "bash", &json!({"command": "rm -rf build"}), reply);
        assert!(response.is_consistent());
        assert_eq!(reply_of(&response), reply, "{reply:?} survives the frame");
    }
    assert_eq!(
        response_of("req-1", "bash", &json!({}), PermissionReply::Reject).error_message(),
        Some(REFUSED_AT_DIALOG)
    );
}

/// An update this build does not recognise is read as once, and a frame
/// whose arms contradict its tag is a refusal.
#[test]
fn an_unknown_update_is_once_and_an_inconsistent_frame_refuses() {
    let response = PermissionResponse::success(
        "req-1",
        PermissionResponseBody {
            updated_input: json!({}),
            permission_updates: vec![json!({"type": "setMode", "mode": "acceptEdits"})],
        },
    );
    assert_eq!(reply_of(&response), PermissionReply::Once);

    let crossed: PermissionResponse = serde_json::from_value(json!({
        "request_id": "req-1",
        "subtype": "success",
        "error": "but also no",
    }))
    .expect("the wire decodes it");
    assert!(!crossed.is_consistent());
    assert_eq!(reply_of(&crossed), PermissionReply::Reject);
}

fn asks(team: &Team) -> Asks {
    Asks::new(MemberName::parse("worker").expect("a member name"), &team.team, &team.root)
}

fn lead_answers(response: PermissionResponse) -> LeadFrame {
    LeadFrame::parse(LEAD, LEAD, Frame::PermissionResponse(response)).expect("the lead's")
}

/// A forwarded ask is written to the lead as this member, and the lead's
/// answer to it comes back as the reply — once.
#[tokio::test]
async fn a_forwarded_ask_reaches_the_lead_and_its_answer_comes_back_once() {
    let team = Team::new();
    let asks = asks(&team);

    asks.forward(&ask("req-1", &[])).await.expect("the lead's inbox takes it");
    assert_eq!(asks.waiting(), 1);

    let held = team.held(LEAD);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].from, "worker", "asked as the member, by construction");
    let Some(Frame::PermissionRequest(request)) = held[0].frame() else {
        panic!("the lead was handed something other than a permission request");
    };
    assert_eq!(request.request_id, "req-1");
    assert_eq!(request.agent_id, "worker@session-abcd1234");

    let resolved = asks.resolve(lead_answers(response_of(
        "req-1",
        "bash",
        &json!({}),
        PermissionReply::Always,
    )));
    assert_eq!(
        resolved,
        Resolved::Answered {
            id: PermissionId::from("req-1".to_owned()),
            reply: PermissionReply::Always,
        }
    );
    assert_eq!(asks.waiting(), 0, "answered means no longer waiting");

    // The same answer again is stale, and says so.
    let logged = Capture::default();
    let again = {
        let _guard = tracing::dispatcher::set_default(&logged.subscriber());
        asks.resolve(lead_answers(response_of(
            "req-1",
            "bash",
            &json!({}),
            PermissionReply::Always,
        )))
    };
    assert_eq!(again, Resolved::Stale { request_id: "req-1".to_owned() });
    assert!(
        logged.text().contains(IGNORED_STALE_ANSWER),
        "the ignoring is not silent: {}",
        logged.text()
    );
}

/// A cancelled turn ends its dialog wait with a `PermissionReplied`, and
/// retiring on it is what makes the lead's later answer stale rather than
/// applied to a call that no longer exists.
#[tokio::test]
async fn a_retired_ask_makes_a_late_answer_stale() {
    let team = Team::new();
    let asks = asks(&team);
    asks.forward(&ask("req-1", &[])).await.expect("forwarded");

    assert!(asks.retire(&PermissionId::from("req-1".to_owned())));
    assert!(
        !asks.retire(&PermissionId::from("req-1".to_owned())),
        "retiring twice is a miss, not an error"
    );
    assert_eq!(asks.waiting(), 0);
    assert!(matches!(
        asks.resolve(lead_answers(response_of("req-1", "bash", &json!({}), PermissionReply::Once))),
        Resolved::Stale { .. }
    ));
}

/// A lead frame that is not an answer is handed straight back, so the
/// caller's other handlers see it; and an event that is not an ask is
/// refused rather than written.
#[tokio::test]
async fn only_answers_are_read_and_only_asks_are_forwarded() {
    let team = Team::new();
    let asks = asks(&team);

    let shutdown = LeadFrame::parse(
        LEAD,
        LEAD,
        Frame::ShutdownRequest(ganja_protocol::team::ShutdownRequest {
            request_id: "req-9".to_owned(),
            from: LEAD.to_owned(),
            reason: None,
            timestamp: record::now_iso8601(),
        }),
    )
    .expect("the lead's");
    assert_eq!(asks.resolve(shutdown), Resolved::NotAnAnswer { kind: "shutdown_request" });

    let not_an_ask = Event::QuestionRejected {
        session_id: SessionId::from("member-session".to_owned()),
        id: crate::protocol::QuestionId::from("q-1".to_owned()),
    };
    assert_eq!(asks.forward(&not_an_ask).await, Err(Unforwarded::NotAnAsk));
    assert!(team.held(LEAD).is_empty(), "nothing was written");
}

/// A `tracing` subscriber a test can read back — the fixture
/// `tests/teammate_frames.rs` keeps, for one synchronous call here.
#[derive(Clone, Default)]
struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("the log is never poisoned")).into_owned()
    }

    fn subscriber(&self) -> tracing::Dispatch {
        tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .finish(),
        )
    }
}

impl std::io::Write for Capture {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("the log is never poisoned").extend_from_slice(buffer);

        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
