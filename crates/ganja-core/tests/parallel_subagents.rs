//! Several `task` calls in one assistant step, running at the same time.
//!
//! Acceptance criterion 4 of `.omc/plans/2026-08-11-claude-runtime-port.md`,
//! end to end: the children overlap, their results land as they finish rather
//! than in call order, each one's progress stays on its own part, two children
//! asking permission at once produce two dialogs answered one at a time and
//! routed by id, a cancel while both are waiting still ends the turn, the
//! configured cap really caps, and none of the children's own words reach the
//! stream a frontend subscribes to.
//!
//! **Why a router rather than [`ScriptedProvider`]'s one ordered queue.** Every
//! other subagent suite here leans on a single script popped in request order,
//! which is itself the proof that the child is a real loop. That proof stops
//! working the moment two children run at once: the order two concurrent loops
//! reach the provider in is exactly what is under test, so a shared queue would
//! hand child A the answer written for child B. [`Router`] keys each queue by
//! the prompt the turn opened with instead — the parent's, and one per child —
//! so every loop still consumes its own script in its own order and no
//! assertion here depends on which of them gets there first.
//!
//! [`ScriptedProvider`]: ganja_testkit::ScriptedProvider

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::{
    Config, Engine, SessionId, SessionInfo, Storage,
    permission::Permissions,
    protocol::{
        Command, Event, FinishReason, PartBody, PermissionId, PermissionReply, Role, ToolState,
        Usage,
    },
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent},
    storage,
    tool::{Registry, Tool, ToolCtx, ToolError, ToolOutput},
};
use ganja_testkit::{ScriptedProvider, drain, says, tool_call};
use serde_json::{Value, json};

/// How long any wait here is allowed to take before the test calls it a hang.
///
/// Generous on purpose: every one of these deadlines exists to turn a deadlock
/// into a readable failure, not to measure anything. A machine slow enough to
/// need more than this has other problems.
const PATIENCE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// the provider double
// ---------------------------------------------------------------------------

/// A provider that keeps one script queue per conversation, chosen by the
/// prompt that opened it.
struct Router {
    scripts: Mutex<HashMap<String, VecDeque<Vec<ProviderEvent>>>>,
    /// Every request, in the order it arrived — for assertions about *which*
    /// loops ran, never about the order two concurrent ones ran in.
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

impl Router {
    fn new(
        scripts: Vec<(&str, Vec<Vec<ProviderEvent>>)>,
    ) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
        let seen: Arc<Mutex<Vec<ChatRequest>>> = Arc::default();
        let routed = scripts
            .into_iter()
            .map(|(opening, steps)| (opening.to_owned(), steps.into()))
            .collect();

        (
            Arc::new(Self {
                scripts: Mutex::new(routed),
                seen: Arc::clone(&seen),
            }),
            seen,
        )
    }
}

/// The text a request's conversation opened with: the first user message's
/// first text part.
///
/// The **first** rather than the last, because a parent's later requests carry
/// the task calls it made — whose arguments spell out every child's prompt — so
/// keying on anything nearer the tail would route the parent's own second
/// request into a child's queue.
fn opening(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .and_then(|message| message.parts.iter().find_map(|part| part.as_text()))
        .unwrap_or_default()
        .to_owned()
}

#[async_trait]
impl Provider for Router {
    fn id(&self) -> &str {
        "recorder"
    }

    fn accepts_attachment(&self, _mime: &str) -> bool {
        true
    }

    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let opening = opening(&request);
        self.seen
            .lock()
            .expect("the request log is never poisoned")
            .push(request);

        let script = {
            let mut scripts = self.scripts.lock().expect("the scripts are never poisoned");
            let queue = scripts
                .get_mut(&opening)
                .unwrap_or_else(|| panic!("no script is routed to a turn opening {opening:?}"));

            queue
                .pop_front()
                .unwrap_or_else(|| vec![ProviderEvent::Finish(FinishReason::Completed)])
        };

        Ok(stream::iter(script).boxed())
    }
}

// ---------------------------------------------------------------------------
// the tool doubles
// ---------------------------------------------------------------------------

/// What a [`Gate`] call is asked to do, and what it reports back.
#[derive(Default)]
struct Traffic {
    /// Callers that must wait for [`release`] after the barrier.
    held: HashMap<String, Arc<tokio::sync::Notify>>,
}

