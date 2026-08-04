//! The task tool: the model hands work to a subagent and reads back one answer.
//!
//! Spec: upstream `packages/opencode/src/tool/task.ts`. A call runs a **real
//! second agent loop** — its own history, its own rules, its own model — and
//! returns that loop's last words wrapped in upstream's XML. Everything the
//! subagent did in between is the subagent's business; the parent reads a
//! result, not a transcript.
//!
//! # Why it does not go through the engine
//!
//! [`Engine::send`](crate::engine::Engine::send) runs one turn at a time, and
//! the parent's turn is still holding that slot while this tool runs — a child
//! asking the engine for a turn would wait for a turn that is waiting for it.
//! So the child drives [`run_turn`](crate::session::run_turn) directly, with a
//! [`Turn`](crate::session::Turn) of its own.
//!
//! # What the parent sees
//!
//! The child's events go to a **private channel**, not the one the frontend is
//! subscribed to: every event on that stream is understood to belong to the
//! engine's one current session, and there is no session id on the wire to say
//! otherwise. What crosses over is exactly two things:
//!
//! - the child's permission requests and their replies, forwarded verbatim,
//!   because a subagent that asks a question nobody can see is a subagent that
//!   hangs — and the reply routes back through the *parent's* pending slot,
//!   which is free precisely because the parent is blocked here;
//! - progress on the parent's own tool part: `{current_tool, toolcalls}` in
//!   [`ToolState::Running::metadata`], which is what lets a frontend render
//!   upstream's single inline row without a single new event variant.
//!
//! # Depth
//!
//! One level, fixed (**D9**). A child's registry is this registry without the
//! task tool, so a subagent is not refused the call — it is never offered it.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{self, Agent},
    engine::EVENT_CAPACITY,
    permission::{Action, Permissions, Rule, TASK},
    protocol::{Event, FinishReason, MessageId, Part, PartBody, PartId, Role, ToolState, Usage},
    provider::Provider,
    session::{PendingReply, Persist, SessionState, Turn, TurnKind, run_turn},
    storage::{self, SessionId, SessionInfo},
    tool::{FileTimes, Registry, Tool, ToolCtx, ToolError, ToolOutput},
};

/// The tool id, which is also the permission key. Both are a permanent
/// commitment: a rule stored under this name has to keep meaning what it meant.
pub const ID: &str = "task";

/// What the model is told the tool is for, ported verbatim from upstream
/// `packages/opencode/src/tool/task.txt` (MIT; see `THIRD_PARTY_NOTICES.md`).
const DESCRIPTION: &str = include_str!("../prompt/task.txt");

/// Header upstream appends the per-caller agent roster under
/// (`tool/registry.ts`, `describeTask`).
const ROSTER_HEADER: &str = "Available agent types and the tools they have access to:";

/// What upstream shows for an agent that describes itself nowhere.
const NO_DESCRIPTION: &str = "This subagent should only be called manually by the user.";

/// What a call reads when this build offered the tool without anything behind
/// it. Not reachable through the engine, which registers the tool only when it
/// has agents to spawn.
const NO_AGENTS: &str = "This session has no subagents to delegate to.";

/// The permission a subagent's ruleset gets denied unless it says otherwise, so
/// that a subagent cannot delegate its way past the depth limit even if the
/// registry it was handed did offer the tool.
const TODOWRITE: &str = "todowrite";

/// The pattern that covers every call to a permission.
const ANY: &str = "*";

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
}

