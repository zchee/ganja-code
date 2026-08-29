use std::sync::Arc;
use std::time::Duration;

use ganja_protocol::team::{
    Frame, IdleNotification, PermissionRequest, PermissionResponse, PermissionResponseBody,
    ShutdownApproved, TaskAssignment, TeamPermissionUpdate,
};
use ganja_team::{LEAD, MailboxMessage, MemberName, ShimCli, mailbox, record};

use super::{DIALOG_QUEUE_FULL, Delivered, LeadInbox, NO_DIALOG_SURFACE};
use crate::Storage;
use crate::permission::Permissions;
use crate::provider::FakeProvider;
use crate::teammate::{
    Delivery, Exited as MemberExited, InProcess, Lent, MemberBackend, PaneFate, SpawnRequest,
    SpawnSpec, Spawned, Surface, TeammateBackend, TeammateRegistry, Unsupported, member,
};
use crate::tool::Registry as Tools;

/// A registry over a throwaway teams root, with the lead's inbox seeded.
fn registry(home: &std::path::Path) -> Arc<TeammateRegistry> {
    Arc::new(TeammateRegistry::for_session(home, "224cbeab-4e62-497c-aa8f-d05cc33ce7ba", home))
}

fn write(inbox: &std::path::Path, from: &str, text: &str) {
    mailbox::write(inbox, MailboxMessage::new(from, text, record::now_iso8601()))
        .expect("the inbox takes a message");
}

fn write_frame(inbox: &std::path::Path, from: &str, frame: &Frame) {
    let message =
        MailboxMessage::from_frame(from, frame, record::now_iso8601()).expect("the frame encodes");
    mailbox::write(inbox, message).expect("the inbox takes a frame");
}

#[tokio::test]
async fn a_plain_message_is_carried_out_and_stays_until_it_is_delivered() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let inbox = registry.lead_inbox();
    write(&inbox, "w1", "the parser is done");

    let lead = LeadInbox::new(Arc::clone(&registry));
    let pass = lead.poll().await;

    assert_eq!(pass.messages.len(), 1);
    assert_eq!(pass.messages[0].from, "w1");
    assert_eq!(pass.messages[0].body, "the parser is done");
    // A peer this registry never started gives no consumption signal, so
    // the lead may not render one.
    assert_eq!(pass.messages[0].delivery, Delivery::FireAndForget);
    assert_eq!(
        mailbox::read(&inbox).expect("the inbox reads").valid.len(),
        1,
        "a message the caller has not delivered yet is still owed"
    );

    lead.delivered(&pass.messages).await;

    assert!(
        mailbox::read(&inbox).expect("the inbox reads").valid.is_empty(),
        "a delivered message does not remain"
    );
}

#[tokio::test]
async fn a_control_frame_is_acted_on_and_never_handed_out_to_be_queued() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let inbox = registry.lead_inbox();
    write_frame(
        &inbox,
        "w1",
        &Frame::ShutdownApproved(ShutdownApproved {
            request_id: "req-1".to_owned(),
            from: "w1".to_owned(),
            timestamp: record::now_iso8601(),
            pane_id: Some("in-process".to_owned()),
            backend_type: Some("in-process".to_owned()),
        }),
    );
    write_frame(
        &inbox,
        "w2",
        &Frame::IdleNotification(IdleNotification {
            from: "w2".to_owned(),
            timestamp: record::now_iso8601(),
            idle_reason: None,
            summary: Some("waiting for review".to_owned()),
            completed_task_id: None,
            completed_status: None,
            failure_reason: None,
        }),
    );

    let pass = LeadInbox::new(registry).poll().await;

    assert!(pass.messages.is_empty(), "a control frame is acted on, never queued: {pass:?}");
    assert_eq!(pass.retired.len(), 1);
    assert_eq!(pass.retired[0].name, "w1");
    assert_eq!(pass.idle.len(), 1);
    assert_eq!(pass.idle[0].summary.as_deref(), Some("waiting for review"));
    assert!(
        mailbox::read(&inbox).expect("the inbox reads").valid.is_empty(),
        "a frame the lead acted on leaves the inbox in the same pass"
    );
}

