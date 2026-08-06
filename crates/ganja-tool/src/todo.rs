//! The `todowrite` tool: the task list a turn keeps for itself.
//!
//! Spec: upstream `packages/opencode/src/tool/todo.ts`, its prompt in
//! `todowrite.txt`, and `packages/schema/src/session-todo.ts` for the item
//! shape. Upstream registers exactly one todo tool — `todowrite` — and stores
//! the list in the session so a frontend can render it; the `todoread` name
//! survives there only as a permission alias, so nothing here registers it.
//!
//! The list lives in the process, which is right for as long as a process
//! serves one session. It moves into session storage with the persistence
//! work.

use std::sync::Mutex;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Tool, ToolCtx, ToolError, ToolOutput};

/// Where a task has got to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    Pending,
    /// Actively being worked on. Upstream's prompt allows exactly one.
    InProgress,
    /// Finished successfully.
    Completed,
    /// No longer needed.
    Cancelled,
}

/// How much a task matters relative to the others.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoPriority {
    /// Do this before the rest.
    High,
    /// The default weight.
    Medium,
    /// Do this after the rest.
    Low,
}

/// One task on the list.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct TodoItem {
    /// Brief description of the task
    pub content: String,
    /// Current status of the task: pending, in_progress, completed, cancelled
    pub status: TodoStatus,
    /// Priority level of the task: high, medium, low
    pub priority: TodoPriority,
}

/// What the model passes to `todowrite`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The updated todo list
    todos: Vec<TodoItem>,
}

/// Keeps the task list for the session.
#[derive(Debug, Default)]
pub struct TodoWriteTool {
    /// The list as the last call left it.
    todos: Mutex<Vec<TodoItem>>,
}

impl TodoWriteTool {
    /// Builds a tool holding an empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The list as it stands, for a frontend that renders it.
    #[must_use]
    pub fn todos(&self) -> Vec<TodoItem> {
        self.todos
            .lock()
            .expect("the todo list is never poisoned")
            .clone()
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn id(&self) -> &str {
        "todowrite"
    }

    fn description(&self) -> &str {
        include_str!("todowrite.txt")
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        // Read straight off the arguments, because a description is wanted
        // before the call runs and the stored list is still the previous one.
        let remaining = args
            .get("todos")
            .and_then(serde_json::Value::as_array)
            .map_or(0, |todos| {
                todos
                    .iter()
                    .filter(|todo| todo.get("status") != Some(&serde_json::json!("completed")))
                    .count()
            });

        title(remaining)
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let _ = ctx;
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        let output = serde_json::to_string_pretty(&args.todos)
            .expect("a list of todos is JSON by construction");
        let remaining = args
            .todos
            .iter()
            .filter(|todo| todo.status != TodoStatus::Completed)
            .count();

        // The stored list is what a frontend renders between calls; the
        // metadata is what it renders this call with.
        let metadata = serde_json::json!({ "todos": args.todos });
        *self.todos.lock().expect("the todo list is never poisoned") = args.todos;

        Ok(ToolOutput {
            title: title(remaining),
            output,
            metadata,
        })
    }
}

/// How upstream titles a write: the work still outstanding.
fn title(remaining: usize) -> String {
    format!("{remaining} todos")
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::{TodoItem, TodoPriority, TodoStatus, TodoWriteTool};
    use crate::{FileTimes, Tool, ToolCtx, ToolError};

    /// A context no todo call looks at, since the list is not on disk.
    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: PathBuf::from("."),
            cancel: CancellationToken::new(),
            call_id: "call-1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: crate::Credentials::Unguarded,
            spawn: None,
            ask: None,
        }
    }

    /// The list every test writes.
    fn todos() -> serde_json::Value {
        serde_json::json!({
            "todos": [
                { "content": "port the shell tool", "status": "completed", "priority": "high" },
                { "content": "port the todo tool", "status": "in_progress", "priority": "high" },
                { "content": "port webfetch", "status": "pending", "priority": "medium" },
                { "content": "port the plan tool", "status": "cancelled", "priority": "low" },
            ]
        })
    }

    #[tokio::test]
    async fn a_write_reports_the_work_that_is_left_and_hands_back_the_list() {
        let tool = TodoWriteTool::new();

        let out = tool.run(todos(), &ctx()).await.expect("a list is written");

        assert_eq!(
            out.title, "3 todos",
            "upstream counts everything that is not completed"
        );
        assert_eq!(
            out.metadata["todos"].as_array().map(Vec::len),
            Some(4),
            "the metadata carries the whole list for a frontend to render"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out.output)
                .expect("the output is the list as JSON"),
            todos()["todos"],
            "the model is handed back exactly what it wrote"
        );
    }

    #[tokio::test]
    async fn the_list_survives_between_calls_and_the_last_write_wins() {
        let tool = TodoWriteTool::new();

        tool.run(todos(), &ctx()).await.expect("a list is written");
        assert_eq!(tool.todos().len(), 4);

        tool.run(
            serde_json::json!({
                "todos": [
                    { "content": "port webfetch", "status": "completed", "priority": "medium" },
                ]
            }),
            &ctx(),
        )
        .await
        .expect("a shorter list replaces the longer one");

        assert_eq!(
            tool.todos(),
            vec![TodoItem {
                content: "port webfetch".to_owned(),
                status: TodoStatus::Completed,
                priority: TodoPriority::Medium,
            }],
            "a write replaces the list rather than appending to it"
        );
    }

    #[tokio::test]
    async fn an_empty_list_is_a_legitimate_write() {
        let tool = TodoWriteTool::new();
        tool.run(todos(), &ctx()).await.expect("a list is written");

        let out = tool
            .run(serde_json::json!({ "todos": [] }), &ctx())
            .await
            .expect("clearing the list is allowed");

        assert_eq!(out.title, "0 todos");
        assert!(tool.todos().is_empty());
    }

    #[tokio::test]
    async fn a_status_outside_the_schema_is_refused_rather_than_stored() {
        let tool = TodoWriteTool::new();

        let refused = tool
            .run(
                serde_json::json!({
                    "todos": [{ "content": "x", "status": "in-progress", "priority": "high" }]
                }),
                &ctx(),
            )
            .await
            .expect_err("`in-progress` is not one of the four statuses");

        assert!(
            matches!(refused, ToolError::InvalidArgs(_)),
            "got {refused:?}"
        );
        assert!(
            tool.todos().is_empty(),
            "a refused call must not have written anything"
        );
    }

    #[test]
    fn the_one_line_description_counts_the_work_left_in_the_call() {
        let tool = TodoWriteTool::new();

        assert_eq!(tool.describe(&todos()), "3 todos");
        assert_eq!(tool.describe(&serde_json::json!({})), "0 todos");
    }

    #[test]
    fn the_prompt_and_schema_are_what_the_model_is_given() {
        let tool = TodoWriteTool::new();
        let schema = serde_json::to_value(tool.schema()).expect("a schema is JSON");

        assert_eq!(tool.id(), "todowrite");
        assert!(
            tool.description()
                .contains("Create and maintain a structured task list"),
            "the ported prompt should reach the model intact"
        );
        assert_eq!(schema["required"], serde_json::json!(["todos"]));
        assert!(
            schema.to_string().contains("in_progress"),
            "the schema should spell out the statuses it accepts: {schema}"
        );
    }
}
