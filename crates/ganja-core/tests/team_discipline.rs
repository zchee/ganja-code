//! The two engine-native team guards, as a real turn loop runs them.
//!
//! `teammate::discipline`'s own tests walk the decisions; what this pins is
//! that the turn loop gathers the right facts, at the right two seams, and
//! that the blocks it renders reach the model and nothing else. Three
//! properties in particular are only visible from out here:
//!
//! - a continuation is a **request**, not a message — the transcript must
//!   never grow a user part the person did not type;
//! - the breaker really stops the loop, so a team that cannot finish hands the
//!   session back rather than talking to itself;
//! - the nag is one block for a whole fan-out batch, because it is decided
//!   over the step's calls rather than per call.
//!
//! The provider double answers by **what it was asked** rather than by
//! position in a queue, for `team_tasks.rs`'s reason: a lead and an in-process
//! teammate share one provider and a persistent engine asks for a title beside
//! them, so a FIFO script would hand one conversation's answer to the other.
//!
//! Every root is handed in and nothing here reads or writes the environment,
//! so this binary may hold more than one test.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Command, Event, Part, Role};
use ganja_core::provider::{ChatRequest, Provider, ProviderError, ProviderEvent};
use ganja_core::teammate::TeammateRegistry;
use ganja_core::tool::Registry;
use ganja_core::{Engine, Storage};
use ganja_protocol::FinishReason;
use ganja_team::task::{NewTask, Store, TaskStatus, Update};
use ganja_team::{TeamName, TeamsRoot};
use ganja_testkit::{LEAD_SESSION_ID, RecordedSpawns, TEAM, caller, says, spawn_with_prompt};
use serde_json::json;

/// How long a lead is given to settle. Generous against a loaded machine: the
/// breaker case is six provider round trips and a teammate's runner polls a
/// mailbox between them.
const EVENTUALLY: Duration = Duration::from_secs(20);

/// The lead's prompt, appearing nowhere else, so its conversation can be told
/// from the teammate's and from a title request.
const PROMPT: &str = "look after the team, zarquon";

/// What the teammate is spawned with. Answered with a bare finish, so it goes
/// idle after one turn and simply stays in the registry.
const WORK_PROMPT: &str = "stand by, zarquon";

/// The teammate's name.
const WORKER: &str = "worker-1";

/// The tag the continuation block carries.
const CONTINUATION_TAG: &str = "team_still_working";

/// The tag the name nag carries.
const NAG_TAG: &str = "teammate_naming";

/// What the model does with the step it is asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    /// Say something and stop, every time. The turn ends here unless a guard
    /// keeps it alive, which is the whole point.
    OnlyTalks,
    /// Delegate twice without naming anybody, then only talk.
    FansOutAnonymously,
    /// Delegate twice, both naming a teammate, then only talk.
    FansOutNamed,
}

/// A provider that answers the lead by its script and everybody else with a
/// bare finish.
struct Director {
    script: Script,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

/// Everything a request's messages say, as plain text.
fn transcript(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| part.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether this conversation has already delegated.
fn already_delegated(request: &ChatRequest) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .any(|part| matches!(&part.body, ganja_core::protocol::PartBody::Tool { tool, .. } if tool == "task"))
}

/// Two `task` calls in **one** step, which is what makes this a fan-out rather
/// than two steps that each delegated once.
fn fan_out(named: bool) -> Vec<ProviderEvent> {
    let mut script = Vec::new();
    for (index, who) in ["alpha", "beta"].iter().enumerate() {
        let id = format!("call-{index}");
        let args = if named {
            json!({"name": who, "description": "take a piece of it"})
        } else {
            json!({"description": "take a piece of it"})
        };
        script.push(ProviderEvent::ToolCallStart { id: id.clone(), name: "task".to_owned() });
        script.push(ProviderEvent::ToolCallDelta { id: id.clone(), json: args.to_string() });
        script.push(ProviderEvent::ToolCallEnd { id });
    }
    script.push(ProviderEvent::Finish(FinishReason::Completed));

    script
}

#[async_trait]
impl Provider for Director {
    fn id(&self) -> &str {
        "recorder"
    }

    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let text = transcript(&request);
        let script = if !text.contains(PROMPT) {
            // Somebody else's conversation: the teammate's own turn, or a
            // title request. Neither is what this suite is about.
            vec![ProviderEvent::Finish(FinishReason::Completed)]
        } else if already_delegated(&request) {
            says("talked")
        } else {
            match self.script {
                Script::OnlyTalks => says("talked"),
                Script::FansOutAnonymously => fan_out(false),
                Script::FansOutNamed => fan_out(true),
            }
        };
        self.seen.lock().expect("the request log is never poisoned").push(request);

        Ok(stream::iter(script).boxed())
    }
}

