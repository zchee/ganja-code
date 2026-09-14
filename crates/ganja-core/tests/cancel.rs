//! Proves a cancelled turn stops promptly and stays stopped.
//!
//! # Why the clock is paused
//!
//! The budget used to be read off the wall clock, and on 2026-09-14 the
//! 4-core ubuntu CI runner read 271 ms against it (bead `ganja-code-ape9`).
//! Measured before the number was touched (2026-09-15, 16-core macOS, 20 runs
//! per condition): the cancel reached the token in 8–32 µs, and the turn's
//! `MessageFinished` arrived 71–259 µs after it on an idle machine, 80–893 µs
//! beside a full 8-thread workspace run (load average ~46), and 69–592 µs with
//! 32 CPU spinners added on top of that (load average 69). No fragment was
//! delivered after the cancel in any of the sixty runs.
//!
//! Nothing between the cancel and the finish waits on a timer: the step loop's
//! `select!` is biased towards the cancel, the fake races the same token
//! against its own sleep, and an [`Engine::new`] has no snapshots, storage or
//! hooks to pay for on the way out. What is left is in-memory work on this
//! test's one runtime thread. So the 271 ms was the runner not scheduling that
//! thread — some three hundred times the worst the engine was measured to
//! spend — and a wall-clock assertion was measuring the runner.
//!
//! The assertion now measures the engine's own time instead. Tokio's clock is
//! paused, so it moves only when every task is waiting on a timer, and the
//! fake's cadence is a virtual [`CADENCE`] ten times the budget: a turn that
//! waited for the provider's next fragment before stopping reads a whole
//! cadence, and one that stops on the cancel reads zero, whatever else the
//! machine is doing.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use ganja_core::Engine;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Command, Event, FinishReason};
use ganja_core::provider::{FakeProvider, fake};
use ganja_core::tool::Registry;
use tokio::time::Instant;

/// The plan's budget: a cancel is visible within a tenth of a second.
const CANCEL_BUDGET: Duration = Duration::from_millis(100);

/// The fake's delay between fragments, in paused time: far past the budget,
/// so waiting for one fragment is a failure rather than a close call.
const CADENCE: Duration = Duration::from_secs(1);

/// How long the stream is watched after the turn ended to prove it stays
/// quiet — several cadences, so a provider still streaming would be caught.
const QUIET: Duration = Duration::from_secs(3);

/// Fragments taken before cancelling, so the cancel lands mid-stream.
const WARMUP_FRAGMENTS: usize = 3;

/// A reply with words to spare past the warmup, so a turn that ignored the
/// cancel would still have fragments left to send.
const REPLY: &str = "one two three four five six seven eight nine ten eleven twelve";

#[tokio::test(start_paused = true)]
async fn cancelling_mid_stream_finishes_the_turn_inside_the_budget() {
    let engine = Engine::new(
        Arc::new(FakeProvider::new(REPLY, CADENCE)),
        fake::MODEL,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hello".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
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
    engine.send(Command::CancelTurn).await.expect("a streaming engine accepts a cancel");

    // The queue is lossless, so a fragment already queued at the cancel would
    // still arrive. None can be: the last one was read above and the next is a
    // whole cadence away. Any that turns up is one the turn waited for.
    let mut late = 0usize;
    let reason = loop {
        match events.next().await {
            Some(Event::PartDelta { .. }) => late += 1,
            Some(Event::MessageFinished { reason, .. }) => break reason,
            other => panic!("expected the turn to finish, got {other:?}"),
        }
    };
    let elapsed = issued.elapsed();

    assert_eq!(reason, FinishReason::Cancelled);
    assert!(
        elapsed < CANCEL_BUDGET,
        "the turn took {elapsed:?} of paused time to stop, budget is {CANCEL_BUDGET:?}"
    );
    assert_eq!(late, 0, "fragments arrived after the cancel: the turn waited for the provider");
    assert!(
        tokio::time::timeout(QUIET, events.next()).await.is_err(),
        "the provider kept streaming after the turn was cancelled"
    );
}
