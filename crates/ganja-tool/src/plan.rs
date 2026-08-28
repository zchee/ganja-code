//! Spec: upstream packages/opencode/src/tool/plan.ts
//!
//! The two plan doors. `plan_exit` is the way out: the plan agent asks whether
//! its finished plan should pass to the build agent. `plan_enter` is the way
//! in: the build agent asks whether a request should be planned before it is
//! implemented.
//!
//! # What is here and what is not
//!
//! Switching agents is the engine's vocabulary, not a tool's, so each request
//! crosses [`Switcher`] as intent recorded now and applied at a later engine
//! entry. What stays here is each door's own half: the empty argument shape,
//! the fixed question the person reads, and the approval sentence the model
//! reads back. The engine wires the seam only when its registry holds the
//! agent a door leads to, so [`ToolCtx::switch`] being present is the whole
//! statement that switching is possible, just as [`ToolCtx::spawn`] being
//! present says delegation is.
//!
//! Ganja does not copy the model onto a synthetic message as upstream does;
//! the engine's adoption rule keeps the active model when the target agent
//! prefers none, which both build and plan do.
//!
//! # `plan_enter` is synthesized rather than ported (**D477**, `plan-enter-synthesized`)
//!
//! Upstream v1.18.22 ships the *vocabulary* of a plan-enter door and none of
//! its body. `tool/plan-enter.txt` is the model-facing description;
//! `agent/agent.ts:127` defaults the `plan_enter` permission to deny;
//! `agent/agent.ts:147-150` allows it on the **build** agent beside
//! `question: "allow"`; `cli/cmd/run.ts:439` denies it on a headless run. But
//! `tool/plan.ts` defines only `PlanExitTool` and `tool/registry.ts` wires
//! only that one: there is no upstream tool body to port. Ganja's
//! [`PlanEnterTool`] is therefore **synthesized** — from `plan_exit`'s landed
//! shape plus the description upstream did write — rather than translated:
//! same empty arguments, same question seam, same [`Switcher`] contract, with
//! the question, option, title and output sentences derived from that
//! description's own phrasing in exit's sentence shape, because there is no
//! published wording to be verbatim against. Where exit's text carries the
//! standing plan-is-prose deviation, enter's needs none: upstream's
//! `plan-enter.txt` never mentions a plan file, so it is included here
//! verbatim.
//!
//! [`ToolCtx::spawn`]: crate::ToolCtx::spawn
//! [`ToolCtx::switch`]: crate::ToolCtx::switch

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::question::{self, Choice, Prompt};
use crate::{Tool, ToolCtx, ToolError, ToolOutput};

/// The exit tool's id, which is also its permission key. Both are a permanent
/// commitment: a rule stored under this name has to keep meaning what it meant.
pub const EXIT_ID: &str = "plan_exit";

/// The enter tool's id, under the same commitment. The name is upstream's own
/// — its permission table, its build-agent delta and its headless refusal all
/// spell it — so a rule written against upstream's vocabulary decides the
/// right tool here even though the body is ganja's (**D477**).
pub const ENTER_ID: &str = "plan_enter";

/// What the model is told `plan_exit` is for, carrying upstream
/// `packages/opencode/src/tool/plan-exit.txt` with one plan-is-prose deviation:
/// `After you have written a complete plan to the plan file` becomes `After
/// you have presented the complete plan`, because ganja has no plan file — a
/// plan is prose in the transcript, extending the standing `agent.rs`
/// deviation here.
pub const EXIT_DESCRIPTION: &str = include_str!("plan_exit.txt");

/// What the model is told `plan_enter` is for, carrying upstream
/// `packages/opencode/src/tool/plan-enter.txt` **verbatim** — the one half of
/// the enter door upstream actually wrote (**D477**). No plan-is-prose
/// deviation is needed: unlike exit's, this text never names a plan file.
pub const ENTER_DESCRIPTION: &str = include_str!("plan_enter.txt");

/// The two answers, upstream's own labels. `No` is compared literally, so it
/// is a constant rather than a spelling repeated at the check.
const YES_LABEL: &str = "Yes";
const NO_LABEL: &str = "No";

/// What an exit call reads when the engine offered the tool without a build
/// agent behind it. Not reachable through the engine, which wires the seam
/// only when the registry holds that agent.
const NO_BUILD_AGENT: &str = "this session has no build agent to switch to";

/// The mirror, for an enter call on a session with no plan agent. Unreachable
/// for the same reason and stated for the same one: a build that registered
/// the tool and wired no switcher must answer the model in words.
const NO_PLAN_AGENT: &str = "this session has no plan agent to switch to";

/// What the model passes to either door: upstream's empty `Schema.Struct({})`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {}

/// Records that this session should enter another agent.
///
/// A seam like [`question::Asker`] and [`crate::task::Subagents`], because
/// switching agents is engine vocabulary a tool may not name. Both methods are
/// synchronous and infallible because each only records intent and waits for
/// nothing: the engine announces the switch at the turn boundary and applies
/// it at its next entry. The engine wires one only where at least one door is
/// registered, and registers a door only when its target agent is in the
/// roster, so presence is ability per direction rather than in general.
pub trait Switcher: Send + Sync + std::fmt::Debug {
    /// Records the intent that the next engine entry should use the build
    /// agent.
    fn switch_to_build(&self);