/// Everything a child agent loop needs that does not change between calls.
///
/// Held by the turn and cloned into each [`Spawn`]. Its own [`std::fmt::Debug`]
/// because [`Provider`] has none, and because a derived one would be a wall of
/// prompt text in every tool-call log line.
pub struct Host {
    /// Who answers the child's requests. The same provider the parent uses:
    /// the instance is fixed when the engine is built.
    pub(crate) provider: Arc<dyn Provider>,
    /// What the parent is asking, which a subagent naming no model of its own
    /// inherits.
    pub(crate) model: String,
    /// Agents the parent may spawn.
    pub(crate) agents: Arc<agent::Registry>,
    /// Tools the **child** is offered: this build's registry without the task
    /// tool, which is the whole of the depth guard.
    pub(crate) tools: Arc<Registry>,
    /// The parent's rules, which the child derives its own from.
    pub(crate) permissions: Arc<std::sync::Mutex<Permissions>>,
    /// The half of the system prompt an agent replaces, for a subagent that
    /// brings no prompt of its own.
    pub(crate) base_prompt: Option<String>,
    /// The half no agent replaces.
    pub(crate) prompt_suffix: Option<String>,
    /// Where the child's tool calls resolve relative paths.
    pub(crate) cwd: PathBuf,
    /// Where the project starts, for the same two uses the parent has.
    pub(crate) root: PathBuf,
    /// The store, when the engine persists. A child session is an ordinary
    /// stored session that names its parent.
    pub(crate) persistence: Option<Arc<SessionState>>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Host")
            .field("provider", &self.provider.id())
            .field("model", &self.model)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

/// What one task call needs: the session-wide [`Host`], plus where in the
/// parent's transcript this call is so its progress can be reported.
#[derive(Clone)]
pub struct Spawn {
    pub(crate) host: Arc<Host>,
    /// The parent turn's event sender — used for the parent's own tool part and
    /// for forwarding the child's permission dialogs, and for nothing else.
    pub(crate) events: mpsc::Sender<Event>,
    /// Where an open permission request waits, shared with the parent turn: the
    /// parent is blocked inside this call, so the slot is the child's to use and
    /// a reply routed to the parent reaches the child.
    pub(crate) pending: Arc<std::sync::Mutex<Option<PendingReply>>>,
    /// The parent message holding this call's part.
    pub(crate) message_id: MessageId,
    /// The part this call's progress is reported on.
    pub(crate) part_id: PartId,
}

impl std::fmt::Debug for Spawn {
    /// Hand-written because the pending-reply slot is a channel end with no
    /// [`Debug`] of its own, and because what is worth reading here is where
    /// the call sits, not the machinery behind it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Spawn")
            .field("host", &self.host)
            .field("message_id", &self.message_id)
            .field("part_id", &self.part_id)
            .finish_non_exhaustive()
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
    /// Builds the tool as `caller` sees it: upstream's description, then every
    /// agent `caller` may delegate to, sorted by name.
    #[must_use]
    pub fn new(agents: &agent::Registry, caller: &Agent) -> Self {
        Self {
            description: describe(agents, caller),
        }
    }
}

/// Upstream's `describeTask`: the base text, the roster header, and one line
/// per agent this caller may spawn.
///
/// The filter is upstream's — `mode !== "primary"`, then the caller's own rules
/// consulted for `task`/`<name>` and dropped when they say deny — and the sort
/// is upstream's ascending-by-name. `hidden` deliberately does not filter here:
/// it hides an agent from the pickers a *person* uses, not from the model.
fn describe(agents: &agent::Registry, caller: &Agent) -> String {
    let mut roster: Vec<&Agent> = agents
        .agents()
        .iter()
        .filter(|agent| agent.spawnable() && !denies_task(&caller.rules, &agent.name))
        .collect();
    roster.sort_by(|left, right| left.name.cmp(&right.name));

    let lines: String = roster
        .iter()
        .map(|agent| {
            format!(
                "\n- {}: {}",
                agent.name,
                agent.description.as_deref().unwrap_or(NO_DESCRIPTION)
            )
        })
        .collect();

    format!("{DESCRIPTION}\n{ROSTER_HEADER}{lines}")
}

/// Whether `rules` refuse a task call naming `subagent`, by the same
/// last-match-wins walk the gate does.
fn denies_task(rules: &[Rule], subagent: &str) -> bool {
    rules
        .iter()
        .rev()
        .find(|rule| {
            crate::permission::matches(TASK, &rule.permission)
                && crate::permission::matches(subagent, &rule.pattern)
        })
        .is_some_and(|rule| rule.action == Action::Deny)
}

#[async_trait]
impl Tool for TaskTool {
    fn id(&self) -> &'static str {
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

        format!("task: {agent} — {what}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let Some(spawn) = ctx.spawn.as_ref() else {
            return Err(ToolError::Failed(NO_AGENTS.to_owned()));
        };

