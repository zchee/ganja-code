//! Draining a turn's event stream to its finish — with or without answering
//! permission dialogs along the way.

use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{Command, Engine, Event, PermissionReply};

/// Collects every event up to and including the turn's finish.
///
/// Returns whatever was collected if the stream ends first, rather than
/// panicking: a turn that never reaches [`Event::MessageFinished`] fails
/// whatever the test asserts about `seen.last()` next, which is a clearer
/// signal in a healthy suite than a panic at the drain site — and the
/// distinction is otherwise unobservable, since the stream never actually
/// closes early in a passing run.
pub async fn drain(events: &mut BoxStream<'static, Event>) -> Vec<Event> {
    let mut seen = Vec::new();

    loop {
        let Some(event) = events.next().await else {
            return seen;
        };
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
        let Some(event) = events.next().await else {
            return seen;
        };
        if let Event::PermissionRequested { id, .. } = &event {
            engine
                .send(Command::ReplyPermission {
                    id: id.clone(),
                    reply,
                })
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
