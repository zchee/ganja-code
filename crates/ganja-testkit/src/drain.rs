//! Draining a turn's event stream to its finish — with or without answering
//! permission dialogs along the way — or up to the dialog it holds open on.

use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::BoxStream;
use ganja_core::Engine;
use ganja_protocol::{Command, Event, PermissionId, PermissionReply};

/// Collects every event up to and including the turn's finish.
///
/// A stream that ends before [`Event::MessageFinished`] is a broken fixture —
/// a dropped engine, a turn task that died — and panics right here, at the
/// drain site. Handing back the partial collection instead would let a
/// negative assertion pass vacuously: "no permission was ever requested" is
/// trivially true of a transcript that never happened.
pub async fn drain(events: &mut BoxStream<'static, Event>) -> Vec<Event> {
    let mut seen = Vec::new();

    loop {
        let event = events.next().await.expect("the turn should finish before the stream ends");
        let finished = matches!(event, Event::MessageFinished { .. });
        seen.push(event);

        if finished {
            return seen;
        }
    }
}

/// The same, answering every permission request along the way with `reply`.
pub async fn drain_answering(
    engine: &Engine,
    events: &mut BoxStream<'static, Event>,
    reply: PermissionReply,
) -> Vec<Event> {
    let mut seen = Vec::new();

    loop {
        let event = events.next().await.expect("the turn should finish before the stream ends");
        if let Event::PermissionRequested { id, .. } = &event {
            engine
                .send(Command::ReplyPermission { id: id.clone(), reply })
                .await
                .expect("a reply is never refused");
        }
        let finished = matches!(event, Event::MessageFinished { .. });
        seen.push(event);

        if finished {
            return seen;
        }
    }
}

/// The same, always answering [`PermissionReply::Once`] — for suites where
/// every dialog should just be let through.
pub async fn drain_allowing(engine: &Engine, events: &mut BoxStream<'static, Event>) -> Vec<Event> {
    drain_answering(engine, events, PermissionReply::Once).await
}

/// Reads events up to the first permission dialog, handing back its id and
/// everything seen, and leaving the turn held open on it.
///
/// A turn that finishes before any dialog is a failure named here, not a wait
/// on a stream that never ends: the engine outlives its turns, so `events`
/// has no end to reach. Bounded at `limit` for the third shape, an engine
/// that neither finishes nor asks: under nextest that would be cut by the
/// per-test timeout without a word, and under a plain `cargo test` it would
/// hang.
pub async fn held_at_dialog(
    events: &mut BoxStream<'static, Event>,
    limit: Duration,
) -> (PermissionId, Vec<Event>) {
    tokio::time::timeout(limit, async {
        let mut seen = Vec::new();
        loop {
            let event = events.next().await.expect("the dialog arrives before the stream ends");
            let waiting = match &event {
                Event::PermissionRequested { id, .. } => Some(id.clone()),
                _ => None,
            };
            let finished = matches!(event, Event::MessageFinished { .. });
            seen.push(event);
            if let Some(id) = waiting {
                return (id, seen);
            }
            assert!(
                !finished,
                "the turn finished before a call raised its dialog: {:?}",
                seen.last()
            );
        }
    })
    .await
    .unwrap_or_else(|_| panic!("a call's dialog is raised within {limit:?}"))
}
