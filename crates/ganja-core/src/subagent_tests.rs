use std::sync::Arc;

use ganja_team::{MailboxMessage, MemberName, TeamName, TeamsRoot, mailbox};
use serde::Deserialize;
use tokio::sync::mpsc;

use super::{
    Address, Backends, Body, Caller, FRAME_OVER_SOCKET, FRAME_OVER_SOCKET_SOLO, HeldWire, Host,
    Incoming, MAX_HOP_CHAIN_ENTRIES, MESSAGE_ROUTE, MemberBackend, NOT_A_SESSION_SOCKET,
    NotReceived, NotSpawned, ONE_WAY_NOTE, PeerFacts, PermissionReply, Postbox, RECEIVED,
    ReceiptStatus, Reserved, SOCKET_LEAD_UNNAMED, SOCKET_OVERSIZED, SOCKET_REFUSED,
    SOCKET_UNREACHABLE, SenderMode, Sent, SocketDelivered, SocketMessage, SocketReceipt,
    SoloPostbox, Spawn, SpawnAsk, SpawnAsker, SpawnRequest, TEAM_GONE, TEAM_ROUTE, Teammate,
    TeammateRegistry, TeammateSpawn, Teammated, Teammates, Undelivered, Watched, async_trait,
    deliver_to_lead, denies_task, held_note, identity, receive_ladder, roster, subagent_rules,
    team, watch,
};
use crate::agent::{self, Registry};
use crate::config::Config;
use crate::engine::Fanout;
use crate::permission::{Action, Permissions, Rule};
use crate::protocol::{
    Event, MessageId, Part, PartBody, PartId, PeerMessageId, SessionId, ToolState,
};
use crate::tool::Tool as _;
use crate::tool::task::{DESCRIPTION, ROSTER_HEADER, Subagents as _, TaskTool};

fn registry() -> Registry {
    Registry::from_config(&Config::default()).expect("the default config resolves agents")
}

/// The ungated compose of the socket door's two halves — the ladder, then
/// the delivery tail — which is what this suite exercises: the rungs and
/// the write are this file's, while the admission gate between them is
/// the engine's and is pinned by the engine's own suite
/// (`Engine::receive_peer_message`).
async fn receive(
    registry: &Arc<TeammateRegistry>,
    incoming: Incoming,
) -> Result<Sent, NotReceived> {
    let message = receive_ladder(registry, incoming)?;
    let (sent, _identity) = deliver_to_lead(registry, message).await?;

    Ok(sent)
}

#[test]
fn the_team_routes_this_side_speaks_match_the_server_and_client_twins() {
    let server = include_str!("../../ganja-serve/src/routes.rs");
    let client = include_str!("../../ganja-client/src/lib.rs");

    assert_eq!(TEAM_ROUTE, "/team");
    assert_eq!(format!("{TEAM_ROUTE}/{}{MESSAGE_ROUTE}", "some-lead"), "/team/some-lead/message");
    assert!(server.contains("/team/{name}/message"));
    assert!(client.contains("/team/{name}/message"));
}

/// A child's thinking is not a child's answer (bead `pwe`), and the
/// accumulator is where that could go wrong without anybody seeing it.
///
/// What the watcher collects becomes the **parent's tool result** — text
/// the parent model reads as the report it asked for, and which every
/// later request in the parent's conversation then carries. So a thought
/// leaking here is a thought that reaches a model, by a route no wire
/// encoder guards: the arm that keeps `open` where it is on a
/// `ReasoningText` part is the whole of the barrier, and moving it into
/// the text arm would have the child's scratch paper delivered as its
/// conclusion.
#[tokio::test]
async fn a_childs_thinking_is_not_the_answer_its_parent_is_handed() {
    const THOUGHT: &str = "the-user-is-probably-testing-me";

    let (events, received) = mpsc::channel(64);
    let (parent, _parent_reader) = mpsc::channel(64);
    let watched = Watched {
        events: Arc::new(Fanout::new(parent)),
        session_id: SessionId::from("ses_parent".to_owned()),
        tools: Arc::new(crate::tool::Registry::new(Vec::new())),
        message_id: MessageId::ascending(),
        part_id: PartId::ascending(),
        command: "look something up".to_owned(),
    };

    let message_id = MessageId::ascending();
    let session_id = SessionId::from("ses_child".to_owned());
    // Thinking on *both* sides of the reply, which is the shape that makes
    // this an assertion rather than an accident: what the parent is handed
    // is the last text part the child opened (upstream's `findLast`), so a
    // trailing thought is the one that would actually be delivered as the
    // answer. A thought that only ever preceded the reply would be
    // overwritten by it and prove nothing.
    let mut stream = Vec::new();
    for (part, delta) in [
        (Part::reasoning_text(""), THOUGHT),
        (Part::text(""), "the answer"),
        (Part::reasoning_text(""), "and-a-trailing-second-thought"),
    ] {
        let part_id = part.id.clone();
        stream.push(Event::PartStarted {
            session_id: session_id.clone(),
            message_id: message_id.clone(),
            part,
        });
        stream.push(Event::PartDelta {
            session_id: session_id.clone(),
            message_id: message_id.clone(),
            part_id,
            delta: delta.to_owned(),
        });
    }
    for event in stream {
        events.send(event).await.expect("the watcher is listening");
    }
    drop(events);

    let outcome = watch(received, watched).await;

    assert_eq!(
        outcome.text, "the answer",
        "the parent is handed the child's conclusion and nothing else"
    );
    assert!(
        !outcome.text.contains(THOUGHT) && !outcome.text.contains("second-thought"),
        "the child's thinking reached the parent's tool result: {}",
        outcome.text
    );
}

/// The log under the row (2026-08-15): every distinct call the child
/// makes joins `calls` in order, a call republishing its running part as
/// it streams joins once, the cap keeps the newest, and the finished
/// outcome carries the log out to the completed part's metadata.
#[tokio::test]
async fn the_watcher_logs_the_childs_calls_in_order_and_keeps_the_newest() {
    let (events, received) = mpsc::channel(2048);
    let (parent, parent_reader) = mpsc::channel(64);
    // The reports the watcher publishes are nobody's to read here, and an
    // undrained lossless subscriber would park the watcher at the
    // channel's cap — dropping it makes every report a cheap refusal.
    drop(parent_reader);
    let watched = Watched {
        events: Arc::new(Fanout::new(parent)),
        session_id: SessionId::from("ses_parent".to_owned()),
        tools: Arc::new(crate::tool::Registry::new(Vec::new())),
        message_id: MessageId::ascending(),
        part_id: PartId::ascending(),
        command: "map it".to_owned(),
    };

    let message_id = MessageId::ascending();
    let session_id = SessionId::from("ses_child".to_owned());
    let call = |index: usize, state: ToolState| Part {
        id: PartId::from(format!("prt_{index}")),
        body: PartBody::Tool {
            call_id: format!("call_{index}"),
            tool: format!("tool-{index}"),
            state,
        },
    };
    for index in 0..105 {
        events
            .send(Event::PartStarted {
                session_id: session_id.clone(),
                message_id: message_id.clone(),
                part: call(index, ToolState::Pending { input: None }),
            })
            .await
            .expect("the watcher is listening");
        // The same running part twice, the way a streaming call
        // republishes: one row in the log, not two.
        for _ in 0..2 {
            events
                .send(Event::PartUpdated {
                    session_id: session_id.clone(),
                    message_id: message_id.clone(),
                    part: call(
                        index,
                        ToolState::Running {
                            input: serde_json::Value::Null,
                            metadata: serde_json::Value::Null,
                            started: 0,
                        },
                    ),
                })
                .await
                .expect("the watcher is listening");
        }
    }
    drop(events);

    let outcome = watch(received, watched).await;

    assert_eq!(outcome.toolcalls, 105, "the count is the true total");
    assert_eq!(outcome.calls.len(), super::CALL_LOG, "the log holds exactly the cap");
    assert_eq!(
        outcome.calls.first().map(String::as_str),
        Some("tool-5"),
        "the oldest five fell off the cap"
    );
    assert_eq!(
        outcome.calls.last().map(String::as_str),
        Some("tool-104"),
        "the newest call ends the log"
    );
}

#[test]
fn the_description_is_upstreams_text_followed_by_the_callers_roster() {
    let agents = registry();
    let build = agents.get(agent::BUILD).expect("build is builtin");
    let tool = TaskTool::new(&roster(&agents, build));
    let described = tool.description();

    assert!(described.starts_with(DESCRIPTION), "upstream's text comes first, unedited");
    // Only the tail past the header is the roster: upstream's own text
    // carries `- ` bullets of its own.
    let (_, listed) = described.split_once(ROSTER_HEADER).expect("the roster header is appended");
    let roster: Vec<&str> = listed.lines().filter(|line| line.starts_with("- ")).collect();
    assert_eq!(roster.len(), 2, "two subagents ship: {roster:?}");
    assert!(roster[0].starts_with("- explore: "), "sorted by name");
    assert!(roster[1].starts_with("- general: "));
}

/// The planning agent denies `task: general`, so what it may delegate to
/// is what is left.
#[test]
fn an_agent_that_denies_a_subagent_is_not_offered_it() {
    let agents = registry();
    let plan = agents.get(agent::PLAN).expect("plan is builtin");
    let tool = TaskTool::new(&roster(&agents, plan));
    let described = tool.description();

    assert!(described.contains("- explore: "));
    assert!(!described.contains("- general: "), "plan denies task:general: {described}");
}

#[test]
fn a_subagent_may_not_delegate_and_may_not_keep_a_todo_list() {
    let agents = registry();
    let explore = agents.get(agent::EXPLORE).expect("explore is builtin");
    let rules = subagent_rules(explore, &Permissions::default());

    assert!(denies_task(&rules, "general"));
    assert_eq!(
        rules
            .iter()
            .rev()
            .find(|rule| rule.permission == "todowrite")
            .map(|rule| rule.action.clone()),
        Some(Action::Deny)
    );
}

/// `general` already says something about `todowrite`, so upstream leaves
/// that decision alone rather than appending a second one.
#[test]
fn a_subagent_that_already_rules_on_todowrite_keeps_its_own_rule() {
    let agents = registry();
    let general = agents.get(agent::GENERAL).expect("general is builtin");
    let rules = subagent_rules(general, &Permissions::default());

    assert_eq!(
        rules.iter().filter(|rule| rule.permission == "todowrite").count(),
        1,
        "the appended denial would be a second one: {rules:?}"
    );
}

