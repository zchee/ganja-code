use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ganja_team::{MemberName, TeamName, TeamsRoot, mailbox};

use super::{
    Delivery, Exited, InProcess, Lent, MemberBackend, PaneFate, SpawnRequest, SpawnSpec, Spawned,
    Surface, TEAMS_DIR, TeammateBackend, TeammateRegistry, Unsupported, claude_root_under,
    session_team,
};
use crate::Storage;
use crate::permission::Permissions;
use crate::provider::FakeProvider;
use crate::tool::Registry as Tools;

/// Why [`Never`] refuses.
pub(crate) const NEVER: &str = "this door spawns nothing";

/// A backend that spawns nothing at all, refusing in its own sentence and
/// answering for whichever surface it was built as.
///
/// A fixture rather than a real pane backend, because a real one spawns:
/// a test that leaned on `GanjaPane` refusing would split a pane into
/// whichever tmux session the developer happens to be sitting in the day
/// its body lands.
#[derive(Debug)]
pub(crate) struct Never(pub(crate) MemberBackend);

#[async_trait::async_trait]
impl TeammateBackend for Never {
    fn backend(&self) -> MemberBackend {
        self.0
    }

    async fn spawn(&self, _spec: &SpawnSpec, _lent: Lent) -> Result<Arc<dyn Spawned>, Unsupported> {
        Err(Unsupported { backend: self.0, reason: NEVER.to_owned() })
    }

    // Never seeded, since nothing is ever spawned; the native words, so a
    // test that reads them reads a real preamble and not a placeholder.
    fn preamble(&self, spec: &SpawnSpec) -> String {
        crate::teammate::preamble::native(crate::teammate::preamble::Names::of(spec), &spec.prompt)
    }

    fn delivery(&self) -> Delivery {
        Delivery::FireAndForget
    }
}

/// An empty registry over a tree that goes away with `home`.
pub(crate) fn registry(home: &Path) -> Arc<TeammateRegistry> {
    Arc::new(TeammateRegistry::new(
        TeamsRoot::new(home.join("teams")),
        TeamName::parse("session-abcd1234").expect("a team name"),
        "01998ad0-0000-7000-8000-000000000000",
        home,
    ))
}

/// A backend that really starts a teammate, over a store under `home`.
fn in_process(home: &Path) -> Arc<dyn TeammateBackend> {
    Arc::new(InProcess::new(
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        Arc::new(Tools::new(Vec::new())),
        Storage::open(home.join("storage")),
        |_| Permissions::default(),
    ))
}

/// A spawn asking for `name` on `backend`, at its dullest.
fn request(name: &str, backend: MemberBackend, home: &Path) -> SpawnRequest {
    SpawnRequest {
        name: name.to_owned(),
        backend,
        agent_type: "general".to_owned(),
        model: "recorder-model".to_owned(),
        color: None,
        prompt: "hold the fort".to_owned(),
        cwd: home.to_path_buf(),
        plan_mode_required: false,
    }
}

/// What the registry is still holding names for.
fn reserved(registry: &TeammateRegistry) -> BTreeSet<String> {
    registry.reserved.lock().expect("the reserved names are never poisoned").clone()
}

/// **A name a completed spawn took is claimed for good.**
///
/// The defect this answers is a stale snapshot rather than a lost lock:
/// [`TeammateRegistry::claim`] compares against a `taken()` view read
/// before its own await, so a spawn that *began and finished* inside that
/// window is in neither that view nor — if a spent name were released — the
/// reservation set. The next claimer would then resolve to the same name
/// and its `start` would evict a teammate that is already running, which is
/// exactly the orphan the reservation exists to prevent.
///
/// Read off the set directly, and deliberately: staging that interleave
/// through the public door would need one claimer's task to be woken and
/// then starved for the whole of another spawn, which no scheduler here
/// promises. The property that closes the window is that the set never
/// shrinks on success, and that is a fact this can assert without a race.
/// `two_spawns_of_one_name_at_once_get_two_teammates` covers the
/// overlapping-spawn half through the door itself.
#[tokio::test]
async fn a_name_a_completed_spawn_took_stays_claimed() {
    let home = ganja_testkit::temp_dir();
    let registry = registry(home.path());

    let started = registry
        .spawn(in_process(home.path()), request("worker", MemberBackend::InProcess, home.path()))
        .await
        .expect("a teammate joins");
    assert_eq!(started.name.as_str(), "worker");

    assert!(
        reserved(&registry).contains("worker"),
        "a spent name given back is a name a stale claimer would take"
    );

    registry.shutdown().await;
    assert!(
        reserved(&registry).contains("worker"),
        "and a teammate that has been shut down still holds the name it \
             ran under: its transcript and its member record both still say so"
    );
}