        // Upstream does not check the mode here — only the permission dialog
        // stands between the model and `subagent_type: "build"`. This build
        // refuses it: an agent the roster never offered is one the model has no
        // business naming, and running a *primary* agent unattended is the one
        // thing subagent mode exists to prevent
        // (deviation: task-spawns-subagents-only).
        let Some(agent) = spawn
            .host
            .agents
            .get(&args.subagent_type)
            .filter(|agent| agent.spawnable())
        else {
            // Upstream's wording, because the model reads it and retries.
            return Err(ToolError::Failed(format!(
                "Unknown agent type: {} is not a valid agent type",
                args.subagent_type
            )));
        };

        let child = Child::open(spawn, agent, args.task_id.as_deref());
        let outcome = child
            .run(spawn, agent, &args, ctx.cancel.child_token())
            .await;

        match outcome.stop {
            ChildStop::Cancelled => Err(ToolError::Cancelled),
            ChildStop::Failed(error) => Err(ToolError::Failed(render(
                &child.session,
                "error",
                "task_error",
                &error,
            ))),
            ChildStop::Completed => Ok(ToolOutput {
                // Upstream titles the part with what the model called the task.
                title: args.description.clone(),
                output: render(&child.session, "completed", "task_result", &outcome.text),
                metadata: serde_json::json!({
                    "session": child.session.as_str(),
                    "agent": agent.name,
                    "model": child.model,
                    "toolcalls": outcome.toolcalls,
                }),
            }),
        }
    }
}

/// Upstream's `renderOutput` (`tool/task.ts`), which is the shape the parent
/// model reads a delegated result in.
fn render(session: &SessionId, state: &str, tag: &str, text: &str) -> String {
    format!(
        "<task id=\"{}\" state=\"{state}\">\n<{tag}>\n{text}\n</{tag}>\n</task>",
        session.as_str()
    )
}

/// One child session, resolved: where it writes and what it runs as.
struct Child {
    session: SessionId,
    model: String,
    /// The transcript a resumed child continues from; empty for a fresh one.
    history: Vec<crate::protocol::Message>,
    /// Whether the session record still has to be created.
    fresh: bool,
}

impl Child {
    /// Resolves the session a call runs in: the one `task_id` names when the
    /// store still holds it, and a new one otherwise.
    fn open(spawn: &Spawn, agent: &Agent, task_id: Option<&str>) -> Self {
        let model = agent
            .model
            .clone()
            .filter(|model| crate::provider::serves(spawn.host.provider.id(), model))
            .unwrap_or_else(|| spawn.host.model.clone());

        let resumed = task_id.zip(spawn.host.persistence.as_ref()).and_then(
            |(id, state)| -> Option<(SessionId, Vec<crate::protocol::Message>)> {
                let id = SessionId::from(id.to_owned());
                state.storage.load_info(&id).ok().flatten()?;
                let transcript = state.storage.load_transcript(&id).unwrap_or_default();

                Some((id, transcript))
            },
        );

        match resumed {
            Some((session, history)) => Self {
                session,
                model,
                history,
                fresh: false,
            },
            // A `task_id` the store cannot answer for starts a fresh session
            // rather than failing the call: the model asked for work to happen,
            // and the id was a hint about where to continue it
            // (deviation: task-id-miss-starts-fresh).
            None => Self {
                session: SessionId::ascending(),
                model,
                history: Vec::new(),
                fresh: true,
            },
        }
    }

