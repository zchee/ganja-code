use std::time::Duration;

use ganja_core::{
    team::{MemberName, TeamName, record},
    teammate::posture::Posture,
};
use ganja_protocol::{
    Event, FinishReason, PermissionId, PermissionMode, PermissionReply, SessionId,
    team::{
        Frame, IdleReason, ModeSetRequest, PermissionResponse, PermissionResponseBody,
        PlanApprovalResponse, TaskAssignment,
    },
};
use tempfile::TempDir;

use super::{
    Flags, Inbox, Membership, PARENT, RECORD_POLL, flags, held, membership, shutdown_request,
    write, write_frame,
};

/// The `w1` fixture, under a temporary home.
fn member(home: &TempDir, pane: Option<&str>) -> Membership {
    membership(home.path(), pane)
}

/// The root the flags resolve to is the one a lead of the same session
/// writes into — asked of the registry, never spelled here.
#[test]
fn a_member_reads_the_directory_its_lead_wrote_into() {
    let home = tempfile::tempdir().expect("a temporary home");
    let member = member(&home, Some("%7"));
    let lead =
        ganja_core::teammate::TeammateRegistry::for_session(home.path(), PARENT, home.path());

    assert_eq!(member.lead_inbox(), lead.lead_inbox());
    assert_eq!(
        member.inbox(),
        lead.root()
            .inbox_path(lead.team(), &MemberName::parse("w1").expect("a name")),
    );
    assert_eq!(
        member.team(),
        &TeamName::parse("session-224cbeab").expect("a team")
    );
    assert_eq!(member.color(), Some("blue"));
    assert_eq!(member.parent_session_id(), PARENT);
    assert_eq!(
        member.surface(),
        &ganja_core::team::Surface::Pane {
            id: "%7".to_owned()
        }
    );
}

/// The id the lead recorded has to be the one these flags describe.
#[test]
fn an_agent_id_naming_another_member_is_refused_before_anything_runs() {
    let home = tempfile::tempdir().expect("a temporary home");
    let mut wrong = flags("w1");
    wrong.agent_id = "w2@session-224cbeab".to_owned();

    let refused = Membership::resolve(wrong, home.path(), home.path(), None)
        .expect_err("a mismatched id is refused");

    assert!(
        refused.to_string().contains("w1@session-224cbeab"),
        "the refusal names what was expected: {refused}"
    );
    assert!(
        Membership::resolve(
            Flags {
                name: "main".to_owned(),
                ..flags("main")
            },
            home.path(),
            home.path(),
            None,
        )
        .is_err(),
        "and the reserved name is refused by the grammar"
    );
}

/// The seeded message — the preamble around the task — is the first message the first pass finds, and it is
/// still owed until the app says it landed (§10.3-2).
#[tokio::test]
async fn the_seeded_prompt_is_a_plain_message_that_stays_until_delivered() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, None));
    write(&inbox.inbox, "team-lead", "start on the parser");

    let pass = inbox.poll().await;

    assert_eq!(pass.messages.len(), 1);
    assert_eq!(pass.messages[0].from, "team-lead");
    assert_eq!(pass.messages[0].body, "start on the parser");
    assert_eq!(held(&inbox.inbox).len(), 1, "not pruned by the read");

    inbox.delivered(&pass.messages).await;

    assert!(held(&inbox.inbox).is_empty(), "delivered means gone");
}

/// §6.1's first step: a shutdown request goes ahead of everything.
#[tokio::test]
async fn a_shutdown_request_goes_ahead_of_everything_else() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, Some("%3")));
    write(&inbox.inbox, "team-lead", "one");
    write(&inbox.inbox, "w2", "two");
    write_frame(&inbox.inbox, "team-lead", &shutdown_request("req-9"));

    let pass = inbox.poll().await;

    assert_eq!(pass.shutdown.as_deref(), Some("req-9"));
    assert!(
        pass.messages.is_empty(),
        "nothing else is delivered: {pass:?}"
    );
    assert_eq!(
        held(&inbox.inbox).len(),
        2,
        "the request left the inbox, the messages it jumped did not"
    );
}