/// A refused spawn's name is free again, because nothing was registered
/// under it: there is no member for a later teammate of that name to evict,
/// and holding it would refuse a name for no reason anybody could see.
#[tokio::test]
async fn a_name_a_refused_spawn_never_spent_is_free_again() {
    let home = ganja_testkit::temp_dir();
    let registry = registry(home.path());

    let refused = registry
        .spawn(
            Arc::new(Never(MemberBackend::Ganja)),
            request("worker", MemberBackend::Ganja, home.path()),
        )
        .await
        .expect_err("this backend spawns nothing");
    assert!(
        refused.to_string().contains(NEVER),
        "refused by the backend, not before it: {refused}"
    );

    assert!(
        reserved(&registry).is_empty(),
        "a spawn that started nothing kept a name nobody can use"
    );
}

/// The invariant [`TeammateRegistry::view`] promises and `ganja-tool`'s
/// roster reads: the lead is the first row and the only one with
/// `is_lead`, before a spawn and after one, and a fresh registry's ring
/// is empty.
#[tokio::test]
async fn the_view_starts_at_the_lead_and_no_other_row_ever_leads() {
    let home = ganja_testkit::temp_dir();
    let registry = registry(home.path());

    let fresh = registry.view();
    assert_eq!(fresh.team, "session-abcd1234");
    assert_eq!(fresh.lead, "team-lead");
    assert_eq!(fresh.members.len(), 1, "the lead and nobody else");
    assert!(fresh.members[0].is_lead);
    assert_eq!(fresh.members[0].name, "team-lead");
    assert!(fresh.members[0].recent_calls.is_empty());

    registry
        .spawn(in_process(home.path()), request("w1", MemberBackend::InProcess, home.path()))
        .await
        .expect("a teammate joins");
    let led = registry.view();
    assert!(led.members[0].is_lead, "the lead is still the first row");
    assert_eq!(
        led.members.iter().filter(|member| member.is_lead).count(),
        1,
        "and the only one that leads: {led:?}"
    );

    registry.shutdown().await;
}

/// **D503**'s ring, off the same events a teammate engine publishes: a
/// running call joins once however often its part republishes, a second
/// call reading identically stays one row, and the ring keeps the newest
/// [`RECENT_CALLS`] in order.
#[tokio::test]
async fn the_ring_keeps_distinct_running_calls_in_order_deduped_and_capped() {
    use futures::StreamExt as _;

    use crate::protocol::{MessageId, PartId, SessionId};

    let part = |id: &str, tool: &str| crate::protocol::Event::PartUpdated {
        session_id: SessionId::from("ses_w1".to_owned()),
        message_id: MessageId::from("msg_1".to_owned()),
        part: crate::protocol::Part {
            id: PartId::from(id.to_owned()),
            body: crate::protocol::PartBody::Tool {
                call_id: id.to_owned(),
                tool: tool.to_owned(),
                state: crate::protocol::ToolState::Running {
                    input: serde_json::Value::Null,
                    metadata: serde_json::Value::Null,
                    started: 0,
                },
            },
        },
    };
    let fold = |events: Vec<crate::protocol::Event>, ring: &Arc<super::Mutex<_>>| {
        super::fold_calls(
            futures::stream::iter(events.into_iter().map(Ok)).boxed(),
            Arc::new(Tools::new(Vec::new())),
            Arc::clone(ring),
            "w1".to_owned(),
            tokio_util::sync::CancellationToken::new(),
        )
    };
    let ring = Arc::new(super::Mutex::new(std::collections::VecDeque::new()));

    // One call republishing as it streams, then a second call whose line
    // reads the same: one row carries both.
    fold(vec![part("prt_a", "read"), part("prt_a", "read"), part("prt_b", "read")], &ring).await;
    assert_eq!(
        ring.lock().expect("the ring").iter().cloned().collect::<Vec<_>>(),
        ["read"],
        "a republished part and an identical line are one row"
    );

    // More distinct calls than the ring holds: the newest win, in order.
    let over = super::RECENT_CALLS + 2;
    fold(
        (0..over).map(|index| part(&format!("prt_{index}"), &format!("tool-{index}"))).collect(),
        &ring,
    )
    .await;
    let held: Vec<String> = ring.lock().expect("the ring").iter().cloned().collect();
    assert_eq!(held.len(), super::RECENT_CALLS, "capped: {held:?}");
    assert_eq!(held.first().map(String::as_str), Some("tool-2"), "{held:?}");
    assert_eq!(
        held.last().map(String::as_str),
        Some(format!("tool-{}", over - 1).as_str()),
        "the newest call ends the ring: {held:?}"
    );
}