    /// Runs the child loop to its finish and reports what it said.
    async fn run(
        &self,
        spawn: &Spawn,
        agent: &Agent,
        args: &Args,
        cancel: CancellationToken,
    ) -> Outcome {
        let host = &spawn.host;
        let persist = host.persistence.as_ref().map(|state| {
            if self.fresh {
                create(state, &self.session, agent, &args.description, &self.model);
            }
            Persist::new(Arc::clone(state), self.session.clone())
        });

        let permissions = {
            let parent = host
                .permissions
                .lock()
                .expect("the permission rules are never poisoned");
            parent.derive_subagent(subagent_rules(agent, &parent))
        };

        // The child's own channel. Its events never reach the frontend — see
        // the module docs — so the watcher below is the only reader, and it is
        // what turns them back into the two things the parent is entitled to.
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let watcher = tokio::spawn(watch(
            receiver,
            Watched {
                events: spawn.events.clone(),
                tools: Arc::clone(&host.tools),
                message_id: spawn.message_id.clone(),
                part_id: spawn.part_id.clone(),
                command: args.description.clone(),
            },
        ));

        run_turn(Turn {
            provider: Arc::clone(&host.provider),
            model: self.model.clone(),
            system: crate::instruction::joined(
                agent.prompt.as_deref().or(host.base_prompt.as_deref()),
                host.prompt_suffix.as_deref(),
            ),
            // Upstream's plan/build reminders are about the agent a *person*
            // switched to; a subagent runs the prompt it was built with.
            reminders: Vec::new(),
            kind: TurnKind::Prompt {
                mentions: Vec::new(),
            },
            tools: Arc::clone(&host.tools),
            permissions: Arc::new(std::sync::Mutex::new(permissions)),
            cwd: host.cwd.clone(),
            root: host.root.clone(),
            // A fresh read log: what the parent read is not what the child may
            // write over, and the read-before-write rule is per conversation.
            files: Arc::new(FileTimes::default()),
            prompt: args.prompt.clone(),
            cancel,
            pending: Arc::clone(&spawn.pending),
            events,
            slot: Arc::new(Mutex::new(None)),
            history: Arc::new(Mutex::new(self.history.clone())),
            // The child's task tool is absent from its registry, so nothing
            // below it can spawn anything.
            spawn: None,
            persist,
        })
        .await;

        let outcome = watcher.await.unwrap_or_else(|_| Outcome {
            stop: ChildStop::Failed("the subagent task did not finish".to_owned()),
            ..Outcome::default()
        });

        if let Some(state) = &host.persistence {
            settle(state, &self.session, outcome.usage);
        }

        outcome
    }
}

/// The rules a subagent runs under: its own, then what the parent session
/// insists on, then the two denials that keep it from delegating further.
///
/// Upstream's `deriveSubagentSessionPermission` (`agent/subagent-permissions.ts`)
/// appends `task`/`todowrite` denials unless the subagent's own set already
/// mentions them, which is how `general`'s explicit `todowrite: deny` and an
/// agent that deliberately re-enables it both survive.
///
/// This is the child's whole ruleset. It reaches the child through
/// [`Permissions::derive_subagent`], which leaves the parent's stored answers
/// behind — a set assembled this carefully would mean nothing under a tier
/// that outranks all of it.
fn subagent_rules(agent: &Agent, parent: &Permissions) -> Vec<Rule> {
    let mut rules = agent.rules.clone();
    rules.extend(parent.inherited_by_subagent());

    for permission in [TASK, TODOWRITE] {
        if !Permissions::baseline_mentions(&agent.rules, permission) {
            rules.push(Rule {
                permission: permission.to_owned(),
                pattern: ANY.to_owned(),
                action: Action::Deny,
            });
        }
    }

    rules
}

/// Writes the child's session record before its first byte streams, naming the
/// parent so a listing can tell a delegated conversation from a real one.
fn create(state: &SessionState, session: &SessionId, agent: &Agent, what: &str, model: &str) {
    let created = crate::protocol::now();
    let parent = state
        .live
        .lock()
        .expect("the live session is never poisoned")
        .info
        .as_ref()
        .map(|info| info.id.clone());

    let info = SessionInfo {
        id: session.clone(),
        version: storage::VERSION,
        // Upstream's default child title, so a stored child says what it was
        // for without spending a title request on finding out.
        title: Some(format!("{what} (@{} subagent)", agent.name)),
        created,
        updated: created,
        usage: Usage::default(),
        context_tokens: 0,
        summary: None,
        agent: Some(agent.name.clone()),
        model: Some(model.to_owned()),
        parent,
    };

    if let Err(error) = state.storage.save_info(&info) {
        tracing::warn!(
            session = session.as_str(),
            %error,
            "could not create the subagent's session on disk; it runs in memory"
        );
    }
}