#[test]
fn a_parents_denial_reaches_the_child_and_a_parents_allowance_does_not() {
    let mut parent = Permissions::default();
    parent.set_baseline(vec![
        Rule { permission: "webfetch".to_owned(), pattern: "*".to_owned(), action: Action::Deny },
        Rule {
            permission: "bash".to_owned(),
            pattern: "cargo *".to_owned(),
            action: Action::Allow,
        },
    ]);

    let agents = registry();
    let general = agents.get(agent::GENERAL).expect("general is builtin");
    let rules = subagent_rules(general, &parent);

    assert!(
        rules.iter().any(|rule| rule.permission == "webfetch" && rule.action == Action::Deny),
        "a denial travels down: {rules:?}"
    );
    assert!(
        !rules.iter().any(|rule| rule.pattern == "cargo *"),
        "an allowance does not: {rules:?}"
    );
}

/// One teammate, its postbox, and the team both are members of.
///
/// Every root is handed in — the store, the teams directory — so nothing
/// here touches process-wide state and the module keeps holding one tests
/// module rather than earning a binary.
struct Team {
    /// Dropping this deletes the tree both roots are under.
    _home: tempfile::TempDir,
    root: TeamsRoot,
    team: TeamName,
    registry: Arc<TeammateRegistry>,
    /// The postbox of a teammate called `worker`.
    worker: Postbox,
}

impl Team {
    async fn new() -> Self {
        let home = ganja_testkit::temp_dir();
        let registry = crate::teammate::tests::registry(home.path());
        let root = registry.root().clone();
        let team = registry.team().clone();
        registry
            .spawn(
                Arc::new(crate::teammate::InProcess::new(
                    Arc::new(crate::provider::FakeProvider::new(
                        "on it",
                        std::time::Duration::ZERO,
                    )),
                    Arc::new(crate::tool::Registry::new(Vec::new())),
                    crate::Storage::open(home.path().join("storage")),
                    |_| Permissions::default(),
                )),
                SpawnRequest {
                    name: "worker".to_owned(),
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
            .expect("a teammate joins");

        // A second `Teammate` under the name the registry just resolved,
        // because the one the registry made is behind its own handle and a
        // postbox only ever reads a name off the value it is given. What
        // this stands in for is the installation the registry itself does
        // when it starts a teammate's engine; what it proves is what
        // `Postbox::of` binds.
        let teammate = Teammate::new(
            "worker",
            Arc::new(crate::provider::FakeProvider::new("on it", std::time::Duration::ZERO)),
            "recorder-model",
            Arc::new(crate::tool::Registry::new(Vec::new())),
            Permissions::default(),
            crate::Storage::open(home.path().join("storage")),
        );
        let worker = Postbox::of(&registry, &teammate);

        Self { _home: home, root, team, registry, worker }
    }

    /// A session-named socket path in a private (`0700`) directory under
    /// this team's home — what the deliver arm's address gate admits.
    /// Nothing is bound at it; the caller decides what listens there.
    fn session_socket_path(&self, stem: &str) -> std::path::PathBuf {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

        let run = self._home.path().join("run");
        if !run.exists() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&run)
                .expect("a private socket directory is made");
            std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700))
                .expect("and held at 0700 whatever the umask");
        }

        run.join(format!("{stem}.sock"))
    }

    /// Every message in `name`'s inbox that checked out.
    fn inbox(&self, name: &str) -> Vec<MailboxMessage> {
        let member = MemberName::parse(name).expect("a member name");

        mailbox::read(&self.root.inbox_path(&self.team, &member)).expect("an inbox reads").valid
    }
}

/// Records every spawn it was asked about and answers each with `answer`.
#[derive(Debug)]
struct Asked {
    seen: std::sync::Mutex<Vec<SpawnAsk>>,
    answer: PermissionReply,
}

impl Asked {
    fn answering(answer: PermissionReply) -> Self {
        Self { seen: std::sync::Mutex::new(Vec::new()), answer }
    }

    fn seen(&self) -> Vec<SpawnAsk> {
        self.seen.lock().expect("no panic").clone()
    }
}

#[async_trait]
impl SpawnAsker for Asked {
    async fn ask(&self, request: SpawnAsk) -> PermissionReply {
        self.seen.lock().expect("no panic").push(request);

        self.answer
    }
}

/// An asker that must never be reached.
///
/// The refusals that precede the dialog are what this proves: a `SpawnAsk`
/// arriving here is the defect, so it panics where it happens rather than
/// recording an answer a later assertion would have to notice was wrong.
struct NeverAsked;

#[async_trait]
impl SpawnAsker for NeverAsked {
    async fn ask(&self, request: SpawnAsk) -> PermissionReply {
        panic!("nobody may be asked about this spawn, and one was: {request:?}");
    }
}

/// A `task` call's request, at its dullest.
fn wanted() -> TeammateSpawn {
    TeammateSpawn {
        name: "worker".to_owned(),
        backend: None,
        agent_type: "general".to_owned(),
        prompt: "have a look at the parser".to_owned(),
    }
}

/// A caller whose rules are `rules` and who works in `cwd`, judged against
/// `project_root`.
fn caller(rules: Vec<Rule>, cwd: &std::path::Path, project_root: &std::path::Path) -> Caller {
    let mut permissions = Permissions::default();
    permissions.set_baseline(rules);

    Caller {
        model: "recorder-model".to_owned(),
        cwd: cwd.to_path_buf(),
        permissions: Arc::new(std::sync::Mutex::new(permissions)),
        project_root: project_root.to_path_buf(),
    }
}

use crate::teammate::tests::{NEVER, Never, registry as team_registry};

/// A door onto one team, over [`Never`] — a backend that spawns nothing
/// at all.
///
/// The refusal that backend answers with is not what these tests read — a
/// gate that let the spawn through is visible as *reaching* the backend at
/// all, and a gate that stopped it is visible as its own sentence — so this
/// buys the gate's own claims without a running teammate under them.
fn door(home: &std::path::Path) -> Teammates {
    use ganja_protocol::team::MemberBackend;

    Teammates::new(
        team_registry(home),
        // Every surface answers `Never`, and the two pane slots answer for the
        // surfaces they stand in for rather than for `in-process`: the map is
        // keyed by what a backend says it is, so a `Never(InProcess)` in the
        // `ganja` slot would key over the engine's own entry.
        Backends::new()
            .with_in_process(Arc::new(Never(MemberBackend::InProcess)))
            .with(Arc::new(Never(MemberBackend::Ganja)))
            .with(Arc::new(Never(MemberBackend::Claude)))
            .with(Arc::new(Never(MemberBackend::Codex)))
            .with(Arc::new(Never(MemberBackend::Agy)))
            .with(Arc::new(Never(MemberBackend::Grok))),
    )
}

/// **U-1, D538.** A surface this session assembled no implementation for is
/// refused *by name*, and never quietly served another.
///
/// The exhaustiveness a field-per-surface `Backends` used to buy is the one
/// thing this ruling traded away, so what replaces it is asserted rather than
/// described: the sentence the model reads names the backend it asked for, and
/// a `tracing::warn!` beside it names the backend and the session, because a
/// map missing a surface is a wiring fault in whoever assembled it rather than
/// something the model did.
///
/// And the refusal comes **before** the dialog: the asker here panics rather
/// than answering, so a spawn that asked a person to approve a teammate this
/// session cannot start fails the test instead of passing it.
#[tokio::test]
async fn a_backend_absent_from_the_map_is_refused_by_name() {
    let home = ganja_testkit::temp_dir();
    let capture = ganja_testkit::LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();

    // Scoped rather than global: this binary holds many tests, and a global
    // default would be installed once and read by all of them.
    let refused = tracing::subscriber::with_default(subscriber, || {
        futures::executor::block_on(async {
            let registry = team_registry(home.path());
            let door = Teammates::new(
                Arc::clone(&registry),
                Backends::new().with_in_process(Arc::new(Never(MemberBackend::InProcess))),
            );
            let mut wanted = wanted();
            wanted.backend = Some("codex".to_owned());

            door.start(wanted, &caller(Vec::new(), home.path(), home.path()), &NeverAsked)
                .await
                .expect_err("no codex backend was assembled")
        })
    });

    assert_eq!(
        refused.reason, "this session has no codex backend",
        "the refusal names the surface asked for and nothing else"
    );
    let logged = capture.logged();
    assert!(logged.contains("codex"), "the warn line names the backend: {logged}");
    assert!(
        logged.contains("did not assemble"),
        "and says the assembly is what is missing: {logged}"
    );
}

/// **U-2, D538.** The in-process entry is the engine's own, and an assembler
/// offering one is a mistake named at the offending call.
///
/// A panic rather than a silent overwrite because the two are not the same
/// value: the engine's is built out of that session's provider, tool set and
/// store, and an assembled one that quietly won would hand every teammate of
/// that session somebody else's conversation.
#[test]
#[should_panic(expected = "the in-process backend is the engine's own")]
fn backends_with_refuses_the_in_process_entry() {
    let _ = Backends::new().with(Arc::new(Never(MemberBackend::InProcess)));
}

/// A teammate that would work outside the lead's project is not routine,
/// and the person asked is shown **where** — which is the only thing that
/// makes the question answerable.
#[tokio::test]
async fn a_spawn_outside_the_project_is_asked_about_and_discloses_the_directory() {
    let home = ganja_testkit::temp_dir();
    let elsewhere = ganja_testkit::temp_dir();
    let asked = Asked::answering(PermissionReply::Once);

    let refused = door(home.path())
        .start(wanted(), &caller(Vec::new(), elsewhere.path(), home.path()), &asked)
        .await
        .expect_err("the backend under this door spawns nothing");

    assert!(refused.reason.contains(NEVER), "an approved spawn reaches the backend: {refused:?}");
    let seen = asked.seen();
    let ask = seen.first().expect("somebody was asked: {seen:?}");
    assert_eq!(
        ask.directories,
        vec![crate::permission::resolve(elsewhere.path())],
        "and shown where it would work: {ask:?}"
    );
    assert!(ask.title.contains("worker"), "the dialog names the teammate: {ask:?}");
    assert_eq!(
        ask.args.get("cwd").and_then(|cwd| cwd.as_str()),
        Some(elsewhere.path().to_string_lossy().as_ref())
    );
    assert!(
        !ask.args.to_string().contains("have a look at the parser"),
        "and a spawn prompt is not put on a dialog: {ask:?}"
    );
}

