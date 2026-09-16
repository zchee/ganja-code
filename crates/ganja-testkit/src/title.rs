//! Telling a session's title request apart from its other requests.
//!
//! The engine's title prompt is private (`TITLE_PROMPT` in `session.rs`), so a
//! suite recognises the request by one phrase of it. Three suites asked the
//! same question of three shapes — a recorded [`ChatRequest`], a Responses body
//! read off a socket, and a relayed exchange — each with its own copy of the
//! phrase.

use ganja_core::provider::ChatRequest;
use serde_json::Value;

/// The phrase of the engine's title prompt a title request is recognised by.
const TITLE_MARKER: &str = "title generator";

/// Whether `request` is the one a session's title is asked with.
///
/// ```
/// use ganja_core::provider::ChatRequest;
///
/// let title = ChatRequest {
///     system: Some("You are a title generator. You output ONLY a thread title.".to_owned()),
///     ..ChatRequest::default()
/// };
/// assert!(ganja_testkit::is_title_request(&title));
/// assert!(!ganja_testkit::is_title_request(&ChatRequest::default()));
/// ```
#[must_use]
pub fn is_title_request(request: &ChatRequest) -> bool {
    request.system.as_deref().is_some_and(|system| system.contains(TITLE_MARKER))
}

/// Whether the Responses `body` is a title request: the system prompt rides in
/// its `instructions`.
///
/// ```
/// let title = serde_json::json!({"instructions": "You are a title generator."});
/// assert!(ganja_testkit::is_title_body(&title));
/// assert!(!ganja_testkit::is_title_body(&serde_json::json!({"instructions": "Answer."})));
/// ```
#[must_use]
pub fn is_title_body(body: &Value) -> bool {
    body["instructions"].as_str().is_some_and(|text| text.contains(TITLE_MARKER))
}
