use std::sync::Arc;

use async_trait::async_trait;

use super::{ID, KillShellTool};
use crate::job::{JobRead, JobStatus, Jobs, JobsError, State};
use crate::{Tool as _, ToolCtx, ToolError};

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
    let mut ctx = ToolCtx::fixture(std::env::temp_dir());
    ctx.jobs = jobs;
    ctx
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
    let jobs: Arc<dyn Jobs> =
        Arc::new(Scripted { answer: Err(JobsError::NotFound("bash_9".to_owned())) });

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

    assert!(out.output.contains("exited with code 0"), "got {:?}", out.output);
}

#[test]
fn the_tool_id_is_kill_shell() {
    assert_eq!(ID, "kill_shell");
    assert_eq!(KillShellTool.id(), "kill_shell");
}
