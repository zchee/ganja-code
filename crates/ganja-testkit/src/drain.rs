//! Draining a turn's event stream to its finish — with or without answering
//! permission dialogs along the way.

use futures::StreamExt as _;
use futures::stream::BoxStream;
use ganja_core::Engine;
use ganja_protocol::{Command, Event, PermissionReply};

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