/// A tool every batched child calls, which is where their overlap becomes
/// observable.
///
/// The barrier is the whole proof: it releases only once `width` callers have
/// arrived, so a sequential executor parks the first one forever and the test
/// fails on its deadline instead of passing by accident. What each caller does
/// *after* the barrier is what lets one of them finish last on purpose.
struct Gate {
    barrier: Arc<tokio::sync::Barrier>,
    traffic: Arc<Mutex<Traffic>>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Gate {
    /// A gate that releases in groups of `width`, with the handles a test reads
    /// it through.
    fn new(width: usize) -> (Arc<Self>, Arc<Mutex<Traffic>>, Arc<AtomicUsize>) {
        let traffic: Arc<Mutex<Traffic>> = Arc::default();
        let peak = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                barrier: Arc::new(tokio::sync::Barrier::new(width)),
                traffic: Arc::clone(&traffic),
                in_flight: Arc::new(AtomicUsize::new(0)),
                peak: Arc::clone(&peak),
            }),
            traffic,
            peak,
        )
    }
}

/// Makes `who` wait at the gate until [`release`] nudges it.
fn hold(traffic: &Mutex<Traffic>, who: &str) {
    traffic
        .lock()
        .expect("the gate is never poisoned")
        .held
        .insert(who.to_owned(), Arc::default());
}

/// Lets a held caller through. A permit, not a broadcast: the nudge is stored
/// when it arrives before the caller does.
fn release(traffic: &Mutex<Traffic>, who: &str) {
    let notify = traffic
        .lock()
        .expect("the gate is never poisoned")
        .held
        .get(who)
        .cloned()
        .expect("nobody is held under that name");
    notify.notify_one();
}

#[async_trait]
impl Tool for Gate {
    fn id(&self) -> &str {
        "gate"
    }

    fn description(&self) -> &str {
        "waits until every caller has arrived"
    }

    fn schema(&self) -> schemars::Schema {
        ganja_testkit::placeholder_schema()
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let who = args["who"].as_str().unwrap_or("?").to_owned();

        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);

        self.barrier.wait().await;

        let held = self
            .traffic
            .lock()
            .expect("the gate is never poisoned")
            .held
            .get(&who)
            .cloned();
        if let Some(held) = held {
            held.notified().await;
        }

        self.in_flight.fetch_sub(1, Ordering::SeqCst);

        Ok(ToolOutput {
            title: "gate".to_owned(),
            output: format!("{who} passed"),
            metadata: json!({}),
        })
    }
}

/// A tool that answers with a canned line and counts that it ran, under
/// whatever name it is given — `webfetch` here, because that name is
/// ask-gated by the builtin rules and this suite needs a dialog.
struct Canned {
    id: &'static str,
    calls: Arc<AtomicUsize>,
}

impl Canned {
    fn new(id: &'static str) -> (Arc<Self>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                id,
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }
}

#[async_trait]
impl Tool for Canned {
    fn id(&self) -> &str {
        self.id
    }

    fn description(&self) -> &str {
        "answers with a canned output"
    }

    fn schema(&self) -> schemars::Schema {
        ganja_testkit::placeholder_schema()
    }

    async fn run(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        Ok(ToolOutput {
            title: self.id.to_owned(),
            output: "canned".to_owned(),
            metadata: json!({}),
        })
    }
}

// ---------------------------------------------------------------------------
// scripts and engines
// ---------------------------------------------------------------------------

/// One assistant step delegating to every `(agent, prompt)` in order.
///
/// Each call gets a distinct provider id, because two `ToolCallStart`s sharing
/// one id are the same call streamed twice and the second is dropped.
fn delegates(children: &[(&str, &str)]) -> Vec<ProviderEvent> {
    let mut script = Vec::new();
    for (index, (agent, prompt)) in children.iter().enumerate() {
        let id = format!("call_{index}");
        script.push(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: "task".to_owned(),
        });
        script.push(ProviderEvent::ToolCallDelta {
            id: id.clone(),
            json: json!({
                "description": *prompt,
                "prompt": *prompt,
                "subagent_type": *agent,
            })
            .to_string(),
        });
        script.push(ProviderEvent::ToolCallEnd { id });
    }
    script.push(ProviderEvent::Finish(FinishReason::Completed));

    script
}

/// A config whose parent agent may delegate without a dialog, optionally
/// naming a fan-out cap.
///
/// The allow is written on the **build agent alone**, which matters twice: it
/// is where the gate actually reads a parent's rules from, and only denials
/// travel down to a subagent — so it cannot quietly authorize anything a child
/// does. Without it every test here would have to answer two dialogs before
/// reaching the ones it is about.
fn config(concurrency: Option<usize>) -> Config {
    let mut asked = json!({
        "agent": { "build": { "permission": { "task": "allow" } } }
    });
    if let Some(concurrency) = concurrency {
        asked["agents"] = json!({ "concurrency": concurrency });
    }

    serde_json::from_value(asked).expect("the fixture is a config")
}