    /// The mirror: the next engine entry should use the plan agent
    /// (**D477**).
    fn switch_to_plan(&self);
}

/// Everything two otherwise identical doors differ by: the fixed words a
/// person and a model read, and which direction a Yes records.
///
/// The call body is written once against this, in [`run_door`], so the two
/// doors cannot drift out of each other's shape — the mirror is structural
/// rather than a resemblance somebody maintains.
struct Door {
    /// What the person is asked, in one sentence.
    question: &'static str,
    header: &'static str,
    yes_description: &'static str,
    no_description: &'static str,
    /// What the model reads when there is no [`Switcher`] to record through.
    missing: &'static str,
    title: &'static str,
    output: &'static str,
    /// The [`Switcher`] method a Yes calls. A function pointer rather than a
    /// flag, so a direction is only ever expressed in the trait's own two
    /// methods.
    record: fn(&dyn Switcher),
}

/// The way out, upstream-verbatim but for the plan-file clause: upstream
/// interpolates the plan file path before `is complete`, while ganja's
/// completed plan is the prose already present in the transcript (deviation:
/// plan-is-prose).
const EXIT: Door = Door {
    question: "The plan is complete. Would you like to switch to the build agent and start \
               implementing?",
    header: "Build Agent",
    yes_description: "Switch to build agent and start implementing the plan",
    no_description: "Stay with plan agent to continue refining the plan",
    missing: NO_BUILD_AGENT,
    title: "Switching to build agent",
    output: "User approved switching to build agent. Wait for further instructions.",
    record: |switcher| switcher.switch_to_build(),
};

/// The way in, synthesized (**D477**). Upstream published no enter-side
/// wording, so every sentence here is derived rather than copied: the question
/// from `plan-enter.txt`'s own two claims — "This tool will ask the user if
/// they want to switch to plan agent" and "You want to research and design
/// before making changes" — cast in exit's one-sentence shape, and the header,
/// options, title and output as exit's with the two agents exchanged.
const ENTER: Door = Door {
    question: "Would you like to switch to the plan agent to research and design before \
               implementing?",
    header: "Plan Agent",
    yes_description: "Switch to plan agent and start planning the work",
    no_description: "Stay with build agent to continue implementing",
    missing: NO_PLAN_AGENT,
    title: "Switching to plan agent",
    output: "User approved switching to plan agent. Wait for further instructions.",
    record: |switcher| switcher.switch_to_plan(),
};

/// Assembles the one fixed question a door presents.
fn prompt(door: &Door) -> Prompt {
    Prompt {
        question: door.question.to_owned(),
        header: door.header.to_owned(),
        options: vec![
            Choice { label: YES_LABEL.to_owned(), description: door.yes_description.to_owned() },
            Choice { label: NO_LABEL.to_owned(), description: door.no_description.to_owned() },
        ],
        multiple: None,
    }
}

/// Asks `door`'s question and, unless the answer was `No`, records its switch.
///
/// Upstream's `answers[0]?.[0] === "No"` is the whole decline check, so any
/// *other* answer — including a skipped, empty one — proceeds, faithfully.
async fn run_door(
    door: &Door,
    args: serde_json::Value,
    ctx: &ToolCtx,
) -> Result<ToolOutput, ToolError> {
    let _: Args =
        serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

    let answers = match ctx.ask.as_ref() {
        None => return Err(ToolError::Failed(question::DISMISSED.to_owned())),
        Some(asker) => asker.ask(vec![prompt(door)]).await,
    };
    let answers = match answers {
        Ok(answers) => answers,
        Err(question::Unanswered::Dismissed) => {
            return Err(ToolError::Failed(question::DISMISSED.to_owned()));
        }
        Err(question::Unanswered::Cancelled) => return Err(ToolError::Cancelled),
    };

    if answers.first().and_then(|answer| answer.first()).is_some_and(|answer| answer == NO_LABEL) {
        return Err(ToolError::Failed(question::DISMISSED.to_owned()));
    }

    let Some(switcher) = ctx.switch.as_ref() else {
        return Err(ToolError::Failed(door.missing.to_owned()));
    };
    (door.record)(switcher.as_ref());

    Ok(ToolOutput {
        title: door.title.to_owned(),
        output: door.output.to_owned(),
        metadata: serde_json::json!({}),
    })
}

/// Asks whether a completed plan should pass to the build agent.
#[derive(Debug, Default)]
pub struct PlanExitTool;

#[async_trait]
impl Tool for PlanExitTool {
    fn id(&self) -> &str {
        EXIT_ID
    }

    fn description(&self) -> &str {
        EXIT_DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        run_door(&EXIT, args, ctx).await
    }
}

/// Asks whether a request should be planned before it is implemented
/// (**D477**).
#[derive(Debug, Default)]
pub struct PlanEnterTool;

#[async_trait]
impl Tool for PlanEnterTool {
    fn id(&self) -> &str {
        ENTER_ID
    }

    fn description(&self) -> &str {
        ENTER_DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        run_door(&ENTER, args, ctx).await
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
