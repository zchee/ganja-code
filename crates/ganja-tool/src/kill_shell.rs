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
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use super::{ID, KillShellTool};
    use crate::{
        Credentials, FileTimes, Tool as _, ToolCtx, ToolError,
        job::{JobRead, JobStatus, Jobs, JobsError, State},
    };

    /// A [`Jobs`] whose `kill` answer is scripted, so this module's tests
    /// exercise the tool's own shaping without a real process anywhere.
    #[derive(Debug)]
    struct Scripted {
        answer: Result<JobStatus, JobsError>,
    }

    #[async_trait]
    impl Jobs for Scripted {
        async fn start(&self, _command: String, _child: tokio::process::Child) -> JobStatus {
            unimplemented!("kill_shell never starts a job")
        }

        async fn output(&self, _bash_id: &str) -> Result<JobRead, JobsError> {
            unimplemented!("kill_shell never reads a job's output")
        }

        async fn kill(&self, _bash_id: &str) -> Result<JobStatus, JobsError> {
            self.answer.clone()
        }

        fn list(&self) -> Vec<JobStatus> {
            Vec::new()
        }
    }

    fn ctx(jobs: Option<Arc<dyn Jobs>>) -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            call_id: "call_1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: Credentials::Unguarded,
            spawn: None,
            postbox: None,
            ask: None,
            switch: None,
            jobs,
        }
    }

    #[tokio::test]
    async fn a_call_with_no_jobs_handle_is_refused_politely() {
        let refused = KillShellTool
            .run(serde_json::json!({ "bash_id": "bash_1" }), &ctx(None))
            .await
            .expect_err("a context with no jobs handle has nothing to kill");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("not available")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_id_is_refused_by_name() {
        let jobs: Arc<dyn Jobs> = Arc::new(Scripted {
            answer: Err(JobsError::NotFound("bash_9".to_owned())),
        });

        let refused = KillShellTool
            .run(serde_json::json!({ "bash_id": "bash_9" }), &ctx(Some(jobs)))
            .await
            .expect_err("nothing this registry knows answers");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("bash_9")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn a_killed_job_reports_its_terminal_status() {
        let jobs: Arc<dyn Jobs> = Arc::new(Scripted {
            answer: Ok(JobStatus {
                id: "bash_1".to_owned(),
                command: "sleep 300".to_owned(),
                state: State::Killed,
            }),
        });

        let out = KillShellTool
            .run(serde_json::json!({ "bash_id": "bash_1" }), &ctx(Some(jobs)))
            .await
            .expect("a known id answers");

        assert!(out.output.contains("killed"), "got {:?}", out.output);
        assert_eq!(out.metadata["status"], "killed");
    }

    /// Killing something already dead is not an error — the tool's own
    /// contract, exercised through the shaping this module owns.
    #[tokio::test]
    async fn an_already_exited_job_reports_that_rather_than_erroring() {
        let jobs: Arc<dyn Jobs> = Arc::new(Scripted {
            answer: Ok(JobStatus {
                id: "bash_1".to_owned(),
                command: "true".to_owned(),
                state: State::Exited { code: Some(0) },
            }),
        });

        let out = KillShellTool
            .run(serde_json::json!({ "bash_id": "bash_1" }), &ctx(Some(jobs)))
            .await
            .expect("an already-terminal job is still answered, not refused");

        assert!(
            out.output.contains("exited with code 0"),
            "got {:?}",
            out.output
        );
    }

    #[test]
    fn the_tool_id_is_kill_shell() {
        assert_eq!(ID, "kill_shell");
        assert_eq!(KillShellTool.id(), "kill_shell");
    }
}
