//! Proves a cancelled turn stops promptly and stays stopped.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use ganja_core::{Command, Engine, Event, FinishReason, provider::FakeProvider, provider::fake};

/// The plan's budget: a cancel is visible within a tenth of a second.
const CANCEL_BUDGET: Duration = Duration::from_millis(100);

/// How long the stream is watched after the turn ended to prove it stays quiet.
const QUIET: Duration = Duration::from_millis(50);

/// Fragments taken before cancelling, so the cancel lands mid-stream.
const WARMUP_FRAGMENTS: usize = 3;

#[tokio::test]
async fn cancelling_mid_stream_finishes_the_turn_inside_the_budget() {
    let engine = Engine::new(Arc::new(FakeProvider::default()), fake::MODEL);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hello".to_owned(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let mut fragments = 0;
    while fragments < WARMUP_FRAGMENTS {
        match events.next().await {
            Some(Event::PartDelta { .. }) => fragments += 1,
            Some(Event::MessageStarted { .. } | Event::PartStarted { .. }) => {}
            other => panic!("expected the reply to stream, got {other:?}"),
        }
    }

    let issued = Instant::now();
    engine
        .send(Command::CancelTurn)
        .await
        .expect("a streaming engine accepts a cancel");

    let reason = loop {
        match events.next().await {
            // Fragments already queued when the cancel landed are still
            // delivered: the queue is lossless, the turn is what stops.
            Some(Event::PartDelta { .. }) => {}
            Some(Event::MessageFinished { reason, .. }) => break reason,
            other => panic!("expected the turn to finish, got {other:?}"),
        }
    };
    let elapsed = issued.elapsed();

    assert_eq!(reason, FinishReason::Cancelled);
    assert!(
        elapsed < CANCEL_BUDGET,
        "the turn took {elapsed:?} to stop, budget is {CANCEL_BUDGET:?}"
    );
    assert!(
        tokio::time::timeout(QUIET, events.next()).await.is_err(),
        "the provider kept streaming after the turn was cancelled"
    );
}
