//! The `todowrite` tool: the task list a turn keeps for itself.
//!
//! Spec: upstream `packages/opencode/src/tool/todo.ts`, its prompt in
//! `todowrite.txt`, and `packages/schema/src/session-todo.ts` for the item
//! shape. Upstream registers exactly one todo tool — `todowrite` — and stores
//! the list in the session so a frontend can render it; the `todoread` name
//! survives there only as a permission alias, so nothing here registers it.
//!
//! The list travels in each call's `metadata`, which is the copy a frontend
//! renders; upstream's session-stored copy has no consumer here, so none is
//! kept in the process.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Tool, ToolCtx, ToolError, ToolOutput};

/// Where a task has got to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TodoStatus {
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
enum TodoPriority {
    /// Do this before the rest.
    High,
    /// The default weight.
    Medium,
    /// Do this after the rest.
    Low,
}

/// One task on the list.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
struct TodoItem {
    /// Brief description of the task
    content: String,
    /// Current status of the task: pending, in_progress, completed, cancelled
    status: TodoStatus,
    /// Priority level of the task: high, medium, low
    priority: TodoPriority,
}

/// What the model passes to `todowrite`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The updated todo list
    todos: Vec<TodoItem>,
}

/// Validates and echoes the task list; the state is the transcript's.
#[derive(Debug, Default)]
pub struct TodoWriteTool;

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

        // The metadata carries the whole list, which is what a frontend
        // renders the call with.
        let metadata = serde_json::json!({ "todos": args.todos });

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
#[path = "todo_tests.rs"]
mod tests;
