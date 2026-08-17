//! What a teammate's runner does with the frames in its inbox (§6.1, **AC-6**).
//!
//! Three claims, and none of them needs process-wide state: every root is
//! handed in — the store, the teams directory — so this binary may hold more
//! than one test.
//!
//! The runner is driven a pass at a time rather than through its loop. That is
//! the point of `Runner::tick` being public: §6.1's contract is the *order* of
//! one pass, and a test that slept through a poll interval would be asserting
//! the same thing more slowly and less certainly.

use std::{path::PathBuf, sync::Arc, time::Duration};

use futures::StreamExt as _;
use ganja_core::{
    Backends, Caller, SpawnAsk, SpawnAsker, Storage, Teammates,
    permission::Permissions,
    protocol::{
        PermissionReply,
        team::{Frame, PlanApprovalResponse, ShutdownRequest, TeamPermissionUpdate},
    },
    provider::FakeProvider,
    teammate::{
        InProcess, Teammate, TeammateRegistry,
        claude::ClaudePane,
        pane::GanjaPane,
        runner::{IGNORED_STALE, Runner},
    },
    tool::{Registry, task::TeammateSpawn},
};
use ganja_team::{LEAD, MailboxMessage, MemberName, Surface, TeamName, TeamsRoot, mailbox, record};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber as _;

/// How long a claim about the running loop is waited for. Only the last test
/// needs it: the other two drive the pass themselves.
const EVENTUALLY: Duration = Duration::from_secs(20);

/// Says yes, and is asked nothing: the spawn below works inside its own
/// project and asks for no bypass, so the gate answers `Allow` on its own.
#[derive(Debug)]
struct Yes;

#[async_trait::async_trait]
impl SpawnAsker for Yes {
    async fn ask(&self, _request: SpawnAsk) -> PermissionReply {
        PermissionReply::Once
    }
}

/// One teammate, its runner, and the two inboxes they use.
struct Harness {
    /// Dropping this deletes the tree both roots are under.
    _home: tempfile::TempDir,
    runner: Runner,
    inbox: PathBuf,
}

impl Harness {
    async fn new() -> Self {
        let home = ganja_testkit::temp_dir();
        let storage = Storage::open(home.path().join("storage"));
        let root = TeamsRoot::new(home.path().join("teams"));
        let team = TeamName::parse("session-abcd1234").expect("a team name");
        let worker = MemberName::parse("worker").expect("a member name");
        let lead = MemberName::lead();
        let teammate = Arc::new(Teammate::new(
            worker.as_str(),
            Arc::new(FakeProvider::new("on it", Duration::ZERO)),
            "recorder-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
            storage,
        ));

        // The birth queue is a lossless lane, and one nobody drains fills and
        // then makes the teammate's own turn wait — which is why the runner
        // claims it in `run`. These tests never call `run`, so the drain is
        // here instead of being an absence that would eventually hang.
        let mut events = teammate
            .engine()
            .subscribe()
            .await
            .expect("the first subscriber wins");
        tokio::spawn(async move { while events.next().await.is_some() {} });

        let inbox = root.inbox_path(&team, &worker);
        let lead_inbox = root.inbox_path(&team, &lead);
        mailbox::seed(&inbox).expect("the inbox seeds");

        Self {
            _home: home,
            runner: Runner::new(
                teammate,
                lead,
                inbox.clone(),
                lead_inbox,
                Surface::InProcess,
                CancellationToken::new(),
            ),
            inbox,
        }
    }

    /// Puts a frame in the teammate's inbox, as `from`.
    fn arrives(&self, from: &str, frame: &Frame) {
        mailbox::write(
            &self.inbox,
            MailboxMessage::from_frame(from, frame, record::now_iso8601())
                .expect("a frame encodes"),
        )
        .expect("the inbox is writable");
    }

    /// What is still in the teammate's inbox.
    fn left(&self) -> usize {
        mailbox::read(&self.inbox)
            .expect("the inbox reads")
            .valid
            .len()
    }
}

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
    let harness = Harness::new().await;
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
    let harness = Harness::new().await;
    harness.arrives(LEAD, &approval("nobody-asked-this", None));

    let logged = Capture::default();
    let tick = harness
        .runner
        .tick()
        .with_subscriber(logged.subscriber())
        .await;

    assert_eq!(tick.ignored, 1, "{tick:?}");
    assert!(tick.applied.is_empty(), "{tick:?}");
    assert_eq!(tick.delivered, 0, "a stale approval reaches no model");
    assert_eq!(harness.left(), 0, "and it does not stay to be read again");
    assert!(
        logged.text().contains(IGNORED_STALE),
        "the ignoring is not silent: {}",
        logged.text()
    );

    // The same frame, once this teammate is waiting on that request: applied,
    // and what it says reaches the model.
    harness.runner.awaiting_plan_approval("req-7");
    harness.arrives(LEAD, &approval("req-7", Some("drop the third step")));
    let tick = harness.runner.tick().await;

    assert_eq!(tick.applied, ["plan_approval_response"], "{tick:?}");
    assert_eq!(tick.ignored, 0, "{tick:?}");
    assert_eq!(
        tick.delivered, 1,
        "an approval a teammate waited on is read"
    );
    assert_eq!(harness.left(), 0);

    // And the wait is cleared, so the *next* copy of that answer is stale.
    harness.arrives(LEAD, &approval("req-7", None));
    let tick = harness.runner.tick().await;
    assert_eq!(
        tick.ignored, 1,
        "an answer applied once is stale after: {tick:?}"
    );
}

