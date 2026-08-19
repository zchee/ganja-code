//! The teammate fixture family every P25 suite in `ganja-core/tests`
//! re-declared per binary: the two spawn askers, the registry-and-door
//! builder, the calling turn, the spawn request, the team-file readers, the
//! runner harness, and the poll-until wait.
//!
//! Everything that starts a teammate through these fixtures goes through
//! [`Teammates::start`], the one door onto the registry — the entry beneath
//! it is crate-internal precisely so nothing can reach a spawn the
//! permission gate never saw, and a fixture calling past the gate would be a
//! fixture of a path production has not got.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Backends, Caller, SpawnAsk, SpawnAsker, Storage, Teammates,
    permission::Permissions,
    provider::{FakeProvider, Provider},
    team::{
        MailboxMessage, MemberName, MemberRecord, Spawn, Surface, TeamFile, TeamName, TeamsRoot,
        mailbox, record,
    },
    teammate::{
        InProcess, SpawnSpec, Teammate, TeammateRegistry, claude::ClaudePane, pane::GanjaPane,
        runner::Runner,
    },
    tool::{Registry, task::TeammateSpawn},
};
use ganja_protocol::{Event, PermissionReply, team::Frame};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// The lead's session id every fixture team is led by.
pub const LEAD_SESSION_ID: &str = "01998ad0-0000-7000-8000-000000000000";

/// The team every fixture member joins.
pub const TEAM: &str = "session-abcd1234";

/// The default task a fixture spawn is started with. Suites that assert on
/// the prompt itself pass their own through [`spawn_with_prompt`].
pub const TASK: &str = "have a look at the parser";

/// Says yes to everything, and is asked nothing: a fixture spawn works
/// inside its own project and asks for no bypass, so the gate answers
/// `Allow` on its own. It exists because the door requires one, and saying
/// yes is the answer that cannot mask a failure — a test that passed only
/// because somebody refused would be testing the refusal.
#[derive(Debug)]
pub struct AllowSpawn;

#[async_trait]
impl SpawnAsker for AllowSpawn {
    async fn ask(&self, _request: SpawnAsk) -> PermissionReply {
        PermissionReply::Once
    }
}

/// A person who says yes, and a record of every time they were asked.
///
/// Recorded rather than only answered: what a suite wants from the gate is
/// usually its **silence** — a teammate working inside the project with no
/// bypass has nothing anybody needs to approve — and an asker that only said
/// yes would let that silence break without a test noticing.
#[derive(Debug, Default)]
pub struct RecordedSpawns {
    asked: Mutex<Vec<SpawnAsk>>,
}

impl RecordedSpawns {
    /// Fails naming what was asked, which is the whole diagnostic.
    pub fn asked_nobody(&self) {
        let asked = self.asked.lock().expect("the ask log is never poisoned");
        assert!(
            asked.is_empty(),
            "a teammate working inside the project asks nobody: {asked:?}"
        );
    }

    /// Every ask that reached this person, in the order they arrived.
    ///
    /// The mirror of [`RecordedSpawns::asked_nobody`], and it exists because
    /// its absence had a cost: a suite meaning *"this dialog does fire"* had
    /// no way to say so, so the claim was evidenced in a comment describing a
    /// run somebody did once by hand. A path where somebody **is** asked is as
    /// much a promise as one where nobody is.
    pub fn asked(&self) -> Vec<SpawnAsk> {
        self.asked
            .lock()
            .expect("the ask log is never poisoned")
            .clone()
    }
}

#[async_trait]
impl SpawnAsker for RecordedSpawns {
    async fn ask(&self, request: SpawnAsk) -> PermissionReply {
        self.asked
            .lock()
            .expect("the ask log is never poisoned")
            .push(request);

        PermissionReply::Once
    }
}

/// The default backends over `storage`: the real in-process backend — a fake
/// provider over a real store, which is what a teammate needs to have a
/// session at all — and production's own two pane values, which a test that
/// controls no tmux never asks to spawn.
pub fn backends(storage: Storage) -> Backends {
    backends_with(
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        Arc::new(Registry::new(Vec::new())),
        storage,
        |_| Permissions::default(),
    )
}

