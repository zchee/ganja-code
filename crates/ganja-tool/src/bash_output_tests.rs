use std::sync::Arc;

use async_trait::async_trait;

use super::{BashOutputTool, ID};
use crate::job::{JobRead, JobStatus, Jobs, JobsError, State};
use crate::{Tool as _, ToolCtx, ToolError};

/// A [`Jobs`] whose every answer is scripted, so this module's tests
/// exercise the tool's own shaping without a real process anywhere.
#[derive(Debug, Default)]
struct Scripted {
    read: Option<JobRead>,
}

#[async_trait]
impl Jobs for Scripted {
    async fn start(&self, _command: String, _child: tokio::process::Child) -> JobStatus {
        unimplemented!("bash_output never starts a job")
    }

    async fn output(&self, bash_id: &str) -> Result<JobRead, JobsError> {
        self.read.clone().ok_or_else(|| JobsError::NotFound(bash_id.to_owned()))
    }

    async fn kill(&self, _bash_id: &str) -> Result<JobStatus, JobsError> {
        unimplemented!("bash_output never kills a job")
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
    let refused = BashOutputTool
        .run(serde_json::json!({ "bash_id": "bash_1" }), &ctx(None))
        .await
        .expect_err("a context with no jobs handle has nothing to poll");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("not available")),
        "got {refused:?}"
    );
}

#[tokio::test]
async fn an_unknown_id_is_refused_by_name() {
    let jobs: Arc<dyn Jobs> = Arc::new(Scripted::default());
    let refused = BashOutputTool
        .run(serde_json::json!({ "bash_id": "bash_9" }), &ctx(Some(jobs)))
        .await
        .expect_err("nothing this registry knows answers");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("bash_9")),
        "got {refused:?}"
    );
}

#[tokio::test]
async fn new_output_and_status_reach_the_model() {
    let jobs: Arc<dyn Jobs> = Arc::new(Scripted {
        read: Some(JobRead {
            chunk: "one\ntwo\n".to_owned(),
            status: JobStatus {
                id: "bash_1".to_owned(),
                command: "sleep 5".to_owned(),
                state: State::Running,
            },
        }),
    });

    let out = BashOutputTool
        .run(serde_json::json!({ "bash_id": "bash_1" }), &ctx(Some(jobs)))
        .await
        .expect("a known id answers");

    assert!(out.output.contains("one\ntwo"), "got {:?}", out.output);
    assert!(out.output.contains("<status>running</status>"));
    assert_eq!(out.metadata["bash_id"], "bash_1");
    assert_eq!(out.metadata["status"], "running");
}

#[tokio::test]
async fn no_new_output_says_so_rather_than_printing_nothing() {
    let jobs: Arc<dyn Jobs> = Arc::new(Scripted {
        read: Some(JobRead {
            chunk: String::new(),
            status: JobStatus {
                id: "bash_1".to_owned(),
                command: "sleep 5".to_owned(),
                state: State::Running,
            },
        }),
    });

    let out = BashOutputTool
        .run(serde_json::json!({ "bash_id": "bash_1" }), &ctx(Some(jobs)))
        .await
        .expect("a known id answers");

    assert!(out.output.starts_with("(no new output)"), "got {:?}", out.output);
}

#[tokio::test]
async fn a_filter_keeps_only_matching_lines() {
    let jobs: Arc<dyn Jobs> = Arc::new(Scripted {
        read: Some(JobRead {
            chunk: "info: starting\nerror: bad\ninfo: done\n".to_owned(),
            status: JobStatus {
                id: "bash_1".to_owned(),
                command: "build".to_owned(),
                state: State::Exited { code: Some(0) },
            },
        }),
    });

    let out = BashOutputTool
        .run(serde_json::json!({ "bash_id": "bash_1", "filter": "^error" }), &ctx(Some(jobs)))
        .await
        .expect("a known id answers");

    assert!(out.output.contains("error: bad"), "got {:?}", out.output);
    assert!(!out.output.contains("info:"), "got {:?}", out.output);
    assert!(out.output.contains("exited with code 0"));
}

#[tokio::test]
async fn an_invalid_filter_is_refused_as_a_bad_argument() {
    let jobs: Arc<dyn Jobs> = Arc::new(Scripted {
        read: Some(JobRead {
            chunk: "line\n".to_owned(),
            status: JobStatus {
                id: "bash_1".to_owned(),
                command: "x".to_owned(),
                state: State::Running,
            },
        }),
    });

    let refused = BashOutputTool
        .run(serde_json::json!({ "bash_id": "bash_1", "filter": "(" }), &ctx(Some(jobs)))
        .await
        .expect_err("an unparseable pattern cannot filter anything");

    assert!(matches!(refused, ToolError::InvalidArgs(_)), "got {refused:?}");
}

#[test]
fn the_tool_id_is_bash_output() {
    assert_eq!(ID, "bash_output");
    assert_eq!(BashOutputTool.id(), "bash_output");
}
