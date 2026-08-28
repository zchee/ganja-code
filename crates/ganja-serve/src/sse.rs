//! `GET /event`: the engine's event stream as server-sent events.
//!
//! Spec: upstream `packages/opencode/src/server/routes/instance/httpapi/handlers/event.ts`.
//! The shape is upstream's — a connected frame before anything else, a
//! heartbeat every ten seconds of silence, and the three headers that keep
//! proxies from buffering the stream — with one deliberate difference:
//! upstream wraps its control frames as `event: message` data whose inner
//! `type` is `server.connected` / `server.heartbeat`, where here they are
//! *named* SSE events (`event: connected`, `event: heartbeat`) and only real
//! engine events travel as `event: message`. Ganja's `Event` enum is the
//! wire type, and a control frame pretending to be one would be a lie a
//! deserializer trips over (deviation: control-frames-are-named-sse-events).
//!
//! The subscription is [`ganja_core::Engine::subscribe_droppable`], claimed in the
//! handler **before** the response exists: registration is the atomic point
//! after which nothing published is lost, so a client that reads the
//! connected frame knows every later engine event is either in its stream or
//! after its registration. A subscriber that falls behind is evicted whole —
//! the stream then ends with a terminal `event: evicted` frame naming the
//! overflow, and the client re-reads state over the REST surface rather than
//! trusting a torn transcript.

use std::convert::Infallible;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use ganja_core::Evicted;
use ganja_protocol::Event;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::state::AppState;

/// Frames buffered between the pump and the HTTP connection. Small on
/// purpose: the engine-side queue is the one with the eviction policy, and a
/// deep buffer here would only delay noticing a dead client.
const FRAME_QUEUE: usize = 16;

pub(crate) async fn events(State(state): State<AppState>) -> Response {
    // Registered before the response body exists — see the module docs.
    let subscription = state.engine.subscribe_droppable();

    let (frames, body) = mpsc::channel::<Result<Bytes, Infallible>>(FRAME_QUEUE);
    tokio::spawn(pump(subscription, frames, state.heartbeat, state.shutdown.clone()));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        // The trio upstream sets (`handlers/event.ts:80-82`), so nothing
        // between the engine and the client coalesces or sniffs the stream.
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from_stream(ReceiverStream::new(body)))
        .expect("a static header set always builds")
}

/// Drives one client's stream: connected first, then engine events and
/// heartbeats until the engine ends, the client leaves, the server is asked
/// to stop, or the subscription is evicted. A failed send means the client
/// is gone, which ends the pump and drops the engine-side subscription with
/// it; the shutdown watch is what lets a graceful shutdown drain a stream
/// that would otherwise never end.
async fn pump(
    mut events: BoxStream<'static, Result<Event, Evicted>>,
    frames: mpsc::Sender<Result<Bytes, Infallible>>,
    heartbeat: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    if frames.send(Ok(frame("connected", "{}"))).await.is_err() {
        return;
    }

    let mut ticker = tokio::time::interval(heartbeat);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // An interval's first tick completes immediately; the connected frame
    // already said hello, so the clock starts one period out.
    ticker.reset();

    loop {
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => {
                    // A protocol event is serde-derived data the engine built;
                    // it cannot fail to serialize.
                    let Ok(json) = serde_json::to_string(&event) else {
                        continue;
                    };
                    if frames.send(Ok(frame("message", &json))).await.is_err() {
                        return;
                    }
                }
                Some(Err(evicted)) => {
                    // Observable, then over: the one thing worse than a torn
                    // stream is a torn stream that looks whole.
                    let notice = serde_json::json!({
                        "type": "evicted",
                        "message": evicted.to_string(),
                    });
                    let _ = frames.send(Ok(frame("evicted", &notice.to_string()))).await;
                    return;
                }
                None => return,
            },
            _ = ticker.tick() => {
                if frames.send(Ok(frame("heartbeat", "{}"))).await.is_err() {
                    return;
                }
            }
            // Flipped or dropped, the answer is the same: end the stream so
            // the connection can drain and the shutdown can finish.
            _ = shutdown.changed() => return,
        }
    }
}

/// One SSE frame. The data is always a single JSON document, which
/// `serde_json` never puts a raw newline in, so the one-`data:`-line shape
/// holds without a splitter.
fn frame(event: &str, data: &str) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
