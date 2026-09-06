//! Tool doubles shared by suites that need to prove a call reached — or
//! never reached — a registry entry.
//!
//! Every scripted-turn suite eventually needs a tool that records what it
//! was asked and hands back a canned reply, or one that blocks until its
//! turn is cancelled. What differs between suites is never the *shape* of
//! that tool, only the id it answers to, the words it hands back, and
//! whether a test needs to know the instant it started running — all values,
//! not forks.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ganja_tool::task::Subagents;
use ganja_tool::{Credentials, FileTimes, Tool, ToolCtx, ToolError, ToolOutput};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// A [`ToolCtx`] for a suite driving one tool call directly: a scratch
/// working directory, nothing guarded, and `spawn` as its only seam.
///
/// The other seams stay [`None`] on purpose — a call that reaches for a
/// postbox, a question, a switch or the job registry through this fixture is
/// a call the suite meant to drive some other way.
///
/// ```
/// let (subagents, _asked) = ganja_testkit::ScriptedSubagents::new(Vec::new());
/// let ctx = ganja_testkit::tool_ctx(subagents);
/// assert_eq!(ctx.call_id, "call_1");
/// ```
#[must_use]
pub fn tool_ctx(spawn: Arc<dyn Subagents>) -> ToolCtx {
    ToolCtx {
        cwd: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        call_id: "call_1".to_owned(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: Some(spawn),
        postbox: None,
        tasks: None,
        ask: None,
        switch: None,
        jobs: None,
    }
}

/// A permissive placeholder schema, for a tool double whose script never
/// exercises argument validation — the loop never validates a call's
/// arguments against its schema, but the registry still has to advertise
/// one.
#[derive(schemars::JsonSchema)]
#[expect(
    dead_code,
    reason = "never constructed; the field exists so the derived schema advertises one property"
)]
struct PlaceholderArgs {
    key: Option<String>,
}

/// The schema every tool double in this module advertises.
pub fn placeholder_schema() -> schemars::Schema {
    schemars::schema_for!(PlaceholderArgs)
}

/// Records every invocation's arguments and answers with a canned `title`
/// and `output`, so that "the call never ran" is a fact a test can read off
/// the handle rather than infer.
pub struct RecorderTool {
    id: &'static str,
    title: String,
    output: String,
    calls: Arc<Mutex<Vec<Value>>>,
}

impl RecorderTool {
    /// A tool named `id` that answers every call with `title`/`output`.
    ///
    /// ```
    /// let (_tool, calls) = ganja_testkit::RecorderTool::new("lookup", "lookup ran", "found it");
    /// assert!(calls.lock().unwrap().is_empty(), "nothing has run yet");
    /// ```
    pub fn new(
        id: &'static str,
        title: impl Into<String>,
        output: impl Into<String>,
    ) -> (Arc<Self>, Arc<Mutex<Vec<Value>>>) {
        let calls: Arc<Mutex<Vec<Value>>> = Arc::default();
        (
            Arc::new(Self {
                id,
                title: title.into(),
                output: output.into(),
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }
}

#[async_trait]
impl Tool for RecorderTool {
    fn id(&self) -> &str {
        self.id
    }

    fn description(&self) -> &str {
        "records what it was asked"
    }

    fn schema(&self) -> schemars::Schema {
        placeholder_schema()
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.calls.lock().expect("the call log is never poisoned").push(args);

        Ok(ToolOutput {
            title: self.title.clone(),
            output: self.output.clone(),
            metadata: serde_json::json!({}),
        })
    }
}

/// Blocks until the turn's cancel token fires, then answers
/// [`ToolError::Cancelled`] — for suites proving a cancel actually reaches a
/// call already in flight.
///
/// The optional entry signal is for a test that has to know the instant this
/// started running, to land its cancel deterministically mid-execution
/// rather than race it.
pub struct BlockingTool {
    id: &'static str,
    description: &'static str,
    entered: Option<tokio::sync::mpsc::Sender<()>>,
}

impl BlockingTool {
    /// A tool named `id` that blocks silently until cancelled.
    pub fn new(id: &'static str, description: &'static str) -> Arc<Self> {
        Arc::new(Self { id, description, entered: None })
    }

    /// The same, sending on `entered` the instant it starts running.
    pub fn with_entry_signal(
        id: &'static str,
        description: &'static str,
        entered: tokio::sync::mpsc::Sender<()>,
    ) -> Arc<Self> {
        Arc::new(Self { id, description, entered: Some(entered) })
    }
}

#[async_trait]
impl Tool for BlockingTool {
    fn id(&self) -> &str {
        self.id
    }

    fn description(&self) -> &str {
        self.description
    }

    fn schema(&self) -> schemars::Schema {
        placeholder_schema()
    }

    async fn run(&self, _args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if let Some(entered) = &self.entered {
            let _ = entered.send(()).await;
        }
        ctx.cancel.cancelled().await;

        Err(ToolError::Cancelled)
    }
}
