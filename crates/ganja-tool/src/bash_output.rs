//! The `bash_output` tool: polls a background job's output.
//!
//! Spec: Claude Code's `BashOutput` (2.1.x). Upstream opencode has no
//! equivalent — see [`crate::job`]'s module doc and **D454**. This tool has
//! nothing to do but ask [`ToolCtx::jobs`] and shape the answer; the
//! mechanics live in whatever implements [`crate::job::Jobs`].

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    Tool, ToolCtx, ToolError, ToolOutput, job,
    job::{JobsError, State},
};

/// The tool id, and the permission key.
pub const ID: &str = "bash_output";

/// What the model is told the tool is for. No upstream text to port from —
/// see this module's doc comment — so this is ganja's own, written in the
/// house voice the ported prompts use.
const DESCRIPTION: &str = "\
Retrieves output from a running or completed background shell started with \
the bash tool's run_in_background parameter.

- Always returns only output produced since the last time this tool was \
  called for that shell.
- Returns stdout and stderr interleaved in the order they arrived, exactly \
  as bash's own output does.
- Pass filter as a regular expression to keep only the output lines that \
  match it; everything else in the new output is dropped.
- Nothing here notifies you when a background shell has new output or has \
  finished — call this tool again to find out. There is no push, only poll.
- bash_id is required, and is the id the original bash call reported when it \
  ran with run_in_background set to true.";

/// What the model passes to `bash_output`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The ID of the background shell to retrieve output from
    bash_id: String,
    /// Optional regular expression to filter the output lines. Only lines
    /// matching this regex will be included in the result.
    #[serde(default)]
    filter: Option<String>,
}

/// Polls a background job's output.
pub struct BashOutputTool;

#[async_trait]
impl Tool for BashOutputTool {
    fn id(&self) -> &str {
        ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let id = args
            .get("bash_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");

        format!("bash_output {id}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let Some(jobs) = ctx.jobs.as_ref() else {
            return Err(ToolError::Failed(job::NO_JOBS.to_owned()));
        };

        let read = jobs.output(&args.bash_id).await.map_err(describe_error)?;
        let chunk = match &args.filter {
            Some(pattern) => filter_lines(&read.chunk, pattern)?,
            None => read.chunk,
        };
        let body = if chunk.is_empty() {
            "(no new output)".to_owned()
        } else {
            chunk
        };

        Ok(ToolOutput {
            title: format!("bash_output {}", args.bash_id),
            output: format!(
                "{body}\n\n<status>{}</status>",
                state_word(&read.status.state)
            ),
            metadata: serde_json::json!({
                "bash_id": read.status.id,
                "status": state_word(&read.status.state),
            }),
        })
    }
}

/// The sentence the model reads when its `bash_id` names nothing.
fn describe_error(error: JobsError) -> ToolError {
    match error {
        JobsError::NotFound(id) => ToolError::Failed(format!("no background shell with id {id}")),
    }
}

/// [`State`] as a word, for the status line and the metadata alike — one
/// place, so the two can never disagree about what they are reporting.
pub(crate) fn state_word(state: &State) -> String {
    match state {
        State::Running => "running".to_owned(),
        State::Exited { code: Some(code) } => format!("exited with code {code}"),
        State::Exited { code: None } => "exited".to_owned(),
        State::Killed => "killed".to_owned(),
    }
}

/// `text`, keeping only the lines `pattern` matches.
///
/// Built on `grep-regex`/`grep-matcher` rather than a second regex crate:
/// this is exactly the niche those crates are built for — matching a pattern
/// line by line over a byte stream — and `grep` already pulls them in for
/// the same reason.
fn filter_lines(text: &str, pattern: &str) -> Result<String, ToolError> {
    use grep_matcher::Matcher as _;

    let matcher = grep_regex::RegexMatcher::new(pattern)
        .map_err(|error| ToolError::InvalidArgs(format!("invalid filter {pattern:?}: {error}")))?;

    let kept: Vec<&str> = text
        .lines()
        .filter(|line| matcher.is_match(line.as_bytes()).unwrap_or(false))
        .collect();

    Ok(kept.join("\n"))
}

#[cfg(test)]
#[path = "bash_output_tests.rs"]
mod tests;