/// A rule that refuses is not a question. The spawn is refused in the
/// gate's own words and nobody is asked, because asking would be inviting
/// somebody to overturn an answer they already gave.
#[tokio::test]
async fn a_spawn_a_rule_denies_is_refused_without_anybody_being_asked() {
    let home = ganja_testkit::temp_dir();
    let elsewhere = ganja_testkit::temp_dir();
    let asked = Asked::answering(PermissionReply::Once);
    let denied = vec![Rule {
        permission: crate::permission::EXTERNAL_DIRECTORY.to_owned(),
        pattern: super::ANY.to_owned(),
        action: Action::Deny,
    }];

    let refused = door(home.path())
        .start(wanted(), &caller(denied, elsewhere.path(), home.path()), &asked)
        .await
        .expect_err("a denied spawn does not happen");

    assert!(
        refused.reason.contains("a rule refuses work in"),
        "the gate's own sentence reaches the model: {refused:?}"
    );
    assert!(!refused.reason.contains(NEVER), "and the backend was never reached: {refused:?}");
    assert!(asked.seen().is_empty(), "a deny raises no dialog: {:?}", asked.seen());
}

/// A person who says no is answered by not starting anything, in a sentence
/// that says which of the two refusals it was.
#[tokio::test]
async fn a_spawn_refused_at_the_dialog_starts_nothing() {
    let home = ganja_testkit::temp_dir();
    let elsewhere = ganja_testkit::temp_dir();
    let asked = Asked::answering(PermissionReply::Reject);

    let refused = door(home.path())
        .start(wanted(), &caller(Vec::new(), elsewhere.path(), home.path()), &asked)
        .await
        .expect_err("a refused spawn does not happen");

    assert_eq!(refused.reason, super::REFUSED_BY_HAND);
    assert_eq!(asked.seen().len(), 1, "and it was asked exactly once");
}

/// A [`Host`] whose calling turn works in `cwd`, judged against `root` —
/// the two values [`Spawn::caller`] hands the spawn gate, divergent here
/// so the gate asks. Everything else is the least the type will hold.
fn host_at(cwd: &std::path::Path, root: &std::path::Path, teammates: Arc<Teammates>) -> Arc<Host> {
    Arc::new(Host {
        provider: Arc::new(crate::provider::FakeProvider::new("on it", std::time::Duration::ZERO)),
        model: "recorder-model".to_owned(),
        small_model: None,
        agents: Arc::new(registry()),
        tools: Arc::new(crate::tool::Registry::new(Vec::new())),
        deferral: crate::tool::deferral::Deferral::none(),
        permissions: Arc::new(std::sync::Mutex::new(Permissions::default())),
        base_prompt: None,
        prompt_suffix: None,
        cwd: cwd.to_path_buf(),
        root: root.to_path_buf(),
        credentials: crate::tool::Credentials::Unguarded,
        lsp: None,
        persistence: None,
        jobs: None,
        hooks: None,
        concurrency: crate::config::AgentsConfig::DEFAULT_CONCURRENCY,
        teammates: Some(teammates),
        identity: Arc::new(identity::Identity::new(std::env::temp_dir())),
    })
}

/// A [`Spawn`] over `host` whose fanout this test reads: the value
/// `session.rs` builds per `task` call, built here so its own
/// [`SpawnAsker`] impl — the register→publish→select→terminal-reply dance
/// — is what these tests drive, not a stub's.
fn spawn_over(host: Arc<Host>) -> (Spawn, mpsc::Receiver<Event>) {
    let (events, received) = mpsc::channel(64);

    (
        Spawn {
            host,
            events: Arc::new(Fanout::new(events)),
            session_id: SessionId::from("ses_parent".to_owned()),
            pending: Arc::default(),
            message_id: MessageId::ascending(),
            part_id: PartId::ascending(),
            cancel: tokio_util::sync::CancellationToken::new(),
        },
        received,
    )
}

/// Drives one `task {name}` spawn through the real [`Spawn`] up to its
/// dialog: returns the join handle and the dialog's id, having asserted
/// the published request names the `task` tool and this call's own part.
async fn raised_dialog(
    spawn: &Spawn,
    received: &mut mpsc::Receiver<Event>,
) -> (tokio::task::JoinHandle<Result<Teammated, NotSpawned>>, crate::protocol::PermissionId) {
    let door = spawn.clone();
    let handle = tokio::spawn(async move { door.spawn_teammate(wanted()).await });

    let Some(Event::PermissionRequested { session_id, id, call_id, tool, .. }) =
        received.recv().await
    else {
        panic!("the gate's question crosses the calling turn's fanout");
    };
    assert_eq!(tool, crate::tool::task::ID, "the dialog names the tool");
    assert_eq!(
        call_id,
        spawn.part_id.as_str(),
        "and the part the call reports on, so a frontend can say which"
    );
    assert_eq!(session_id, spawn.session_id, "addressed to the caller's own session");

    (handle, id)
}

/// The engine-side half of the `task {name}` dialog (**D504**): the
/// request is answered **by its id** through the shared pending-reply
/// registry, a yes reaches the backend, and the terminal
/// [`Event::PermissionReplied`] retires the entry on the way out.
#[tokio::test]
async fn the_task_doors_dialog_is_answered_by_id_and_a_yes_reaches_the_backend() {
    let home = ganja_testkit::temp_dir();
    let elsewhere = ganja_testkit::temp_dir();
    let (spawn, mut received) =
        spawn_over(host_at(elsewhere.path(), home.path(), Arc::new(door(home.path()))));

    let (handle, id) = raised_dialog(&spawn, &mut received).await;
    assert!(
        spawn.pending.lock().expect("no panic").answer_permission(&id, PermissionReply::Once),
        "the reply routes by the id the request carried"
    );

    let refused = handle
        .await
        .expect("the door settles")
        .expect_err("the backend under this door spawns nothing");
    assert!(refused.reason.contains(NEVER), "an approved spawn reaches the backend: {refused:?}");
    let Some(Event::PermissionReplied { id: replied, reply, .. }) = received.recv().await else {
        panic!("the wait ends in the terminal reply every other permission wait sends");
    };
    assert_eq!(replied, id);
    assert_eq!(reply, PermissionReply::Once);
    assert!(
        !spawn.pending.lock().expect("no panic").answer_permission(&id, PermissionReply::Once),
        "and the entry is closed behind it"
    );
}

/// A person's no through the engine-side dialog reads as the same
/// [`REFUSED_BY_HAND`](super::REFUSED_BY_HAND) sentence the seam-level
/// door refuses in — one refusal, whichever layer asked.
#[tokio::test]
async fn a_rejected_task_door_dialog_reads_refused_by_hand() {
    let home = ganja_testkit::temp_dir();
    let elsewhere = ganja_testkit::temp_dir();
    let (spawn, mut received) =
        spawn_over(host_at(elsewhere.path(), home.path(), Arc::new(door(home.path()))));

    let (handle, id) = raised_dialog(&spawn, &mut received).await;
    assert!(
        spawn.pending.lock().expect("no panic").answer_permission(&id, PermissionReply::Reject)
    );

    let refused =
        handle.await.expect("the door settles").expect_err("a refused spawn does not happen");
    assert_eq!(refused.reason, super::REFUSED_BY_HAND);
    let Some(Event::PermissionReplied { reply, .. }) = received.recv().await else {
        panic!("a no is still answered terminally");
    };
    assert_eq!(reply, PermissionReply::Reject);
}

/// A cancelled turn does not strand its spawn dialog: nothing else closes
/// this entry — the registry is an `Arc` that outlives the turn — so the
/// cancel arm itself must retire it, answer terminally, and read as a
/// refusal.
#[tokio::test]
async fn a_cancelled_turn_closes_the_task_doors_open_dialog() {
    let home = ganja_testkit::temp_dir();
    let elsewhere = ganja_testkit::temp_dir();
    let (spawn, mut received) =
        spawn_over(host_at(elsewhere.path(), home.path(), Arc::new(door(home.path()))));

    let (handle, id) = raised_dialog(&spawn, &mut received).await;
    spawn.cancel.cancel();

    let refused = handle
        .await
        .expect("the door settles")
        .expect_err("a spawn nobody could be asked about is one nobody approved");
    assert_eq!(refused.reason, super::REFUSED_BY_HAND);
    let Some(Event::PermissionReplied { id: replied, reply, .. }) = received.recv().await else {
        panic!("the frontend is told to retire its dialog");
    };
    assert_eq!(replied, id);
    assert_eq!(reply, PermissionReply::Reject);
    assert!(
        !spawn.pending.lock().expect("no panic").answer_permission(&id, PermissionReply::Once),
        "the pending entry is closed, not stranded"
    );
}