fn backends_with(
    provider: Arc<dyn Provider>,
    tools: Arc<Registry>,
    storage: Storage,
    posture: impl Fn(&SpawnSpec) -> Permissions + Send + Sync + 'static,
) -> Backends {
    Backends {
        in_process: Arc::new(InProcess::new(provider, tools, storage, posture)),
        pane: Arc::new(GanjaPane),
        claude: Arc::new(ClaudePane),
        // **The rule for all three shim CLIs**, stated once here because the
        // three arrive one wave apart and the reason is the same each time: a
        // fixture lead is *production on a machine where the foreign CLI is
        // not installed*. Never a stub once that CLI's wave has landed — the
        // not-built-yet sentence stops being true, and a fixture asserting a
        // retired refusal is a fixture asserting a lie. W5 was the wave that
        // retired the last of them, so no slot below is a stub any more.
        // Never this process's own
        // `PATH` either: a spawn there would find the developer's real binary,
        // take a real turn and spend somebody's quota from inside the ordinary
        // test suite. A suite that wants a child which answers points the
        // backend at a fake one, the way `shim_support` does.
        //
        // How a backend is made harmless depends on what it does with a
        // `PATH`, which is why the three slots below do not look alike.

        // codex searches, so it is given an empty search path: the spawn is
        // refused by naming the binary, exactly as it would be on a machine
        // without one.
        codex: Arc::new(
            ganja_core::teammate::shim::ShimBackend::new(Arc::new(
                ganja_core::teammate::codex::Codex::new(),
            ))
            .searching(std::ffi::OsString::new()),
        ),
        // agy searches nothing and spawns nothing: W4 measured its floor and
        // it does not hold, so the real backend refuses every spawn. It is
        // already harmless, and giving it an empty `PATH` would suggest the
        // `PATH` was what made it so.
        agy: Arc::new(ganja_core::teammate::agy::Agy::new()),
        // grok searches, exactly as codex does, so it gets the same empty
        // search path and refuses by naming the binary.
        grok: Arc::new(
            ganja_core::teammate::shim::ShimBackend::new(Arc::new(
                ganja_core::teammate::grok::Grok::new(),
            ))
            .searching(std::ffi::OsString::new()),
        ),
    }
}

/// A team over `home`, and the gated door onto it, with every default:
/// [`TEAM`] led by [`LEAD_SESSION_ID`], the store under `home/storage`, and
/// [`backends`]' in-process value.
///
/// ```
/// let home = ganja_testkit::temp_dir();
/// let (root, team, registry, _door) = ganja_testkit::team(home.path());
/// assert_eq!(registry.running(), 0);
/// assert!(
///     ganja_testkit::team_file(&root, &team).is_none(),
///     "a session that never spawns a teammate leaves no team on disk"
/// );
/// ```
pub fn team(home: &Path) -> (TeamsRoot, TeamName, Arc<TeammateRegistry>, Arc<Teammates>) {
    team_with(
        home,
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        Arc::new(Registry::new(Vec::new())),
        Storage::open(home.join("storage")),
        |_| Permissions::default(),
    )
}

/// [`team`], with the in-process backend's four handles named by the caller:
/// the provider a teammate's turns ask, the tool set it is offered, the store
/// its session lands in, and the posture its rules are derived by.
pub fn team_with(
    home: &Path,
    provider: Arc<dyn Provider>,
    tools: Arc<Registry>,
    storage: Storage,
    posture: impl Fn(&SpawnSpec) -> Permissions + Send + Sync + 'static,
) -> (TeamsRoot, TeamName, Arc<TeammateRegistry>, Arc<Teammates>) {
    let root = TeamsRoot::new(home.join("teams"));
    let team = TeamName::parse(TEAM).expect("a team name");
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        LEAD_SESSION_ID,
        home,
    ));
    let door = Arc::new(Teammates::new(
        Arc::clone(&registry),
        backends_with(provider, tools, storage, posture),
    ));

    (root, team, registry, door)
}

/// The calling turn, as the spawn gate reads it: a teammate inherits the
/// model and the directory of the turn that started it, and the rules a
/// spawn is judged by are the **lead's** own.
///
/// `cwd` and `project_root` are the same directory here, which is the
/// ordinary case and the one that asks nobody anything: a teammate working
/// inside the project discloses no directory and raises no dialog.
///
/// ```
/// let project = ganja_testkit::temp_dir();
/// let caller = ganja_testkit::caller(project.path());
/// assert_eq!(caller.cwd, caller.project_root);
/// ```
pub fn caller(project: &Path) -> Caller {
    caller_with(project, Arc::new(Mutex::new(Permissions::default())))
}