/// The answer names the pane and the backend, stamped with this member's
/// own name, in the lead's inbox.
#[tokio::test]
async fn a_shutdown_answer_names_the_pane_and_reaches_the_lead() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, Some("%3")));

    inbox.approve_shutdown("req-9").await;

    let written = held(&inbox.lead_inbox);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].from, "w1");
    match written[0].frame() {
        Some(Frame::ShutdownApproved(approved)) => {
            assert_eq!(approved.request_id, "req-9");
            assert_eq!(approved.from, "w1");
            assert_eq!(approved.pane_id.as_deref(), Some("%3"));
            assert_eq!(approved.backend_type.as_deref(), Some("tmux"));
        }
        other => panic!("a shutdown_approved was expected, got {other:?}"),
    }
}

/// Outside tmux there is no pane to name, and none is invented.
#[tokio::test]
async fn a_member_with_no_pane_names_none() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, Some("")));

    inbox.approve_shutdown("req-1").await;

    match held(&inbox.lead_inbox)[0].frame() {
        Some(Frame::ShutdownApproved(approved)) => {
            assert_eq!(approved.pane_id, None);
            assert_eq!(approved.backend_type, None);
        }
        other => panic!("a shutdown_approved was expected, got {other:?}"),
    }
}

/// §10.3-3's mapping, and the frame's own `from`.
#[tokio::test]
async fn the_turns_end_maps_onto_the_three_idle_reasons() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, None));

    inbox.report_idle(FinishReason::Completed, None).await;
    inbox.report_idle(FinishReason::Cancelled, None).await;
    inbox
        .report_idle(FinishReason::Failed, Some("the provider hung up"))
        .await;

    let reasons: Vec<_> = held(&inbox.lead_inbox)
        .iter()
        .map(|message| {
            assert_eq!(message.from, "w1");
            match message.frame() {
                Some(Frame::IdleNotification(idle)) => {
                    assert_eq!(idle.from, "w1");
                    (idle.idle_reason, idle.failure_reason)
                }
                other => panic!("an idle_notification was expected, got {other:?}"),
            }
        })
        .collect();

    assert_eq!(
        reasons,
        [
            (Some(IdleReason::Available), None),
            (Some(IdleReason::Interrupted), None),
            (
                Some(IdleReason::Failed),
                Some("the provider hung up".to_owned())
            ),
        ]
    );
}

/// §7-2 as a type: the lead's mode is applied, a peer's identical frame is
/// dropped by name, and a mode this build cannot hold is refused rather
/// than rounded (**D496**). Every one of them leaves the inbox.
#[tokio::test]
async fn a_mode_is_taken_from_the_lead_only_and_refused_by_name_when_unknown() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, None));
    let bypass = |from: &str| {
        Frame::ModeSetRequest(ModeSetRequest {
            mode: "bypassPermissions".to_owned(),
            from: from.to_owned(),
        })
    };
    write_frame(&inbox.inbox, "team-lead", &bypass("team-lead"));
    write_frame(&inbox.inbox, "w2", &bypass("team-lead"));
    write_frame(
        &inbox.inbox,
        "team-lead",
        &Frame::ModeSetRequest(ModeSetRequest {
            mode: "plan".to_owned(),
            from: "team-lead".to_owned(),
        }),
    );

    let pass = inbox.poll().await;

    assert_eq!(pass.modes, [PermissionMode::Bypass]);
    assert_eq!(pass.dropped, ["mode_set_request", "mode_set_request"]);
    assert!(pass.messages.is_empty());
    assert!(held(&inbox.inbox).is_empty(), "every frame left the inbox");
}

/// Nothing here asks for a plan, so every approval is stale — and a peer's
/// approval is not even that, it is dropped.
#[tokio::test]
async fn a_plan_approval_is_stale_by_definition_and_leaves_the_inbox() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, None));
    // No `from` on this frame at all: the envelope is the only sender it
    // has, which is exactly why the peer's copy below is dropped on the
    // envelope alone.
    let approval = Frame::PlanApprovalResponse(PlanApprovalResponse {
        request_id: "plan-1".to_owned(),
        approved: true,
        feedback: None,
        timestamp: record::now_iso8601(),
        permission_mode: None,
    });
    write_frame(&inbox.inbox, "team-lead", &approval);
    write_frame(&inbox.inbox, "w2", &approval);

    let pass = inbox.poll().await;

    assert_eq!(pass.ignored, 1);
    assert_eq!(pass.dropped, ["plan_approval_response"]);
    assert!(held(&inbox.inbox).is_empty());
}

