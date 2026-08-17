//! The task tool: the model hands work to a subagent and reads back one answer.
//!
//! Spec: upstream `packages/opencode/src/tool/task.ts`. A call runs a **real
//! second agent loop** — its own history, its own rules, its own model — and
//! returns that loop's last words wrapped in upstream's XML. Everything the
//! subagent did in between is the subagent's business; the parent reads a
//! result, not a transcript.
//!
//! # What is here and what is not
//!
//! Running a conversation is not something a tool knows how to do: it needs a
//! provider, an agent roster, a permission tier and a second agent loop, none
//! of which this layer may name. So the running is somebody else's, reached
//! through [`Subagents`] — one call in, one [`Delegated`] answer out — and what
//! stays here is the part that really is the tool's: the schema, the roster the
//! model is offered, the arguments, and the bytes the parent model finally
//! reads. The engine's implementation, and the reasons a child loop is built
//! the way it is, live in the engine's own `subagent` module.
//!
//! That seam is also the depth guard, restated: [`ToolCtx::spawn`] is [`None`]
//! on every turn a subagent runs, so a child that somehow held this tool would
//! have nothing to delegate through.
//!
//! # The second door: `name` starts a teammate (**D501**)
//!
//! Spec for this half: Claude Code's teammates — §4.1's spawn sequence, whose
//! `Task` call takes a teammate's name and answers before the work does.
//! Upstream opencode has no teammates and no counterpart to any of it.
//!
//! A call carrying `name` is not a delegation at all. It **starts a member of
//! this session's team** and returns as soon as that member is running: the
//! prompt travels to it through a mailbox rather than being awaited here, and
//! everything said to it afterwards travels the same way. So the two doors of
//! one tool differ in the one thing that matters to the model reading the
//! result — whether the answer it just read is the work, or the news that the
//! work has started.
//!
//! `backend` names the surface that member runs on and is **never inferred**:
//! its absence means [the default](TeammateSpawn::backend), and a value nothing
//! answers to is refused by name rather than quietly falling back. Which values
//! exist, and what each costs, is the engine's answer — this layer carries the
//! argument and reads back the sentence.
//!
//! [`ToolCtx::spawn`]: crate::ToolCtx::spawn

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{Tool, ToolCtx, ToolError, ToolOutput};

/// The tool id, which is also the permission key. Both are a permanent
/// commitment: a rule stored under this name has to keep meaning what it meant.
pub const ID: &str = "task";

/// What the model is told the tool is for, ported verbatim from upstream
/// `packages/opencode/src/tool/task.txt` (MIT; see `THIRD_PARTY_NOTICES.md`).
pub const DESCRIPTION: &str = include_str!("task.txt");

/// Header upstream appends the per-caller agent roster under
/// (`tool/registry.ts`, `describeTask`).
pub const ROSTER_HEADER: &str = "Available agent types and the tools they have access to:";

/// What upstream shows for an agent that describes itself nowhere.
const NO_DESCRIPTION: &str = "This subagent should only be called manually by the user.";

/// What a call reads when this build offered the tool without anything behind
/// it. Not reachable through the engine, which registers the tool only when it
/// has agents to spawn.
const NO_AGENTS: &str = "This session has no subagents to delegate to.";

/// What a call naming a teammate reads when this session leads no team.
///
/// Public because the engine's own implementation of [`Subagents`] answers with
/// it too: a session with a team and a session without one should refuse in one
/// sentence, and the sentence a model reads is this tool's to write.
pub const NO_TEAM: &str = "This session has no team to start a teammate in.";

/// What a call reads when it named a surface but no teammate.
///
/// Refused rather than ignored: `backend` is meaningless to a delegation, and
/// the likeliest way to send one is a `name` that was meant to be there and
/// was not — which, ignored, would deliver a subagent where a teammate was
/// asked for and say nothing about it.
const BACKEND_WITHOUT_NAME: &str = "`backend` names the surface a teammate runs on, so it goes with `name`. A call without a `name` delegates to a subagent, which has no surface to choose.";

/// What a call reads when it asked to continue a conversation and start a new
/// one at once.
const NAME_WITH_TASK_ID: &str = "`task_id` continues a delegated conversation and `name` starts a teammate, which is a new one. Name one or the other.";

/// What a started teammate reads back as, ahead of the engine's own account of
/// where the prompt went.
const STARTED: &str = "Teammate started:";