/// §2.1's own example, and the property that makes it useful: one session
/// id always names one team, so a resume rejoins rather than orphans.
#[test]
fn a_session_names_its_own_team_and_a_pre_uuid_id_falls_back_to_the_default() {
    assert_eq!(session_team("224cbeab-4e62-497c-aa8f-d05cc33ce7ba").as_str(), "session-224cbeab");
    assert_eq!(
        session_team("224CBEAB-4e62-497c-aa8f-d05cc33ce7ba").as_str(),
        "session-224cbeab",
        "the directory name is one spelling, whichever case the id is in"
    );
    // The per-process counter P1 minted and W1 retired: eight hex digits
    // cannot be taken from it, so there is no team name to derive.
    assert_eq!(session_team("ses_0001").as_str(), "default");
    assert_eq!(session_team("").as_str(), "default");
}

/// The lead reading a `shutdown_approved` is what takes a member out of
/// both the roster it renders and the document a resume would read.
#[tokio::test]
async fn retiring_a_teammate_forgets_it_and_rewrites_the_team_file_without_it() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    registry
        .spawn(in_process(home.path()), request("w1", MemberBackend::InProcess, home.path()))
        .await
        .expect("the teammate starts");

    assert_eq!(registry.view().members.len(), 2, "the lead and w1");
    assert!(registry.retire("w1").await.expect("the team file rewrites"));

    assert_eq!(registry.view().members.len(), 1, "only the lead is left in the roster");
    let document = std::fs::read_to_string(registry.root().config_path(registry.team()))
        .expect("the team file is on disk");
    assert!(
        !document.contains("\"w1\""),
        "a retired member is out of the document too:\n{document}"
    );
    assert!(
        !registry.retire("w1").await.expect("a second retire is fine"),
        "a shutdown read twice is ordinary rather than an error"
    );
}

/// A spec `record` will write a row for, at its dullest — the registry's own
/// team, lead and root, so the document it lands in is the one this test reads
/// back.
fn spec(registry: &TeammateRegistry, name: &str, home: &Path) -> SpawnSpec {
    SpawnSpec {
        name: MemberName::parse(name).expect("a member name"),
        team: registry.team().clone(),
        lead: registry.lead().clone(),
        root: registry.root().clone(),
        backend: MemberBackend::InProcess,
        agent_type: "general".to_owned(),
        model: "recorder-model".to_owned(),
        color: "blue".to_owned(),
        prompt: "hold the fort".to_owned(),
        cwd: home.to_path_buf(),
        plan_mode_required: false,
        parent_session_id: registry.lead_session_id().to_owned(),
    }
}

/// **U-7: the whole rewrite is one critical section, so a spawn that lands
/// inside it is not lost.**
///
/// [`TeammateRegistry::mark_records_inactive`] is a read-modify-write of an
/// entire document, and so is [`TeammateRegistry::record`]. Read the file
/// *outside* the `team_file` lock and take the lock only for the write — which
/// is what the reaper's retire path did until **Dv-13**, spelled out of the
/// registry through five public items — and a `record` that lands between the
/// two is written back over: a teammate that is running, holds a mailbox, and
/// no team file remembers.
///
/// Pinned twice, and the first half is the one that cannot race. A predicate
/// held open inside the mutation proves the lock is *already* held there, which
/// a `try_lock` from outside answers with no timing at all; the second half
/// then lets a real `record` contend for it and reads the document back with
/// both rows in it. The predicate blocks a worker thread on purpose, which is
/// why this test asks for more than one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retiring_records_holds_the_team_file_lock_across_read_and_write() {
    let home = ganja_testkit::temp_dir();
    let registry = registry(home.path());
    registry
        .record(&spec(&registry, "stale", home.path()), Surface::InProcess)
        .await
        .expect("a previous lead's row is in the document");

    // Async on the way out so the test never blocks a worker waiting for it,
    // blocking on the way in because the predicate is not a future.
    let (reached, mut entered) = tokio::sync::mpsc::unbounded_channel();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);

    let retiring = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .mark_records_inactive(move |member| {
                    if member.name != "stale" {
                        return false;
                    }
                    reached.send(()).expect("the test is still listening");
                    held.lock()
                        .expect("the release is never poisoned")
                        .recv()
                        .expect("the test releases the retire");

                    true
                })
                .await
                .expect("the team file rewrites")
        }
    });

    entered.recv().await.expect("the retire reached the stale row");
    assert!(
        registry.team_file.try_lock().is_err(),
        "the lock is held while the document is being read and mutated, not only while it is \
         written"
    );

    let recording = tokio::spawn({
        let registry = Arc::clone(&registry);
        let spec = spec(&registry, "w1", home.path());
        async move { registry.record(&spec, Surface::InProcess).await }
    });
    // Long enough for the spawn above to reach the lock and wait on it, which
    // is the interleave the assertion below is about. Nothing depends on it:
    // the document is correct whether or not the two ever overlapped.
    tokio::time::sleep(Duration::from_millis(50)).await;
    release.send(()).expect("the retire is still waiting to be let go");

    let retired = retiring.await.expect("the retire finishes");
    recording.await.expect("the record finishes").expect("the team file rewrites");

    assert_eq!(retired, vec!["stale".to_owned()], "the previous lead's row is the one retired");
    let file = registry.read_team().await.expect("the team file reads back");
    assert!(
        file.members.iter().any(|member| member.name == "w1"),
        "a record that landed during the retire is still in the document: {:?}",
        file.members.iter().map(|member| member.name.clone()).collect::<Vec<_>>()
    );
    assert!(
        file.members.iter().any(|member| member.name == "stale" && member.is_active == Some(false)),
        "and the retired row is marked inactive rather than dropped"
    );
}

