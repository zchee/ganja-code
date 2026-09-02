//! The two engine-native team guards, as a real turn loop runs them.
//!
//! `teammate::discipline`'s own tests walk the decisions; what this pins is
//! that the turn loop gathers the right facts, at the right two seams, and
//! that the blocks it renders reach the model and nothing else. Four
//! properties in particular are only visible from out here:
//!
//! - a continuation is a **request**, not a message — the transcript must
//!   never grow a user part the person did not type, and where the block sits
//!   in that request is half of what it means: it answers the reply the model
//!   just wrote, so it is the last thing the model reads rather than a part
//!   folded into the prompt that opened the turn;
//! - the breaker really stops the loop, so a team that cannot finish hands the
//!   session back rather than talking to itself;
//! - the nag is one block for a whole fan-out batch, because it is decided
//!   over the step's calls rather than per call;
//! - what the tail acts on is read **at the tail**, not snapshotted at the
//!   turn's start: a member that joins mid-turn is seen by that turn's own
//!   registry read, and a person who types mid-turn puts the budget back
//!   through the steer arm — so a spawn continues the running turn, and a
//!   steer buys it a fresh five.
//!
//! The provider double is [`ganja_testkit::Director`] for the reason its own
//! doc gives: a lead and an in-process teammate share one provider and a
//! persistent engine asks for a title beside them, so a FIFO script would hand
//! one conversation's answer to the other.
//!
//! Every root is handed in and nothing here reads or writes the environment,
//! so this binary may hold more than one test.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use ganja_core::permission::Permissions;
use ganja_core::protocol::team::PeerPayload;
use ganja_core::protocol::{Command, Event, Message, Part, PartBody, Role};
use ganja_core::provider::{ChatRequest, ProviderEvent};
use ganja_core::teammate::TeammateRegistry;
use ganja_core::tool::Registry;
use ganja_core::{Engine, Storage};
use ganja_protocol::FinishReason;
use ganja_team::task::{NewTask, Store, TaskStatus, Update};
use ganja_team::{TeamName, TeamsRoot};
use ganja_testkit::{
    LEAD_SESSION_ID, RecordedSpawns, TEAM, caller, says, spawn_with_prompt, transcript,
};
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

/// What a person types into a turn that is already running. Appears nowhere
/// else, so the first request carrying it can be named exactly.
const STEER: &str = "actually, start with the lexer";

/// What a teammate says into a turn that is already running. Appears nowhere
/// else, so the first request carrying it can be named exactly.
const PEER_REPORT: &str = "alpha is done, taking beta";

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

/// Whether this conversation has already delegated.
fn already_delegated(request: &ChatRequest) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .any(|part| matches!(&part.body, PartBody::Tool { tool, .. } if tool == "task"))
}