/// §7-1, as this side keeps it after **D-5**'s pane half landed.
///
/// Dropping a `permission_request` beside a `task_assignment` was
/// correct while the only asker was in-process and crossed on the
/// forwarding channel. A pane's asks travel §5's frames, so it is
/// now **routed** (the two tests below), and what §7-1 forbids is pinned
/// by the two frames that stay dropped: `team_permission_update`, the
/// reference's own first control, and `permission_response`, an answer to
/// a question this side never asks over a frame. Both are constructed
/// rather than described, because a build that grew a handler for either
/// here would be taking a rule, or a decision, out of a file — and this
/// is what would notice.
#[tokio::test]
async fn a_permission_update_an_answer_and_an_unhandled_frame_are_all_dropped_by_name() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let inbox = registry.lead_inbox();
    let mut payload = serde_json::Map::new();
    payload.insert("mode".to_owned(), serde_json::json!("acceptEdits"));
    write_frame(&inbox, "w1", &Frame::TeamPermissionUpdate(TeamPermissionUpdate { payload }));
    write_frame(
        &inbox,
        "w1",
        &Frame::PermissionResponse(PermissionResponse::success(
            "req-1",
            PermissionResponseBody {
                updated_input: serde_json::json!({}),
                permission_updates: Vec::new(),
            },
        )),
    );
    write_frame(
        &inbox,
        "w1",
        &Frame::TaskAssignment(TaskAssignment {
            task_id: "t-1".to_owned(),
            subject: "look at the parser".to_owned(),
            description: "the whole of it".to_owned(),
            assigned_by: "w1".to_owned(),
            timestamp: record::now_iso8601(),
        }),
    );

    let pass = LeadInbox::new(registry).poll().await;

    assert_eq!(pass.dropped, ["team_permission_update", "permission_response", "task_assignment"]);
    assert!(pass.messages.is_empty(), "a frame is never delivered as prose either");
    assert!(pass.asked.is_empty(), "and none of them is an ask");
    assert!(
        mailbox::read(&inbox).expect("the inbox reads").valid.is_empty(),
        "a named drop leaves the inbox rather than being read again forever"
    );
}

/// One `permission_request` as a pane writes it.
fn ask(request_id: &str) -> Frame {
    Frame::PermissionRequest(PermissionRequest {
        request_id: request_id.to_owned(),
        agent_id: "w1@session-224cbeab".to_owned(),
        tool_name: "bash".to_owned(),
        tool_use_id: "call-1".to_owned(),
        description: "rm -rf build".to_owned(),
        input: serde_json::json!({"command": "rm -rf build"}),
        permission_suggestions: Vec::new(),
    })
}

/// The frames in `name`'s inbox, once there is at least one, or after a
/// bounded wait: the answer is written by a task the pass spawned.
async fn answered(inbox: &std::path::Path) -> Vec<MailboxMessage> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let held = mailbox::read(inbox).map(|contents| contents.valid).unwrap_or_default();
        if !held.is_empty() || tokio::time::Instant::now() >= deadline {
            return held;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A pane's ask is put on the same channel the in-process asks ride, as a
/// dialog the frontend already knows how to show, and the answer given
/// there lands in the asker's inbox as one `permission_response` — no
/// frontend code of its own on either side.
#[tokio::test]
async fn a_pane_permission_request_raises_one_dialog_and_its_answer_lands_in_the_askers_inbox() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let (surface, mut dialogs) = tokio::sync::mpsc::channel(4);
    registry.forward_dialogs_to(surface);
    write_frame(&registry.lead_inbox(), "w1", &ask("req-1"));

    let pass = LeadInbox::new(Arc::clone(&registry)).poll().await;

    assert_eq!(pass.asked.len(), 1, "{pass:?}");
    assert_eq!(pass.asked[0].name, "w1");
    assert_eq!(pass.asked[0].request_id, "req-1");
    assert_eq!(pass.asked[0].tool, "bash");
    assert!(pass.asked[0].raised, "it reached the channel");
    assert!(pass.dropped.is_empty(), "nothing was dropped: {pass:?}");
    assert!(
        mailbox::read(&registry.lead_inbox()).expect("the inbox reads").valid.is_empty(),
        "a routed ask leaves the lead's inbox in the same pass"
    );

    let forwarded = dialogs.try_recv().expect("exactly one dialog was raised");
    assert!(dialogs.try_recv().is_err(), "and only one");
    assert_eq!(forwarded.teammate, "w1");
    let crate::protocol::Event::PermissionRequested { id, tool, title, args, .. } =
        &forwarded.request
    else {
        panic!("the channel carries permission requests: {:?}", forwarded.request);
    };
    assert_ne!(
        id.as_str(),
        "req-1",
        "the dialog id is the lead's own mint, never the pane's string"
    );
    assert_eq!(tool, "bash");
    assert_eq!(title, "rm -rf build");
    assert_eq!(args, &serde_json::json!({"command": "rm -rf build"}));

    forwarded
        .reply
        .send(crate::protocol::PermissionReply::Once)
        .expect("the answer task is waiting");

    let inbox = registry
        .root()
        .inbox_path(registry.team(), &MemberName::parse("w1").expect("a member name"));
    let held = answered(&inbox).await;
    assert_eq!(held.len(), 1, "one answer, in the asker's own inbox");
    assert_eq!(held[0].from, LEAD, "stamped as the lead");
    let Some(Frame::PermissionResponse(response)) = held[0].frame() else {
        panic!("the answer is a permission response: {:?}", held[0]);
    };
    assert_eq!(response.request_id(), "req-1");
    assert_eq!(
        member::reply_of(&response),
        crate::protocol::PermissionReply::Once,
        "and it says what the person said"
    );
}