/// The team file is somebody else's document too, so the write that
/// replaces it may not leave its own scaffolding in the directory a real
/// `claude` walks.
///
/// The failure is forced rather than injected: a **directory** at the
/// target path is a rename `persist` cannot complete, and it reaches that
/// step with everything before it having succeeded. `write_team` is called
/// straight rather than through `record`, because [`read_team`] would
/// refuse the same directory first and the write would never run.
///
/// This is the half the old code got wrong. It staged at
/// `config.json.new-<pid>` with [`std::fs::write`] and renamed, and a
/// rename that failed returned the error and left the staged file where it
/// fell — permanently, since the name is per-process and the next run of
/// this build would write the same one.
///
/// [`read_team`]: TeammateRegistry::read_team
#[tokio::test]
async fn a_team_file_write_that_cannot_rename_leaves_nothing_behind() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let path = registry.root().config_path(registry.team());
    std::fs::create_dir_all(&path).expect("the target is a directory nothing can rename onto");

    let refused = {
        let writing = registry.team_file.lock().await;
        let file = ganja_team::TeamFile::new(registry.team(), "01998ad0", "/tmp", 1);

        registry
            .write_team(file, &writing)
            .await
            .expect_err("a rename onto a directory cannot succeed")
    };

    assert!(
        matches!(refused, super::SpawnError::TeamFile { doing: "written", .. }),
        "the failure is the write it was: {refused}"
    );
    let left: Vec<_> = std::fs::read_dir(path.parent().expect("the team has a directory"))
        .expect("the team directory is readable")
        .map(|entry| entry.expect("the entry is readable").file_name())
        .collect();
    assert_eq!(
        left,
        vec![std::ffi::OsString::from("config.json")],
        "nothing of the write survives it"
    );
}