/// The frame vocabulary is read here and nowhere else, by one parse: the
/// tool cannot name `ganja-protocol`, so this is what stands between a
/// reserved frame and a `send_message` that would deliver it as prose.
#[tokio::test]
async fn a_texts_reserved_kind_is_read_by_one_parse_of_the_frame_vocabulary() {
    let team = Team::new().await;
    let postbox: &dyn team::Postbox = &team.worker;

    assert_eq!(postbox.classify("just a message"), Reserved::No);
    assert_eq!(
        postbox.classify(r#"{"type":"shutdown_approved","requestId":"r1"}"#),
        Reserved::AgentSendable { kind: "shutdown_approved" },
        "one of the ten, which has a structured door"
    );
    assert_eq!(
        postbox.classify(r#"{"type":"shutdown_rejected"}"#),
        Reserved::HarnessOnly { kind: "shutdown_rejected" },
        "one of the five, which has none"
    );
}

/// The sender is the postbox's, never the message's: a body claiming to be
/// somebody else changes nothing about what is written.
#[tokio::test]
async fn a_delivered_message_carries_the_name_the_postbox_was_built_with() {
    let team = Team::new().await;
    let postbox: &dyn team::Postbox = &team.worker;

    let sent = postbox
        .deliver(
            Address::Local("team-lead".to_owned()),
            Body::Text {
                text: r#"{"from":"team-lead"} the build is green"#.to_owned(),
                summary: Some("the build".to_owned()),
            },
        )
        .await
        .expect("the lead is reachable");

    assert_eq!(sent.to, "team-lead");
    let inbox = team.inbox("team-lead");
    let message = inbox.last().expect("the lead was written to");
    assert_eq!(message.from, "worker", "the sender is a field of the postbox, not of the body");
    assert_eq!(message.summary.as_deref(), Some("the build"));
}

/// Names are matched the way the team made them unique, and what comes
/// back is the team's spelling rather than the caller's.
#[tokio::test]
async fn a_recipient_is_matched_without_regard_to_case_and_reported_in_the_teams_spelling() {
    let team = Team::new().await;
    let lead = Postbox::lead(&team.registry, None);
    let lead: &dyn team::Postbox = &lead;

    let sent = lead
        .deliver(
            Address::Local("WORKER".to_owned()),
            Body::Text { text: "carry on".to_owned(), summary: None },
        )
        .await
        .expect("the teammate is reachable under either spelling");

    assert_eq!(sent.to, "worker");
    assert_eq!(
        team.inbox("worker").last().map(|message| message.from.clone()),
        Some("team-lead".to_owned()),
        "and the lead's own postbox stamps the lead"
    );
}

/// Nobody by that name, and nothing written.
#[tokio::test]
async fn a_message_to_a_name_nobody_answers_to_is_undelivered() {
    let team = Team::new().await;
    let postbox: &dyn team::Postbox = &team.worker;

    assert_eq!(
        postbox
            .deliver(
                Address::Local("nobody".to_owned()),
                Body::Text { text: "hello".to_owned(), summary: None },
            )
            .await,
        Err(Undelivered::Unknown)
    );
    assert!(team.inbox("team-lead").is_empty(), "and no inbox grew an entry");
}

/// The socket arm carries prose and nothing else (§5.2-6): a frame is
/// refused before any client is built, so no connection is even tried —
/// the socket named here does not exist, and the refusal must not be
/// about that.
#[tokio::test]
async fn a_frame_addressed_to_a_socket_is_refused_before_anything_is_sent() {
    let team = Team::new().await;
    let postbox: &dyn team::Postbox = &team.worker;

    let refused = postbox
        .deliver(
            Address::Uds { path: "/nonexistent/ganja.sock".into() },
            Body::Frame(serde_json::json!({"type": "idle_notification"})),
        )
        .await;

    assert_eq!(
        refused,
        Err(Undelivered::Failed { reason: FRAME_OVER_SOCKET.to_owned() }),
        "the frame is refused as a rule, not as a dead socket"
    );
}

/// **D505, the D498 premise held at the last gate**: the deliver arm
/// judges the address as the tool's rung 3 does — a session socket of
/// ours, or refused by the clause it fails — before any client is built,
/// so a caller reaching the trait without the tool in front of it is
/// still kept off every other listener on this machine.
#[tokio::test]
async fn an_address_that_is_not_a_session_socket_of_ours_is_refused_before_anything_is_sent() {
    let team = Team::new().await;
    let postbox: &dyn team::Postbox = &team.worker;

    for to in ["/var/run/docker.sock", "/tmp/tmux-501/default", "/nonexistent-ganja/0198c1a2.sock"]
    {
        let refused = postbox
            .deliver(
                Address::Uds { path: to.into() },
                Body::Text { text: "anyone".to_owned(), summary: None },
            )
            .await;
        let Err(Undelivered::Failed { reason }) = refused else {
            panic!("{to}: refused as not a session socket, not {refused:?}");
        };
        assert!(
            reason.starts_with(NOT_A_SESSION_SOCKET) && reason.contains(to),
            "{to}: the sentence names the rule and the socket: {reason}"
        );
    }
}

/// A session socket nothing listens at any more — bound once and dropped,
/// its file left behind — passes the address gate and is a typed failure
/// that names the socket and the OS's reason, under the deadline: never a
/// hang, never a panic.
#[tokio::test]
async fn a_dead_socket_is_a_typed_failure_naming_the_socket() {
    let team = Team::new().await;
    let postbox: &dyn team::Postbox = &team.worker;
    let path = team.session_socket_path("0198c1a2");
    drop(std::os::unix::net::UnixListener::bind(&path).expect("a socket binds"));
    assert!(path.exists(), "the file outlives its listener");

    let failed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        postbox.deliver(
            Address::Uds { path: path.clone() },
            Body::Text { text: "anyone there".to_owned(), summary: None },
        ),
    )
    .await
    .expect("a dead socket answers within the deadline");

    let Err(Undelivered::Failed { reason }) = failed else {
        panic!("a dead socket is a failure, not {failed:?}");
    };
    assert!(
        reason.starts_with(SOCKET_UNREACHABLE),
        "the sentence says the session may be gone: {reason}"
    );
    assert!(reason.contains(&path.display().to_string()), "and names the socket: {reason}");
    assert!(team.inbox("team-lead").is_empty(), "and nothing local was written");
}

/// **Contract-level**, not end to end: the far end here is a hand-rolled
/// responder that answers the two routes with the bytes `ganja-serve`'s
/// socket router puts on the wire, so what this pins is *this* side —
/// which routes it drives, in which order, and what it stamps the message
/// with. The real server end is pinned in `ganja-serve/tests/team.rs`,
/// and the two processes together in `ganja-cli/tests/uds.rs` (AC-9).
#[tokio::test]
async fn a_socket_delivery_asks_who_leads_and_posts_to_them_stamped_with_its_identity() {
    let team = Team::new().await;
    let postbox: &dyn team::Postbox = &team.worker;
    let path = team.session_socket_path("0198c1a2");
    let peer = PeerStub::listen(&path).await;

    let sent = postbox
        .deliver(
            Address::Uds { path: path.clone() },
            Body::Text {
                text: "the release is out".to_owned(),
                summary: Some("release".to_owned()),
            },
        )
        .await
        .expect("a listening peer takes the message");

    assert_eq!(
        sent,
        Sent {
            to: "team-lead@session-feedbeef".to_owned(),
            note: "It is in that inbox and will be read on the next pass.".to_owned(),
        },
        "the far side's answer is reported in the far side's terms"
    );

    let requests = peer.requests();
    assert_eq!(
        requests.iter().map(|(method, route, _)| format!("{method} {route}")).collect::<Vec<_>>(),
        vec!["GET /team", "POST /team/team-lead/message"],
        "who leads is asked before anything is posted"
    );
    let posted: SocketMessage =
        serde_json::from_str(&requests[1].2).expect("the body is the wire shape");
    assert_eq!(
        posted.from, "worker@session-abcd1234",
        "stamped with the sender's derived identity, never a bare name"
    );
    assert_eq!(posted.text, "the release is out");
    assert_eq!(posted.summary, Some("release".to_owned()));
    assert!(
        posted.message_id.is_some(),
        "a fresh id is minted for every send (D532), bound or not"
    );
    assert_eq!(
        (posted.from_mode, posted.hop_chain, posted.reply_to),
        (None, Vec::new(), None),
        "this postbox was never given a `PeerFacts` beyond the `Unbound` default: no \
         class asserted, nothing to forward, nowhere to reply"
    );
}

/// A peer that answers with a refusal is reported in the peer's own
/// sentence, so the model reads why rather than a status code.
#[tokio::test]
async fn a_peers_refusal_reaches_the_sender_in_the_peers_words() {
    let team = Team::new().await;
    let postbox: &dyn team::Postbox = &team.worker;
    let path = team.session_socket_path("0198c1a2");
    let _peer = PeerStub::listen_refusing(&path, 404, "This session leads no team").await;

    let failed = postbox
        .deliver(
            Address::Uds { path: path.clone() },
            Body::Text { text: "hello".to_owned(), summary: None },
        )
        .await;

    let Err(Undelivered::Failed { reason }) = failed else {
        panic!("a refusal is a failure, not {failed:?}");
    };
    assert!(reason.starts_with(SOCKET_REFUSED), "{reason}");
    assert!(reason.contains("(404)"), "the status is there: {reason}");
    assert!(reason.contains("This session leads no team"), "and the peer's own sentence: {reason}");
}

/// What a peer answers is read under a cap and refused past
/// it — declared oversize before a byte, undeclared oversize the moment
/// the cap is passed — so a listener that is not a session cannot hand
/// this side an unbounded body to hold. The refusal names the socket, and
/// nothing is posted after it.
#[tokio::test]
async fn an_oversized_answer_is_refused_unread_and_nothing_is_posted() {
    let team = Team::new().await;
    let postbox: &dyn team::Postbox = &team.worker;
    let path = team.session_socket_path("0198c1a2");
    let peer = PeerStub::serve(&path, |_, _| (200, "x".repeat(super::SOCKET_BODY_CAP + 1))).await;

    let failed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        postbox.deliver(
            Address::Uds { path: path.clone() },
            Body::Text { text: "hello".to_owned(), summary: None },
        ),
    )
    .await
    .expect("an oversized answer is refused within the deadline");

    let Err(Undelivered::Failed { reason }) = failed else {
        panic!("an oversized answer is a failure, not {failed:?}");
    };
    assert!(reason.starts_with(SOCKET_OVERSIZED), "{reason}");
    assert!(reason.contains(&path.display().to_string()), "{reason}");
    assert_eq!(
        peer.requests()
            .iter()
            .map(|(method, route, _)| format!("{method} {route}"))
            .collect::<Vec<_>>(),
        vec!["GET /team"],
        "the roster read was refused, and nothing was posted after it"
    );
}

/// A peer's *words* — its refusal's sentence here — are cut to a line
/// before the model reads them: the body cap bounds what is held, this
/// bounds what is repeated.
#[tokio::test]
async fn a_peers_sentence_is_cut_to_a_line_before_the_model_reads_it() {
    let team = Team::new().await;
    let postbox: &dyn team::Postbox = &team.worker;
    let path = team.session_socket_path("0198c1a2");
    let long = "no ".repeat(2_000);
    let _peer = PeerStub::listen_refusing(&path, 404, &long).await;

    let failed = postbox
        .deliver(
            Address::Uds { path: path.clone() },
            Body::Text { text: "hello".to_owned(), summary: None },
        )
        .await;

    let Err(Undelivered::Failed { reason }) = failed else {
        panic!("a refusal is a failure, not {failed:?}");
    };
    assert!(reason.starts_with(SOCKET_REFUSED), "{reason}");
    assert!(
        reason.chars().count() < long.chars().count(),
        "the peer's sentence was cut: {} chars",
        reason.chars().count()
    );
    assert!(reason.contains(&"no ".repeat(100)), "and what is left is the head of it: {reason}");
}

