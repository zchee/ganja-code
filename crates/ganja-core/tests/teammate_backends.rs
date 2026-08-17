//! Which surfaces there are, how one is named, what a build that cannot have
//! one says, and what two spawns racing for one name get (**D501**, **AC-27**,
//! AC-14's P25a leg).
//!
//! Every root is handed in, and nothing here **writes** process-wide state, so
//! this binary holds several tests. It does read some — `tempfile` resolves
//! `TMPDIR` for the directories below — which is a read every test in the
//! workspace makes and no test here can disturb.
//!
//! What it deliberately does *not* hold is the door-equivalence claim: that a
//! `task` call and `/team spawn` build the same request is asserted where each
//! door lives, because a core test binary cannot see the TUI one.
//!
//! Everything that starts a teammate here goes through
//! [`ganja_core::Teammates::start`], which is the **only** door onto the
//! registry's spawn: the entry beneath it is crate-internal precisely so that
//! nothing can reach a spawn the permission gate never saw, and a test calling
//! past the gate would be a test of a path production has not got.

use std::{sync::Arc, time::Duration};

use ganja_core::{
    Backends, Caller, SpawnAsk, SpawnAsker, Storage, Teammates,
    permission::Permissions,
    protocol::{PermissionReply, team::MemberBackend},
    provider::FakeProvider,
    teammate::{
        BACKENDS, DEFAULT_BACKEND, Delivery, InProcess, REFUSED_UNTIL_P25B, TeammateBackend,
        TeammateRegistry, backend_name, claude::ClaudePane, pane::GanjaPane, parse_backend,
    },
    tool::{Registry, task::TeammateSpawn},
};
use ganja_team::{MemberName, TeamFile, TeamName, TeamsRoot, mailbox};

/// The task every teammate here is started with. Nothing reads it; what
/// matters is that a spawn that failed left none of it behind.
const TASK: &str = "have a look at the parser";

/// Says yes to everything, and is asked nothing by any test here.
///
/// Every spawn below works inside its own project and asks for no bypass, so
/// [`ganja_core::teammate::posture::spawn_gate`] answers `Allow` and this is
/// never reached. It exists because the door requires one, and saying yes is
/// the answer that cannot mask a failure: a test that passed only because
/// somebody refused would be testing the refusal.
#[derive(Debug)]
struct Yes;

#[async_trait::async_trait]
impl SpawnAsker for Yes {
    async fn ask(&self, _request: SpawnAsk) -> PermissionReply {
        PermissionReply::Once
    }
}

/// A team over `home`, and the door onto it.
///
/// The in-process backend is the real one — a fake provider over a real store,
/// which is what a teammate needs to have a session at all — and both pane
/// slots hold this build's own refusing skeletons, so a request naming one is
/// refused by the same value production would refuse it with.
fn team(home: &std::path::Path) -> (TeamsRoot, TeamName, Arc<TeammateRegistry>, Arc<Teammates>) {
    let root = TeamsRoot::new(home.join("teams"));
    let team = TeamName::parse("session-abcd1234").expect("a team name");
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-0000-7000-8000-000000000000",
        home,
    ));
    let door = Arc::new(Teammates::new(
        Arc::clone(&registry),
        Backends {
            in_process: Arc::new(InProcess::new(
                Arc::new(FakeProvider::new("on it", Duration::ZERO)),
                Arc::new(Registry::new(Vec::new())),
                Storage::open(home.join("storage")),
                |_| Permissions::default(),
            )),
            pane: Arc::new(GanjaPane),
            claude: Arc::new(ClaudePane),
        },
    ));

    (root, team, registry, door)
}

/// The calling turn, as the gate reads it. `cwd` and `project_root` are one
/// directory, which is the case that discloses nothing and asks nobody.
fn caller(home: &std::path::Path) -> Caller {
    Caller {
        model: "recorder-model".to_owned(),
        cwd: home.to_path_buf(),
        permissions: Arc::new(std::sync::Mutex::new(Permissions::default())),
        project_root: home.to_path_buf(),
    }
}

/// A spawn of `name` on `backend`, with everything else the same, so two
/// spawns differ only where a test is looking.
fn request(name: &str, backend: Option<&str>) -> TeammateSpawn {
    TeammateSpawn {
        name: name.to_owned(),
        backend: backend.map(str::to_owned),
        agent_type: "general".to_owned(),
        prompt: TASK.to_owned(),
    }
}