/// A member-supplied request id never becomes the key a lead's dialogs
/// are held under: two members reusing one id get two dialogs the lead
/// can tell apart, and each answer lands in the inbox of the member that
/// asked, carrying that member's own id back.
#[tokio::test]
async fn two_members_reusing_one_request_id_get_two_dialogs_and_the_right_answers() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let (surface, mut dialogs) = tokio::sync::mpsc::channel(4);
    registry.forward_dialogs_to(surface);
    write_frame(&registry.lead_inbox(), "w1", &ask("shared"));
    write_frame(&registry.lead_inbox(), "w2", &ask("shared"));

    let pass = LeadInbox::new(Arc::clone(&registry)).poll().await;
    assert_eq!(pass.asked.len(), 2, "{pass:?}");

    let first = dialogs.try_recv().expect("w1's dialog");
    let second = dialogs.try_recv().expect("w2's dialog");
    assert!(dialogs.try_recv().is_err());
    assert_eq!(first.teammate, "w1");
    assert_eq!(second.teammate, "w2");
    let (
        crate::protocol::Event::PermissionRequested { id: one, .. },
        crate::protocol::Event::PermissionRequested { id: two, .. },
    ) = (&first.request, &second.request)
    else {
        panic!("the channel carries permission requests");
    };
    assert_ne!(one, two, "one member's id cannot shadow another's dialog");

    first.reply.send(crate::protocol::PermissionReply::Once).expect("w1's answer task is waiting");
    second
        .reply
        .send(crate::protocol::PermissionReply::Reject)
        .expect("w2's answer task is waiting");

    for (name, reply) in [
        ("w1", crate::protocol::PermissionReply::Once),
        ("w2", crate::protocol::PermissionReply::Reject),
    ] {
        let inbox = registry
            .root()
            .inbox_path(registry.team(), &MemberName::parse(name).expect("a member name"));
        let held = answered(&inbox).await;
        assert_eq!(held.len(), 1, "one answer for {name}: {held:?}");
        let Some(Frame::PermissionResponse(response)) = held[0].frame() else {
            panic!("the answer is a permission response: {:?}", held[0]);
        };
        assert_eq!(response.request_id(), "shared", "the frame's own id goes back to {name}");
        assert_eq!(member::reply_of(&response), reply, "{name}'s own answer");
    }
}

