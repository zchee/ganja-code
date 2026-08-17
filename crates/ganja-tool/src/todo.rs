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
mod tests {
    use std::path::PathBuf;

    use super::TodoWriteTool;
    use crate::{Tool, ToolCtx, ToolError};

    /// A context no todo call looks at, since the list is not on disk.
    fn ctx() -> ToolCtx {
        ToolCtx::fixture(PathBuf::from("."))
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
        let tool = TodoWriteTool;

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
    async fn a_second_write_carries_only_its_own_list() {
        let tool = TodoWriteTool;

        tool.run(todos(), &ctx()).await.expect("a list is written");

        let out = tool
            .run(
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
            out.metadata["todos"],
            serde_json::json!([
                { "content": "port webfetch", "status": "completed", "priority": "medium" },
            ]),
            "a write replaces the list rather than appending to it"
        );
    }

    #[tokio::test]
    async fn an_empty_list_is_a_legitimate_write() {
        let tool = TodoWriteTool;
        tool.run(todos(), &ctx()).await.expect("a list is written");

        let out = tool
            .run(serde_json::json!({ "todos": [] }), &ctx())
            .await
            .expect("clearing the list is allowed");

        assert_eq!(out.title, "0 todos");
        assert_eq!(
            out.metadata["todos"],
            serde_json::json!([]),
            "the metadata hands a frontend the emptied list"
        );
    }

    #[tokio::test]
    async fn a_status_outside_the_schema_is_refused() {
        let tool = TodoWriteTool;

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
    }

    #[test]
    fn the_one_line_description_counts_the_work_left_in_the_call() {
        let tool = TodoWriteTool;

        assert_eq!(tool.describe(&todos()), "3 todos");
        assert_eq!(tool.describe(&serde_json::json!({})), "0 todos");
    }

    #[test]
    fn the_prompt_and_schema_are_what_the_model_is_given() {
        let tool = TodoWriteTool;
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