/// [`caller`], judged by a live ruleset the test also holds — for the suite
/// whose whole claim is about what those rules deny.
pub fn caller_with(project: &Path, permissions: Arc<Mutex<Permissions>>) -> Caller {
    Caller {
        model: "recorder-model".to_owned(),
        cwd: project.to_path_buf(),
        permissions,
        project_root: project.to_path_buf(),
    }
}

/// A spawn of `name` on `backend` with the default [`TASK`], so two spawns
/// differ only where a test is looking.
pub fn spawn(name: &str, backend: Option<&str>) -> TeammateSpawn {
    spawn_with_prompt(name, backend, TASK)
}

/// [`spawn`], carrying the prompt a suite asserts on.
pub fn spawn_with_prompt(name: &str, backend: Option<&str>, prompt: &str) -> TeammateSpawn {
    TeammateSpawn {
        name: name.to_owned(),
        backend: backend.map(str::to_owned),
        agent_type: "general".to_owned(),
        prompt: prompt.to_owned(),
    }
}

/// The team file as it stands on disk, or [`None`] for one never written.
pub fn team_file(root: &TeamsRoot, team: &TeamName) -> Option<TeamFile> {
    let text = std::fs::read_to_string(root.config_path(team)).ok()?;

    Some(serde_json::from_str(&text).expect("the team file this build wrote decodes"))
}

/// The **teammates** the team file records, sorted, or the empty account of
/// a file that was never written.
///
/// The lead is a member of that file too — it is the team's own roster, not
/// a list of the people it started — and it is dropped here because what a
/// suite reads this for is what a spawn wrote.
pub fn teammates_recorded(root: &TeamsRoot, team: &TeamName) -> Vec<String> {
    let Some(file) = team_file(root, team) else {
        return Vec::new();
    };
    let mut names: Vec<String> = file
        .members
        .into_iter()
        .map(|member| member.name)
        .filter(|name| name != ganja_core::team::LEAD)
        .collect();
    names.sort();

    names
}

/// Writes a team file led by `lead_session` in `cwd`, holding one teammate
/// record per `(name, spawn)` pair, and answers where it was written.
///
/// The seeder the pane suites plant a pre-existing team with: production's
/// own writer is the registry's and crate-internal, so a test that needs a
/// team *on disk before anything runs* writes the document itself.
pub fn seed_team_file(
    root: &TeamsRoot,
    team: &TeamName,
    lead_session: &str,
    cwd: &Path,
    members: &[(MemberName, Spawn)],
) -> PathBuf {
    let mut file = TeamFile::new(
        team,
        lead_session,
        cwd.to_string_lossy().into_owned(),
        record::now_millis(),
    );
    for (name, spawn) in members {
        file.members.push(MemberRecord::teammate(
            name,
            team,
            spawn.clone(),
            record::now_millis(),
        ));
    }
    let path = root.config_path(team);
    std::fs::create_dir_all(path.parent().expect("a team file has a directory"))
        .expect("the team directory is made");
    std::fs::write(
        &path,
        record::document(&file).expect("the team file encodes"),
    )
    .expect("the team file is written");

    path
}

/// One teammate, its runner, and the two inboxes they use — for suites that
/// drive the runner a pass at a time through [`Runner::tick`] rather than
/// through its loop.
pub struct RunnerHarness {
    /// Dropping this deletes the tree both roots are under.
    _home: TempDir,
    /// The teammate under the runner, for the engine-side reads a tick's
    /// account cannot carry.
    pub teammate: Arc<Teammate>,
    pub runner: Runner,
    /// The teammate's own inbox on disk.
    pub inbox: PathBuf,
    /// The teammate engine's birth queue, when the harness was built with
    /// `drain: false` — the caller reads it, because the announcements it
    /// asserts on arrive there.
    pub events: Option<BoxStream<'static, Event>>,
}