/// An ask nobody can be shown — no dialog surface at all — is refused
/// into the asker's inbox rather than left to wait on a dialog nobody
/// will see; and a channel that is full is refused the same way.
#[tokio::test]
async fn a_pane_permission_request_nobody_can_be_shown_is_refused_into_its_inbox() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let inbox = registry
        .root()
        .inbox_path(registry.team(), &MemberName::parse("w1").expect("a member name"));

    // No surface attached at all.
    write_frame(&registry.lead_inbox(), "w1", &ask("req-1"));
    let pass = LeadInbox::new(Arc::clone(&registry)).poll().await;
    assert_eq!(pass.asked.len(), 1, "{pass:?}");
    assert!(!pass.asked[0].raised, "it could not be raised");

    let held = answered(&inbox).await;
    assert_eq!(held.len(), 1);
    let Some(Frame::PermissionResponse(response)) = held[0].frame() else {
        panic!("the refusal is a permission response: {:?}", held[0]);
    };
    assert_eq!(response.request_id(), "req-1");
    assert_eq!(response.error_message(), Some(NO_DIALOG_SURFACE));
    assert_eq!(member::reply_of(&response), crate::protocol::PermissionReply::Reject);
    mailbox::prune_delivered(&inbox, &[mailbox::identity(&held[0])]).expect("pruned");

    // A surface whose queue is full: one slot, already taken.
    let (surface, mut dialogs) = tokio::sync::mpsc::channel(1);
    registry.forward_dialogs_to(surface);
    write_frame(&registry.lead_inbox(), "w1", &ask("req-2"));
    write_frame(&registry.lead_inbox(), "w1", &ask("req-3"));
    let pass = LeadInbox::new(Arc::clone(&registry)).poll().await;
    assert_eq!(pass.asked.len(), 2, "{pass:?}");
    assert!(pass.asked[0].raised, "the first takes the slot");
    assert!(!pass.asked[1].raised, "the second finds it full");

    let held = answered(&inbox).await;
    assert_eq!(held.len(), 1, "only the refused one is answered so far");
    let Some(Frame::PermissionResponse(response)) = held[0].frame() else {
        panic!("the refusal is a permission response: {:?}", held[0]);
    };
    assert_eq!(response.request_id(), "req-3");
    assert_eq!(response.error_message(), Some(DIALOG_QUEUE_FULL));

    // The one that was raised is still waiting on the person.
    let forwarded = dialogs.try_recv().expect("req-2 is on the channel");
    drop(forwarded);
    // Dropping the reply sender is the lead giving up on the dialog, which
    // is written back as the refusal it is.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline
        && mailbox::read(&inbox).map(|contents| contents.valid.len()).unwrap_or_default() < 2
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let held = mailbox::read(&inbox).expect("the inbox reads").valid;
    assert_eq!(held.len(), 2, "{held:?}");
    let Some(Frame::PermissionResponse(response)) = held[1].frame() else {
        panic!("the refusal is a permission response: {:?}", held[1]);
    };
    assert_eq!(response.request_id(), "req-2");
    assert_eq!(member::reply_of(&response), crate::protocol::PermissionReply::Reject);
}

/// The other half of a `shutdown_approved`, which the frame test above
/// cannot see: the member it names is really in the roster and really in
/// the team file, and one pass takes it out of both.
///
/// Driven through [`LeadInbox::poll`] rather than through
/// [`TeammateRegistry::retire`] directly, because the pass is what a lead
/// runs and the rewrite is the half only the lead can do — a document that
/// went on naming a conversation that has ended is what a resumed session
/// would read back.
#[tokio::test]
async fn a_shutdown_approved_takes_the_member_out_of_the_roster_and_the_team_file() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    registry
        .spawn(
            Arc::new(InProcess::new(
                Arc::new(FakeProvider::new("on it", Duration::ZERO)),
                Arc::new(Tools::new(Vec::new())),
                Storage::open(home.path().join("storage")),
                |_| Permissions::default(),
            )),
            SpawnRequest {
                name: "w1".to_owned(),
                backend: MemberBackend::InProcess,
                agent_type: "general".to_owned(),
                model: "recorder-model".to_owned(),
                color: None,
                prompt: "hold the fort".to_owned(),
                cwd: home.path().to_path_buf(),
                plan_mode_required: false,
            },
        )
        .await
        .expect("the teammate starts");
    assert_eq!(registry.view().members.len(), 2, "the lead and w1");

    let inbox = registry.lead_inbox();
    write_frame(
        &inbox,
        "w1",
        &Frame::ShutdownApproved(ShutdownApproved {
            request_id: "req-1".to_owned(),
            from: "w1".to_owned(),
            timestamp: record::now_iso8601(),
            pane_id: None,
            backend_type: Some("in-process".to_owned()),
        }),
    );

    let pass = LeadInbox::new(Arc::clone(&registry)).poll().await;

    assert_eq!(pass.retired.len(), 1);
    assert_eq!(pass.retired[0].name, "w1");
    assert_eq!(registry.view().members.len(), 1, "only the lead is left in the roster");
    let document = std::fs::read_to_string(registry.root().config_path(registry.team()))
        .expect("the team file is on disk");
    assert!(
        !document.contains("\"w1\""),
        "and the document a resume reads no longer names it:\n{document}"
    );
}