/// An engine over `provider` offering `tools`, running the builtin agents.
fn engine(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>, config: &Config) -> Engine {
    Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(tools)),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(config))
    .with_concurrency(config.agents.concurrency())
}

/// The final state of every `task` part on the stream, keyed by the
/// description the call was made with — which is this suite's name for a
/// child.
fn task_parts(seen: &[Event]) -> HashMap<String, ToolState> {
    let mut found = HashMap::new();
    for event in seen {
        let (Event::PartUpdated { part, .. } | Event::PartStarted { part, .. }) = event else {
            continue;
        };
        let PartBody::Tool { tool, state, .. } = &part.body else {
            continue;
        };
        if tool != "task" {
            continue;
        }
        let described = match state {
            ToolState::Pending { .. } => None,
            ToolState::Running { input, .. }
            | ToolState::Completed { input, .. }
            | ToolState::Error { input, .. } => input["description"].as_str().map(str::to_owned),
        };
        if let Some(described) = described {
            found.insert(described, state.clone());
        }
    }

    found
}

/// The order in which the stream reported `task` calls finishing, named by the
/// part they finished on.
fn finish_order(seen: &[Event]) -> Vec<String> {
    seen.iter()
        .filter_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    tool,
                    state: ToolState::Completed { input, .. },
                    ..
                } if tool == "task" => input["description"].as_str().map(str::to_owned),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// the tests
// ---------------------------------------------------------------------------

/// Two children reach an ask-gated tool at the same time, and both dialogs
/// stand open together.
///
/// **The clause this whole wave was written test-first for.** Under one shared
/// reply cell the second child's request evicts the first's, and the turn ends
/// as something other than the two answers it was owed; under a registry keyed
/// by request id, both wait, and the reply that names each one reaches it. The
/// test never answers the first dialog until it has seen the second, so a
/// build that can only hold one request at a time deadlocks here rather than
/// passing with a subtly wrong transcript.
///
/// The replies go out **newest first**, which is the routing assertion: an
/// implementation that handed a reply to "whatever is waiting" would give
/// child alpha the answer addressed to child beta and neither test below it
/// would notice.
#[tokio::test]
async fn two_children_asking_at_once_hold_two_dialogs_answered_by_id() {
    let (provider, _requests) = Router::new(vec![
        (
            "delegate two ways",
            vec![
                delegates(&[("general", "alpha-child"), ("general", "beta-child")]),
                says("both children are back"),
            ],
        ),
        (
            "alpha-child",
            vec![
                tool_call("webfetch", json!({ "url": "https://alpha.test" })),
                says("alpha found it"),
            ],
        ),
        (
            "beta-child",
            vec![
                tool_call("webfetch", json!({ "url": "https://beta.test" })),
                says("beta found it"),
            ],
        ),
    ]);
    let (webfetch, fetches) = Canned::new("webfetch");
    let engine = engine(provider, vec![webfetch], &config(None));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate two ways".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let mut open: Vec<PermissionId> = Vec::new();
    let mut seen = Vec::new();
    let drained = tokio::time::timeout(PATIENCE, async {
        loop {
            let event = events
                .next()
                .await
                .expect("the turn should finish before the stream ends");
            if let Event::PermissionRequested { id, tool, .. } = &event {
                assert_eq!(
                    tool, "webfetch",
                    "the parent's own delegation runs under an allow rule: {event:?}"
                );
                open.push(id.clone());
                if open.len() == 2 {
                    // Newest first. Nothing before this point answered
                    // anything, so both requests really were open at once.
                    for id in open.iter().rev() {
                        engine
                            .send(Command::ReplyPermission {
                                id: id.clone(),
                                reply: PermissionReply::Once,
                            })
                            .await
                            .expect("a reply is never refused");
                    }
                }
            }
            let finished = matches!(event, Event::MessageFinished { .. });
            seen.push(event);
            if finished {
                return seen;
            }
        }
    })
    .await
    .expect("two concurrent children must be able to hold two dialogs at once");

    assert_eq!(
        open.len(),
        2,
        "both children asked, and neither was answered until both had: {open:?}"
    );
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        2,
        "each reply reached the child that asked, so both calls ran"
    );

    let parts = task_parts(&drained);
    for (child, answer) in [
        ("alpha-child", "alpha found it"),
        ("beta-child", "beta found it"),
    ] {
        let Some(ToolState::Completed { output, .. }) = parts.get(child) else {
            panic!("{child} did not complete: {parts:?}");
        };
        assert!(
            output.contains(answer),
            "{child}'s part carries its own child's answer: {output}"
        );
    }

    let Some(Event::MessageFinished { reason, .. }) = drained.last() else {
        panic!("a turn always finishes");
    };
    assert_eq!(*reason, FinishReason::Completed);
}