/// What the model passes to `task`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// A short (3-5 words) description of the task
    description: String,
    /// The task for the agent to perform
    prompt: String,
    /// The type of specialized agent to use for this task
    subagent_type: String,
    /// The id of a previous task to continue, as returned by an earlier call
    #[serde(default)]
    task_id: Option<String>,
    /// The name to give a teammate. Naming one starts a member of this
    /// session's team, which runs on past this call and is reached afterwards
    /// with send_message, instead of delegating a subagent and waiting for its
    /// answer
    #[serde(default)]
    name: Option<String>,
    /// Which surface a teammate runs on: "in-process", "pane" or "claude".
    /// Absent means in-process. Only meaningful alongside name
    #[serde(default)]
    backend: Option<String>,
}

/// One subagent as the model is offered it.
///
/// The roster is assembled where the agents are — which of them a caller may
/// delegate to is that caller's rules' business — and arrives here as the two
/// strings the description is written out of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offered {
    /// What the model names in `subagent_type`.
    pub name: String,
    /// The one line it is listed under, when it describes itself at all.
    pub description: Option<String>,
}

/// One delegated conversation, as the model asked for it.
#[derive(Clone, Debug)]
pub struct Delegation {
    /// The subagent the model named. Nothing here has checked that anything
    /// goes by that name; see [`Unanswered::Unknown`].
    pub subagent_type: String,
    /// What the subagent is being asked to do.
    pub prompt: String,
    /// The model's own few words for the task, which title the parent's part
    /// and the child's stored session.
    pub description: String,
    /// An earlier conversation to continue, named by the id an earlier call
    /// reported.
    pub task_id: Option<String>,
}

/// What a finished delegation reports back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delegated {
    /// The conversation it ran in, which a later call may name to continue it.
    pub task_id: String,
    /// The subagent that ran.
    pub agent: String,
    /// The model it ran on, which is its own when it named one.
    pub model: String,
    /// Its last words, which are the answer the parent model reads.
    pub text: String,
    /// How many tools it called, which the parent's inline row shows.
    pub toolcalls: usize,
    /// The calls it made, in order, each named the way its running row named
    /// it — capped by the engine, with `toolcalls` the true total.
    pub calls: Vec<String>,
}

/// Why a delegation came back without an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unanswered {
    /// Nothing this session may spawn goes by that name. Reported rather than
    /// asked about beforehand, so that the sentence the model reads and retries
    /// on is written in exactly one place — [`TaskTool::run`], below.
    Unknown,
    /// The parent's cancel reached the child.
    Cancelled,
    /// The child's loop ended without an answer.
    Failed {
        /// The conversation it failed in, which the parent model is shown
        /// alongside the message.
        task_id: String,
        /// What went wrong, in the terms the parent model reads next.
        message: String,
    },
}

/// One teammate a call asked to start, as the model named it.
///
/// The team's own half — which team, which lead, whose store, and a name
/// nothing else in the team answers to — is the engine's to fill in. What
/// crosses is what this call decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeammateSpawn {
    /// The name asked for. Nothing here has checked that it is a name a team
    /// will take, or that it is free; both are the far side's to answer.
    pub name: String,
    /// Which surface it should run on, as the model spelled it. [`None`] is
    /// the far side's default rather than a value chosen here — a tool that
    /// wrote the default in would be a second place for it to drift.
    pub backend: Option<String>,
    /// The `subagent_type` the call named, which the team records as the kind
    /// of agent this teammate is.
    pub agent_type: String,
    /// What it is being asked to do. Travels to the teammate through its
    /// mailbox rather than being awaited here.
    pub prompt: String,
}

/// What a started teammate reports back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Teammated {
    /// The name it really answers to, which is not always the one that was
    /// asked for: a team resolves collisions rather than refusing them, so a
    /// second `worker` is a `worker` with a counter. Reported back so the
    /// transcript carries the name a later `send_message` has to use.
    pub name: String,
    /// Its `<name>@<team>` identity.
    pub agent_id: String,
    /// The surface it is running on, spelled as the `backend` argument spells
    /// it — echoed from the far side rather than from the arguments, so a
    /// defaulted value is visible instead of assumed.
    pub backend: String,
    /// What became of the prompt, in the terms the model reads next. The far
    /// side's sentence: where a spawn's instructions go is its fact.
    pub note: String,
}