/// The **teammates** the team file records, sorted, or the empty account of a
/// file that was never written.
///
/// The lead is a member of that file too — it is the team's own roster, not a
/// list of the people it started — and it is dropped here because every claim
/// below is about what a spawn wrote.
fn recorded(root: &TeamsRoot, team: &TeamName) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.config_path(team)) else {
        return Vec::new();
    };
    let file: TeamFile =
        serde_json::from_str(&text).expect("the team file this build wrote decodes");
    let mut names: Vec<String> = file
        .members
        .into_iter()
        .map(|member| member.name)
        .filter(|name| name != ganja_team::LEAD)
        .collect();
    names.sort();

    names
}

/// **AC-27.** A value outside the three is refused *by name*, and the refusal
/// carries the list — because the useful half of "no such backend" is which
/// ones there are, and a typo is the only way anybody reaches this.
#[test]
fn an_unknown_backend_value_is_refused_naming_the_three() {
    let refused = parse_backend("tmux").expect_err("there is no backend called tmux");

    assert_eq!(refused.value, "tmux");
    let sentence = refused.to_string();
    assert!(sentence.contains("tmux"), "{sentence}");
    for backend in BACKENDS {
        assert!(
            sentence.contains(backend),
            "the refusal must list {backend}: {sentence}"
        );
    }

    // A near-miss is refused too: the argument is a value, not a prefix.
    assert!(parse_backend("in process").is_err());
    assert!(parse_backend("").is_err());
    assert!(parse_backend("Claude").is_err());
}

/// The argument's vocabulary and the document's are the same three words.
///
/// Written out in [`BACKENDS`] and matched in [`backend_name`], then checked
/// here against what the type actually serializes as: two hand-written lists
/// that agree today are two lists that could stop agreeing, and this is what
/// says so.
#[test]
fn every_backend_value_is_spelled_the_way_it_is_serialized() {
    for name in BACKENDS {
        let backend = parse_backend(name).expect("a listed value parses");
        assert_eq!(backend_name(backend), name);
        assert_eq!(
            serde_json::to_value(backend).expect("a backend serializes"),
            serde_json::json!(name),
            "the argument and the member record must spell {name} the same"
        );
    }
}

/// Neither door infers a backend, and the one they fall back to is the one with
/// no window: an in-process teammate is what a spawn that said nothing gets.
#[test]
fn the_default_backend_is_the_one_in_this_process() {
    assert_eq!(DEFAULT_BACKEND, MemberBackend::InProcess);
    assert_eq!(backend_name(DEFAULT_BACKEND), "in-process");
}

/// What each backend can promise about a delivery (**D501**, spent by
/// **D503**).
///
/// The split is the whole reason `delivery()` is on the trait: a real `claude`
/// pane marks a message read when it reads it, not when a turn takes it on, so
/// a lead waiting for an acknowledgement from one would wait forever.
#[tokio::test]
async fn each_backend_says_what_it_can_promise_about_a_delivery() {
    let in_process = InProcess::new(
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        Arc::new(Registry::new(Vec::new())),
        Storage::open(ganja_testkit::temp_dir().path().join("storage")),
        |_| Permissions::default(),
    );

    assert_eq!(in_process.delivery(), Delivery::Acknowledged);
    assert_eq!(GanjaPane.delivery(), Delivery::Acknowledged);
    assert_eq!(ClaudePane.delivery(), Delivery::FireAndForget);

    assert_eq!(in_process.backend(), MemberBackend::InProcess);
    assert_eq!(GanjaPane.backend(), MemberBackend::Pane);
    assert_eq!(ClaudePane.backend(), MemberBackend::Claude);
}

/// **AC-14's P25a leg.** Both pane values refuse, they refuse with the same
/// sentence, and the sentence names the phase that will change the answer.
///
/// Identical on purpose: one door spawning where the other refuses would be two
/// behaviours wearing one argument, and a refusal that named neither the phase
/// nor the alternative would leave a reader guessing whether their session or
/// their build was the problem.
#[tokio::test]
async fn the_two_pane_backends_refuse_identically_until_p25b() {
    let home = ganja_testkit::temp_dir();
    let (root, team, registry, door) = team(home.path());
    let caller = caller(home.path());

    let ganja = door
        .start(request("worker", Some("pane")), &caller, &Yes)
        .await
        .expect_err("this build has no panes yet");
    let claude = door
        .start(request("worker", Some("claude")), &caller, &Yes)
        .await
        .expect_err("nor real claude ones");

    // The sentence a model reads is the surface it asked for, then why. So the
    // heads differ by exactly the backend's own name and the tails are one
    // string — which is what "refuse identically" is a claim about.
    assert!(
        ganja.reason.ends_with(REFUSED_UNTIL_P25B) && claude.reason.ends_with(REFUSED_UNTIL_P25B),
        "a pane refusal names the phase that changes the answer: {ganja:?} / {claude:?}"
    );
    assert!(ganja.reason.contains("P25b"), "{}", ganja.reason);
    assert!(
        ganja.reason.contains("pane") && claude.reason.contains("claude"),
        "a refusal still says which surface was asked for: {ganja:?} / {claude:?}"
    );
    assert_ne!(
        ganja.reason, claude.reason,
        "and the two are still told apart by it"
    );

    // A refused spawn leaves nothing behind: no member, no teammate, and — the
    // half that would otherwise still be there tomorrow — no task sitting in a
    // mailbox nothing will ever read.
    assert!(
        recorded(&root, &team).is_empty(),
        "a team file was written for a teammate that never started"
    );
    assert_eq!(registry.running(), 0);
    let inbox = root.inbox_path(&team, &MemberName::parse("worker").expect("a member name"));
    assert!(
        mailbox::read(&inbox)
            .expect("the inbox reads")
            .valid
            .is_empty(),
        "a refused spawn left its prompt in an inbox"
    );
}