/// A backend that answers `claude` and makes a pane out of nothing.
///
/// A fixture rather than `ganja_teammate_local::claude::ClaudePane`, which would
/// need a tmux server and a `claude` on the machine to reach this test's one
/// question: what a lead does when the **roster** holds a claude-backed
/// member. `owns_inbox` mirrors the real one so no stray prompt lands under
/// the ganja root and the assertions below count only what they meant to.
#[derive(Debug)]
struct AsClaude;

#[async_trait::async_trait]
impl TeammateBackend for AsClaude {
    fn backend(&self) -> MemberBackend {
        MemberBackend::Claude
    }

    fn owns_inbox(&self) -> bool {
        true
    }

    /// Never read, and that is this backend's own contract: a backend that
    /// [owns its inbox](TeammateBackend::owns_inbox) seeds the message itself,
    /// so the registry never asks for one.
    fn preamble(&self, _spec: &SpawnSpec) -> String {
        String::new()
    }

    async fn spawn(&self, _spec: &SpawnSpec, _lent: Lent) -> Result<Arc<dyn Spawned>, Unsupported> {
        Ok(Arc::new(AsPane))
    }

    fn delivery(&self) -> Delivery {
        Delivery::FireAndForget
    }
}

/// What [`AsClaude`] hands back: a pane on the roster that nothing runs.
#[derive(Debug)]
struct AsPane;

#[async_trait::async_trait]
impl Spawned for AsPane {
    fn surface(&self) -> Surface {
        Surface::Pane { id: "%17".to_owned() }
    }

    fn start(self: Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        Vec::new()
    }

    fn alive(&self) -> bool {
        true
    }

    fn recent(&self) -> Vec<String> {
        Vec::new()
    }

    async fn kill(&self) {}
}

/// Puts a claude-backed `w1` in the roster.
async fn claude_member(registry: &Arc<TeammateRegistry>, cwd: &std::path::Path) {
    registry
        .spawn(
            Arc::new(AsClaude),
            SpawnRequest {
                name: "w1".to_owned(),
                backend: MemberBackend::Claude,
                agent_type: "general".to_owned(),
                model: "recorder-model".to_owned(),
                color: None,
                prompt: "hold the fort".to_owned(),
                cwd: cwd.to_path_buf(),
                plan_mode_required: false,
            },
        )
        .await
        .expect("the claude-backed member is registered");
}

/// One idle frame, as a `claude` pane's harness would write it.
fn went_idle() -> Frame {
    Frame::IdleNotification(IdleNotification {
        from: "w1".to_owned(),
        timestamp: record::now_iso8601(),
        idle_reason: None,
        summary: Some("read the brief".to_owned()),
        completed_task_id: None,
        completed_status: None,
        failure_reason: None,
    })
}