/// A lead over its own store, wired to a team, its birth queue drained.
struct Lead {
    home: tempfile::TempDir,
    root: TeamsRoot,
    team: TeamName,
    engine: Engine,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    /// The text of every user message this session **announced**, which is
    /// what a frontend draws and what the transcript holds. Collected off the
    /// event stream rather than read back through `resume`, so nothing this
    /// assertion does moves the engine it is asserting about.
    said: Arc<Mutex<Vec<String>>>,
    asker: RecordedSpawns,
}

impl Lead {
    async fn new(script: Script) -> Self {
        let home = ganja_testkit::temp_dir();
        let storage = Storage::open(home.path().join("storage"));
        let root = TeamsRoot::new(home.path().join("teams"));
        let team = TeamName::parse(TEAM).expect("a team name");
        let registry = Arc::new(TeammateRegistry::new(
            root.clone(),
            team.clone(),
            LEAD_SESSION_ID,
            home.path(),
        ));
        let requests: Arc<Mutex<Vec<ChatRequest>>> = Arc::default();
        let provider = Arc::new(Director { script, seen: Arc::clone(&requests) });

        let engine = Engine::persistent(
            provider,
            "recorder-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
            storage,
        )
        .with_teammates(registry, ganja_testkit::externals());
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        let said: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorder = Arc::clone(&said);
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if let Event::MessageStarted { message, .. } = event
                    && message.role == Role::User
                {
                    let mut said = recorder.lock().expect("the said log is never poisoned");
                    said.extend(message.parts.iter().filter_map(Part::as_text).map(str::to_owned));
                }
            }
        });

        Self { home, root, team, engine, requests, said, asker: RecordedSpawns::default() }
    }

    /// The team's task documents, read the way any other process would.
    fn store(&self) -> Store {
        Store::new(self.root.tasks_dir(&self.team))
    }

    /// Files one task in `status`, without the model touching anything.
    ///
    /// The guards read the documents, so seeding them directly is what keeps
    /// each test about the guard rather than about whether a scripted model
    /// remembered to call `task_create`.
    fn seed_task(&self, status: TaskStatus) {
        let store = self.store();
        let task = store
            .create(NewTask::new("port the parser", "start from the spec"))
            .expect("a fresh store files a task");
        if status != TaskStatus::Pending {
            store
                .update(&task.id, Update { status: Some(status), ..Update::default() })
                .expect("the status moves");
        }
    }

    /// Puts a live teammate in the registry, so the guards see a team.
    async fn spawn_a_member(&self) {
        self.engine
            .teammates()
            .expect("this session leads a team")
            .start(
                spawn_with_prompt(WORKER, Some("in-process"), WORK_PROMPT),
                &caller(self.home.path()),
                &self.asker,
            )
            .await
            .expect("an in-process teammate starts on a session that has a store");
    }

    async fn prompt(&self, text: &str) {
        self.engine
            .send(Command::SendPrompt {
                text: text.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
    }

    /// Prompts, then waits for the turn — however many continuations it takes
    /// — to be over.
    async fn prompt_and_settle(&self, text: &str) {
        self.prompt(text).await;
        assert!(self.engine.settle(EVENTUALLY).await, "the turn ended inside its budget");
    }

    /// Only the lead's own **step** requests.
    ///
    /// A persistent engine also asks this conversation for a title, and that
    /// request carries the same text — so it would be counted as a step by a
    /// filter that looked at the words alone. A title request is offered no
    /// tools, which is what tells the two apart.
    fn lead_requests(&self) -> Vec<ChatRequest> {
        self.requests
            .lock()
            .expect("the request log is never poisoned")
            .iter()
            .filter(|request| !request.tools.is_empty() && transcript(request).contains(PROMPT))
            .cloned()
            .collect()
    }

    /// How many of the lead's requests carried `tag`.
    fn carrying(&self, tag: &str) -> usize {
        self.lead_requests().iter().filter(|request| transcript(request).contains(tag)).count()
    }

    /// Every user message this session announced.
    fn stored_user_text(&self) -> Vec<String> {
        self.said.lock().expect("the said log is never poisoned").clone()
    }

    async fn finish(self) {
        self.engine.shutdown_teammates().await;
        drop(self.home);
    }
}

/// The blocker's whole reason to exist: the model stopped, the team had not.
#[tokio::test]
async fn a_turn_that_would_end_with_open_work_and_a_live_member_keeps_going() {
    let lead = Lead::new(Script::OnlyTalks).await;
    lead.spawn_a_member().await;
    lead.seed_task(TaskStatus::Pending);

    lead.prompt_and_settle(PROMPT).await;

    let requests = lead.lead_requests();
    assert_eq!(
        requests.len(),
        1 + 5,
        "the first request, then one per auto-continuation until the breaker: {}",
        requests.len(),
    );
    assert_eq!(
        lead.carrying(CONTINUATION_TAG),
        5,
        "every continued request says why it is continuing, exactly once",
    );
    assert!(
        !transcript(&requests[0]).contains(CONTINUATION_TAG),
        "the opening request has nothing to continue",
    );

    lead.finish().await;
}

/// The breaker: five, and then the session goes back to the person. The turn
/// really ends — `settle` above returning is what says so — rather than
/// continuing quietly with the block suppressed.
#[tokio::test]
async fn the_breaker_stops_the_loop_after_five_continuations() {
    let lead = Lead::new(Script::OnlyTalks).await;
    lead.spawn_a_member().await;
    lead.seed_task(TaskStatus::InProgress);

    lead.prompt_and_settle(PROMPT).await;

    assert_eq!(lead.carrying(CONTINUATION_TAG), 5, "the sixth is refused");
    assert_eq!(
        lead.store().list().expect("the list reads").len(),
        1,
        "and the task is still there: the guard reports, it does not tidy",
    );

    lead.finish().await;
}

/// A continuation belongs to the request and never to the transcript: a stored
/// user part the person did not type would show up in the next resume as
/// something they said.
#[tokio::test]
async fn a_continuation_is_never_written_into_the_transcript() {
    let lead = Lead::new(Script::OnlyTalks).await;
    lead.spawn_a_member().await;
    lead.seed_task(TaskStatus::Pending);

    lead.prompt_and_settle(PROMPT).await;

    let stored = lead.stored_user_text();
    assert_eq!(stored, vec![PROMPT.to_owned()], "one prompt was typed, so one is stored");
    assert!(
        !stored.iter().any(|text| text.contains(CONTINUATION_TAG)),
        "and no synthetic instruction joined it: {stored:?}",
    );

    lead.finish().await;
}

/// A drained list ends the turn like any other session's. This is the case
/// that keeps the guard from being an infinite loop with a counter.
#[tokio::test]
async fn a_team_whose_work_is_finished_ends_its_turn_at_once() {
    let lead = Lead::new(Script::OnlyTalks).await;
    lead.spawn_a_member().await;
    lead.seed_task(TaskStatus::Completed);

    lead.prompt_and_settle(PROMPT).await;

    assert_eq!(lead.lead_requests().len(), 1, "nothing was open, so nothing continued");
    assert_eq!(lead.carrying(CONTINUATION_TAG), 0);

    lead.finish().await;
}

/// Open work with nobody running is not a stranded team: it is a list
/// somebody filed before spawning anybody, which is the pipeline's own
/// ordinary first step.
#[tokio::test]
async fn open_work_with_no_live_member_does_not_continue_a_turn() {
    let lead = Lead::new(Script::OnlyTalks).await;
    lead.seed_task(TaskStatus::Pending);

    lead.prompt_and_settle(PROMPT).await;

    assert_eq!(lead.lead_requests().len(), 1, "a lead leading nobody ends its turn");
    assert_eq!(lead.carrying(CONTINUATION_TAG), 0);

    lead.finish().await;
}

/// The nag, and the property that makes it a per-step guard rather than a
/// per-call one: two anonymous delegations in one step earn one block.
#[tokio::test]
async fn a_whole_anonymous_fan_out_is_nagged_once() {
    let lead = Lead::new(Script::FansOutAnonymously).await;
    lead.spawn_a_member().await;
    // No task is seeded, so the continuation blocker has nothing to say and
    // this test is about the nag alone.

    lead.prompt_and_settle(PROMPT).await;

    assert_eq!(lead.carrying(NAG_TAG), 1, "one block for the step, not one per call in the batch",);
    assert_eq!(lead.carrying(CONTINUATION_TAG), 0, "an empty list continues nothing");

    lead.finish().await;
}

/// A named delegation is what the nag is asking for, so it says nothing.
#[tokio::test]
async fn a_named_fan_out_is_not_nagged() {
    let lead = Lead::new(Script::FansOutNamed).await;
    lead.spawn_a_member().await;

    lead.prompt_and_settle(PROMPT).await;

    assert_eq!(lead.carrying(NAG_TAG), 0, "every call named somebody");

    lead.finish().await;
}

/// Outside a team the nag has nothing to be about: an anonymous subagent is a
/// first-class thing to want, and always was (**D462**).
#[tokio::test]
async fn a_session_leading_nobody_is_never_nagged_about_a_name() {
    let lead = Lead::new(Script::FansOutAnonymously).await;

    lead.prompt_and_settle(PROMPT).await;

    assert_eq!(lead.carrying(NAG_TAG), 0, "no team, no teammate to prefer");

    lead.finish().await;
}