/// A `task_assignment` from the lead becomes this member's next turn, and
/// is pruned by the identity the app holds even though the body it holds
/// is the rendering rather than the frame.
#[tokio::test]
async fn a_task_assignment_from_the_lead_becomes_a_message_and_prunes_by_the_rendered_identity() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, None));
    let assignment = |assigned_by: &str| {
        Frame::TaskAssignment(TaskAssignment {
            task_id: "t-1".to_owned(),
            subject: "look at the parser".to_owned(),
            description: "the whole of it".to_owned(),
            assigned_by: assigned_by.to_owned(),
            timestamp: record::now_iso8601(),
        })
    };
    write_frame(&inbox.inbox, "team-lead", &assignment("team-lead"));
    write_frame(&inbox.inbox, "w2", &assignment("team-lead"));

    let pass = inbox.poll().await;

    assert_eq!(pass.messages.len(), 1);
    assert_eq!(pass.messages[0].from, "team-lead");
    assert_eq!(
        pass.messages[0].summary.as_deref(),
        Some("look at the parser")
    );
    assert_eq!(
        pass.messages[0].body,
        "look at the parser\n\nthe whole of it"
    );
    assert_eq!(
        pass.dropped,
        ["task_assignment"],
        "a peer cannot assign work"
    );
    assert_eq!(
        held(&inbox.inbox).len(),
        1,
        "the lead's stays until delivered"
    );

    inbox.delivered(&pass.messages).await;

    assert!(
        held(&inbox.inbox).is_empty(),
        "and the rendered identity found it"
    );
}

/// The other frames the harness may write are named and dropped, never
/// read as prose.
#[tokio::test]
async fn an_unhandled_frame_is_dropped_by_name_and_never_delivered() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, None));
    write_frame(
        &inbox.inbox,
        "team-lead",
        &Frame::TeammateTerminated(ganja_protocol::team::TeammateTerminated {
            message: "w2 is gone".to_owned(),
        }),
    );

    let pass = inbox.poll().await;

    assert_eq!(pass.dropped, ["teammate_terminated"]);
    assert!(pass.messages.is_empty());
    assert!(held(&inbox.inbox).is_empty());
}

/// A pane's asks go to the lead's own dialog (D-5), and since **D513** the
/// launch line carries nothing that could say otherwise.
#[test]
fn a_pane_forwards_its_asks_to_its_lead() {
    let home = tempfile::tempdir().expect("a temporary home");

    assert_eq!(member(&home, None).posture(), Posture::ForwardToLead);
    assert_eq!(
        membership(home.path(), Some("%5")).posture(),
        Posture::ForwardToLead,
        "a pane id changes the surface, never the posture"
    );
}

/// The record is the lead's to write before it types the launch line, so
/// the wait is defensive — and a lead that never writes one is refused
/// naming the file rather than waited on forever.
#[tokio::test]
async fn a_member_waits_for_its_record_and_refuses_when_no_lead_writes_one() {
    let home = tempfile::tempdir().expect("a temporary home");
    let member = member(&home, None);
    assert_eq!(member.record().expect("no file is no record"), None);

    let refused = member
        .await_record(RECORD_POLL)
        .await
        .expect_err("nothing wrote a record");
    assert!(
        refused.to_string().contains("config.json"),
        "the refusal names the file: {refused}"
    );

    // A lead of the same session writes it a moment later, exactly as the
    // registry does — the record after the spawn — and the wait finds it.
    let lead =
        ganja_core::teammate::TeammateRegistry::for_session(home.path(), PARENT, home.path());
    let path = lead.root().config_path(lead.team());
    let mut team = ganja_core::team::TeamFile::new(
        lead.team(),
        PARENT,
        home.path().display().to_string(),
        record::now_millis(),
    );
    team.members.push(ganja_core::team::MemberRecord::teammate(
        member.name(),
        lead.team(),
        ganja_core::team::Spawn {
            agent_type: "general".to_owned(),
            model: "recorder-model".to_owned(),
            color: "blue".to_owned(),
            prompt: "start on the parser".to_owned(),
            plan_mode_required: false,
            surface: ganja_core::team::Surface::Pane {
                id: "%7".to_owned(),
            },
            cwd: home.path().display().to_string(),
        },
        record::now_millis(),
    ));
    let writer = tokio::spawn(async move {
        tokio::time::sleep(RECORD_POLL * 2).await;
        std::fs::create_dir_all(path.parent().expect("a team dir")).expect("the dir");
        std::fs::write(
            &path,
            ganja_core::team::record::document(&team).expect("the team encodes"),
        )
        .expect("the team file is written");
    });

    let found = member
        .await_record(Duration::from_secs(5))
        .await
        .expect("the record arrived within the wait");
    writer.await.expect("the writer finished");

    assert_eq!(found.name, "w1");
    assert_eq!(found.model.as_deref(), Some("recorder-model"));
}