/// **Two spawns of one name at once are two teammates, not one and a ghost.**
///
/// The window is real and it is wide: a spawn crosses four awaits between
/// reading which names are taken and registering itself, and `task` bodies run
/// up to `agents.concurrency` at a time. Without a synchronous reservation both
/// spawns resolve to `worker`, share one inbox, and the second registration
/// evicts the first from the member map — leaving a teammate with its own
/// engine and its own turn that nothing holds, and therefore that
/// [`TeammateRegistry::shutdown`] never ends.
///
/// The two run in a [`tokio::task::JoinSet`], and on this runtime that makes
/// the race **deterministic** rather than likely: the taken-names read hops to
/// the blocking pool, so the first spawn is guaranteed to yield there and the
/// second is guaranteed to read the same snapshot it did.
#[tokio::test]
async fn two_spawns_of_one_name_at_once_get_two_teammates() {
    let home = ganja_testkit::temp_dir();
    let (root, team, registry, door) = team(home.path());
    let caller = caller(home.path());

    let mut spawning = tokio::task::JoinSet::new();
    for _ in 0..2 {
        let door = Arc::clone(&door);
        let caller = caller.clone();
        spawning.spawn(async move { door.start(request("worker", None), &caller, &Yes).await });
    }

    let mut names = Vec::new();
    while let Some(joined) = spawning.join_next().await {
        let started = joined
            .expect("neither spawn panicked")
            .expect("both spawns start: a taken name is resolved, never refused");
        names.push(started.name);
    }
    names.sort();

    assert_eq!(names.len(), 2);
    assert_ne!(
        names[0], names[1],
        "two teammates cannot answer to one name: {names:?}"
    );
    assert!(
        names.contains(&"worker".to_owned()),
        "one of them is still the name that was asked for: {names:?}"
    );

    // Both are real: two records on disk, two inboxes, and — the assertion the
    // orphan fails — two teammates the registry still holds.
    assert_eq!(
        recorded(&root, &team),
        names,
        "each teammate wrote its own member record"
    );
    for name in &names {
        let member = MemberName::parse(name).expect("a resolved name is a member name");
        assert!(
            root.inbox_path(&team, &member).exists(),
            "{name} was given a mailbox of its own"
        );
    }
    assert_eq!(
        registry.running(),
        2,
        "a teammate the map forgot is a teammate nothing can shut down"
    );

    registry.shutdown().await;
    assert_eq!(
        registry.running(),
        0,
        "and both of them really went when the team did"
    );
}

/// The plan-approval wait is a seam something can actually reach.
///
/// It answers a real defect rather than a hypothetical: the runner's loop used
/// to **consume** the value it ran on, so nothing outlived it holding one — and
/// every approval the lead ever sent would have been ignored as answering
/// nothing, while the method saying otherwise sat there uncallable. What is
/// still absent is the asking side, which is why this pins reachability rather
/// than a round trip.
#[tokio::test]
async fn a_running_teammate_can_be_told_which_plan_approval_it_is_waiting_on() {
    let home = ganja_testkit::temp_dir();
    let (_root, _team, registry, door) = team(home.path());

    let started = door
        .start(request("worker", None), &caller(home.path()), &Yes)
        .await
        .expect("an in-process teammate starts");

    assert!(
        registry.awaiting_plan_approval(&started.name, "plan-request-1"),
        "a running in-process teammate has a loop to tell"
    );
    assert!(
        !registry.awaiting_plan_approval("nobody", "plan-request-1"),
        "a name this team never had is nobody to tell"
    );

    registry.shutdown().await;
}