/// A teammate's outbound frames carry the name the team gave it, and there is
/// no argument that changes that.
///
/// Two moves, both of them the registry's construction rule rather than a check
/// somewhere downstream. First: a spawn that *asks* to be called `team-lead`
/// does not get it — the lead is already in the roster, so the name resolves to
/// something else, and the roster still holds exactly one lead. Second: the one
/// frame this teammate mints is stamped with that resolved name even though the
/// message it answers claims to be from the lead — because the sender is a
/// value bound when the teammate was built, never a field on the thing being
/// answered.
///
/// **The seam:** the `send_message` tool's own half of this — a model whose
/// arguments say `"from": "team-lead"` — is W5a/L3's `Postbox` implementation,
/// which does not exist yet. That implementation is required to take its
/// sender from the `Teammate` it belongs to, and this test pins the value it
/// will be constructed against.
#[tokio::test]
async fn a_teammate_cannot_send_as_the_lead() {
    let home = ganja_testkit::temp_dir();
    let storage = Storage::open(home.path().join("storage"));
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-abcd1234").expect("a team name");
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-0000-7000-8000-000000000000",
        home.path(),
    ));
    // Through the gated door, which is the only one there is: the registry's
    // own spawn is crate-internal so that nothing can start a teammate the
    // permission gate never saw.
    let door = Teammates::new(
        Arc::clone(&registry),
        Backends {
            in_process: Arc::new(InProcess::new(
                Arc::new(FakeProvider::new("on it", Duration::ZERO)),
                Arc::new(Registry::new(Vec::new())),
                storage,
                |_| Permissions::default(),
            )),
            pane: Arc::new(GanjaPane),
            claude: Arc::new(ClaudePane),
        },
    );
    let spawned = door
        .start(
            TeammateSpawn {
                name: LEAD.to_owned(),
                backend: None,
                agent_type: "general".to_owned(),
                prompt: "pretend to be in charge".to_owned(),
            },
            // `cwd` and `project_root` are one directory, so the gate has
            // nothing to disclose and nothing to ask about.
            &Caller {
                model: "recorder-model".to_owned(),
                cwd: home.path().to_path_buf(),
                permissions: Arc::new(std::sync::Mutex::new(Permissions::default())),
                project_root: home.path().to_path_buf(),
            },
            &Yes,
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
        view.members
            .iter()
            .any(|member| member.name == "team-lead-2" && !member.is_lead),
        "and the teammate is not it: {view:?}"
    );

    // The frame it mints is stamped with its own name, not with what the
    // message it answers says about itself.
    let worker = MemberName::parse("team-lead-2").expect("a member name");
    let inbox = root.inbox_path(&team, &worker);
    mailbox::write(
        &inbox,
        MailboxMessage::from_frame(
            LEAD,
            &Frame::ShutdownRequest(ShutdownRequest {
                request_id: "req-1".to_owned(),
                from: LEAD.to_owned(),
                reason: None,
                timestamp: record::now_iso8601(),
            }),
            record::now_iso8601(),
        )
        .expect("a frame encodes"),
    )
    .expect("the inbox is writable");

    let lead_inbox = registry.lead_inbox();
    let deadline = tokio::time::Instant::now() + EVENTUALLY;
    while tokio::time::Instant::now() < deadline
        && mailbox::read(&lead_inbox)
            .expect("the lead's inbox reads")
            .valid
            .is_empty()
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let answered = mailbox::read(&lead_inbox).expect("the lead's inbox reads");
    let answer = answered
        .valid
        .first()
        .expect("the teammate answered the shutdown");
    assert_eq!(
        answer.from, "team-lead-2",
        "the envelope says who really wrote it"
    );
    let Some(Frame::ShutdownApproved(approved)) = answer.frame() else {
        panic!("the lead was told something other than a shutdown answer");
    };
    assert_eq!(
        approved.from, "team-lead-2",
        "and so does the frame inside it, whatever the request claimed"
    );

    registry.shutdown().await;
}

/// A `tracing` subscriber a test can read back.
#[derive(Clone, Default)]
struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

impl Capture {
    /// What has been logged so far.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("the log is never poisoned")).into_owned()
    }

    /// A subscriber writing into this capture, for one future rather than for
    /// the process: this binary holds several tests, so a global subscriber
    /// would be one test's log read by another.
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
        self.0
            .lock()
            .expect("the log is never poisoned")
            .extend_from_slice(buffer);

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