/// Why a teammate was not started.
///
/// One sentence rather than a kind, because every reason is the far side's —
/// a `backend` value nothing answers to, a surface this build has not got, a
/// name a team refused, a mailbox that would not open — and a kind enumerated
/// here would be this crate holding half of a vocabulary it cannot see the
/// other half of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotSpawned {
    /// What went wrong, as the model reads it and retries on it.
    pub reason: String,
}

/// Runs a whole subagent conversation on a `task` call's behalf, and starts
/// this session's teammates.
///
/// Deliberately says nothing about *how*: a conversation, a provider, an agent
/// loop, a team and a mailbox are the engine's vocabulary, and a tool that
/// named them would be a tool the engine cannot be assembled without. What
/// crosses is a request of strings and an answer of strings.
///
/// [`std::fmt::Debug`] is required because [`ToolCtx`] derives it, and an
/// implementation is expected to render where the call sits rather than the
/// machinery behind it.
#[async_trait]
pub trait Subagents: std::fmt::Debug + Send + Sync {
    /// Runs `request` to its finish and reports what the subagent said.
    ///
    /// `cancel` is the call's own token: firing it is what ends the child's
    /// loop, and [`Unanswered::Cancelled`] is what comes back when it does.
    async fn delegate(
        &self,
        request: Delegation,
        cancel: CancellationToken,
    ) -> Result<Delegated, Unanswered>;

    /// Starts a teammate and answers as soon as it is running.
    ///
    /// No cancellation token, and the absence is the whole difference from
    /// [`Subagents::delegate`]: a teammate outlives the call that started it,
    /// so by the time this answers there is nothing left for the call's own
    /// token to end. What ends a teammate is the team, not a turn.
    ///
    /// The default refuses, which is an answer rather than a stub: running
    /// subagents and leading a team are separate capabilities, and an
    /// implementation that has the first without the second — a fixture, a
    /// session that never joined a team — refuses in the same sentence a
    /// session with no team reads.
    ///
    /// # Errors
    ///
    /// [`NotSpawned`], carrying the one sentence the model reads next.
    async fn spawn_teammate(&self, request: TeammateSpawn) -> Result<Teammated, NotSpawned> {
        let _ = request;

        Err(NotSpawned {
            reason: NO_TEAM.to_owned(),
        })
    }
}

/// Runs a subagent.
pub struct TaskTool {
    /// Upstream's text plus the roster of agents *this* caller may spawn.
    /// Rendered once, because the caller does not change while a registry does
    /// not: the engine rebuilds the registry when the session switches agent.
    description: String,
}

impl TaskTool {
    /// Builds the tool as one caller sees it: upstream's description, then
    /// every agent in `roster`.
    #[must_use]
    pub fn new(roster: &[Offered]) -> Self {
        Self {
            description: describe(roster),
        }
    }
}

/// Upstream's `describeTask`: the base text, the roster header, and one line
/// per agent the caller may spawn, sorted ascending by name as upstream sorts
/// them.
fn describe(roster: &[Offered]) -> String {
    let mut listed: Vec<&Offered> = roster.iter().collect();
    listed.sort_by(|left, right| left.name.cmp(&right.name));

    let lines: String = listed
        .iter()
        .map(|offered| {
            format!(
                "\n- {}: {}",
                offered.name,
                offered.description.as_deref().unwrap_or(NO_DESCRIPTION)
            )
        })
        .collect();

    format!("{DESCRIPTION}\n{ROSTER_HEADER}{lines}")
}