/// Three `task` calls in one step run at the same time, and their results are
/// applied as they finish rather than in the order they were called.
///
/// The barrier proves the overlap: it opens only when all three children have
/// reached it, so a sequential executor never gets past the first. The held
/// child proves the fan-in: it is the call the model made **first** and the one
/// that finishes **last**, which is a transcript no call-ordered executor can
/// produce.
#[tokio::test]
async fn three_task_calls_run_concurrently_and_land_as_they_finish() {
    let (provider, _requests) = Router::new(vec![
        (
            "delegate three ways",
            vec![
                delegates(&[
                    ("general", "alpha-child"),
                    ("general", "beta-child"),
                    ("general", "gamma-child"),
                ]),
                says("all three are back"),
            ],
        ),
        (
            "alpha-child",
            vec![
                tool_call("gate", json!({ "who": "alpha-child" })),
                says("alpha is done"),
            ],
        ),
        (
            "beta-child",
            vec![
                tool_call("gate", json!({ "who": "beta-child" })),
                says("beta is done"),
            ],
        ),
        (
            "gamma-child",
            vec![
                tool_call("gate", json!({ "who": "gamma-child" })),
                says("gamma is done"),
            ],
        ),
    ]);
    let (gate, traffic, _peak) = Gate::new(3);
    // The first call the model made is the last one let go.
    hold(&traffic, "alpha-child");
    let engine = engine(provider, vec![gate], &config(None));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate three ways".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let mut seen = Vec::new();
    let drained = tokio::time::timeout(PATIENCE, async {
        loop {
            let event = events
                .next()
                .await
                .expect("the turn should finish before the stream ends");
            let completed = matches!(&event, Event::PartUpdated { part, .. } if matches!(
                &part.body,
                PartBody::Tool { tool, state: ToolState::Completed { .. }, .. } if tool == "task"
            ));
            let finished = matches!(event, Event::MessageFinished { .. });
            seen.push(event);
            // Two of the three are home; the one still at the gate is the one
            // the model asked for first.
            if completed && finish_order(&seen).len() == 2 {
                release(&traffic, "alpha-child");
            }
            if finished {
                return seen;
            }
        }
    })
    .await
    .expect("three children must run concurrently, or the barrier never opens");

    let order = finish_order(&drained);
    assert_eq!(order.len(), 3, "all three delegations finished: {order:?}");
    assert_eq!(
        order.last().map(String::as_str),
        Some("alpha-child"),
        "the call made first finished last, so results land as they complete: {order:?}"
    );

    let parts = task_parts(&drained);
    for child in ["alpha-child", "beta-child", "gamma-child"] {
        assert!(
            matches!(parts.get(child), Some(ToolState::Completed { .. })),
            "{child} completed: {parts:?}"
        );
    }

    let Some(Event::MessageFinished { reason, .. }) = drained.last() else {
        panic!("a turn always finishes");
    };
    assert_eq!(*reason, FinishReason::Completed);
}

