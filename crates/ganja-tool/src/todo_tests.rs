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

    assert_eq!(out.title, "3 todos", "upstream counts everything that is not completed");
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

    assert!(matches!(refused, ToolError::InvalidArgs(_)), "got {refused:?}");
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
        tool.description().contains("Create and maintain a structured task list"),
        "the ported prompt should reach the model intact"
    );
    assert_eq!(schema["required"], serde_json::json!(["todos"]));
    assert!(
        schema.to_string().contains("in_progress"),
        "the schema should spell out the statuses it accepts: {schema}"
    );
}