/// The lead's name a peer answers goes into a URL, so it is held
/// to the member-name grammar first — a listener in a session socket's
/// shape that names its lead `../../global/health` (traversal), one with
/// a `?` (a query), one with a `#` (a fragment, which reaches a different
/// path than traversal does), or one longer than any name is refused,
/// and no POST is formed at all: the stub records what arrived, and what
/// arrived is the one GET. The refusal repeats the peer's word cut to a
/// line, so the over-long name pins the length clause both ways.
#[tokio::test]
async fn a_lead_name_the_grammar_refuses_forms_no_post() {
    let too_long = "a".repeat(ganja_protocol::team::DISPLAY_FIELD_CAP + 1);
    for hostile in ["../../global/health", "team-lead?x=1", "team-lead#f", too_long.as_str()] {
        let team = Team::new().await;
        let postbox: &dyn team::Postbox = &team.worker;
        let path = team.session_socket_path("0198c1a2");
        let named = hostile.to_owned();
        let peer = PeerStub::serve(&path, move |method, route| {
            if method == "GET" && route == "/team" {
                (
                    200,
                    serde_json::json!({
                        "team": "session-feedbeef",
                        "lead": named,
                        "members": [],
                    })
                    .to_string(),
                )
            } else {
                (200, serde_json::json!({"to": "x", "note": "must not be reached"}).to_string())
            }
        })
        .await;

        let failed = postbox
            .deliver(
                Address::Uds { path: path.clone() },
                Body::Text { text: "hello".to_owned(), summary: None },
            )
            .await;
        let Err(Undelivered::Failed { reason }) = failed else {
            panic!("{hostile:?}: a refused lead name is a failure, not {failed:?}");
        };
        assert!(
            reason.starts_with(SOCKET_LEAD_UNNAMED),
            "{hostile:?}: named as the rule: {reason}"
        );
        assert!(
            reason.chars().count() < hostile.chars().count() + 400,
            "{hostile:?}: the peer's word is cut to a line, never repeated whole: {} chars",
            reason.chars().count()
        );
        let requests = peer.requests();
        assert_eq!(
            requests
                .iter()
                .map(|(method, route, _)| format!("{method} {route}"))
                .collect::<Vec<_>>(),
            vec!["GET /team"],
            "{hostile:?}: no POST was formed"
        );
    }
}

/// The receiving end: a peer's message to the lead lands in the lead's
/// inbox stamped with the peer's derived identity — the one recipient the
/// lead's own postbox could never reach, and the whole reason the socket
/// exists.
#[tokio::test]
async fn a_received_peer_message_reaches_the_lead_stamped_as_a_peer() {
    let team = Team::new().await;

    let sent = receive(
        &team.registry,
        Incoming {
            from: "team-lead@session-feedbeef".to_owned(),
            to: "team-lead".to_owned(),
            text: "how far along is W7".to_owned(),
            summary: Some("W7".to_owned()),
        },
    )
    .await
    .expect("a peer reaches the lead");
    assert_eq!(sent.to, "team-lead");

    let inbox = team.inbox("team-lead");
    assert_eq!(inbox.len(), 1, "one message landed: {inbox:?}");
    assert_eq!(inbox[0].from, "team-lead@session-feedbeef");
    assert_eq!(inbox[0].text, "how far along is W7");
    assert_eq!(inbox[0].summary.as_deref(), Some("W7"));

    // The lead is matched as every name here is — without regard to
    // case — and a member is *not* reachable this way: the socket
    // delivers to the session, which is its lead, and the refusal names
    // the lead so the peer knows where to write.
    assert!(
        receive(
            &team.registry,
            Incoming {
                from: "team-lead@session-feedbeef".to_owned(),
                to: "Team-Lead".to_owned(),
                text: "again".to_owned(),
                summary: None,
            },
        )
        .await
        .is_ok()
    );
    let seeded = team.inbox("worker");
    assert_eq!(
        receive(
            &team.registry,
            Incoming {
                from: "team-lead@session-feedbeef".to_owned(),
                to: "worker".to_owned(),
                text: "and you".to_owned(),
                summary: None,
            },
        )
        .await,
        Err(NotReceived::NotTheLead { name: "worker".to_owned(), lead: "team-lead".to_owned() })
    );
    assert_eq!(team.inbox("worker"), seeded, "nothing reached the member");
}

/// What a peer says about itself is bounded before it lands
/// in the lead's prompt — an identity with a control character or past
/// the display cap is refused (never truncated: two peers must not read
/// as one), and a summary past it is cut as it is at every other seam.
#[tokio::test]
async fn a_peers_identity_is_plain_and_bounded_and_its_summary_is_capped() {
    let team = Team::new().await;
    let peer = |from: &str, summary: Option<&str>| Incoming {
        from: from.to_owned(),
        to: "team-lead".to_owned(),
        text: "hello".to_owned(),
        summary: summary.map(str::to_owned),
    };

    for bad in [
        "team-lead@session-\x1b[31mred",
        "team-lead@session-\nfeedbeef",
        &format!("team-lead@{}", "s".repeat(300)),
    ] {
        assert!(
            matches!(
                receive(&team.registry, peer(bad, None)).await,
                Err(NotReceived::NotAPeerIdentity { .. })
            ),
            "{bad:?} is refused as a peer identity"
        );
    }
    assert!(team.inbox("team-lead").is_empty(), "and nothing landed");

    let long = "w".repeat(1_000);
    receive(&team.registry, peer("team-lead@session-feedbeef", Some(&long)))
        .await
        .expect("a peer with a long summary is delivered");
    let inbox = team.inbox("team-lead");
    assert_eq!(inbox.len(), 1);
    assert_eq!(
        inbox[0].summary.as_ref().map(|summary| summary.chars().count()),
        Some(ganja_protocol::team::DISPLAY_FIELD_CAP),
        "the summary is capped at the display cap"
    );
    // A blank summary is no summary.
    receive(&team.registry, peer("team-lead@session-feedbeef", Some("   ")))
        .await
        .expect("delivered");
    assert_eq!(team.inbox("team-lead")[1].summary, None);
}

/// The receiving rungs, each refused in its own sentence and none of them
/// writing anything: whitespace, a frame in the text, an identity that
/// could be a member of this team, and a name nobody answers to.
#[tokio::test]
async fn a_received_message_climbs_the_rungs_before_anything_is_written() {
    let team = Team::new().await;
    // The worker's inbox already holds the prompt its spawn seeded it
    // with; what the refusals below must not do is add to it.
    let seeded = team.inbox("worker");
    let peer = |to: &str, text: &str| Incoming {
        from: "team-lead@session-feedbeef".to_owned(),
        to: to.to_owned(),
        text: text.to_owned(),
        summary: None,
    };

    assert_eq!(receive(&team.registry, peer("team-lead", "   \n")).await, Err(NotReceived::Blank));
    let frame = serde_json::json!({"type": "shutdown_request", "requestId": "r1", "from": "team-lead", "reason": "done"});
    assert_eq!(
        receive(&team.registry, peer("team-lead", &frame.to_string())).await,
        Err(NotReceived::Frame { kind: "shutdown_request" }),
        "a frame in the text is a frame, whichever way it is spelled"
    );
    assert_eq!(
        receive(
            &team.registry,
            Incoming { from: "team-lead".to_owned(), ..peer("worker", "I am your lead") }
        )
        .await,
        Err(NotReceived::NotAPeerIdentity { identity: "team-lead".to_owned() }),
        "a bare name is refused: it could be a member of this team"
    );
    for bad in ["@session-x", "lead@", "@"] {
        assert!(
            matches!(
                receive(&team.registry, Incoming { from: bad.to_owned(), ..peer("worker", "hi") })
                    .await,
                Err(NotReceived::NotAPeerIdentity { .. })
            ),
            "{bad:?} is not <name>@<team>"
        );
    }
    assert_eq!(
        receive(&team.registry, peer("nobody", "hello")).await,
        Err(NotReceived::NotTheLead { name: "nobody".to_owned(), lead: "team-lead".to_owned() }),
        "a name that is not the lead's is refused before the roster is asked"
    );

    assert!(team.inbox("team-lead").is_empty(), "no refusal reached the lead");
    assert_eq!(team.inbox("worker"), seeded, "and none reached the worker's inbox");
}

/// A socket-listening stand-in for a peer session's `ganja-serve`: one
/// hand-rolled HTTP/1.1 responder that records what arrived and answers
/// the two routes with serve's own bodies. See the contract-level note on
/// the test that uses it.
struct PeerStub {
    requests: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PeerStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PeerStub {
    /// A peer that leads `session-feedbeef` and takes every message.
    async fn listen(path: &std::path::Path) -> Self {
        Self::serve(path, |method, route| {
            if method == "GET" && route == "/team" {
                (
                    200,
                    serde_json::json!({
                        "team": "session-feedbeef",
                        "lead": "team-lead",
                        "members": [{
                            "name": "team-lead",
                            "agent_id": "team-lead@session-feedbeef",
                            "backend": "in-process",
                            "is_lead": true,
                        }],
                    })
                    .to_string(),
                )
            } else {
                (
                    200,
                    serde_json::json!({
                        "to": "team-lead",
                        "note": "It is in that inbox and will be read on the next pass.",
                    })
                    .to_string(),
                )
            }
        })
        .await
    }

    /// A peer that refuses everything with `status` and `message`, in
    /// serve's own refusal envelope.
    async fn listen_refusing(path: &std::path::Path, status: u16, message: &str) -> Self {
        let message = message.to_owned();
        Self::serve(path, move |_, _| {
            (status, serde_json::json!({"type": "not_found", "message": message}).to_string())
        })
        .await
    }

    async fn serve(
        path: &std::path::Path,
        answer: impl Fn(&str, &str) -> (u16, String) + Send + Sync + 'static,
    ) -> Self {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::UnixListener::bind(path).expect("a socket binds");
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = Vec::new();
                // Read to the end of the head, then the declared body.
                let head = loop {
                    let mut chunk = [0u8; 1024];
                    let Ok(read) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end;
                    }
                };
                let text = String::from_utf8_lossy(&buffer[..head]).into_owned();
                let mut lines = text.lines();
                let mut request = lines.next().unwrap_or_default().split_whitespace();
                let method = request.next().unwrap_or_default().to_owned();
                let route = request.next().unwrap_or_default().to_owned();
                let length = lines
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = buffer[head + 4..].to_vec();
                while body.len() < length {
                    let mut chunk = [0u8; 1024];
                    let Ok(read) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..read]);
                }
                let body = String::from_utf8_lossy(&body).into_owned();
                let (status, answer) = answer(&method, &route);
                seen.lock().expect("the request log is never poisoned").push((method, route, body));
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        Self { requests, task }
    }

    fn requests(&self) -> Vec<(String, String, String)> {
        self.requests.lock().expect("the request log is never poisoned").clone()
    }
}