/// **A real `claude` answers under its own root, and the lead reads it
/// there.**
///
/// The gap this closes: `$CLAUDE_CONFIG_DIR/teams` is where a `claude`
/// teammate writes (§2.1) and `<ganja config home>/teams` is where the lead's
/// own inbox lives, so a pass over one root alone never saw the other's
/// replies at all. Both are read here, in one pass, and each is pruned where
/// its own entries were.
#[tokio::test]
async fn a_claude_teammates_answer_under_its_own_root_reaches_the_leads_pass() {
    let home = tempfile::tempdir().expect("a temporary home");
    let elsewhere = tempfile::tempdir().expect("a temporary claude config home");
    let registry = registry(home.path());
    let claude = ganja_team::TeamsRoot::new(elsewhere.path().join("teams"));
    claude_member(&registry, home.path()).await;

    // A frame in claude's root, and a plain message in the lead's own — so
    // the pass has to have read both to hand back both.
    let under_claude = claude.inbox_path(registry.team(), registry.lead());
    write_frame(&under_claude, "w1", &went_idle());
    write(&registry.lead_inbox(), "w2", "the parser is done");

    let lead = LeadInbox::reading(Arc::clone(&registry), Some(claude.clone()));
    let pass = lead.poll().await;

    assert_eq!(pass.idle.len(), 1, "the claude root was read: {pass:?}");
    assert_eq!(pass.idle[0].summary.as_deref(), Some("read the brief"));
    assert_eq!(pass.messages.len(), 1, "and so was the lead's own");
    assert_eq!(pass.messages[0].from, "w2");
    assert!(
        mailbox::read(&under_claude).expect("claude's inbox reads").valid.is_empty(),
        "a frame acted on is pruned in the root it was found in"
    );
    assert_eq!(
        mailbox::read(&registry.lead_inbox()).expect("the lead's inbox reads").valid.len(),
        1,
        "and a plain message is still owed until the caller delivers it"
    );

    lead.delivered(&pass.messages).await;

    assert!(
        mailbox::read(&registry.lead_inbox()).expect("the lead's inbox reads").valid.is_empty(),
        "a delivered message does not remain"
    );
}

/// The gate: a lead with no claude teammate does not read — and on a
/// delivery, does not write — inside another program's config directory.
#[tokio::test]
async fn a_lead_with_no_claude_teammate_never_looks_in_claudes_root() {
    let home = tempfile::tempdir().expect("a temporary home");
    let elsewhere = tempfile::tempdir().expect("a temporary claude config home");
    let registry = registry(home.path());
    let claude = ganja_team::TeamsRoot::new(elsewhere.path().join("teams"));
    let under_claude = claude.inbox_path(registry.team(), registry.lead());
    write_frame(&under_claude, "w1", &went_idle());

    let pass = LeadInbox::reading(Arc::clone(&registry), Some(claude.clone())).poll().await;

    assert!(pass.is_empty(), "nothing of another program's is this lead's to read: {pass:?}");
    assert_eq!(
        mailbox::read(&under_claude).expect("claude's inbox reads").valid.len(),
        1,
        "and nothing of it was pruned either"
    );

    // The same lead, once a claude member joins, does read it — so what the
    // assertions above pin is the roster and not the path.
    claude_member(&registry, home.path()).await;
    let pass = LeadInbox::reading(Arc::clone(&registry), Some(claude)).poll().await;
    assert_eq!(pass.idle.len(), 1, "{pass:?}");
}

/// **An ask read under claude's root is answered under claude's root.**
///
/// The write half of the two-root read, and the half that was still wrong
/// after the read was fixed: the answer went to this registry's own root
/// unconditionally, so a real `claude` teammate's `permission_request` — which
/// arrives from `$CLAUDE_CONFIG_DIR/teams` — was answered into a file that
/// member never reads, and its pane would wait forever on a dialog a person
/// had already answered. Nothing in the frame says which directory it came
/// from and the sender's *name* is the same in both, so the origin root is the
/// only thing that can decide it.
#[tokio::test]
async fn an_ask_found_under_claudes_root_is_answered_under_claudes_root() {
    let home = tempfile::tempdir().expect("a temporary home");
    let elsewhere = tempfile::tempdir().expect("a temporary claude config home");
    let registry = registry(home.path());
    let claude = ganja_team::TeamsRoot::new(elsewhere.path().join("teams"));
    claude_member(&registry, home.path()).await;
    let (surface, mut dialogs) = tokio::sync::mpsc::channel(4);
    registry.forward_dialogs_to(surface);

    write_frame(&claude.inbox_path(registry.team(), registry.lead()), "w1", &ask("req-1"));

    let lead = LeadInbox::reading(Arc::clone(&registry), Some(claude.clone()));
    let pass = lead.poll().await;
    assert_eq!(pass.asked.len(), 1, "{pass:?}");
    assert!(pass.asked[0].raised, "it reached the channel");

    dialogs
        .try_recv()
        .expect("the dialog was raised")
        .reply
        .send(crate::protocol::PermissionReply::Once)
        .expect("the answer task is waiting");

    let asker = MemberName::parse("w1").expect("a member name");
    let under_claude = answered(&claude.inbox_path(registry.team(), &asker)).await;
    assert_eq!(
        under_claude.len(),
        1,
        "the answer is in the root the ask came from: {under_claude:?}"
    );
    let Some(Frame::PermissionResponse(response)) = under_claude[0].frame() else {
        panic!("the answer is a permission response: {:?}", under_claude[0]);
    };
    assert_eq!(response.request_id(), "req-1");
    assert_eq!(member::reply_of(&response), crate::protocol::PermissionReply::Once);
    assert!(
        mailbox::read(&registry.root().inbox_path(registry.team(), &asker))
            .expect("the ganja-root inbox reads")
            .valid
            .is_empty(),
        "and nothing was written into the root the ask did not come from"
    );
}