/// The document is shared, so its mode is the owner's to set and not this
/// writer's to narrow.
///
/// A temporary is created `0600` and a rename carries that mode onto the
/// target, so a rewrite that copied nothing across would silently take a
/// group-readable team file private under a peer already reading it.
/// `ganja-team`'s mailbox defends the same property for the same reason
/// — `a_rewrite_keeps_the_inboxes_existing_mode` is this test's twin.
#[cfg(unix)]
#[tokio::test]
async fn a_team_file_rewrite_keeps_the_documents_existing_mode() {
    use std::os::unix::fs::PermissionsExt as _;

    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let path = registry.root().config_path(registry.team());

    let write = async || {
        let writing = registry.team_file.lock().await;
        let file = ganja_team::TeamFile::new(registry.team(), "01998ad0", "/tmp", 1);

        registry.write_team(file, &writing).await.expect("the team file writes");
    };

    write().await;
    assert_eq!(
        std::fs::metadata(&path).expect("the team file is there").permissions().mode() & 0o777,
        0o600,
        "a document this created is private, whatever the umask would have said"
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
        .expect("the mode is settable");
    write().await;

    assert_eq!(
        std::fs::metadata(&path).expect("the team file is there").permissions().mode() & 0o777,
        0o640,
        "a rewrite neither tightens nor loosens what the owner set"
    );
}

/// The bytes reach the disk before the rename does.
///
/// Asserted against the source because there is nowhere else to assert it:
/// an `fsync` leaves no trace a reader in this process — or any other —
/// can observe, and the outcome it buys is only ever visible after a power
/// loss. What it prevents is specific and worth the unusual test: without
/// it a crash can leave the *renamed* file present and **empty**, which is
/// the one damaged state a foreign reader cannot tell from a team that has
/// no members. So this pins the call rather than its effect, and the
/// ordering that makes it mean anything — a `sync_all` after the rename
/// would be a `sync_all` of nothing.
#[test]
fn the_team_file_is_synced_before_it_is_renamed_into_place() {
    // Bounded to the one function, and it has to be: this test's own
    // source carries both needles, so a search over the rest of the file
    // would find them here and pass against a writer that syncs nothing.
    // A method's closing brace is the only `}` this file indents by four
    // spaces, which is what makes the end of a body findable at all.
    let body = include_str!("teammate.rs")
        .split_once("    async fn write_team(")
        .expect("the writer is still called that")
        .1;
    let body = body.split_once("\n    }\n").expect("the writer still ends").0;

    let synced = body.find(".sync_all()").expect("the bytes are still synced");
    let renamed = body.find(".persist(").expect("the file is still renamed");

    assert!(synced < renamed, "the sync is what the rename publishes, so it comes first");
}

/// A backend that hands out a pane-shaped handle and remembers every
/// handle it is asked to end and every launch it is asked for — what a
/// real pane backend does, minus tmux. `launch` also reads the team file
/// as a pane's process would, so a test can see whether the record was
/// there yet.
#[derive(Debug, Default)]
struct Recording {
    /// Shared with every pane this backend hands back, because the logs are
    /// the *backend's* and the kill is the member's since **D538**.
    killed: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// `(name, whether the team file named it at launch time)`.
    launched: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
    /// Refuse every launch, so the unwind can be watched.
    refuse_launch: bool,
}

/// Why a refusing [`Recording`] refuses.
const UNLAUNCHABLE: &str = "the launch line could not be typed";

impl Recording {
    fn killed(&self) -> Vec<(String, String)> {
        self.killed.lock().expect("the kill log is never poisoned").clone()
    }

    fn launched(&self) -> Vec<(String, bool)> {
        self.launched.lock().expect("the launch log is never poisoned").clone()
    }
}

#[async_trait::async_trait]
impl TeammateBackend for Recording {
    fn backend(&self) -> MemberBackend {
        MemberBackend::Ganja
    }

    // What a `ganja` pane would seed, so the inbox a test reads back holds
    // the real native preamble around the prompt.
    fn preamble(&self, spec: &SpawnSpec) -> String {
        crate::teammate::preamble::native(crate::teammate::preamble::Names::of(spec), &spec.prompt)
    }

    async fn spawn(&self, spec: &SpawnSpec, _lent: Lent) -> Result<Arc<dyn Spawned>, Unsupported> {
        Ok(Arc::new(RecordedPane {
            id: "%7".to_owned(),
            birth: "48213".to_owned(),
            spec: spec.clone(),
            refuse_launch: self.refuse_launch,
            launched: Arc::clone(&self.launched),
            killed: Arc::clone(&self.killed),
        }))
    }

    fn delivery(&self) -> Delivery {
        Delivery::Acknowledged
    }
}

/// What [`Recording`] hands back: a pane that never was, holding the logs the
/// backend reads.
#[derive(Debug)]
struct RecordedPane {
    id: String,
    birth: String,
    spec: SpawnSpec,
    refuse_launch: bool,
    launched: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
    killed: Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

#[async_trait::async_trait]
impl Spawned for RecordedPane {
    fn surface(&self) -> Surface {
        Surface::Pane { id: self.id.clone() }
    }

    async fn launch(&self) -> Result<(), Unsupported> {
        let spec = &self.spec;
        let recorded = std::fs::read_to_string(spec.root.config_path(&spec.team))
            .is_ok_and(|document| document.contains(&format!("\"{}\"", spec.name)));
        self.launched
            .lock()
            .expect("the launch log is never poisoned")
            .push((spec.name.as_str().to_owned(), recorded));
        if self.refuse_launch {
            return Err(Unsupported {
                backend: MemberBackend::Ganja,
                reason: UNLAUNCHABLE.to_owned(),
            });
        }

        Ok(())
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

    async fn kill(&self) {
        self.killed
            .lock()
            .expect("the kill log is never poisoned")
            .push((self.id.clone(), self.birth.clone()));
    }
}

/// **D514.** The first message in a teammate's inbox is its backend's
/// preamble around the task — the registry seeds what the backend says,
/// never the bare prompt — and the member record keeps the prompt as
/// typed. Pinned over a backend that runs nothing, so the seed is still
/// there to read.
#[tokio::test]
async fn the_registry_seeds_the_backends_preamble_as_the_first_message() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let backend = Arc::new(Recording::default());
    let spawned = registry
        .spawn(
            Arc::clone(&backend) as Arc<dyn TeammateBackend>,
            request("w1", MemberBackend::Ganja, home.path()),
        )
        .await
        .expect("the recording backend spawns");

    let inbox = registry.root().inbox_path(registry.team(), &spawned.name);
    let held = mailbox::read(&inbox).expect("the inbox reads").valid;
    assert_eq!(held.len(), 1, "one seed, one message: {held:?}");
    assert_eq!(held[0].from, MemberName::lead().as_str());
    assert_eq!(
        held[0].text,
        crate::teammate::preamble::native(
            crate::teammate::preamble::Names {
                name: "w1",
                team: registry.team().as_str(),
                lead: MemberName::lead().as_str(),
            },
            "hold the fort",
        ),
        "the first message is the backend's preamble around the task"
    );
    assert!(
        held[0].text.ends_with("hold the fort"),
        "and the task is what it ends with: {}",
        held[0].text
    );
    let recorded = ganja_testkit::team_file(registry.root(), registry.team())
        .and_then(|file| file.member("w1").cloned())
        .expect("w1 is recorded");
    assert_eq!(
        recorded.prompt.as_deref(),
        Some("hold the fort"),
        "the record keeps the prompt as typed, not the preamble"
    );

    registry.shutdown().await;
}

/// §6.2's other half: reading a `shutdown_approved` ends the surface the
/// member ran on — through the backend that spawned it and against the
/// handle recorded at spawn, exactly once, and never again for a name the
/// registry no longer holds.
#[tokio::test]
async fn retiring_a_teammate_ends_its_surface_through_the_recorded_handle_once() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let backend = Arc::new(Recording::default());
    registry
        .spawn(
            Arc::clone(&backend) as Arc<dyn TeammateBackend>,
            request("w1", MemberBackend::Ganja, home.path()),
        )
        .await
        .expect("the recording backend spawns");
    assert!(backend.killed().is_empty(), "spawning ends nothing");

    assert!(registry.retire("w1").await.expect("the retire lands"));
    assert_eq!(
        backend.killed(),
        [("%7".to_owned(), "48213".to_owned())],
        "the recorded pair, through the backend that minted it"
    );

    assert!(!registry.retire("w1").await.expect("a second retire is fine"));
    registry.shutdown().await;
    assert_eq!(
        backend.killed().len(),
        1,
        "a member already retired is not ended a second time by anybody"
    );
}

/// §4.1's step order as the registry keeps it: the surface is launched
/// **after** its record is in the team file — a pane's process reads that
/// record first — and a launch that is refused unwinds the whole spawn:
/// the handle is killed, the record and the seeded prompt are taken back
/// out, and the name is free again.
#[tokio::test]
async fn a_surface_is_launched_after_its_record_exists_and_a_refused_launch_unwinds() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());

    let backend = Arc::new(Recording::default());
    registry
        .spawn(
            Arc::clone(&backend) as Arc<dyn TeammateBackend>,
            request("w1", MemberBackend::Ganja, home.path()),
        )
        .await
        .expect("the recording backend spawns and launches");
    assert_eq!(
        backend.launched(),
        [("w1".to_owned(), true)],
        "launched once, and the team file already named it"
    );
    assert!(backend.killed().is_empty());
    assert!(reserved(&registry).contains("w1"), "a launched spawn spends its name");

    let refusing = Arc::new(Recording { refuse_launch: true, ..Recording::default() });
    let refused = registry
        .spawn(
            Arc::clone(&refusing) as Arc<dyn TeammateBackend>,
            request("w2", MemberBackend::Ganja, home.path()),
        )
        .await
        .expect_err("a refused launch is a refused spawn");
    assert!(
        refused.to_string().contains(UNLAUNCHABLE),
        "refused in the backend's own words: {refused}"
    );
    assert_eq!(
        refusing.launched(),
        [("w2".to_owned(), true)],
        "the record was there when the launch was asked for"
    );
    assert_eq!(
        refusing.killed(),
        [("%7".to_owned(), "48213".to_owned())],
        "the handle a refused launch leaves is ended"
    );
    assert!(
        !reserved(&registry).contains("w2"),
        "and the name is free again: {:?}",
        reserved(&registry)
    );
    let document = std::fs::read_to_string(registry.root().config_path(registry.team()))
        .expect("the team file is on disk");
    assert!(
        !document.contains("\"w2\""),
        "the record a refused launch had written is taken back out:\n{document}"
    );
    assert!(
        document.contains("\"w1\""),
        "without touching the member that did launch:\n{document}"
    );
    let inbox = registry
        .root()
        .inbox_path(registry.team(), &MemberName::parse("w2").expect("a member name"));
    assert!(
        mailbox::read(&inbox).map(|held| held.valid.is_empty()).unwrap_or(true),
        "and the seeded prompt is gone from an inbox nothing will read"
    );
    assert_eq!(registry.view().members.len(), 2, "the roster holds the lead and w1, never w2");
}

