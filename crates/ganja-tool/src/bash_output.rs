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
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::{BashOutputTool, ID};
    use crate::{
        Tool as _, ToolCtx, ToolError,
        job::{JobRead, JobStatus, Jobs, JobsError, State},
    };

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
            self.read
                .clone()
                .ok_or_else(|| JobsError::NotFound(bash_id.to_owned()))
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

        assert!(
            out.output.starts_with("(no new output)"),
            "got {:?}",
            out.output
        );
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
            .run(
                serde_json::json!({ "bash_id": "bash_1", "filter": "^error" }),
                &ctx(Some(jobs)),
            )
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
            .run(
                serde_json::json!({ "bash_id": "bash_1", "filter": "(" }),
                &ctx(Some(jobs)),
            )
            .await
            .expect_err("an unparseable pattern cannot filter anything");

        assert!(
            matches!(refused, ToolError::InvalidArgs(_)),
            "got {refused:?}"
        );
    }

    #[test]
    fn the_tool_id_is_bash_output() {
        assert_eq!(ID, "bash_output");
        assert_eq!(BashOutputTool.id(), "bash_output");
    }
}