/// A caller is not in its own roster, and exactly one row leads — the
/// invariant `send_message`'s last rung reads the lead's name out of.
#[tokio::test]
async fn a_caller_is_not_in_its_own_roster_and_exactly_one_row_leads() {
    let team = Team::new().await;

    let seen = team::Postbox::roster(&team.worker);
    assert_eq!(
        seen.iter().map(|peer| peer.name.as_str()).collect::<Vec<_>>(),
        vec!["team-lead"],
        "a teammate sees the lead and not itself: {seen:?}"
    );
    assert_eq!(seen.iter().filter(|peer| peer.lead).count(), 1);

    let seen = team::Postbox::roster(&Postbox::lead(&team.registry, None));
    assert_eq!(
        seen.iter().map(|peer| peer.name.as_str()).collect::<Vec<_>>(),
        vec!["worker"],
        "and the lead sees the teammate and not itself: {seen:?}"
    );
    assert_eq!(
        seen.iter().filter(|peer| peer.lead).count(),
        0,
        "so a roster carries at most one lead, and this one carries none"
    );
}

/// **A postbox does not keep the team it speaks for alive.**
///
/// The cycle it would otherwise close is the whole point: the registry
/// holds every teammate, a teammate holds its engine, and that engine
/// holds the postbox installed into it — so a strong handle back to the
/// registry means no teammate's engine is ever dropped, shut down or not,
/// and the leak is the entire team rather than a stray `Arc`.
///
/// What makes this a pin rather than a hope: the roster below can only be
/// empty if the last strong handle really went with the `drop`. Held
/// strongly, the upgrade would still succeed and the lead would still be
/// listed. The two answers are the ones a caller is owed — nobody to
/// address, and a send that says the team has ended rather than blaming
/// the name it was given.
#[tokio::test]
async fn a_postbox_outliving_its_team_answers_that_the_team_has_gone() {
    let Team { _home, root: _root, team: _team, registry, worker } = Team::new().await;

    // Non-vacuous: there is a team to lose, and this postbox can see it.
    assert!(
        !team::Postbox::roster(&worker).is_empty(),
        "the fixture's postbox speaks for a team that exists"
    );

    registry.shutdown().await;
    drop(registry);

    assert!(
        team::Postbox::roster(&worker).is_empty(),
        "a postbox that outlived its team has nobody to address"
    );
    assert_eq!(
        team::Postbox::deliver(
            &worker,
            Address::Local("team-lead".to_owned()),
            Body::Text { text: "anyone there?".to_owned(), summary: None },
        )
        .await,
        Err(Undelivered::Failed { reason: TEAM_GONE.to_owned() }),
        "and says so, rather than reporting a name nobody answers to"
    );
}

/// A session id whose compact hex begins with `stem`, the identity
/// module's own test shape (`teammate::identity::tests::id_for`),
/// reimplemented here because that helper is private to its own module.
fn id_for(stem: &str) -> String {
    let rest = "0".repeat(32 - stem.len());
    let hex = format!("{stem}{rest}");

    format!("{}-{}-7{}-8{}-{}", &hex[..8], &hex[8..12], &hex[13..16], &hex[17..20], &hex[20..32])
}

/// A tempdir at the `0700` mode [`crate::tool::socket::vet_address`]
/// requires of a session socket's directory — `Team::session_socket_path`'s
/// own fix, needed here too because [`ganja_testkit::temp_dir`] does not
/// promise that mode and a resolved-name delivery really connects.
fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = ganja_testkit::temp_dir();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("the fixture directory is chmod-able");

    dir
}

/// Registers `name` at `stem` under `directory` and holds the flock that
/// marks it live (**D527**'s liveness token) — the returned guard must
/// outlive every assertion that depends on the record reading live.
fn live_record(directory: &std::path::Path, stem: &str, name: &str) -> (String, std::fs::File) {
    let session_id = id_for(stem);
    ganja_tool::registry::write(
        directory,
        stem,
        &ganja_tool::registry::Record {
            format: ganja_tool::registry::FORMAT,
            session_id: session_id.clone(),
            name: name.to_owned(),
            name_source: ganja_tool::registry::NameSource::User,
            cwd: directory.to_path_buf(),
            root: directory.to_path_buf(),
            pid: 4242,
            started_at: 1_756_150_000_000,
        },
    )
    .expect("a record writes");
    let held = ganja_tool::socket::open_lock(&directory.join(format!("{stem}.sock")))
        .expect("the lock file opens");
    held.try_lock().expect("nothing else holds a fresh lock");

    (session_id, held)
}

/// **AC-11 / AC-12 / AC-41** (D528, D530/F1): the lead postbox's
/// roster-miss consults the identity index — a unique live session's
/// `Sent.to` composes all three identities a transcript needs to audit
/// it (the name asked, the resolved socket, the far side's reflected
/// answer), a name that later resolves to a different session halts as
/// `NameMoved` without re-pinning, and the new claimant is still
/// reachable — and the pin still untouched — by its own `uds:` address.
#[tokio::test]
async fn a_resolved_name_delivers_moves_and_still_answers_by_address() {
    let team = Team::new().await;
    let registry_dir = private_dir();
    let identity = Arc::new(identity::Identity::new(registry_dir.path()));
    let own_session = Arc::new(std::sync::Mutex::new(SessionId::from("ses-lead-own".to_owned())));
    let lead = Postbox::lead(&team.registry, Some((&identity, Arc::clone(&own_session))));

    let (_id_a, held_a) = live_record(registry_dir.path(), "01110001", "backend");
    let socket_a = registry_dir.path().join("01110001.sock");
    let _peer_a = PeerStub::listen(&socket_a).await;

    let sent = team::Postbox::deliver(
        &lead,
        Address::Local("backend".to_owned()),
        Body::Text { text: "hi".to_owned(), summary: None },
    )
    .await
    .expect("a unique live session resolves and delivers");
    assert_eq!(
        sent.to,
        format!("backend (uds:{} \u{2192} team-lead@session-feedbeef)", socket_a.display()),
        "the transcript can audit all three identities (N6)"
    );
    assert_eq!(identity.pinned("backend").expect("the pin stands").stem, "01110001");

    // A different session claims the name.
    drop(held_a);
    std::fs::remove_file(registry_dir.path().join("01110001.json")).expect("the stale record goes");
    let (_id_b, _held_b) = live_record(registry_dir.path(), "02220002", "backend");
    let socket_b = registry_dir.path().join("02220002.sock");
    let _peer_b = PeerStub::listen(&socket_b).await;

    let moved = team::Postbox::deliver(
        &lead,
        Address::Local("backend".to_owned()),
        Body::Text { text: "hi again".to_owned(), summary: None },
    )
    .await;
    assert!(matches!(moved, Err(Undelivered::NameMoved { .. })), "got {moved:?}");
    assert_eq!(
        identity.pinned("backend").expect("the pin still stands").stem,
        "01110001",
        "a halted resolution never re-pins"
    );

    // The new claimant is still reachable by its own address, and an
    // address neither consults nor creates a pin.
    let by_address = team::Postbox::deliver(
        &lead,
        Address::Uds { path: socket_b.clone() },
        Body::Text { text: "direct".to_owned(), summary: None },
    )
    .await
    .expect("a uds: address bypasses the name and its pin entirely");
    assert_eq!(by_address.to, "team-lead@session-feedbeef");
    assert_eq!(
        identity.pinned("backend").expect("still stands").stem,
        "01110001",
        "a uds: send neither consults nor creates a pin"
    );
}

/// **AC-10**'s deliver half: a roster member and a live registry record
/// can share a name, and the roster wins — the mailbox write lands,
/// nothing is resolved or pinned, and the shadowed session stays
/// reachable at its own `uds:` address. [`Team::new`] always spawns a
/// teammate named `worker`; the record below claims that same spelling
/// for a session the roster never heard of.
#[tokio::test]
async fn a_roster_hit_outranks_a_same_named_live_record() {
    let team = Team::new().await;
    let registry_dir = private_dir();
    let identity = Arc::new(identity::Identity::new(registry_dir.path()));
    let own_session = Arc::new(std::sync::Mutex::new(SessionId::from("ses-lead-own".to_owned())));
    let lead = Postbox::lead(&team.registry, Some((&identity, Arc::clone(&own_session))));

    // Nothing listens at this socket — the roster-hit arm must never
    // reach for it, so a wrong reorder would fail this delivery outright
    // rather than merely go unproven.
    let (_id, _held) = live_record(registry_dir.path(), "07770007", "worker");

    let sent = team::Postbox::deliver(
        &lead,
        Address::Local("worker".to_owned()),
        Body::Text { text: "for the roster member".to_owned(), summary: None },
    )
    .await
    .expect("the roster answers before any socket is touched");
    assert_eq!(sent.to, "worker");
    assert_eq!(
        identity.pinned("worker"),
        None,
        "a roster hit never consults the resolver, so it never pins"
    );
    let inbox = team.inbox("worker");
    assert_eq!(
        inbox.last().map(|message| message.text.as_str()),
        Some("for the roster member"),
        "the mailbox write landed, not a socket delivery"
    );

    // The shadowed session is still reachable — by the one spelling
    // that was never ambiguous: its own address.
    let socket = registry_dir.path().join("07770007.sock");
    let _peer = PeerStub::listen(&socket).await;
    let by_address = team::Postbox::deliver(
        &lead,
        Address::Uds { path: socket.clone() },
        Body::Text { text: "for the shadowed session".to_owned(), summary: None },
    )
    .await
    .expect("a uds: address reaches the record the roster shadowed");
    assert_eq!(by_address.to, "team-lead@session-feedbeef");
}