/// The root is the variable when there is one, the home when there is not,
/// and nothing at all when there is neither — with an empty variable read
/// as unset.
#[test]
fn the_teams_root_follows_the_config_dir_and_falls_back_to_the_home() {
    let named = claude_root_under(
        Some(OsString::from("/tmp/claude-home")),
        Some(PathBuf::from("/home/somebody")),
    )
    .expect("a named config dir is a root");
    assert_eq!(
        named.inbox_path(
            &TeamName::parse("session-abcd1234").expect("a team name"),
            &MemberName::lead(),
        ),
        PathBuf::from("/tmp/claude-home")
            .join(TEAMS_DIR)
            .join("session-abcd1234")
            .join("inboxes")
            .join("team-lead.json")
    );

    let fallen = claude_root_under(None, Some(PathBuf::from("/home/somebody")))
        .expect("a home is a root when the variable is unset");
    assert_eq!(
        fallen.config_path(&TeamName::parse("session-abcd1234").expect("a team name")),
        PathBuf::from("/home/somebody/.claude/teams/session-abcd1234/config.json")
    );

    assert_eq!(
        claude_root_under(Some(OsString::new()), Some(PathBuf::from("/home/somebody"))),
        Some(fallen),
        "an empty variable is unset, not the root directory"
    );
    assert!(claude_root_under(None, None).is_none());
}