/// Whether this request is one of the lead's own **step** requests.
///
/// A persistent engine also asks this conversation for a title, and that
/// request carries the same text — so it would be counted as a step by a
/// filter that looked at the words alone. A title request is offered no tools,
/// which is what tells the two apart.
fn is_lead_step(request: &ChatRequest) -> bool {
    !request.tools.is_empty() && transcript(request).contains(PROMPT)
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

/// A hold the provider double takes on one of the lead's step requests, so a
/// test can act at a point *inside* a running turn instead of racing one.
///
/// The held request announces itself and then waits — on the thread answering
/// it, which is why the tests that hold one ask for a runtime with a second
/// worker. Whatever the test does meanwhile therefore lands strictly after
/// that request was asked and strictly before the step answering it reaches
/// the turn's tail, which is the window every fact below lives in. Nothing
/// sleeps for effect: the test is woken by the request, and the request is
/// released by the test.
struct Gate {
    /// Which of the lead's step requests waits here; 1 is the opening one.
    at: usize,
    /// How many of the lead's step requests have reached the gate.
    seen: AtomicUsize,
    /// Wakes the test, from a context that must not block.
    arrived: tokio::sync::mpsc::UnboundedSender<()>,
    /// And holds the answer until the test sends. A **blocking** receiver
    /// because the answer is chosen in a closure that cannot await, locked by
    /// the one held request alone so no other conversation queues behind it.
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl Gate {
    /// Holds `request` if it is the one being waited for, and otherwise lets
    /// it straight through.
    fn hold(&self, request: &ChatRequest) {
        if !is_lead_step(request) {
            return;
        }
        if self.seen.fetch_add(1, Ordering::Relaxed) + 1 != self.at {
            return;
        }

        // Neither end is an assertion: a test that has already failed drops
        // its `Held`, which disconnects both channels, and the only right
        // thing for a hold nobody is waiting on is to let go quietly rather
        // than add a second panic, on a worker, to the one that matters.
        let _ = self.arrived.send(());
        let _ = self.release.lock().expect("the gate is never poisoned").recv();
    }
}

/// The test's end of a [`Gate`].
///
/// Dropping one releases the hold as surely as [`Held::release`] does, since
/// the gate treats a disconnected channel as a release: a test that fails an
/// assertion before it gets there fails once, rather than leaving a thread
/// parked in a runtime that is being torn down or panicking a second time
/// from the worker that was parked.
struct Held {
    arrived: tokio::sync::mpsc::UnboundedReceiver<()>,
    permit: std::sync::mpsc::Sender<()>,
}

impl Held {
    /// Waits until the turn is inside the held request.
    async fn reached(&mut self) {
        tokio::time::timeout(EVENTUALLY, self.arrived.recv())
            .await
            .expect("the lead reaches the held request inside its budget")
            .expect("the provider double outlives the wait");
    }

    /// Lets the held request be answered, and with it the rest of the turn.
    fn release(self) {
        self.permit.send(()).expect("the held request is still waiting");
    }
}

/// A lead over its own store, wired to a team, its birth queue drained.
struct Lead {
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
    /// Declared last so it is dropped last: the engine's storage lives under
    /// it, and taking the directory away while the engine still holds it is
    /// the reverse of the safe order.
    home: tempfile::TempDir,
}

impl Lead {
    async fn new(script: Script) -> Self {
        Self::build(script, None).await
    }

    /// A lead whose `at`-th step request waits for the test before it is
    /// answered — what the two timing cases below are built on, since each is
    /// about *when* something happened relative to one running turn.
    async fn holding(script: Script, at: usize) -> (Self, Held) {
        let (arrived, waiting) = tokio::sync::mpsc::unbounded_channel();
        let (permit, released) = std::sync::mpsc::channel();
        let gate = Gate { at, seen: AtomicUsize::new(0), arrived, release: Mutex::new(released) };

        (Self::build(script, Some(gate)).await, Held { arrived: waiting, permit })
    }

    async fn build(script: Script, gate: Option<Gate>) -> Self {
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
        let (provider, requests) = ganja_testkit::Director::answering(move |request| {
            if let Some(gate) = &gate {
                gate.hold(request);
            }

            if !transcript(request).contains(PROMPT) {
                // Somebody else's conversation: the teammate's own turn, or a
                // title request. Neither is what this suite is about.
                vec![ProviderEvent::Finish(FinishReason::Completed)]
            } else if already_delegated(request) {
                says("talked")
            } else {
                match script {
                    Script::OnlyTalks => says("talked"),
                    Script::FansOutAnonymously => fan_out(false),
                    Script::FansOutNamed => fan_out(true),
                }
            }
        });

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

        Self { root, team, engine, requests, said, asker: RecordedSpawns::default(), home }
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
    ///
    /// Bounded, because inside a hold this runs a whole in-process teammate on
    /// the one worker the gate has not parked: a wedge there should fail here
    /// with a sentence, not later at nextest's own deadline with none.
    async fn spawn_a_member(&self) {
        let caller = caller(self.home.path());
        let started = self.engine.teammates().expect("this session leads a team").start(
            spawn_with_prompt(WORKER, Some("in-process"), WORK_PROMPT),
            &caller,
            &self.asker,
        );
        tokio::time::timeout(EVENTUALLY, started)
            .await
            .expect("the spawn completes inside its budget")
            .expect("an in-process teammate starts on a session that has a store");
    }

    /// Asserts the turn is still running — what every hold exists to make
    /// true while the test acts.
    ///
    /// Asked of the engine rather than read off the held request's shape: the
    /// gate picks what it holds by counting the lead's step requests, and a
    /// title request that one day looked like a step would be held in its
    /// place — after the turn, where a spawn or a steer proves nothing and the
    /// counts below would still come out right. This is what turns that
    /// silent hollowing into a red test.
    async fn still_turning(&self) {
        assert!(
            !self.engine.settle(Duration::ZERO).await,
            "the held request is inside a running turn",
        );
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

    /// Types into the turn that is already running.
    ///
    /// The `expect` is half the assertion wherever this is called mid-hold: a
    /// steer reaching an engine between turns is refused, so a queued one is
    /// itself proof the turn was still going when the person typed.
    async fn steer(&self, id: &str, text: &str) {
        let sent = self.engine.send(Command::Steer {
            id: id.to_owned(),
            text: text.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        });
        tokio::time::timeout(EVENTUALLY, sent)
            .await
            .expect("the steer is taken inside its budget")
            .expect("a running turn takes a steer");
    }

    /// Delivers a teammate's message into the turn that is already running.
    ///
    /// The same command a person's steer arrives as — that is the whole point
    /// of the case this exists for. What tells the two apart is one field: a
    /// teammate's carries an envelope and no text at all.
    async fn peer_message(&self, id: &str) {
        let sent = self.engine.send(Command::Steer {
            id: id.to_owned(),
            text: String::new(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: vec![PeerPayload::new(WORKER, None, None, PEER_REPORT)],
        });
        tokio::time::timeout(EVENTUALLY, sent)
            .await
            .expect("the teammate's message is taken inside its budget")
            .expect("a running turn takes a teammate's message");
    }

    /// Only the lead's own [step](is_lead_step) requests.
    fn lead_requests(&self) -> Vec<ChatRequest> {
        self.requests
            .lock()
            .expect("the request log is never poisoned")
            .iter()
            .filter(|request| is_lead_step(request))
            .cloned()
            .collect()
    }

    /// How many of the lead's requests carried `tag`.
    fn carrying(&self, tag: &str) -> usize {
        self.lead_requests().iter().filter(|request| transcript(request).contains(tag)).count()
    }

    /// The lead's requests that carried `tag`, whole.
    ///
    /// Where [`Lead::carrying`] counts, this is for the assertions about
    /// *where* in a request a block landed, which is a question only the
    /// messages themselves answer.
    fn requests_carrying(&self, tag: &str) -> Vec<ChatRequest> {
        self.lead_requests()
            .into_iter()
            .filter(|request| transcript(request).contains(tag))
            .collect()
    }

    /// Every user message this session announced.
    fn stored_user_text(&self) -> Vec<String> {
        self.said.lock().expect("the said log is never poisoned").clone()
    }

    /// The same, once at least `count` of them have arrived.
    ///
    /// The events are drained by a task of its own, so an assertion about
    /// *which* messages were announced can otherwise read the log a beat
    /// before the last one lands. The count is the fact to synchronise on;
    /// what the messages are is still the assertion.
    async fn announced(&self, count: usize) -> Vec<String> {
        ganja_testkit::eventually(EVENTUALLY, "the typed messages to be announced", async || {
            let said = self.stored_user_text();
            (said.len() >= count).then_some(said)
        })
        .await
    }

    async fn finish(self) {
        self.engine.shutdown_teammates().await;
    }
}

/// The blocker's whole reason to exist, its breaker, and the line neither may
/// cross — three readings of the one run, because all three are properties of
/// the same six round trips.
///
/// The model stopped and the team had not, so the turn is continued: five
/// times, every continued request saying why and the opening one having
/// nothing to say. Then the session goes back to the person rather than
/// talking to itself, which is what `settle` returning says. The task is still
/// on the list afterwards — the guard reports, it does not tidy — and not one
/// of those five continuations was written into the transcript, where it would
/// come back on the next resume as something the person had typed.
#[tokio::test]
async fn open_work_and_a_live_member_continue_a_turn_five_times_and_never_the_transcript() {
    let lead = Lead::new(Script::OnlyTalks).await;
    lead.spawn_a_member().await;
    // In progress rather than pending: the pty drill files its task through
    // the model, so pending is driven through the guard end to end there;
    // this binary is the one place an in-progress task is.
    lead.seed_task(TaskStatus::InProgress);

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
        "every continued request says why it is continuing, exactly once, and the sixth is refused",
    );
    assert!(
        !transcript(&requests[0]).contains(CONTINUATION_TAG),
        "the opening request has nothing to continue",
    );
    assert_eq!(
        lead.store().list().expect("the list reads").len(),
        1,
        "and the task is still there: the guard reports, it does not tidy",
    );

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

/// The registry is read at the turn's **tail**, not at its start: a member
/// that appears while the model is still talking keeps *that* turn going.
///
/// Which is the pipeline's own first turn — `/team` spawns its members from
/// inside the very turn that then has to carry them — and the case every
/// test above leaves open by spawning before it prompts, since a guard that
/// snapshotted the registry when the turn opened would pass all of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_spawned_inside_a_turn_continues_that_turn() {
    let (lead, mut held) = Lead::holding(Script::OnlyTalks, 1).await;
    // Seeded before the prompt, so the spawn below is the only thing this
    // test moves in time.
    lead.seed_task(TaskStatus::InProgress);

    lead.prompt(PROMPT).await;
    held.reached().await;
    lead.still_turning().await;
    lead.spawn_a_member().await;
    held.release();
    assert!(lead.engine.settle(EVENTUALLY).await, "the turn ended inside its budget");

    let requests = lead.lead_requests();
    assert_eq!(
        requests.len(),
        1 + 5,
        "the request the spawn landed inside, then one per auto-continuation until the breaker: {}",
        requests.len(),
    );
    assert_eq!(
        lead.carrying(CONTINUATION_TAG),
        5,
        "the turn that gained the member is the turn that carried on",
    );
    assert!(
        !transcript(&requests[0]).contains(CONTINUATION_TAG),
        "and the request held open while the member arrived had nothing to continue yet",
    );
    assert_eq!(
        lead.announced(1).await,
        vec![PROMPT.to_owned()],
        "a spawn is not a person typing: one prompt was typed, so one is stored",
    );

    lead.finish().await;
}

/// The mirror, and what makes the case above about *timing* rather than about
/// spawning at all: a member that appears once the turn is over continues
/// nothing. The two differ in one line — which side of the release the spawn
/// sits on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_spawned_after_a_turn_ended_continues_nothing() {
    let (lead, mut held) = Lead::holding(Script::OnlyTalks, 1).await;
    lead.seed_task(TaskStatus::InProgress);

    lead.prompt(PROMPT).await;
    held.reached().await;
    lead.still_turning().await;
    held.release();
    assert!(lead.engine.settle(EVENTUALLY).await, "the turn ended inside its budget");

    lead.spawn_a_member().await;

    assert_eq!(
        lead.lead_requests().len(),
        1,
        "the tail found nobody running, and a member arriving afterwards does not reopen a turn",
    );
    assert_eq!(lead.carrying(CONTINUATION_TAG), 0);

    lead.finish().await;
}

/// A person typing mid-turn puts the continuation budget back, so a turn that
/// had already spent one gets a whole five more.
///
/// The reset is one line, in the arm the loop takes when a steer was drained,
/// and it is the whole of "five *consecutive*": the budget exists to stop a
/// model talking to itself, and a turn somebody is steering is not that. The
/// count is pinned exactly, because a "more than five" would pass just as
/// happily on a turn that never spent one before the steer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_puts_the_continuation_budget_back() {
    // Held at the second step request: the turn has spent exactly one
    // auto-continuation by the time the person types, so a budget that was
    // not put back would leave four.
    let (lead, mut held) = Lead::holding(Script::OnlyTalks, 2).await;
    lead.spawn_a_member().await;
    lead.seed_task(TaskStatus::InProgress);

    lead.prompt(PROMPT).await;
    held.reached().await;
    lead.still_turning().await;
    lead.steer("steer-1", STEER).await;
    held.release();
    assert!(lead.engine.settle(EVENTUALLY).await, "one turn ended inside its budget");

    let requests = lead.lead_requests();
    assert_eq!(
        requests.len(),
        1 + 1 + 1 + 5,
        "the opening request, one continuation, the steer, then a whole fresh five: {}",
        requests.len(),
    );
    assert_eq!(
        lead.carrying(CONTINUATION_TAG),
        1 + 5,
        "six auto-continuations in the one turn, which only a budget put back allows",
    );
    assert!(
        !transcript(&requests[1]).contains(STEER),
        "the steer landed while this request was in flight, so it carries the continuation",
    );
    assert!(
        transcript(&requests[2]).contains(STEER),
        "and the request after it is the one that carries what was typed",
    );

    let stored = lead.announced(2).await;
    assert_eq!(
        stored,
        vec![PROMPT.to_owned(), STEER.to_owned()],
        "both messages a person really typed are in the transcript, in the order they were typed",
    );
    assert!(
        !stored.iter().any(|text| text.contains(CONTINUATION_TAG)),
        "and none of the six continuations joined them: {stored:?}",
    );

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

    let nagged = lead.requests_carrying(NAG_TAG);
    let [nagged] = nagged.as_slice() else { panic!("exactly one request was nagged") };
    let [.., before, last] = nagged.messages.as_slice() else {
        panic!("a nagged request carries the step it is about and the block")
    };
    assert!(
        last.role == Role::User && said(last).contains(NAG_TAG),
        "the nag is the last thing the model reads, after the whole fan-out: {:?}",
        said(last),
    );
    assert_eq!(
        before.role,
        Role::Assistant,
        "sitting directly after the step whose calls it is about, results and all",
    );

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

/// One message's text, however many parts carry it.
///
/// The whole-request [`transcript`] cannot answer a question about *which*
/// message a block landed in, which is exactly the question the two placement
/// tests ask.
fn said(message: &Message) -> String {
    message.parts.iter().filter_map(Part::as_text).collect::<Vec<_>>().join("\n")
}

/// Where the block sits is half of what it means, and the half a wire cares
/// about: a continuation answers the reply the model just wrote, so it is a
/// user message **after** that reply rather than parts folded into the prompt
/// that opened the turn.
///
/// Folded, it would reach the model before the text it is about and leave the
/// request ending on the assistant's own words — which the Anthropic wire
/// sends as a prefill, and refuses outright when that text ends in
/// whitespace. So this reads the five continued requests of the same run the
/// suite's opening test counts, and asks of each one where the block is.
#[tokio::test]
async fn a_continued_request_ends_with_the_block_rather_than_with_the_reply() {
    let lead = Lead::new(Script::OnlyTalks).await;
    lead.spawn_a_member().await;
    lead.seed_task(TaskStatus::InProgress);

    lead.prompt_and_settle(PROMPT).await;

    let continued = lead.requests_carrying(CONTINUATION_TAG);
    assert_eq!(continued.len(), 5, "the five continued requests, this time read whole");
    for request in &continued {
        let [.., before, last] = request.messages.as_slice() else {
            panic!("a continued request carries at least the reply and the block")
        };
        assert!(
            last.role == Role::User && said(last).contains(CONTINUATION_TAG),
            "the block is the last message and it is the user's: {:?}",
            said(last),
        );
        assert_eq!(
            before.role,
            Role::Assistant,
            "sitting directly after the reply it is telling the model to carry on from",
        );
        assert!(
            !said(&request.messages[0]).contains(CONTINUATION_TAG),
            "and never folded into the prompt that opened the turn: {:?}",
            said(&request.messages[0]),
        );
    }

    lead.finish().await;
}

/// A teammate reporting in is not a person taking over: it keeps the turn
/// going like any other drained message and leaves the budget where it was.
///
/// It arrives as the same `Steer` command a person's does — no text, one
/// envelope — so without that distinction a team with something to say every
/// step would refill the five forever and the breaker would never trip. The
/// mirror of the case above it, differing in one line: what the message
/// carries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_teammates_message_does_not_put_the_continuation_budget_back() {
    // Held at the second step request, exactly as the person's case is: the
    // turn has spent one auto-continuation by the time the teammate speaks.
    let (lead, mut held) = Lead::holding(Script::OnlyTalks, 2).await;
    lead.spawn_a_member().await;
    lead.seed_task(TaskStatus::InProgress);

    lead.prompt(PROMPT).await;
    held.reached().await;
    lead.still_turning().await;
    lead.peer_message("peer-1").await;
    held.release();
    assert!(lead.engine.settle(EVENTUALLY).await, "one turn ended inside its budget");

    let requests = lead.lead_requests();
    assert_eq!(
        requests.len(),
        1 + 1 + 1 + 4,
        "the opening request, one continuation, the teammate's message, then only the four \
         the budget had left: {}",
        requests.len(),
    );
    assert_eq!(
        lead.carrying(CONTINUATION_TAG),
        1 + 4,
        "five in the one turn — the budget spent, never put back",
    );
    assert!(
        transcript(&requests[2]).contains(PEER_REPORT),
        "the request after the hold is the one carrying what the teammate said",
    );
    assert_eq!(
        lead.stored_user_text(),
        vec![PROMPT.to_owned()],
        "and a teammate's words are not something a person typed",
    );

    lead.finish().await;
}
