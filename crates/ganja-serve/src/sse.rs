//! Resumable SSE over the process-local journal. Control frames never mint ids.

use std::convert::Infallible;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::EventJournal;
use crate::journal::Registration;
use crate::state::AppState;

const FRAME_QUEUE: usize = 16;

pub(crate) async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let cursor = match headers.get("last-event-id") {
        Some(value) => match value.to_str() {
            Ok(value) => Some(value),
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::empty())
                    .expect("a static response builds");
            }
        },
        None => None,
    };
    let registration = state.journal.register(cursor);
    let (frames, body) = mpsc::channel::<Result<Bytes, Infallible>>(FRAME_QUEUE);
    tokio::spawn(pump(state.journal, registration, frames, state.heartbeat, state.shutdown));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from_stream(ReceiverStream::new(body)))
        .expect("a static header set always builds")
}

async fn pump(
    journal: EventJournal,
    registration: Registration,
    frames: mpsc::Sender<Result<Bytes, Infallible>>,
    heartbeat: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Cancellation encloses sends too: a client not reading must not hold shutdown.
    let run = async {
        let Registration { head, mut cursor, mut seq, replay, mut changed } = registration;
        let connected = serde_json::json!({ "head": head, "replay": replay });
        if frames.send(Ok(frame("connected", &cursor, &connected.to_string()))).await.is_err() {
            return;
        }
        let mut ticker = tokio::time::interval(heartbeat);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.reset();
        loop {
            // Mark the notification observed before checking history, so a concurrent
            // append is either in this batch or wakes the next iteration.
            changed.borrow_and_update();
            let Some(batch) = journal.after(seq) else {
                let notice =
                    serde_json::json!({"type": "evicted", "message": "journal history overflow"});
                let _ = frames.send(Ok(frame("evicted", &cursor, &notice.to_string()))).await;
                return;
            };
            for event in batch {
                if frames.send(Ok(frame("message", &event.id, &event.data))).await.is_err() {
                    return;
                }
                seq = event.seq;
                cursor = event.id;
            }
            tokio::select! {
                result = changed.changed() => if result.is_err() { return; },
                _ = frames.closed() => return,
                _ = ticker.tick() => {
                    if frames.send(Ok(frame("heartbeat", &cursor, "{}"))).await.is_err() {
                        return;
                    }
                }
            }
        }
    };
    tokio::select! {
        _ = run => {},
        _ = shutdown.changed() => {},
    }
}

fn frame(event: &str, id: &str, data: &str) -> Bytes {
    Bytes::from(format!("event: {event}\nid: {id}\ndata: {data}\n\n"))
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
