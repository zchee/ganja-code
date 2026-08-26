//! The `kill_shell` tool: ends a background job outright.
//!
//! Spec: Claude Code's `KillShell` (2.1.x). Upstream opencode has no
//! equivalent — see [`crate::job`]'s module doc and **D454**.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolCtx, ToolError, ToolOutput, bash_output::state_word, job, job::JobsError};

/// The tool id, and the permission key.
pub const ID: &str = "kill_shell";

/// What the model is told the tool is for.
const DESCRIPTION: &str = "\
Kills a running background shell by its ID.

- Takes a bash_id parameter identifying the background shell, reported when \
  the original bash call ran with run_in_background set to true.
- Ends the whole process tree the shell started, not only the shell itself.
- A shell that has already exited or was already killed is reported as such \
  rather than refused: asking to kill something already dead is not an \
  error.
- Use this tool when a background shell is no longer needed, or has run \
  longer than expected.";

/// What the model passes to `kill_shell`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The ID of the background shell to kill
    bash_id: String,
}

/// Ends a background job.
pub struct KillShellTool;

#[async_trait]
impl Tool for KillShellTool {
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

        format!("kill_shell {id}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let Some(jobs) = ctx.jobs.as_ref() else {
            return Err(ToolError::Failed(job::NO_JOBS.to_owned()));
        };

        let status = jobs
            .kill(&args.bash_id)
            .await
            .map_err(|error| match error {
                JobsError::NotFound(id) => {
                    ToolError::Failed(format!("no background shell with id {id}"))
                }
            })?;

        Ok(ToolOutput {
            title: format!("kill_shell {}", args.bash_id),
            output: format!(
                "background shell {} is now {}",
                status.id,
                state_word(&status.state)
            ),
            metadata: serde_json::json!({
                "bash_id": status.id,
                "status": state_word(&status.state),
            }),
        })
    }
}

#[cfg(test)]
#[path = "kill_shell_tests.rs"]
mod tests;