impl RunnerHarness {
    /// A `worker` under [`TEAM`]'s roots, its inbox seeded.
    ///
    /// The birth queue is a lossless lane, and one nobody drains fills and
    /// then makes the teammate's own turn wait — which is why the runner
    /// claims it in `run`. A tick-driven suite never calls `run`, so `drain:
    /// true` spawns the drain here instead of leaving an absence that would
    /// eventually hang; `drain: false` hands the stream to the caller in
    /// [`RunnerHarness::events`] instead.
    pub async fn new(drain: bool) -> Self {
        let home = crate::temp_dir();
        let storage = Storage::open(home.path().join("storage"));
        let root = TeamsRoot::new(home.path().join("teams"));
        let team = TeamName::parse(TEAM).expect("a team name");
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

        let mut events = teammate
            .engine()
            .subscribe()
            .await
            .expect("the first subscriber wins");
        let events = if drain {
            tokio::spawn(async move { while events.next().await.is_some() {} });
            None
        } else {
            Some(events)
        };

        let inbox = root.inbox_path(&team, &worker);
        let lead_inbox = root.inbox_path(&team, &lead);
        mailbox::seed(&inbox).expect("the inbox seeds");

        Self {
            _home: home,
            runner: Runner::new(
                Arc::clone(&teammate),
                lead,
                inbox.clone(),
                lead_inbox,
                Surface::InProcess,
                CancellationToken::new(),
            ),
            teammate,
            inbox,
            events,
        }
    }

    /// Puts a frame in the teammate's inbox, as `from`.
    pub fn arrives(&self, from: &str, frame: &Frame) {
        mailbox::write(
            &self.inbox,
            MailboxMessage::from_frame(from, frame, record::now_iso8601())
                .expect("a frame encodes"),
        )
        .expect("the inbox is writable");
    }

    /// What is still in the teammate's inbox.
    pub fn left(&self) -> usize {
        mailbox::read(&self.inbox)
            .expect("the inbox reads")
            .valid
            .len()
    }
}

/// Polls `read` every 25ms until it answers, or panics with `what` after
/// `limit`.
///
/// The one poll-until wait behind every "the runner will get to it" claim:
/// the sleep is only the poll interval, and the condition is what is
/// synchronised on.
pub async fn eventually<T>(
    limit: Duration,
    what: &str,
    mut read: impl AsyncFnMut() -> Option<T>,
) -> T {
    let started = tokio::time::Instant::now();
    loop {
        if let Some(found) = read().await {
            return found;
        }
        assert!(
            started.elapsed() < limit,
            "waited {limit:?} for {what}, and it never happened"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The write half round-trips through the read half: what a seeder plants
    /// is what a sweep finds.
    #[test]
    fn a_seeded_team_file_reads_back_with_its_members() {
        let home = crate::temp_dir();
        let root = TeamsRoot::new(home.path().join("teams"));
        let team = TeamName::parse(TEAM).expect("a team name");
        let worker = MemberName::parse("w1").expect("a member name");
        let spawn = Spawn {
            agent_type: "general".to_owned(),
            model: "fake/fake".to_owned(),
            color: "blue".to_owned(),
            prompt: "watch the build".to_owned(),
            plan_mode_required: false,
            surface: Surface::Pane {
                id: "%7".to_owned(),
            },
            cwd: home.path().to_string_lossy().into_owned(),
        };

        let path = seed_team_file(
            &root,
            &team,
            LEAD_SESSION_ID,
            home.path(),
            &[(worker, spawn)],
        );

        assert_eq!(path, root.config_path(&team));
        let file = team_file(&root, &team).expect("the seeded file is on disk");
        assert!(file.member("w1").is_some(), "{file:?}");
        assert_eq!(teammates_recorded(&root, &team), vec!["w1".to_owned()]);
    }

    /// The fixture's codex backend resolves no binary at all.
    ///
    /// The B1 hardening: this fixture is safe because an empty search path
    /// resolves nothing, which is a property of `shim::resolve`'s
    /// empty-and-relative component filter rather than of anything written
    /// here. If that filter ever stops dropping the empty component, the
    /// fixture lead would quietly find the developer's real `codex` and start
    /// spending somebody's quota from inside an ordinary test run — a failure
    /// whose only symptom is a slow suite. So it breaks loudly here instead.
    #[test]
    fn the_fixture_codex_backend_resolves_no_binary_at_all() {
        assert!(
            ganja_core::teammate::shim::resolve(&std::ffi::OsString::new(), "codex").is_none(),
            "an empty search path must resolve nothing, or the fixture lead spawns a real codex"
        );
    }
}
