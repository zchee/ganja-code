//! The permission surface over HTTP: a dialog the engine raises appears on
//! `GET /permission`, is answered through `POST /permission/{id}/reply`, and
//! disappears once answered — the whole loop a remote client needs to stand
//! in for the person at the terminal.

mod support;

use std::time::Duration;

use ganja_core::{
    permission::{Action, Permissions, Rule},
    tool::Registry,
};
use ganja_protocol::{Event, PartBody, ToolState};
use ganja_testkit::{RecorderTool, says, tool_call};
use support::{DEADLINE, base_url, loopback_config, scripted_engine};

/// Permissions under which the `lookup` tool always asks, so the scripted
/// call below must raise a dialog rather than run.
fn asking_about_lookup() -> Permissions {
    let mut permissions = Permissions::default();
    permissions.set_baseline(vec![Rule {
        permission: "lookup".to_owned(),
        pattern: "*".to_owned(),
        action: Action::Ask,
    }]);

    permissions
}

/// Polls `GET /permission` until the predicate holds, inside the deadline.
async fn poll_permissions(
    base: &str,
    mut done: impl FnMut(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + DEADLINE;

    loop {
        let listed = reqwest::get(format!("{base}/permission"))
            .await
            .expect("the permission route answers")
            .json::<serde_json::Value>()
            .await
            .expect("the listing is JSON");
        if done(&listed) {
            return listed;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the listing never satisfied the test: {listed}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_dialog_is_listed_answered_over_http_and_then_gone() {
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = scripted_engine(
        vec![
            tool_call("lookup", serde_json::json!({"key": "a"})),
            says("done"),
        ],
        Registry::new(vec![tool]),
        asking_about_lookup(),
    );

    let handle = ganja_serve::serve(engine.clone(), loopback_config())
        .await
        .expect("a loopback server comes up");
    let base = base_url(&handle);

    // Nothing is waiting before the turn.
    let empty = poll_permissions(&base, |listed| listed.as_array().is_some()).await;
    assert_eq!(empty, serde_json::json!([]));

    let mut direct = engine.subscribe().await.expect("a subscriber registers");
    let session = engine.session_id();
    let accepted = reqwest::Client::new()
        .post(format!("{base}/session/{}/prompt_async", session.as_str()))
        .header("content-type", "application/json")
        .body(r#"{"text":"look it up"}"#)
        .send()
        .await
        .expect("the prompt route answers");
    assert_eq!(accepted.status(), 204);

    // The dialog crosses to the HTTP surface with everything a client needs
    // to render and answer it.
    let listed = poll_permissions(&base, |listed| {
        listed.as_array().is_some_and(|list| !list.is_empty())
    })
    .await;
    let request = &listed[0];
    assert_eq!(request["tool"], "lookup");
    assert_eq!(request["session_id"], session.as_str());
    assert_eq!(request["args"]["key"], "a");
    let id = request["id"].as_str().expect("the id travels");
    assert!(id.starts_with("perm_"), "a permission id: {id}");

    // Answer it over HTTP; the turn resumes and the tool runs.
    let replied = reqwest::Client::new()
        .post(format!("{base}/permission/{id}/reply"))
        .header("content-type", "application/json")
        .body(r#"{"response":"once"}"#)
        .send()
        .await
        .expect("the reply route answers");
    assert_eq!(replied.status(), 204);

    let events = ganja_testkit::drain(&mut direct).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::PermissionReplied { .. })),
        "the answer is observable on the stream: {events:?}"
    );
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "an allowed call ran"
    );

    // Answered means gone.
    let after = poll_permissions(&base, |listed| {
        listed.as_array().is_some_and(|list| list.is_empty())
    })
    .await;
    assert_eq!(after, serde_json::json!([]));

    handle.shutdown().await.expect("a clean stop");
}

#[tokio::test]
async fn a_rejected_dialog_never_runs_the_tool_and_the_turn_continues() {
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = scripted_engine(
        vec![
            tool_call("lookup", serde_json::json!({"key": "a"})),
            says("understood"),
        ],
        Registry::new(vec![tool]),
        asking_about_lookup(),
    );

    let handle = ganja_serve::serve(engine.clone(), loopback_config())
        .await
        .expect("a loopback server comes up");
    let base = base_url(&handle);

    let mut direct = engine.subscribe().await.expect("a subscriber registers");
    let session = engine.session_id();
    reqwest::Client::new()
        .post(format!("{base}/session/{}/prompt_async", session.as_str()))
        .header("content-type", "application/json")
        .body(r#"{"text":"look it up"}"#)
        .send()
        .await
        .expect("the prompt route answers");

    let listed = poll_permissions(&base, |listed| {
        listed.as_array().is_some_and(|list| !list.is_empty())
    })
    .await;
    let id = listed[0]["id"].as_str().expect("the id travels").to_owned();

    let replied = reqwest::Client::new()
        .post(format!("{base}/permission/{id}/reply"))
        .header("content-type", "application/json")
        .body(r#"{"response":"reject"}"#)
        .send()
        .await
        .expect("the reply route answers");
    assert_eq!(replied.status(), 204);

    // A refusal is information, never control flow: the call errors, the
    // model reads it, and the turn still finishes.
    let events = ganja_testkit::drain(&mut direct).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::PartUpdated { part, .. }
                if matches!(&part.body, PartBody::Tool { state: ToolState::Error { .. }, .. })
        )),
        "the refusal reaches the transcript as the call's error: {events:?}"
    );
    assert!(
        calls
            .lock()
            .expect("the call log is never poisoned")
            .is_empty(),
        "a rejected call never runs"
    );

    handle.shutdown().await.expect("a clean stop");
}