/// The ask an engine raises, as the app hands it over.
fn asked(id: &str) -> Event {
    Event::PermissionRequested {
        session_id: SessionId::from("ses_fixture".to_owned()),
        id: PermissionId::from(id.to_owned()),
        call_id: "call-1".to_owned(),
        tool: "bash".to_owned(),
        title: "rm -rf build".to_owned(),
        args: serde_json::json!({"command": "rm -rf build"}),
        directories: vec!["/tmp/elsewhere".to_owned()],
    }
}

/// An ask travels to the lead's inbox as §5's `permission_request`, from
/// this member's own name, and is remembered as waiting until the engine's
/// own reply forgets it. The frame's fields are `Asks::forward`'s to fill,
/// and core's own tests pin them.
#[tokio::test]
async fn a_forwarded_ask_reaches_the_lead_as_a_permission_request() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, None));

    inbox
        .asks()
        .forward(&asked("perm-1"))
        .await
        .expect("the lead's inbox takes the ask");

    assert_eq!(inbox.asks().waiting(), 1);
    let written = held(&inbox.lead_inbox);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].from, "w1");
    assert!(
        matches!(written[0].frame(), Some(Frame::PermissionRequest(_))),
        "a permission_request was expected, got {:?}",
        written[0].frame()
    );
    assert!(
        inbox
            .asks()
            .retire(&PermissionId::from("perm-1".to_owned())),
        "an engine's own reply forgets the wait"
    );
    assert_eq!(inbox.asks().waiting(), 0);
}

/// The lead's answer comes back through the pass as the reply the dialog
/// gave — an "always" honored, since the person at the lead's dialog
/// answered this member's own open ask — a peer's copy is dropped on the
/// envelope, and an answer to nothing waited on is ignored. Every one
/// leaves the inbox.
#[tokio::test]
async fn a_leads_answer_resolves_a_waiting_ask_and_a_peers_or_a_stale_one_does_not() {
    let home = tempfile::tempdir().expect("a temporary home");
    let inbox = Inbox::new(member(&home, None));
    inbox
        .asks()
        .forward(&asked("perm-1"))
        .await
        .expect("the ask is forwarded");
    inbox
        .asks()
        .forward(&asked("perm-2"))
        .await
        .expect("the ask is forwarded");
    let allowed = Frame::PermissionResponse(PermissionResponse::success(
        "perm-1",
        PermissionResponseBody {
            updated_input: serde_json::json!({"command": "rm -rf build"}),
            permission_updates: vec![serde_json::json!({
                "type": "addRules",
                "behavior": "allow",
                "rules": [{"toolName": "bash"}],
                "destination": "projectSettings",
            })],
        },
    ));
    let refused = Frame::PermissionResponse(PermissionResponse::error("perm-2", "no"));
    let stale = Frame::PermissionResponse(PermissionResponse::error("perm-9", "no"));
    write_frame(&inbox.inbox, "w2", &allowed);
    write_frame(&inbox.inbox, "team-lead", &allowed);
    write_frame(&inbox.inbox, "team-lead", &refused);
    write_frame(&inbox.inbox, "team-lead", &stale);

    let pass = inbox.poll().await;

    assert_eq!(
        pass.answers,
        [
            (
                PermissionId::from("perm-1".to_owned()),
                PermissionReply::Always
            ),
            (
                PermissionId::from("perm-2".to_owned()),
                PermissionReply::Reject
            ),
        ]
    );
    assert_eq!(
        pass.dropped,
        ["permission_response"],
        "a peer cannot answer"
    );
    assert_eq!(pass.ignored, 1, "an answer to nothing waited on is stale");
    assert_eq!(inbox.asks().waiting(), 0);
    assert!(held(&inbox.inbox).is_empty());
}