/// Adds what the child spent to its own record.
///
/// The turn's own closing write cannot: [`Persist::finish`] only touches the
/// record of the session the engine is *live* on, which is the parent's — that
/// guard is what keeps a child from overwriting its parent's bookkeeping.
fn settle(state: &SessionState, session: &SessionId, usage: Usage) {
    let Ok(Some(mut info)) = state.storage.load_info(session) else {
        return;
    };
    info.usage = usage;
    info.updated = crate::protocol::now();

    if let Err(error) = state.storage.save_info(&info) {
        tracing::warn!(
            session = session.as_str(),
            %error,
            "could not record what the subagent spent"
        );
    }
}

/// What a finished child loop reports back.
#[derive(Default)]
struct Outcome {
    /// The last text part the child produced, which is upstream's
    /// `parts.findLast(p => p.type === "text")`.
    text: String,
    /// How the loop ended.
    stop: ChildStop,
    /// How many tools it called, which the parent's inline row shows.
    toolcalls: usize,
    /// What it spent.
    usage: Usage,
}

/// Why a child loop stopped.
#[derive(Default)]
enum ChildStop {
    /// It ran out of things to say, which is the only success.
    #[default]
    Completed,
    /// The parent's cancel reached it.
    Cancelled,
    /// The provider could not answer. The message is what the parent model
    /// reads.
    Failed(String),
}

/// What the watcher needs to translate a child's events.
struct Watched {
    events: mpsc::Sender<Event>,
    /// The child's registry, so a running call can be named the way a dialog
    /// would name it — `read src/main.rs`, not `read`.
    tools: Arc<Registry>,
    message_id: MessageId,
    part_id: PartId,
    /// What the model called the task, which is the input the parent's part
    /// keeps showing while the child works.
    command: String,
}

/// Reads the child's event stream and does the two things the parent is owed:
/// forwards permission dialogs, and keeps `{current_tool, toolcalls}` on the
/// parent's tool part current.
///
/// Everything else is dropped on purpose. A frontend applying this engine's
/// stream believes every event belongs to the session it is showing, and there
/// is no session id on the wire to tell it otherwise — so a child's messages
/// would arrive as the parent's own. Upstream publishes them and lets its
/// frontend filter by session id; this one cannot, so it does not publish
/// (deviation: subagent-events-stay-off-the-stream).
async fn watch(mut receiver: mpsc::Receiver<Event>, watched: Watched) -> Outcome {
    let mut outcome = Outcome::default();
    let mut current: Option<String> = None;
    // The text part being streamed right now. The last one to be opened is the
    // last one there is, which is what upstream's `findLast` selects.
    let mut open: Option<PartId> = None;

    while let Some(event) = receiver.recv().await {
        match event {
            // A subagent's question is the user's to answer, and the reply
            // routes back through the parent's pending slot.
            Event::PermissionRequested { .. } | Event::PermissionReplied { .. } => {
                let _ = watched.events.send(event).await;
            }
            Event::PartStarted { part, .. } => match &part.body {
                PartBody::Text { .. } => {
                    open = Some(part.id.clone());
                    outcome.text.clear();
                }
                PartBody::Tool { .. } => {
                    outcome.toolcalls += 1;
                    report(&watched, current.as_deref(), outcome.toolcalls).await;
                }
                PartBody::File { .. } | PartBody::StepStart | PartBody::StepFinish { .. } => {}
            },
            Event::PartDelta { part_id, delta, .. } => {
                if open.as_ref() == Some(&part_id) {
                    outcome.text.push_str(&delta);
                }
            }
            Event::PartUpdated { part, .. } => {
                if let PartBody::Tool { tool, state, .. } = &part.body
                    && let ToolState::Running { input, .. } = state
                {
                    current = Some(name_of(&watched, tool, input));
                    report(&watched, current.as_deref(), outcome.toolcalls).await;
                }
            }
            Event::MessageStarted { message } => {
                // A compaction summary arrives as a complete assistant message
                // rather than a streamed one; it is not the child's answer.
                if message.role == Role::User {
                    outcome.text.clear();
                    open = None;
                }
            }
            Event::MessageFinished {
                reason,
                usage,
                error,
                ..
            } => {
                if let Some(usage) = usage {
                    outcome.usage = usage;
                }
                outcome.stop = match reason {
                    FinishReason::Completed => ChildStop::Completed,
                    FinishReason::Cancelled => ChildStop::Cancelled,
                    FinishReason::Failed => ChildStop::Failed(
                        error.unwrap_or_else(|| "the subagent could not answer".to_owned()),
                    ),
                };
            }
        }
    }

    outcome
}