/// Each batched child reports its progress on the part its own call opened.
///
/// One `PartUpdated` stream, three part ids, and every `{current_tool,
/// toolcalls}` reading has to be filed under the call it belongs to: a shared
/// part would show one child's tool name on another child's row.
#[tokio::test]
async fn each_child_reports_progress_on_its_own_part() {
    let (provider, _requests) = Router::new(vec![
        (
            "delegate two ways",
            vec![
                delegates(&[("general", "alpha-child"), ("general", "beta-child")]),
                says("both are back"),
            ],
        ),
        (
            "alpha-child",
            vec![
                tool_call("gate", json!({ "who": "alpha-child" })),
                says("alpha is done"),
            ],
        ),
        (
            "beta-child",
            vec![
                tool_call("gate", json!({ "who": "beta-child" })),
                says("beta is done"),
            ],
        ),
    ]);
    let (gate, _traffic, _peak) = Gate::new(2);
    let engine = engine(provider, vec![gate], &config(None));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate two ways".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let drained = tokio::time::timeout(PATIENCE, drain(&mut events))
        .await
        .expect("two children must run concurrently, or the barrier never opens");

    // Every running-progress update, grouped by the part it was reported on.
    let mut progress: HashMap<String, Vec<Value>> = HashMap::new();
    for event in &drained {
        let Event::PartUpdated { part, .. } = event else {
            continue;
        };
        let PartBody::Tool {
            tool,
            state: ToolState::Running {
                input, metadata, ..
            },
            ..
        } = &part.body
        else {
            continue;
        };
        if tool == "task" && !metadata.is_null() {
            progress
                .entry(part.id.as_str().to_owned())
                .or_default()
                .push(json!({ "input": input, "metadata": metadata }));
        }
    }

    assert_eq!(
        progress.len(),
        2,
        "two calls, two parts carrying progress: {progress:?}"
    );
    for (part_id, reports) in &progress {
        let described: Vec<&str> = reports
            .iter()
            .filter_map(|report| report["input"]["description"].as_str())
            .collect();
        assert!(
            described.windows(2).all(|pair| pair[0] == pair[1]),
            "part {part_id} reported for more than one child: {described:?}"
        );
        let last = reports.last().expect("a part with no reports is not here");
        assert_eq!(
            last["metadata"]["toolcalls"],
            json!(1),
            "each child's own call count stayed on its own part: {last}"
        );
        assert_eq!(
            last["metadata"]["current_tool"],
            json!("gate"),
            "and names what that child was running: {last}"
        );
    }
}

/// The configured cap is the number of children that may run at once.
///
/// Four calls under a cap of two: the gate releases in pairs, so the run only
/// makes progress if exactly two are ever in flight — and the peak it recorded
/// says so directly. An uncapped executor would put all four at the gate, and
/// the peak would say four.
#[tokio::test]
async fn the_configured_cap_is_how_many_children_run_at_once() {
    let children = [
        ("general", "alpha-child"),
        ("general", "beta-child"),
        ("general", "gamma-child"),
        ("general", "delta-child"),
    ];
    let mut scripts = vec![(
        "delegate four ways",
        vec![delegates(&children), says("all four are back")],
    )];
    for (_, prompt) in &children {
        scripts.push((
            *prompt,
            vec![
                tool_call("gate", json!({ "who": *prompt })),
                says("a child is done"),
            ],
        ));
    }
    let (provider, _requests) = Router::new(scripts);

    let config = config(Some(2));
    assert_eq!(
        config.agents.concurrency(),
        2,
        "the knob a frontend reads is the one under test"
    );
    let (gate, _traffic, peak) = Gate::new(2);
    let engine = engine(provider, vec![gate], &config);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate four ways".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let drained = tokio::time::timeout(PATIENCE, drain(&mut events))
        .await
        .expect("a cap of two must still let pairs through");

    assert_eq!(
        peak.load(Ordering::SeqCst),
        2,
        "two at a time, never more and never fewer"
    );
    assert_eq!(
        finish_order(&drained).len(),
        4,
        "and all four children still ran"
    );
}