/// AC-13's configuration — the lead's own root pointed at claude's — is one
/// inbox, not two: a file read twice in a pass would hand the same message
/// out twice, and the second delivery is one the frontend cannot tell from a
/// teammate having said it again.
#[tokio::test]
async fn one_directory_reached_two_ways_is_still_read_once() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    claude_member(&registry, home.path()).await;
    let collapsed = registry.root().clone();
    write(&registry.lead_inbox(), "w1", "the parser is done");

    let pass = LeadInbox::reading(Arc::clone(&registry), Some(collapsed)).poll().await;

    assert_eq!(pass.messages.len(), 1, "once, not twice: {pass:?}");
}

/// §2.3's identity is derivable by any reader, and this is that property
/// holding: a value built from the three fields prunes the one message
/// they came from.
#[test]
fn a_delivered_entry_derives_the_identity_it_will_be_pruned_by() {
    let message = MailboxMessage::new("w1", "done", "2026-08-17T00:00:00.000Z");
    let delivered =
        Delivered::new("w1", "2026-08-17T00:00:00.000Z", "done", Delivery::Acknowledged);

    assert_eq!(delivered.identity(), mailbox::identity(&message));
    assert_ne!(
        Delivered::new("w2", "2026-08-17T00:00:00.000Z", "done", Delivery::Acknowledged).identity(),
        mailbox::identity(&message),
        "the sender is part of what a message is"
    );
}

/// **D541.** An exit retires under the `backendType` the roster already holds
/// for that member, and the field the surface is rebuilt from is what says
/// which: a `ganja` pane posts no CLI and comes back a
/// [`Surface::Pane`] — `tmux`, the word its own record was written with and
/// the word its own `shutdown_approved` would have carried — while an exit
/// naming a CLI comes back exactly what it came back before that field became
/// an [`Option`], the CLI's own name beside its pane.
///
/// Both rows in one pass, because the thing worth pinning is that they differ:
/// one arm reporting the other's word would put a member on the roster under a
/// surface it never ran on, and nothing downstream would notice.
#[tokio::test]
async fn an_exit_retires_under_the_backend_type_its_own_surface_records() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let exits = registry.lend().exits;
    for (name, cli, backend, pane_id) in [
        ("w1", None, MemberBackend::Ganja, "%7"),
        ("w2", Some(ShimCli::Codex), MemberBackend::Codex, "%8"),
    ] {
        exits
            .send(MemberExited {
                name: name.to_owned(),
                cli,
                backend,
                pane_id: pane_id.to_owned(),
                pane: PaneFate::Closed,
                last_words: None,
            })
            .expect("the registry is still holding the receiving half");
    }

    let pass = LeadInbox::new(Arc::clone(&registry)).poll().await;

    let retired: Vec<(&str, Option<&str>, Option<&str>)> = pass
        .retired
        .iter()
        .map(|gone| (gone.name.as_str(), gone.pane_id.as_deref(), gone.backend_type.as_deref()))
        .collect();

    assert_eq!(
        retired,
        vec![("w1", Some("%7"), Some("tmux")), ("w2", Some("%8"), Some("codex")),],
        "the pane member under its record's own word, the shim under its CLI's: {pass:?}"
    );
    assert_eq!(
        pass.exited.iter().map(|gone| gone.name.as_str()).collect::<Vec<_>>(),
        vec!["w1", "w2"],
        "and both are reported so a frontend can say what happened"
    );
}