/// **U-3, D538.** An in-process member's ring subscription is registered
/// before its runner can take a turn.
///
/// The order the four steps of `Spawned::start` run in is this backend's
/// responsibility since the ruling moved them out of the registry, and this is
/// the half of it with a cost: a droppable subscription registered inside its
/// own task, or after the runner's, would miss the very first call the
/// teammate's first turn makes — a ring that is empty for one turn and then
/// silently correct, which is exactly the kind of regression nothing else
/// here would catch.
///
/// Driven through a scripted fake whose first turn calls a tool, so what is
/// asserted is the ring holding that call rather than the shape of the code
/// that fills it.
#[tokio::test]
async fn an_in_process_start_subscribes_before_its_runner_takes_a_turn() {
    let home = tempfile::tempdir().expect("a temporary home");
    let script = home.path().join("script.json");
    std::fs::write(
        &script,
        r#"{"cadence_ms":0,"turns":[
             {"tool_calls":[{"name":"read","args":{"filePath":"src/main.rs"}}]},
             {"text":"done"}
           ]}"#,
    )
    .expect("the script is written");

    let registry = registry(home.path());
    let backend: Arc<dyn TeammateBackend> = Arc::new(InProcess::new(
        Arc::new(FakeProvider::new("on it", Duration::ZERO).with_script(&script)),
        Arc::new(Tools::with_builtins()),
        Storage::open(home.path().join("storage")),
        |_| Permissions::default(),
    ));
    registry
        .spawn(backend, request("w1", MemberBackend::InProcess, home.path()))
        .await
        .expect("the teammate starts");

    // The teammate's first turn is what fills the ring, and it begins as soon
    // as its runner reads the seeded prompt — so this waits on the ring rather
    // than on a clock.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let ring = loop {
        let ring: Vec<String> =
            registry.view().members.iter().flat_map(|member| member.recent_calls.clone()).collect();
        if !ring.is_empty() || tokio::time::Instant::now() >= deadline {
            break ring;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    assert!(
        ring.iter().any(|line| line.contains("read")),
        "the first turn's own call is in the ring, so the subscription preceded the runner: {ring:?}"
    );

    registry.shutdown().await;
}

