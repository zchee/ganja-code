//! Proves the engine's delivery guarantee: a consumer slower than the producer
//! sees every event, in order, because the bounded queue makes the producer
//! wait rather than dropping anything.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::{
    Command, Event, FinishReason,
    engine::{EVENT_CAPACITY, Engine},
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent},
};
use tokio_util::sync::CancellationToken;

const EVENTS: usize = 10_000;

/// How far the producer may drift from the queue ceiling before the assertion
/// calls it unthrottled. The engine holds at most one fragment in flight while
/// it waits for room, plus the three events that open a turn.
const CEILING_SLACK: usize = 8;

/// Fragments the consumer takes between pauses.
///
/// The plan sketched a 10ms pause per event, which is 100 seconds for 10,000
/// events. Pausing once per batch keeps the property being tested — the queue
/// saturates, the producer stalls, nothing is lost — while the test still
/// finishes in about a second.
const DRAIN_BATCH: usize = 64;
const DRAIN_PAUSE: std::time::Duration = std::time::Duration::from_millis(1);

/// Time the producer is given to fill the queue before its progress is checked.
const FILL_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// Emits `count` numbered fragments as fast as it is polled.
struct FloodProvider {
    count: usize,
    produced: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for FloodProvider {
    fn id(&self) -> &str {
        "flood"
    }

    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let produced = Arc::clone(&self.produced);

        Ok(stream::iter(0..self.count)
            .map(move |index| {
                produced.fetch_add(1, Ordering::SeqCst);
                ProviderEvent::TextDelta(index.to_string())
            })
            .boxed())
    }
}

#[tokio::test]
async fn a_slow_consumer_receives_every_event_in_order() {
    let produced = Arc::new(AtomicUsize::new(0));
    let engine = Engine::new(
        Arc::new(FloodProvider {
            count: EVENTS,
            produced: Arc::clone(&produced),
        }),
        "flood-model",
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "flood".to_owned(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // Nothing is read yet, so an unthrottled producer would race to `EVENTS`.
    tokio::time::sleep(FILL_GRACE).await;
    let queued = produced.load(Ordering::SeqCst);
    assert!(
        queued <= EVENT_CAPACITY + CEILING_SLACK,
        "the producer ran past the queue ceiling: {queued} fragments with no reader"
    );
    assert!(
        queued + CEILING_SLACK >= EVENT_CAPACITY,
        "the queue never filled, so backpressure was never exercised: {queued} fragments"
    );

    // A turn opens with both message envelopes and the part the fragments land
    // in, before any fragment addresses it.
    assert!(matches!(
        events.next().await,
        Some(Event::MessageStarted { .. })
    ));
    assert!(matches!(
        events.next().await,
        Some(Event::MessageStarted { .. })
    ));
    let Some(Event::PartStarted { part, .. }) = events.next().await else {
        panic!("the reply's part should be announced before its fragments");
    };

    for index in 0..EVENTS {
        if index.is_multiple_of(DRAIN_BATCH) {
            tokio::time::sleep(DRAIN_PAUSE).await;
        }

        let Some(Event::PartDelta { part_id, delta, .. }) = events.next().await else {
            panic!("fragment {index} arrived out of order or not at all");
        };
        assert_eq!(part_id, part.id, "fragment {index} addressed another part");
        assert_eq!(delta, index.to_string(), "fragment {index} arrived late");
    }

    assert!(matches!(
        events.next().await,
        Some(Event::MessageFinished {
            reason: FinishReason::Completed,
            ..
        })
    ));
    assert_eq!(
        produced.load(Ordering::SeqCst),
        EVENTS,
        "the producer stopped short of its fragment count"
    );
}