/// **AC-13 / AC-14 / AC-15**: ambiguity, a miss and an unreadable
/// registry all refuse rather than guess, and nothing is pinned by
/// either refusal.
#[tokio::test]
async fn a_lead_postbox_refuses_ambiguity_a_miss_and_an_unreadable_registry() {
    let team = Team::new().await;
    let registry_dir = private_dir();
    let identity = Arc::new(identity::Identity::new(registry_dir.path()));
    let own_session = Arc::new(std::sync::Mutex::new(SessionId::from("ses-lead-own".to_owned())));
    let lead = Postbox::lead(&team.registry, Some((&identity, Arc::clone(&own_session))));

    // A miss: nothing this session may address goes by that name.
    let missed = team::Postbox::deliver(
        &lead,
        Address::Local("nobody".to_owned()),
        Body::Text { text: "hi".to_owned(), summary: None },
    )
    .await;
    assert_eq!(missed, Err(Undelivered::Unknown));

    // Two live sessions share a name — a name never held by any roster
    // member, so the roster-hit arm above cannot answer first.
    let (_id1, _held1) = live_record(registry_dir.path(), "03330003", "gateway");
    let (_id2, _held2) = live_record(registry_dir.path(), "04440004", "gateway");
    let ambiguous = team::Postbox::deliver(
        &lead,
        Address::Local("gateway".to_owned()),
        Body::Text { text: "hi".to_owned(), summary: None },
    )
    .await;
    assert!(matches!(ambiguous, Err(Undelivered::Ambiguous { .. })), "got {ambiguous:?}");
    assert_eq!(identity.pinned("gateway"), None, "refusing pins nothing");

    // An unreadable registry: a failure to search, never a verdict.
    let missing = Arc::new(identity::Identity::new(registry_dir.path().join("was-never-there")));
    let lead_missing = Postbox::lead(&team.registry, Some((&missing, own_session)));
    let failed = team::Postbox::deliver(
        &lead_missing,
        Address::Local("gateway".to_owned()),
        Body::Text { text: "hi".to_owned(), summary: None },
    )
    .await;
    assert!(matches!(failed, Err(Undelivered::Failed { .. })), "got {failed:?}");
}

/// The D528 table's frame-body row: a structured body to a resolved name
/// is refused via the socket arm's own frame guard, and — because a
/// frame body is never accepted — pins nothing at all: resolution
/// answered, but the arm that would pin never ran.
#[tokio::test]
async fn a_frame_body_to_a_resolved_name_is_refused_and_pins_nothing() {
    let team = Team::new().await;
    let registry_dir = private_dir();
    let identity = Arc::new(identity::Identity::new(registry_dir.path()));
    let own_session = Arc::new(std::sync::Mutex::new(SessionId::from("ses-lead-own".to_owned())));
    let lead = Postbox::lead(&team.registry, Some((&identity, own_session)));
    let (_id, _held) = live_record(registry_dir.path(), "05550005", "relay");
    let socket = registry_dir.path().join("05550005.sock");
    let _peer = PeerStub::listen(&socket).await;

    let refused = team::Postbox::deliver(
        &lead,
        Address::Local("relay".to_owned()),
        Body::Frame(serde_json::json!({"type": "shutdown_approved", "requestId": "r1"})),
    )
    .await;
    assert_eq!(refused, Err(Undelivered::Failed { reason: FRAME_OVER_SOCKET.to_owned() }));
    assert_eq!(identity.pinned("relay"), None, "a refused frame body pins nothing");
}

/// **D530**: the solo postbox has no roster to consult first, resolves
/// straight through the identity index, stamps `from` as
/// `<self-name>@solo` on the wire, and appends the one-way clause to
/// every success note — and a frame body earns the solo-worded refusal
/// rather than the lead's "member of this team" phrasing.
#[tokio::test]
async fn the_solo_postbox_resolves_directly_stamps_solo_and_appends_the_one_way_note() {
    let registry_dir = private_dir();
    let identity = Arc::new(identity::Identity::new(registry_dir.path()));
    let own_session = Arc::new(std::sync::Mutex::new(SessionId::from("ses-solo-own".to_owned())));
    let self_name = Arc::new(std::sync::Mutex::new("frank".to_owned()));
    let solo =
        SoloPostbox::new(Arc::clone(&self_name), Arc::clone(&identity), Arc::clone(&own_session));

    assert!(team::Postbox::roster(&solo).is_empty(), "a teamless session has no roster");

    let (_id, _held) = live_record(registry_dir.path(), "06660006", "backend");
    let socket = registry_dir.path().join("06660006.sock");
    let peer = PeerStub::listen(&socket).await;

    let sent = team::Postbox::deliver(
        &solo,
        Address::Local("backend".to_owned()),
        Body::Text { text: "hi".to_owned(), summary: None },
    )
    .await
    .expect("a unique live session resolves");
    assert!(
        sent.note.ends_with(ONE_WAY_NOTE),
        "the not-addressable-back clause is appended: {:?}",
        sent.note
    );
    assert_eq!(identity.pinned("backend").expect("the pin stands").stem, "06660006");

    let posted: Vec<_> = peer
        .requests()
        .into_iter()
        .filter(|(method, route, _)| method == "POST" && route.starts_with("/team/"))
        .collect();
    assert_eq!(posted.len(), 1, "exactly one message posted: {posted:?}");
    assert!(
        posted[0].2.contains("\"from\":\"frank@solo\""),
        "the wire carries the reserved solo identity: {}",
        posted[0].2
    );

    // A frame body earns the solo-worded refusal, not the team-worded one.
    let refused = team::Postbox::deliver(
        &solo,
        Address::Uds { path: socket.clone() },
        Body::Frame(serde_json::json!({"anything": "goes"})),
    )
    .await;
    assert_eq!(refused, Err(Undelivered::Failed { reason: FRAME_OVER_SOCKET_SOLO.to_owned() }));
}

// D532/D534's own wire types and the `PeerFacts` composition seam
// (cross-session-full-parity, W1/L1c). Each test names the acceptance
// criterion it pins.

/// **AC-1**: every new field defaults away, so a message carrying none of
/// them serializes to exactly the three keys this wire has always had.
#[test]
fn a_message_with_every_new_field_absent_serializes_byte_identically_to_before_d532() {
    let message = SocketMessage {
        from: "worker@session-abcd1234".to_owned(),
        text: "the release is out".to_owned(),
        summary: Some("release".to_owned()),
        message_id: None,
        from_mode: None,
        hop_chain: Vec::new(),
        reply_to: None,
    };

    let encoded = serde_json::to_string(&message).expect("a plain message serializes");
    assert_eq!(
        encoded,
        r#"{"from":"worker@session-abcd1234","text":"the release is out","summary":"release"}"#,
        "every new field is absent-by-default, so the bytes are today's own, unchanged"
    );

    let decoded: SocketMessage =
        serde_json::from_str(&encoded).expect("today's own shape still parses");
    assert_eq!(decoded, message, "and it round-trips to the same value");
}

/// **AC-2**: a body an old sender wrote — just `from`/`text`, no `summary`
/// and none of D532's four fields — still parses, and every new field reads
/// as the collapsed default a receiver already treats as "nothing new
/// asserted".
#[test]
fn a_new_receiver_reads_an_old_senders_body() {
    let old_sender_body = r#"{"from":"worker@session-abcd1234","text":"hello"}"#;

    let decoded: SocketMessage =
        serde_json::from_str(old_sender_body).expect("an old sender's own body still parses");
    assert_eq!(decoded.summary, None);
    assert_eq!(decoded.message_id, None);
    assert_eq!(decoded.from_mode, None);
    assert_eq!(decoded.hop_chain, Vec::<String>::new());
    assert_eq!(decoded.reply_to, None);
}

/// A stand-in [`PeerFacts`] this suite drives directly — the engine that
/// would ordinarily implement the trait does not exist in this crate's own
/// tests, so **AC-5**'s composition half is pinned against a double instead.
#[derive(Debug)]
struct StubFacts {
    sender_mode: Option<SenderMode>,
    reply_to: Option<std::path::PathBuf>,
    hop_chain: Vec<String>,
}

impl PeerFacts for StubFacts {
    fn sender_mode(&self) -> Option<SenderMode> {
        self.sender_mode
    }

    fn reply_to(&self) -> Option<std::path::PathBuf> {
        self.reply_to.clone()
    }

    fn hop_chain(&self) -> Vec<String> {
        self.hop_chain.clone()
    }
}

/// **AC-5** (composition half): the sender-side composition reads only what
/// [`PeerFacts`] answers — `from_mode` and `hop_chain` cross unmodified, and
/// `reply_to` is composed into the `uds:` spelling here rather than expected
/// pre-formatted. The unbound case (nothing asserted, nothing forwarded, no
/// reply address) is pinned by
/// `a_socket_delivery_asks_who_leads_and_posts_to_them_stamped_with_its_identity`
/// above, which never installs a `PeerFacts` beyond the `Unbound` default.
#[tokio::test]
async fn the_sender_side_composition_reads_only_what_peer_facts_answers() {
    let registry_dir = private_dir();
    let identity = Arc::new(identity::Identity::new(registry_dir.path()));
    let own_session =
        Arc::new(std::sync::Mutex::new(SessionId::from("ses-composed-own".to_owned())));
    let self_name = Arc::new(std::sync::Mutex::new("composer".to_owned()));
    let solo = SoloPostbox::new(Arc::clone(&self_name), identity, own_session).with_peer_facts(
        Arc::new(StubFacts {
            sender_mode: Some(SenderMode::Bypass),
            reply_to: Some(std::path::PathBuf::from("/tmp/ganja-501/deadbeef.sock")),
            hop_chain: vec!["abcd1234".to_owned(), "ef012345".to_owned()],
        }),
    );

    let socket = registry_dir.path().join("06660007.sock");
    let peer = PeerStub::listen(&socket).await;

    let _sent = team::Postbox::deliver(
        &solo,
        Address::Uds { path: socket.clone() },
        Body::Text { text: "carrying a chain".to_owned(), summary: None },
    )
    .await
    .expect("a listening peer takes the message");

    let posted: SocketMessage = peer
        .requests()
        .into_iter()
        .find(|(method, route, _)| method == "POST" && route.starts_with("/team/"))
        .map(|(_, _, body)| serde_json::from_str(&body).expect("the body is the wire shape"))
        .expect("exactly one message posted");

    assert_eq!(posted.from_mode, Some(SenderMode::Bypass), "read straight off the facts");
    assert_eq!(
        posted.reply_to.as_deref(),
        Some("uds:/tmp/ganja-501/deadbeef.sock"),
        "the `uds:` spelling is composed at the send site, not read off PeerFacts pre-formatted"
    );
    assert_eq!(
        posted.hop_chain,
        vec!["abcd1234".to_owned(), "ef012345".to_owned()],
        "the chain crosses exactly as PeerFacts answered it, with no further processing here"
    );
    assert!(
        posted.message_id.is_some(),
        "a fresh id is minted for every send regardless of what PeerFacts answers"
    );
}