/// **U-4, D538.** `spawn`, `launch` and `start` each run once, in that order,
/// and a refused launch is killed rather than started.
///
/// The sequence the registry keeps is the whole of what a backend may rely on,
/// and it is the thing this ruling made a backend's business: what used to be
/// three arms of one `match` on a handle's shape is now three methods on one
/// object, and nothing but a test says they are called the way their docs
/// promise.
#[tokio::test]
async fn spawn_launch_start_run_once_each_in_that_order() {
    /// Every call the registry made, in order.
    #[derive(Debug, Default)]
    struct Log(std::sync::Mutex<Vec<&'static str>>);

    impl Log {
        fn record(&self, what: &'static str) {
            self.0.lock().expect("the call log is never poisoned").push(what);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.0.lock().expect("the call log is never poisoned").clone()
        }
    }

    #[derive(Debug)]
    struct Watching {
        log: Arc<Log>,
        refuse_launch: bool,
    }

    #[async_trait::async_trait]
    impl TeammateBackend for Watching {
        fn backend(&self) -> MemberBackend {
            MemberBackend::Ganja
        }

        fn preamble(&self, spec: &SpawnSpec) -> String {
            crate::teammate::preamble::native(
                crate::teammate::preamble::Names::of(spec),
                &spec.prompt,
            )
        }

        async fn spawn(
            &self,
            _spec: &SpawnSpec,
            _lent: Lent,
        ) -> Result<Arc<dyn Spawned>, Unsupported> {
            self.log.record("spawn");

            Ok(Arc::new(Watched { log: Arc::clone(&self.log), refuse_launch: self.refuse_launch }))
        }

        fn delivery(&self) -> Delivery {
            Delivery::Acknowledged
        }
    }

    #[derive(Debug)]
    struct Watched {
        log: Arc<Log>,
        refuse_launch: bool,
    }

    #[async_trait::async_trait]
    impl Spawned for Watched {
        fn surface(&self) -> Surface {
            Surface::Pane { id: "%3".to_owned() }
        }

        async fn launch(&self) -> Result<(), Unsupported> {
            self.log.record("launch");
            if self.refuse_launch {
                return Err(Unsupported {
                    backend: MemberBackend::Ganja,
                    reason: UNLAUNCHABLE.to_owned(),
                });
            }

            Ok(())
        }

        fn start(self: Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
            self.log.record("start");

            Vec::new()
        }

        fn alive(&self) -> bool {
            true
        }

        fn recent(&self) -> Vec<String> {
            Vec::new()
        }

        async fn kill(&self) {
            self.log.record("kill");
        }
    }

    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let log = Arc::new(Log::default());
    registry
        .spawn(
            Arc::new(Watching { log: Arc::clone(&log), refuse_launch: false }),
            request("w1", MemberBackend::Ganja, home.path()),
        )
        .await
        .expect("the teammate starts");

    assert_eq!(
        log.calls(),
        vec!["spawn", "launch", "start"],
        "each once, and the launch is between the surface and what watches it"
    );

    let refused = Arc::new(Log::default());
    registry
        .spawn(
            Arc::new(Watching { log: Arc::clone(&refused), refuse_launch: true }),
            request("w2", MemberBackend::Ganja, home.path()),
        )
        .await
        .expect_err("a refused launch is a refused spawn");

    assert_eq!(
        refused.calls(),
        vec!["spawn", "launch", "kill"],
        "a launch that would not run is killed and never started"
    );
}

/// **U-5, D538.** A drain takes everything posted since the last one, and a
/// second drain of the same exits finds nothing.
///
/// The contract the `Vec` this replaced kept, restated over the channel that
/// replaced it: the lead's pass acts on each entry exactly once, so an entry
/// left behind would retire a member twice and an entry dropped would leave a
/// dead pane on the roster forever.
#[tokio::test]
async fn take_exited_drains_everything_posted_since_the_last_call() {
    let home = tempfile::tempdir().expect("a temporary home");
    let registry = registry(home.path());
    let exits = registry.lend().exits;

    assert!(registry.take_exited().is_empty(), "nothing has ended yet");

    for name in ["w1", "w2"] {
        exits
            .send(Exited {
                name: name.to_owned(),
                cli: ganja_team::ShimCli::Codex,
                backend: MemberBackend::Codex,
                pane_id: "%4".to_owned(),
                pane: PaneFate::Closed,
                last_words: None,
            })
            .expect("the registry is still holding the receiving half");
    }

    let taken: Vec<String> = registry.take_exited().into_iter().map(|exit| exit.name).collect();

    assert_eq!(taken, vec!["w1", "w2"], "both, in the order they were posted");
    assert!(registry.take_exited().is_empty(), "and each is taken exactly once");
}