/// How a running child call is named on the parent's row.
///
/// Upstream shows the tool's own `state.title`, which its running parts carry
/// and ganja's do not. What stands in is the line a permission dialog would
/// have used for the same call — `read src/main.rs`, not `read` — which is the
/// same sentence by a different route
/// (deviation: task-progress-names-the-call).
fn name_of(watched: &Watched, tool: &str, input: &serde_json::Value) -> String {
    watched
        .tools
        .get(tool)
        .map_or_else(|| tool.to_owned(), |found| found.describe(input))
}

/// Rewrites the parent's tool part with what the child is doing now.
///
/// The part travels whole, as every [`Event::PartUpdated`] does. The parent's
/// own copy is deliberately not touched: this is progress, not transcript, and
/// what reaches the disk is the completed call.
async fn report(watched: &Watched, current: Option<&str>, toolcalls: usize) {
    let mut metadata = serde_json::json!({ "toolcalls": toolcalls });
    if let Some(current) = current {
        metadata["current_tool"] = serde_json::json!(current);
    }

    let _ = watched
        .events
        .send(Event::PartUpdated {
            message_id: watched.message_id.clone(),
            part: Part {
                id: watched.part_id.clone(),
                body: PartBody::Tool {
                    call_id: watched.part_id.as_str().to_owned(),
                    tool: ID.to_owned(),
                    state: ToolState::Running {
                        input: serde_json::json!({ "description": watched.command }),
                        metadata,
                        started: 0,
                    },
                },
            },
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::{DESCRIPTION, ROSTER_HEADER, TaskTool, denies_task, render, subagent_rules};
    use crate::{
        agent::{self, Registry},
        config::Config,
        permission::{Action, Permissions, Rule},
        storage::SessionId,
        tool::Tool as _,
    };

    fn registry() -> Registry {
        Registry::build(&Config::default()).expect("the default config resolves agents")
    }

    #[test]
    fn the_description_is_upstreams_text_followed_by_the_callers_roster() {
        let agents = registry();
        let build = agents.get(agent::BUILD).expect("build is builtin");
        let tool = TaskTool::new(&agents, build);
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
        let tool = TaskTool::new(&agents, plan);
        let described = tool.description();

        assert!(described.contains("- explore: "));
        assert!(
            !described.contains("- general: "),
            "plan denies task:general: {described}"
        );
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
            rules
                .iter()
                .filter(|rule| rule.permission == "todowrite")
                .count(),
            1,
            "the appended denial would be a second one: {rules:?}"
        );
    }

    #[test]
    fn a_parents_denial_reaches_the_child_and_a_parents_allowance_does_not() {
        let mut parent = Permissions::default();
        parent.set_baseline(vec![
            Rule {
                permission: "webfetch".to_owned(),
                pattern: "*".to_owned(),
                action: Action::Deny,
            },
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
            rules
                .iter()
                .any(|rule| rule.permission == "webfetch" && rule.action == Action::Deny),
            "a denial travels down: {rules:?}"
        );
        assert!(
            !rules.iter().any(|rule| rule.pattern == "cargo *"),
            "an allowance does not: {rules:?}"
        );
    }

    /// The exact bytes the parent model reads a delegated answer in. Upstream's
    /// `renderOutput`, and the thing a frontend has no other way to recover.
    #[test]
    fn a_result_is_wrapped_in_upstreams_xml() {
        let session = SessionId::from("ses_1".to_owned());

        assert_eq!(
            render(&session, "completed", "task_result", "it holds a main"),
            "<task id=\"ses_1\" state=\"completed\">\n<task_result>\nit holds a main\n</task_result>\n</task>"
        );
        assert_eq!(
            render(&session, "error", "task_error", "no credentials"),
            "<task id=\"ses_1\" state=\"error\">\n<task_error>\nno credentials\n</task_error>\n</task>"
        );
    }
}