/// A replica of the wire's shape before D532 — three fields,
/// `deny_unknown_fields` — kept only to pin **AC-9**: this suite has no
/// second binary to hold "today's" struct in, so the shape is frozen here
/// instead.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PreD532SocketMessage {
    from: String,
    text: String,
    summary: Option<String>,
}

/// **AC-9**: an old, `deny_unknown_fields` receiver refuses a body carrying
/// D532's new fields readably — a version-skew regression, not a silent
/// misread.
#[test]
fn an_old_receiver_refuses_a_new_body_readably() {
    let new_body = serde_json::to_string(&SocketMessage {
        from: "worker@team".to_owned(),
        text: "hi".to_owned(),
        summary: None,
        message_id: Some(PeerMessageId::ascending()),
        from_mode: Some(SenderMode::Prompting),
        hop_chain: vec!["abcd1234".to_owned()],
        reply_to: Some("uds:/tmp/x.sock".to_owned()),
    })
    .expect("a new sender's body serializes");

    let refused: Result<PreD532SocketMessage, _> = serde_json::from_str(&new_body);
    assert!(
        refused.is_err(),
        "an old, deny_unknown_fields receiver refuses fields it has never heard of, readably \
         rather than silently: {new_body}"
    );
}

/// **AC-25**: `SocketReceipt` round-trips for each of the three statuses
/// this route actually carries.
#[test]
fn a_socket_receipt_round_trips_with_each_status() {
    for status in [ReceiptStatus::Delivered, ReceiptStatus::Denied, ReceiptStatus::Expired] {
        let receipt = SocketReceipt { message_id: PeerMessageId::ascending(), status };
        let encoded = serde_json::to_string(&receipt).expect("a receipt serializes");
        let decoded: SocketReceipt =
            serde_json::from_str(&encoded).expect("and a receipt this crate wrote parses back");
        assert_eq!(decoded, receipt);
    }
}

/// **AC-25**: the string `"held"` — v2's fourth status, which ganja answers
/// synchronously rather than over this route — is refused by name rather
/// than silently accepted as a fourth state this enum does not carry.
#[test]
fn an_unknown_receipt_status_is_refused_by_name_including_held() {
    let held = r#"{"message_id":"some-id","status":"held"}"#;
    let refused: Result<SocketReceipt, _> = serde_json::from_str(held);
    let error = refused.expect_err("`held` is answered synchronously, never over this route");
    assert!(
        error.to_string().contains("held"),
        "the refusal names the value it does not recognize: {error}"
    );

    let other_unknown = r#"{"message_id":"some-id","status":"delayed"}"#;
    assert!(
        serde_json::from_str::<SocketReceipt>(other_unknown).is_err(),
        "any other unrecognized status is refused the same way"
    );
}

/// **AC-25**: the divergence — ganja answers `held` synchronously, never
/// over this route — is stated in `ReceiptStatus`'s own doc, not left for a
/// reader to rediscover.
#[test]
fn the_receipt_status_doc_states_the_synchronous_held_divergence() {
    let source = include_str!("subagent.rs");
    let enum_at =
        source.find("pub enum ReceiptStatus").expect("ReceiptStatus is declared in this file");
    let preceding_doc = &source[..enum_at];
    let doc_start = preceding_doc
        .rfind("/// How a held entry")
        .expect("its doc block opens with this sentence");
    let doc_block = &preceding_doc[doc_start..];

    assert!(
        doc_block.contains("ganja answers **synchronously**")
            && doc_block.contains("SocketDelivered")
            && doc_block.contains("rather than over this route"),
        "the enum's own doc must state the divergence so a later reader meets it at the \
         declaration rather than rediscovering it: {doc_block:?}"
    );
}

/// **AC-48**: a body claiming one more hop than the sender cap is refused
/// at deserialization, before the guard's own thresholds ever see a `Vec`;
/// exactly the cap still parses.
#[test]
fn a_hop_chain_past_the_sender_cap_is_refused_at_parse_and_the_cap_itself_parses() {
    let one_past_the_cap: Vec<String> =
        (0..=MAX_HOP_CHAIN_ENTRIES).map(|n| n.to_string()).collect();
    let body = serde_json::json!({
        "from": "worker@team",
        "text": "x",
        "hop_chain": one_past_the_cap,
    })
    .to_string();
    let refused: Result<SocketMessage, _> = serde_json::from_str(&body);
    let error = refused.expect_err("one more entry than the sender cap is refused");
    assert!(
        error.to_string().contains(&MAX_HOP_CHAIN_ENTRIES.to_string()),
        "the refusal names the cap it exceeded: {error}"
    );

    let exactly_the_cap: Vec<String> = (0..MAX_HOP_CHAIN_ENTRIES).map(|n| n.to_string()).collect();
    let body = serde_json::json!({
        "from": "worker@team",
        "text": "x",
        "hop_chain": exactly_the_cap,
    })
    .to_string();
    let parsed: SocketMessage =
        serde_json::from_str(&body).expect("exactly the cap is still a conforming sender");
    assert_eq!(parsed.hop_chain.len(), MAX_HOP_CHAIN_ENTRIES);
}

/// **AC-52**: `held` absent produces exactly the two-field bytes this answer
/// has always had.
#[test]
fn socket_delivered_with_held_absent_is_byte_identical_to_before_d534() {
    let delivered =
        SocketDelivered { to: "team-lead".to_owned(), note: RECEIVED.to_owned(), held: None };
    let encoded = serde_json::to_string(&delivered).expect("an accept answer serializes");
    // A literal, field-order-sensitive string rather than `serde_json::json!`
    // — that macro's own `Value::Object` sorts keys alphabetically without
    // the `preserve_order` feature, which is exactly the byte-identity claim
    // this test exists to pin, not something to launder through.
    assert_eq!(
        encoded,
        format!(
            r#"{{"to":"team-lead","note":{}}}"#,
            serde_json::to_string(RECEIVED).expect("a plain string serializes")
        ),
        "held absent produces exactly today's two-field bytes, in today's field order"
    );
}

/// **AC-52**: an accept's answer and a refuse's answer are byte-identical
/// to each other with `held` present in neither — the new field does not
/// reopen the enumeration channel D523 closed.
#[test]
fn an_accept_and_a_refuse_answer_are_byte_identical_with_held_absent_from_both() {
    let accept =
        SocketDelivered { to: "team-lead".to_owned(), note: RECEIVED.to_owned(), held: None };
    let refuse =
        SocketDelivered { to: "team-lead".to_owned(), note: RECEIVED.to_owned(), held: None };
    assert_eq!(
        serde_json::to_string(&accept).expect("an accept serializes"),
        serde_json::to_string(&refuse).expect("a refuse serializes"),
        "D523's uniform answer: `held`'s own absence in both keeps the two indistinguishable"
    );
}

/// **AC-52**: a hold's answer carries the typed cause, and round-trips.
#[test]
fn a_holds_answer_carries_the_typed_cause() {
    let delivered = SocketDelivered {
        to: "team-lead".to_owned(),
        note: held_note(ganja_protocol::HoldCause::NoModeAsserted),
        held: Some(HeldWire { cause: ganja_protocol::HoldCause::NoModeAsserted }),
    };
    let encoded = serde_json::to_string(&delivered).expect("a held answer serializes");
    let decoded: SocketDelivered =
        serde_json::from_str(&encoded).expect("and a held answer this crate wrote parses back");
    assert_eq!(decoded.held, Some(HeldWire { cause: ganja_protocol::HoldCause::NoModeAsserted }));
}

/// **AC-52**: a new sender reading an old receiver's answer never sees
/// `held` at all — nothing to register, nothing to post, exactly AC-2's
/// posture mirrored onto the answer side.
#[test]
fn a_new_sender_reading_an_old_receivers_answer_sees_no_held_fact() {
    let old_answer = r#"{"to":"team-lead","note":"It reached that session."}"#;
    let decoded: SocketDelivered =
        serde_json::from_str(old_answer).expect("an old receiver's own answer still parses");
    assert_eq!(
        decoded.held, None,
        "nothing to register, nothing to post — AC-2's posture, mirrored"
    );
}

/// **AC-52** (D2's honest worse direction): an old sender's
/// `deny_unknown_fields` answer type refuses a **new** receiver's held
/// answer — surfacing to that sender's model as a failed send, on a
/// message that is, in fact, in review on the far side.
#[test]
fn an_old_senders_answer_type_refuses_a_new_receivers_held_answer() {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct PreD534SocketDelivered {
        to: String,
        note: String,
    }

    let held_answer = serde_json::to_string(&SocketDelivered {
        to: "team-lead".to_owned(),
        note: held_note(ganja_protocol::HoldCause::ModeMismatch),
        held: Some(HeldWire { cause: ganja_protocol::HoldCause::ModeMismatch }),
    })
    .expect("a held answer serializes");

    let refused: Result<PreD534SocketDelivered, _> = serde_json::from_str(&held_answer);
    assert!(
        refused.is_err(),
        "an old sender's deny_unknown_fields type refuses a `held` field it has never heard of: \
         {held_answer}"
    );
}

/// **AC-52** (D2's standing note): whoever adds the next answer field meets
/// the consequence at `SocketDelivered`'s own declaration.
#[test]
fn socket_delivereds_own_doc_carries_the_standing_note_for_future_fields() {
    let source = include_str!("subagent.rs");
    let struct_at = source
        .find("pub struct SocketDelivered")
        .expect("SocketDelivered is declared in this file");
    let preceding_doc = &source[..struct_at];
    let doc_start = preceding_doc
        .rfind("/// What the socket route answers")
        .expect("its doc block opens with this sentence");
    let doc_block = &preceding_doc[doc_start..];

    assert!(
        doc_block.contains("deny_unknown_fields")
            && doc_block.contains("every additive answer field")
            && doc_block.contains("**old** sender"),
        "the struct's own doc must warn whoever adds the next field that an old sender's parse \
         fails on it too: {doc_block:?}"
    );
}
