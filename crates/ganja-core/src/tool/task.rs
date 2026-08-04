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
//! [`ToolCtx::spawn`]: crate::tool::ToolCtx::spawn

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolError, ToolOutput};

/// The tool id, which is also the permission key. Both are a permanent
/// commitment: a rule stored under this name has to keep meaning what it meant.
pub const ID: &str = "task";

/// What the model is told the tool is for, ported verbatim from upstream
/// `packages/opencode/src/tool/task.txt` (MIT; see `THIRD_PARTY_NOTICES.md`).
pub(crate) const DESCRIPTION: &str = include_str!("../prompt/task.txt");

/// Header upstream appends the per-caller agent roster under
/// (`tool/registry.ts`, `describeTask`).
pub(crate) const ROSTER_HEADER: &str = "Available agent types and the tools they have access to:";

/// What upstream shows for an agent that describes itself nowhere.
const NO_DESCRIPTION: &str = "This subagent should only be called manually by the user.";

/// What a call reads when this build offered the tool without anything behind
/// it. Not reachable through the engine, which registers the tool only when it
/// has agents to spawn.
const NO_AGENTS: &str = "This session has no subagents to delegate to.";

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

/// Runs a whole subagent conversation on a `task` call's behalf.
///
/// Deliberately says nothing about *how*: a conversation, a provider and an
/// agent loop are the engine's vocabulary, and a tool that named them would be
/// a tool the engine cannot be assembled without. What crosses is a request of
/// strings and an answer of strings.
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

        format!("task: {agent} — {what}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let Some(subagents) = ctx.spawn.as_ref() else {
            return Err(ToolError::Failed(NO_AGENTS.to_owned()));
        };

        let delegated = subagents
            .delegate(
                Delegation {
                    subagent_type: args.subagent_type.clone(),
                    prompt: args.prompt,
                    description: args.description.clone(),
                    task_id: args.task_id,
                },
                ctx.cancel.child_token(),
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
}

/// Upstream's `renderOutput` (`tool/task.ts`), which is the shape the parent
/// model reads a delegated result in.
fn render(session: &str, state: &str, tag: &str, text: &str) -> String {
    format!("<task id=\"{session}\" state=\"{state}\">\n<{tag}>\n{text}\n</{tag}>\n</task>")
}

#[cfg(test)]
mod tests {
    use super::{DESCRIPTION, Offered, ROSTER_HEADER, TaskTool, render};
    use crate::tool::Tool as _;

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