#[async_trait]
impl Tool for TaskTool {
    fn id(&self) -> &str {
        ID
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let agent = args
            .get("subagent_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("subagent");
        let what = args
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        // A teammate is named by the row rather than by its agent kind: the
        // name is what a person watching the team, and the next
        // `send_message`, both address it by.
        match args.get("name").and_then(serde_json::Value::as_str) {
            Some(name) => format!("task: teammate {name} — {what}"),
            None => format!("task: {agent} — {what}"),
        }
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let mut args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let Some(subagents) = ctx.spawn.as_ref() else {
            return Err(ToolError::Failed(NO_AGENTS.to_owned()));
        };

        // One argument decides which of the two doors this call is, and it is
        // taken out of `args` here so that neither half can read the other's.
        match args.name.take() {
            Some(name) => start(subagents.as_ref(), name, args).await,
            None => delegate(subagents.as_ref(), args, ctx.cancel.child_token()).await,
        }
    }
}

/// The teammate door: start a member of this session's team, and say so.
///
/// Answers without waiting for any of the work, which is the fact the sentence
/// has to carry — a model reading a delegated result and a model reading this
/// are in two different situations, and only one of them has an answer.
async fn start(
    subagents: &dyn Subagents,
    name: String,
    args: Args,
) -> Result<ToolOutput, ToolError> {
    if args.task_id.is_some() {
        return Err(ToolError::Failed(NAME_WITH_TASK_ID.to_owned()));
    }

    let started = subagents
        .spawn_teammate(TeammateSpawn {
            name,
            backend: args.backend,
            agent_type: args.subagent_type,
            prompt: args.prompt,
        })
        .await;

    match started {
        Ok(teammate) => Ok(ToolOutput {
            title: args.description,
            output: format!(
                "{STARTED} {} on the {} backend. {}",
                teammate.name, teammate.backend, teammate.note
            ),
            metadata: serde_json::json!({
                "teammate": teammate.name,
                "agent_id": teammate.agent_id,
                "backend": teammate.backend,
            }),
        }),
        // The far side's own sentence, passed through: what a team refused and
        // why is its fact, and a wrapper here would be a second voice saying
        // less.
        Err(NotSpawned { reason }) => Err(ToolError::Failed(reason)),
    }
}

/// The delegation door, unchanged: run a subagent and read back its answer.
async fn delegate(
    subagents: &dyn Subagents,
    args: Args,
    cancel: CancellationToken,
) -> Result<ToolOutput, ToolError> {
    if args.backend.is_some() {
        return Err(ToolError::Failed(BACKEND_WITHOUT_NAME.to_owned()));
    }

    let delegated = subagents
        .delegate(
            Delegation {
                subagent_type: args.subagent_type.clone(),
                prompt: args.prompt,
                description: args.description.clone(),
                task_id: args.task_id,
            },
            cancel,
        )
        .await;

    match delegated {
        Ok(done) => Ok(ToolOutput {
            // Upstream titles the part with what the model called the task.
            title: args.description,
            output: render(&done.task_id, "completed", "task_result", &done.text),
            metadata: serde_json::json!({
                "session": done.task_id,
                "agent": done.agent,
                "model": done.model,
                "toolcalls": done.toolcalls,
                "calls": done.calls,
            }),
        }),
        Err(Unanswered::Cancelled) => Err(ToolError::Cancelled),
        // Upstream's wording, because the model reads it and retries.
        Err(Unanswered::Unknown) => Err(ToolError::Failed(format!(
            "Unknown agent type: {} is not a valid agent type",
            args.subagent_type
        ))),
        Err(Unanswered::Failed { task_id, message }) => Err(ToolError::Failed(render(
            &task_id,
            "error",
            "task_error",
            &message,
        ))),
    }
}

/// Upstream's `renderOutput` (`tool/task.ts`), which is the shape the parent
/// model reads a delegated result in.
fn render(session: &str, state: &str, tag: &str, text: &str) -> String {
    format!("<task id=\"{session}\" state=\"{state}\">\n<{tag}>\n{text}\n</{tag}>\n</task>")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use super::{
        BACKEND_WITHOUT_NAME, DESCRIPTION, Delegated, Delegation, NAME_WITH_TASK_ID, NO_TEAM,
        NotSpawned, Offered, ROSTER_HEADER, STARTED, Subagents, TaskTool, TeammateSpawn, Teammated,
        Unanswered, render,
    };
    use crate::{Tool as _, ToolCtx, ToolError};

    /// A seam that records what it was asked and answers a teammate spawn from
    /// a script. Delegation is not what these tests are about, so it is the
    /// one thing this double refuses.
    #[derive(Debug)]
    struct Fake {
        started: Mutex<Vec<TeammateSpawn>>,
        answer: Result<Teammated, NotSpawned>,
    }

    impl Fake {
        fn answering(answer: Result<Teammated, NotSpawned>) -> Arc<Self> {
            Arc::new(Self {
                started: Mutex::new(Vec::new()),
                answer,
            })
        }

        fn spawning() -> Arc<Self> {
            Self::answering(Ok(Teammated {
                name: "worker-2".to_owned(),
                agent_id: "worker-2@session-abcd1234".to_owned(),
                backend: "in-process".to_owned(),
                note: "it reads this through its mailbox".to_owned(),
            }))
        }
    }

    #[async_trait]
    impl Subagents for Fake {
        async fn delegate(
            &self,
            _request: Delegation,
            _cancel: CancellationToken,
        ) -> Result<Delegated, Unanswered> {
            Err(Unanswered::Unknown)
        }

        async fn spawn_teammate(&self, request: TeammateSpawn) -> Result<Teammated, NotSpawned> {
            self.started
                .lock()
                .expect("the spawn log is never poisoned")
                .push(request);

            self.answer.clone()
        }
    }

    /// A context whose only interesting field is the seam under test.
    fn ctx(spawn: Option<Arc<dyn Subagents>>) -> ToolCtx {
        let mut ctx = ToolCtx::fixture(std::env::temp_dir());
        ctx.spawn = spawn;
        ctx
    }

    fn tool() -> TaskTool {
        TaskTool::new(&[Offered {
            name: "general".to_owned(),
            description: None,
        }])
    }

    /// Runs one call against `spawn` and reports what the model would read of
    /// a refusal. [`ToolError`] carries no [`PartialEq`], so the sentence is
    /// what a test compares — which is the half that is the contract anyway.
    async fn refusal(spawn: Arc<dyn Subagents>, args: serde_json::Value) -> String {
        match tool().run(args, &ctx(Some(spawn))).await {
            Err(ToolError::Failed(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The teammate door: the arguments cross whole, and what comes back says
    /// the work has *started* rather than finished.
    #[tokio::test]
    async fn a_call_that_names_a_teammate_starts_one_and_says_so() {
        let spawn = Fake::spawning();
        let output = tool()
            .run(
                serde_json::json!({
                    "description": "spin up a worker",
                    "prompt": "hold the fort",
                    "subagent_type": "general",
                    "name": "worker",
                    "backend": "in-process",
                }),
                &ctx(Some(Arc::clone(&spawn) as Arc<dyn Subagents>)),
            )
            .await
            .expect("a teammate starts");

        assert_eq!(
            *spawn.started.lock().expect("no panic"),
            vec![TeammateSpawn {
                name: "worker".to_owned(),
                backend: Some("in-process".to_owned()),
                agent_type: "general".to_owned(),
                prompt: "hold the fort".to_owned(),
            }]
        );
        assert!(
            output.output.starts_with(STARTED),
            "the result says a teammate started: {}",
            output.output
        );
        assert!(
            output.output.contains("worker-2"),
            "and under the name the team really gave it: {}",
            output.output
        );
    }

    /// A refusal is the far side's sentence, unwrapped.
    #[tokio::test]
    async fn a_refused_spawn_reads_back_the_far_sides_own_sentence() {
        let spawn = Fake::answering(Err(NotSpawned {
            reason: "no backend named \"tmux\"".to_owned(),
        }));
        let read = refusal(
            spawn as Arc<dyn Subagents>,
            serde_json::json!({
                "description": "spin up a worker",
                "prompt": "hold the fort",
                "subagent_type": "general",
                "name": "worker",
                "backend": "tmux",
            }),
        )
        .await;

        assert_eq!(read, "no backend named \"tmux\"");
    }

    /// A `backend` with no `name` is the argument that was meant to carry one:
    /// refused by name rather than delegated to a subagent in silence.
    #[tokio::test]
    async fn a_surface_named_without_a_teammate_is_refused() {
        let spawn = Fake::spawning();
        let read = refusal(
            Arc::clone(&spawn) as Arc<dyn Subagents>,
            serde_json::json!({
                "description": "spin up a worker",
                "prompt": "hold the fort",
                "subagent_type": "general",
                "backend": "pane",
            }),
        )
        .await;

        assert_eq!(read, BACKEND_WITHOUT_NAME);
        assert!(
            spawn.started.lock().expect("no panic").is_empty(),
            "and nothing was started"
        );
    }

    /// Continuing a delegation and starting a teammate are two calls, not one.
    #[tokio::test]
    async fn continuing_a_delegation_and_starting_a_teammate_are_not_one_call() {
        let spawn = Fake::spawning();
        let read = refusal(
            Arc::clone(&spawn) as Arc<dyn Subagents>,
            serde_json::json!({
                "description": "spin up a worker",
                "prompt": "hold the fort",
                "subagent_type": "general",
                "name": "worker",
                "task_id": "01998ad0-0000-7000-8000-000000000000",
            }),
        )
        .await;

        assert_eq!(read, NAME_WITH_TASK_ID);
        assert!(spawn.started.lock().expect("no panic").is_empty());
    }

    /// A seam that runs subagents and leads no team refuses in the tool's own
    /// sentence, which is what the trait's default answers with.
    #[tokio::test]
    async fn a_seam_that_leads_no_team_refuses_a_teammate() {
        /// Nothing but [`Subagents::delegate`]; the teammate door is the
        /// trait's default.
        #[derive(Debug)]
        struct Delegator;

        #[async_trait]
        impl Subagents for Delegator {
            async fn delegate(
                &self,
                _request: Delegation,
                _cancel: CancellationToken,
            ) -> Result<Delegated, Unanswered> {
                Err(Unanswered::Unknown)
            }
        }

        let read = refusal(
            Arc::new(Delegator) as Arc<dyn Subagents>,
            serde_json::json!({
                "description": "spin up a worker",
                "prompt": "hold the fort",
                "subagent_type": "general",
                "name": "worker",
            }),
        )
        .await;

        assert_eq!(read, NO_TEAM);
    }

    /// A call with neither argument is the call this tool has always taken,
    /// and it still reaches the delegation seam.
    #[tokio::test]
    async fn a_call_that_names_no_teammate_still_delegates() {
        let spawn = Fake::spawning();
        let read = refusal(
            Arc::clone(&spawn) as Arc<dyn Subagents>,
            serde_json::json!({
                "description": "find the main",
                "prompt": "where is it",
                "subagent_type": "nobody",
            }),
        )
        .await;

        assert_eq!(
            read, "Unknown agent type: nobody is not a valid agent type",
            "it reached delegate, not spawn_teammate"
        );
        assert!(spawn.started.lock().expect("no panic").is_empty());
    }

    /// A teammate's row is named by the teammate, since that is what a person
    /// watching the team and the next message both address it by.
    #[test]
    fn a_teammate_row_is_named_by_the_teammate() {
        assert_eq!(
            tool().describe(&serde_json::json!({
                "subagent_type": "general",
                "description": "hold the fort",
                "name": "worker",
            })),
            "task: teammate worker — hold the fort"
        );
        assert_eq!(
            tool().describe(&serde_json::json!({
                "subagent_type": "general",
                "description": "find the main",
            })),
            "task: general — find the main"
        );
    }

    /// The order agents are handed over in is nobody's business but this
    /// function's: upstream sorts them, and a registry does not promise one.
    #[test]
    fn a_roster_is_listed_in_name_order_however_it_was_handed_over() {
        let tool = TaskTool::new(&[
            Offered {
                name: "general".to_owned(),
                description: Some("does the general thing".to_owned()),
            },
            Offered {
                name: "explore".to_owned(),
                description: Some("finds things".to_owned()),
            },
        ]);
        let described = tool.description();

        assert!(
            described.starts_with(DESCRIPTION),
            "upstream's text comes first, unedited"
        );
        // Only the tail past the header is the roster: upstream's own text
        // carries `- ` bullets of its own.
        let (_, listed) = described
            .split_once(ROSTER_HEADER)
            .expect("the roster header is appended");
        let roster: Vec<&str> = listed
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        assert_eq!(
            roster,
            vec![
                "- explore: finds things",
                "- general: does the general thing"
            ]
        );
    }

    /// An agent that describes itself nowhere is still offered, under
    /// upstream's stand-in line.
    #[test]
    fn a_subagent_with_nothing_to_say_for_itself_gets_upstreams_line() {
        let tool = TaskTool::new(&[Offered {
            name: "quiet".to_owned(),
            description: None,
        }]);

        assert!(
            tool.description()
                .ends_with("\n- quiet: This subagent should only be called manually by the user."),
            "got {}",
            tool.description()
        );
    }

    /// The exact bytes the parent model reads a delegated answer in. Upstream's
    /// `renderOutput`, and the thing a frontend has no other way to recover.
    #[test]
    fn a_result_is_wrapped_in_upstreams_xml() {
        assert_eq!(
            render("ses_1", "completed", "task_result", "it holds a main"),
            "<task id=\"ses_1\" state=\"completed\">\n<task_result>\nit holds a main\n</task_result>\n</task>"
        );
        assert_eq!(
            render("ses_1", "error", "task_error", "no credentials"),
            "<task id=\"ses_1\" state=\"error\">\n<task_error>\nno credentials\n</task_error>\n</task>"
        );
    }
}
