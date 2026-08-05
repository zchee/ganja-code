//! One turn, two readers: a subscriber on the engine and a client on
//! `GET /event` hold the same transcript, frame for frame.
//!
//! This is the wave's honesty test for the SSE surface, and it is deliberately
//! **one** turn with **two concurrent readers** rather than the same script
//! run twice: ids and timestamps are minted per run, so two runs could only be
//! compared after normalizing away exactly the fields most worth checking.
//! Here nothing is normalized — the direct subscriber's events, serialized,
//! must equal the HTTP client's `event: message` payloads value for value, in
//! the same order, once the SSE envelope and the named control frames
//! (`connected`, `heartbeat`) are stripped.

mod support;

use futures::StreamExt as _;
use ganja_core::{permission::Permissions, tool::Registry};
use ganja_protocol::Event;
use ganja_testkit::{RecorderTool, says, tool_call};
use support::{
    DEADLINE, FAST_HEARTBEAT, Frame, base_url, drain_frames, loopback_config, scripted_engine,
};

/// One open `GET /event` connection, read frame by frame for as long as the
/// test needs it — which is the point: the same reader spans the whole turn.
struct SseReader {
    // `axum::body::Bytes` is the same `bytes::Bytes` reqwest yields; naming
    // it through axum keeps the bytes crate out of this manifest.
    stream: futures::stream::BoxStream<'static, reqwest::Result<axum::body::Bytes>>,
    buffer: Vec<u8>,
    frames: Vec<Frame>,
}

impl SseReader {
    fn new(response: reqwest::Response) -> Self {
        Self {
            stream: response.bytes_stream().boxed(),
            buffer: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// Reads until `done` says the frames collected so far are enough.
    async fn read_until(&mut self, mut done: impl FnMut(&[Frame]) -> bool) -> &[Frame] {
        while !done(&self.frames) {
            let chunk = tokio::time::timeout(DEADLINE, self.stream.next())
                .await
                .expect("the stream should keep speaking within the deadline")
                .expect("the stream should not end before the turn does")
                .expect("the transport should not fail");
            self.buffer.extend_from_slice(&chunk);
            self.frames.extend(drain_frames(&mut self.buffer));
        }

        &self.frames
    }
}

/// Whether a finished turn has appeared among the message frames.
fn holds_the_finish(frames: &[Frame]) -> bool {
    frames.iter().any(|frame| {
        frame.event == "message"
            && serde_json::from_str::<serde_json::Value>(&frame.data)
                .is_ok_and(|value| value["type"] == "message_finished")
    })
}

#[tokio::test]
async fn a_direct_subscriber_and_an_sse_client_see_the_same_turn_frame_for_frame() {
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let mut step_one = vec![ganja_core::provider::ProviderEvent::TextDelta(
        "Let me look. ".to_owned(),
    )];
    step_one.extend(tool_call("lookup", serde_json::json!({"key": "a"})));
    let engine = scripted_engine(
        vec![step_one, says("all done")],
        Registry::new(vec![tool]),
        Permissions::default(),
    );

    let handle = ganja_serve::serve(engine.clone(), loopback_config())
        .await
        .expect("a loopback server with no password comes up");
    let base = base_url(&handle);

    let response = reqwest::get(format!("{base}/event"))
        .await
        .expect("the event stream answers");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    for (header, wanted) in [
        ("cache-control", "no-cache, no-transform"),
        ("x-accel-buffering", "no"),
        ("x-content-type-options", "nosniff"),
    ] {
        assert_eq!(
            response
                .headers()
                .get(header)
                .and_then(|value| value.to_str().ok()),
            Some(wanted),
            "the {header} posture header travels"
        );
    }

    // Reading the connected frame proves the subscription's registration
    // point is behind us: everything the engine emits from here on is either
    // in this client's stream or after it, never lost between.
    let mut reader = SseReader::new(response);
    let first = reader.read_until(|frames| !frames.is_empty()).await;
    assert_eq!(
        first.first().map(|frame| frame.event.as_str()),
        Some("connected"),
        "the connected frame comes before anything else: {first:?}"
    );

    // The direct reader, registered before the prompt like every frontend.
    let mut direct = engine.subscribe().await.expect("a subscriber registers");

    let session = engine.session_id();
    let client = reqwest::Client::new();
    let accepted = client
        .post(format!("{base}/session/{}/prompt_async", session.as_str()))
        .header("content-type", "application/json")
        .body(r#"{"text":"look something up"}"#)
        .send()
        .await
        .expect("the prompt route answers");
    assert_eq!(
        accepted.status(),
        204,
        "a prompt is accepted and nothing more"
    );

    // Both readers to the same finish: the direct one first, then the open
    // SSE connection — the same connection that read the connected frame.
    let direct_events = ganja_testkit::drain(&mut direct).await;
    assert!(
        direct_events
            .iter()
            .any(|event| matches!(event, Event::PartUpdated { .. })),
        "the scripted turn ran its tool: {direct_events:?}"
    );
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1
    );

    let frames = reader.read_until(holds_the_finish).await.to_vec();

    // Strip the envelope and the named control frames; what remains must be
    // the direct transcript exactly.
    let over_http: Vec<serde_json::Value> = frames
        .iter()
        .filter(|frame| frame.event == "message")
        .map(|frame| serde_json::from_str(&frame.data).expect("every message frame is one event"))
        .collect();
    let direct_values: Vec<serde_json::Value> = direct_events
        .iter()
        .map(|event| serde_json::to_value(event).expect("every event serializes"))
        .collect();

    assert_eq!(
        over_http.len(),
        direct_values.len(),
        "both readers hold the same number of events\nhttp: {over_http:#?}\ndirect: {direct_values:#?}"
    );
    for (index, (http, direct)) in over_http.iter().zip(&direct_values).enumerate() {
        assert_eq!(http, direct, "frame {index} diverged");
    }

    // The only names on the wire besides the transcript are the two control
    // frames; anything else would be a frame nobody specified.
    assert!(
        frames
            .iter()
            .all(|frame| matches!(frame.event.as_str(), "connected" | "heartbeat" | "message")),
        "unexpected frame names: {frames:?}"
    );

    // A quiet stream still proves it is alive: with the heartbeat turned all
    // the way down, silence after the turn produces heartbeat frames.
    tokio::time::sleep(FAST_HEARTBEAT * 4).await;
    let after = reader
        .read_until(|frames| frames.iter().any(|frame| frame.event == "heartbeat"))
        .await;
    assert!(
        after.iter().any(|frame| frame.event == "heartbeat"),
        "a silent stream heartbeats"
    );

    handle.shutdown().await.expect("the server stops cleanly");
}