/// A cancel while two dialogs are queued ends the turn, answers both requests,
/// and leaves the engine idle.
///
/// The pre-mortem's own case: the cell that used to hold one request now holds
/// several, and every one of them is owed exactly one terminal event whether it
/// was answered or abandoned. A frontend that opened two dialogs must be able
/// to retire both.
#[tokio::test]
async fn a_cancel_while_two_dialogs_are_queued_answers_both_and_ends_the_turn() {
    let (provider, _requests) = Router::new(vec![
        (
            "delegate two ways",
            vec![
                delegates(&[("general", "alpha-child"), ("general", "beta-child")]),
                says("unreachable"),
            ],
        ),
        (
            "alpha-child",
            vec![
                tool_call("webfetch", json!({ "url": "https://alpha.test" })),
                says("unreachable"),
            ],
        ),
        (
            "beta-child",
            vec![
                tool_call("webfetch", json!({ "url": "https://beta.test" })),
                says("unreachable"),
            ],
        ),
    ]);
    let (webfetch, fetches) = Canned::new("webfetch");
    let engine = engine(provider, vec![webfetch], &config(None));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate two ways".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let mut asked: Vec<PermissionId> = Vec::new();
    let mut seen = Vec::new();
    let drained = tokio::time::timeout(PATIENCE, async {
        loop {
            let event = events
                .next()
                .await
                .expect("the turn should finish before the stream ends");
            if let Event::PermissionRequested { id, .. } = &event {
                asked.push(id.clone());
                if asked.len() == 2 {
                    engine
                        .send(Command::CancelTurn)
                        .await
                        .expect("a cancel is never refused");
                }
            }
            let finished = matches!(event, Event::MessageFinished { .. });
            seen.push(event);
            if finished {
                return seen;
            }
        }
    })
    .await
    .expect("a cancel must reach two queued dialogs");

    assert_eq!(asked.len(), 2, "both children really did ask");
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        0,
        "and neither call ran: the cancel refused them both"
    );

    let answered: Vec<&PermissionId> = drained
        .iter()
        .filter_map(|event| match event {
            Event::PermissionReplied { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    for id in &asked {
        assert_eq!(
            answered.iter().filter(|replied| **replied == id).count(),
            1,
            "every request is answered exactly once, cancelled or not: {answered:?}"
        );
    }

    let Some(Event::MessageFinished { reason, .. }) = drained.last() else {
        panic!("a turn always finishes");
    };
    assert_eq!(*reason, FinishReason::Cancelled);

    engine
        .send(Command::SendPrompt {
            text: "delegate two ways".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a cancelled turn leaves the engine idle");
}

/// Concurrency changes nothing about whose conversation reaches the stream.
///
/// Three children speak three sentences nobody else says. The parent's own
/// `task` parts are entitled to carry them back; every other rendering on the
/// stream carrying one is the leak the `subagent-events-stay-off-the-stream`
/// deviation exists to prevent, and `run --attach`'s "no session filter needed"
/// posture rests on it.
#[tokio::test]
async fn concurrent_children_stay_off_the_subscribed_stream() {
    let (provider, _requests) = Router::new(vec![
        (
            "delegate three ways",
            vec![
                delegates(&[
                    ("general", "alpha-child"),
                    ("general", "beta-child"),
                    ("general", "gamma-child"),
                ]),
                says("the parent speaks for itself"),
            ],
        ),
        ("alpha-child", vec![says("only alpha utters this")]),
        ("beta-child", vec![says("only beta utters this")]),
        ("gamma-child", vec![says("only gamma utters this")]),
    ]);
    let engine = engine(provider, Vec::new(), &config(None));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate three ways".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let drained = tokio::time::timeout(PATIENCE, drain(&mut events))
        .await
        .expect("three children finish");

    let sentinels = [
        "only alpha utters this",
        "only beta utters this",
        "only gamma utters this",
    ];

    // The one place they are allowed: each parent part carries its own child's
    // answer, which is also what stops the sweep below passing vacuously.
    let parts = task_parts(&drained);
    for (child, sentinel) in ["alpha-child", "beta-child", "gamma-child"]
        .into_iter()
        .zip(sentinels)
    {
        let Some(ToolState::Completed { output, .. }) = parts.get(child) else {
            panic!("{child} did not complete: {parts:?}");
        };
        assert!(
            output.contains(sentinel),
            "{child}'s own part carries its answer: {output}"
        );
    }

    for event in &drained {
        for rendering in published(event) {
            for sentinel in sentinels {
                assert!(
                    !rendering.contains(sentinel),
                    "a child's own words reached the stream: {rendering}"
                );
            }
        }
    }

    let roles: Vec<Role> = drained
        .iter()
        .filter_map(|event| match event {
            Event::MessageStarted { message, .. } => Some(message.role),
            _ => None,
        })
        .collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant],
        "one prompt and one assistant turn, whatever ran underneath: {roles:?}"
    );
}

/// Everything of an event a frontend would render, minus the parent's own
/// `task` parts — which are the one channel a child may reach the stream
/// through.
fn published(event: &Event) -> Vec<String> {
    fn render(part: &ganja_core::protocol::Part) -> Option<String> {
        let delegated = matches!(&part.body, PartBody::Tool { tool, .. } if tool == "task");

        (!delegated).then(|| format!("{:?}", part.body))
    }

    match event {
        Event::MessageStarted { message, .. } => message.parts.iter().filter_map(render).collect(),
        Event::PartStarted { part, .. } | Event::PartUpdated { part, .. } => {
            render(part).into_iter().collect()
        }
        Event::PartDelta { delta, .. } => vec![delta.clone()],
        _ => Vec::new(),
    }
}

/// What a resumed session reads back is the transcript in **call** order, even
/// though the results landed in completion order.
///
/// The parts were opened as the model streamed the calls and are rewritten in
/// place as each child comes home, so the message a later process loads is the
/// one the model itself would have built. Pinned by a round trip through the
/// store, the same way steering's own ordering is.
#[tokio::test]
async fn a_stored_turn_replays_its_calls_in_call_order() {
    let directory = tempfile::TempDir::new().expect("a temporary directory is creatable");
    let created = 1;
    let parent = SessionId::ascending();
    let storage = Storage::open(directory.path().join("storage"));
    storage
        .save_info(&SessionInfo {
            id: parent.clone(),
            version: storage::VERSION,
            title: Some("seeded".to_owned()),
            created,
            updated: created,
            usage: Usage::default(),
            context_tokens: 0,
            summary: None,
            agent: None,
            model: None,
            effort: None,
            activated_tools: std::collections::BTreeSet::new(),
            parent: None,
            revert: None,
        })
        .expect("the seeded record writes");

    let (provider, _requests) = Router::new(vec![
        (
            "delegate three ways",
            vec![
                delegates(&[
                    ("general", "alpha-child"),
                    ("general", "beta-child"),
                    ("general", "gamma-child"),
                ]),
                says("all three are back"),
            ],
        ),
        (
            "alpha-child",
            vec![
                tool_call("gate", json!({ "who": "alpha-child" })),
                says("alpha is done"),
            ],
        ),
        (
            "beta-child",
            vec![
                tool_call("gate", json!({ "who": "beta-child" })),
                says("beta is done"),
            ],
        ),
        (
            "gamma-child",
            vec![
                tool_call("gate", json!({ "who": "gamma-child" })),
                says("gamma is done"),
            ],
        ),
    ]);
    let (gate, traffic, _peak) = Gate::new(3);
    hold(&traffic, "alpha-child");

    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(vec![gate])),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(ganja_testkit::agent_registry(&config(None)));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&parent).await.expect("the session loads");

    engine
        .send(Command::SendPrompt {
            text: "delegate three ways".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let mut seen = Vec::new();
    let drained = tokio::time::timeout(PATIENCE, async {
        loop {
            let event = events
                .next()
                .await
                .expect("the turn should finish before the stream ends");
            let completed = matches!(&event, Event::PartUpdated { part, .. } if matches!(
                &part.body,
                PartBody::Tool { tool, state: ToolState::Completed { .. }, .. } if tool == "task"
            ));
            let finished = matches!(event, Event::MessageFinished { .. });
            seen.push(event);
            if completed && finish_order(&seen).len() == 2 {
                release(&traffic, "alpha-child");
            }
            if finished {
                return seen;
            }
        }
    })
    .await
    .expect("three children run concurrently");

    assert_eq!(
        finish_order(&drained).last().map(String::as_str),
        Some("alpha-child"),
        "the first call finished last, or this proves nothing about ordering"
    );

    // Wait for the turn's own closing writes before reading the store.
    engine
        .send(Command::SendPrompt {
            text: "delegate three ways".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a finished turn leaves the engine idle");

    let replayed: Vec<String> = storage
        .load_transcript(&parent)
        .expect("the parent's transcript reads back")
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match &part.body {
            PartBody::Tool {
                tool,
                state: ToolState::Completed { input, .. },
                ..
            } if tool == "task" => input["description"].as_str().map(str::to_owned),
            _ => None,
        })
        .collect();

    assert_eq!(
        replayed,
        vec![
            "alpha-child".to_owned(),
            "beta-child".to_owned(),
            "gamma-child".to_owned()
        ],
        "the stored turn replays in the order the model called, not the order the children came home"
    );
}

/// Ordinary tool calls keep resolving one after another, whatever the batch
/// executor does around them.
///
/// The sequential promise in the step loop's own comment — a later call may
/// depend on an earlier one's effect — is what makes `write` then `read`
/// meaningful, and it is not the `task` arm's to retire.
#[tokio::test]
async fn ordinary_calls_still_resolve_one_after_another() {
    let order: Arc<Mutex<Vec<String>>> = Arc::default();

    struct Sequential {
        id: &'static str,
        order: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Tool for Sequential {
        fn id(&self) -> &str {
            self.id
        }

        fn description(&self) -> &str {
            "records that it ran, and when"
        }

        fn schema(&self) -> schemars::Schema {
            ganja_testkit::placeholder_schema()
        }

        async fn run(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
            self.order
                .lock()
                .expect("the order log is never poisoned")
                .push(format!("{} in", self.id));
            tokio::task::yield_now().await;
            self.order
                .lock()
                .expect("the order log is never poisoned")
                .push(format!("{} out", self.id));

            Ok(ToolOutput {
                title: self.id.to_owned(),
                output: "ran".to_owned(),
                metadata: json!({}),
            })
        }
    }

    let mut step = Vec::new();
    for (index, tool) in ["first", "second"].into_iter().enumerate() {
        let id = format!("call_{index}");
        step.push(ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: tool.to_owned(),
        });
        step.push(ProviderEvent::ToolCallDelta {
            id: id.clone(),
            json: "{}".to_owned(),
        });
        step.push(ProviderEvent::ToolCallEnd { id });
    }
    step.push(ProviderEvent::Finish(FinishReason::Completed));

    let (provider, _requests) = ScriptedProvider::new(vec![step, says("both ran")]);
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(vec![
            Arc::new(Sequential {
                id: "first",
                order: Arc::clone(&order),
            }) as Arc<dyn Tool>,
            Arc::new(Sequential {
                id: "second",
                order: Arc::clone(&order),
            }) as Arc<dyn Tool>,
        ])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "run both".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    tokio::time::timeout(PATIENCE, drain(&mut events))
        .await
        .expect("two ordinary calls finish");

    assert_eq!(
        *order.lock().expect("the order log is never poisoned"),
        vec![
            "first in".to_owned(),
            "first out".to_owned(),
            "second in".to_owned(),
            "second out".to_owned()
        ],
        "an ordinary call finishes before the next one starts"
    );
}

/// Every child that ends fires its own `SubagentStop`, naming the agent it ran
/// as and how it ended.
///
/// The assertion W3 could not make: it wired the hook (**D461**) with no
/// integration test, because a real child needs `with_agents` and this suite's
/// fixture is the first one that builds several of them. Three children, three
/// envelopes — not one for the batch, and not one lost to a sibling that
/// finished at the same moment.
///
/// **One file per firing, deliberately.** W3's own ledger fixture appends to a
/// single file, which is safe only while one hook writes at a time; three
/// children ending concurrently is exactly the case that is not. Each run gets
/// a file of its own through `mktemp`, so an interleaved write cannot make a
/// pair of envelopes unreadable and quietly turn this test into a flake.
#[cfg(unix)]
#[tokio::test]
async fn every_child_that_ends_fires_its_own_subagent_stop() {
    use std::collections::BTreeMap;

    use ganja_core::{
        config::{HookCommand, HookHandler, HookMatcher},
        hook::{HookEvent, Hooks},
    };

    let directory = tempfile::TempDir::new().expect("a temporary directory is creatable");
    let ledger = directory.path().join("stops");
    std::fs::create_dir(&ledger).expect("the ledger directory is creatable");

    let mut hooks_config = BTreeMap::new();
    hooks_config.insert(
        HookEvent::SubagentStop.name().to_owned(),
        vec![HookMatcher {
            matcher: None,
            hooks: vec![HookHandler::Command(HookCommand {
                command: format!("f=$(mktemp {}/stop.XXXXXX); cat > \"$f\"", ledger.display()),
                timeout: None,
            })],
        }],
    );
    let hooks = Hooks::new(&hooks_config, directory.path()).expect("the block compiles");

    let (provider, _requests) = Router::new(vec![
        (
            "delegate three ways",
            vec![
                delegates(&[
                    ("general", "alpha-child"),
                    ("explore", "beta-child"),
                    ("general", "gamma-child"),
                ]),
                says("all three are back"),
            ],
        ),
        ("alpha-child", vec![says("alpha is done")]),
        ("beta-child", vec![says("beta is done")]),
        ("gamma-child", vec![says("gamma is done")]),
    ]);
    let engine = engine(provider, Vec::new(), &config(None)).with_hooks(hooks);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate three ways".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    tokio::time::timeout(PATIENCE, drain(&mut events))
        .await
        .expect("three children finish");

    // A child's hook fires as that child ends, which is before the turn's own
    // finish event — but the writes are files, and a file is not there the
    // instant the shell that makes it is spawned.
    let mut written = Vec::new();
    for _ in 0..500 {
        written = std::fs::read_dir(&ledger)
            .expect("the ledger directory reads")
            .filter_map(|entry| std::fs::read_to_string(entry.expect("an entry reads").path()).ok())
            .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
            .collect();
        if written.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        written.len(),
        3,
        "one firing per child, not one per batch: {written:?}"
    );
    let mut agents: Vec<&str> = written
        .iter()
        .map(|envelope| {
            assert_eq!(envelope["hook_event_name"], "SubagentStop", "{envelope:?}");
            assert_eq!(
                envelope["outcome"], "completed",
                "every child ran to its own end: {envelope:?}"
            );
            envelope["agent"].as_str().expect("the envelope names one")
        })
        .collect();
    agents.sort_unstable();
    assert_eq!(
        agents,
        vec!["explore", "general", "general"],
        "each firing names the agent that actually ran"
    );
}
