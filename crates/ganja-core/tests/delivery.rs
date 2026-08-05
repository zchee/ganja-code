//! Proves the engine's delivery policies, subscriber by subscriber: a lossless
//! consumer slower than the producer sees every event, in order, because its
//! bounded queue makes the producer wait rather than dropping anything; a
//! droppable consumer that stops draining is evicted with an observable error
//! rather than stalling the turn; the first subscriber inherits everything
//! buffered since the engine was born; and a later one joins from its
//! registration on.

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
    Evicted, RevertState, SessionId, Storage,
    engine::{EVENT_CAPACITY, Engine},
    protocol::{Command, Event, FinishReason, Message, Part},
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent},
};
use ganja_testkit::{drain, seed_message, seed_session, seeded_session_info};
use tokio_util::sync::CancellationToken;

const EVENTS: usize = 10_000;

/// How far the producer may drift from the queue ceiling before the assertion
/// calls it unthrottled. The engine holds at most one fragment in flight while
/// it waits for room, plus the four events that open a turn.
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

/// How long a drain that should complete promptly is given before the test
/// calls it wedged. Reached only when delivery is broken — a green run never
/// waits on it — and it exists so a queue nothing feeds fails loudly instead
/// of parking the test forever.
const DRAIN_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

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
        Arc::new(ganja_core::tool::Registry::new(Vec::new())),
        ganja_core::permission::Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "flood".to_owned(),
            mentions: Vec::new(),
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
    // in, before any fragment addresses it. The agent loop also brackets each
    // request with step-marker parts, which carry no fragments and are skipped.
    assert!(matches!(
        events.next().await,
        Some(Event::MessageStarted { .. })
    ));
    assert!(matches!(
        events.next().await,
        Some(Event::MessageStarted { .. })
    ));
    let part = loop {
        match events.next().await {
            Some(Event::PartStarted { part, .. }) if part.as_text().is_some() => break part,
            Some(Event::PartStarted { .. }) => {}
            other => panic!("the reply's part should be announced before its fragments: {other:?}"),
        }
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

    // The step-finish marker precedes the finish; both arrive after every
    // fragment, which is the ordering being proved.
    loop {
        match events.next().await {
            Some(Event::PartStarted { .. }) => {}
            Some(Event::MessageFinished {
                reason: FinishReason::Completed,
                ..
            }) => break,
            other => panic!("the turn should end after the last fragment: {other:?}"),
        }
    }
    assert_eq!(
        produced.load(Ordering::SeqCst),
        EVENTS,
        "the producer stopped short of its fragment count"
    );
}

/// An engine over a flood provider that streams `count` fragments per turn,
/// with no tools and default rules — the delivery tests prove queues, not the
/// loop.
fn flood_engine(count: usize) -> Engine {
    Engine::new(
        Arc::new(FloodProvider {
            count,
            produced: Arc::default(),
        }),
        "flood-model",
        Arc::new(ganja_core::tool::Registry::new(Vec::new())),
        ganja_core::permission::Permissions::default(),
    )
}

/// Starts a turn answering `text`.
async fn prompt(engine: &Engine, text: &str) {
    engine
        .send(Command::SendPrompt {
            text: text.to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
}

/// The text of the user message that opens a drained turn, for telling one
/// turn's frames from another's.
fn opening_prompt(seen: &[Event]) -> Option<String> {
    let Some(Event::MessageStarted {
        session_id: _,
        message,
    }) = seen.first()
    else {
        return None;
    };

    message
        .parts
        .first()
        .and_then(Part::as_text)
        .map(str::to_owned)
}

/// A droppable subscriber that never drains is evicted — its stream ends with
/// the eviction, observably — while the lossless subscriber beside it
/// receives the whole turn: the agent loop never waits on a subscriber that
/// chose to be droppable.
#[tokio::test]
async fn a_wedged_droppable_subscriber_is_evicted_and_the_lossless_one_drains_the_turn() {
    // More fragments than one queue can hold, so the wedged queue must
    // overflow before the turn ends.
    const FLOOD: usize = EVENT_CAPACITY + 16;

    let engine = flood_engine(FLOOD);
    let mut lossless = engine
        .subscribe()
        .await
        .expect("the first subscriber claims the birth queue");
    // Registered and then deliberately never polled until the turn is over.
    let droppable = engine.subscribe_droppable();

    prompt(&engine, "flood").await;

    let mut fragments = 0;
    tokio::time::timeout(DRAIN_PATIENCE, async {
        loop {
            match lossless.next().await {
                Some(Event::PartDelta { .. }) => fragments += 1,
                Some(Event::MessageFinished {
                    reason: FinishReason::Completed,
                    ..
                }) => break,
                Some(_) => {}
                None => panic!("the lossless stream ended before the turn did"),
            }
        }
    })
    .await
    .expect("the turn finishes although the droppable subscriber is wedged");
    assert_eq!(fragments, FLOOD, "the lossless subscriber missed nothing");

    // The engine goes away first, so a broken eviction fails on an ended
    // stream rather than parking the test on a queue that never closes.
    drop(engine);
    let collected: Vec<Result<Event, Evicted>> = droppable.collect().await;
    assert_eq!(
        collected.len(),
        EVENT_CAPACITY + 1,
        "everything the queue held, then the eviction and nothing after it"
    );
    assert!(
        collected[..EVENT_CAPACITY].iter().all(Result::is_ok),
        "what was queued before the eviction is real and in order"
    );
    assert_eq!(
        collected.last(),
        Some(&Err(Evicted)),
        "an eviction is observable, never a silent end"
    );
}

/// A lossless subscriber registered later sees events from its registration
/// on: nothing of the turn that finished before it, and the turn after it
/// frame for frame as the subscriber that was always there.
#[tokio::test]
async fn a_late_subscriber_sees_events_from_its_registration_on() {
    let engine = flood_engine(4);
    let mut first = engine
        .subscribe()
        .await
        .expect("the first subscriber claims the birth queue");

    prompt(&engine, "first").await;
    let turn_one = tokio::time::timeout(DRAIN_PATIENCE, drain(&mut first))
        .await
        .expect("the first turn drains");
    assert_eq!(opening_prompt(&turn_one).as_deref(), Some("first"));

    let mut second = engine
        .subscribe()
        .await
        .expect("a later subscriber registers a queue of its own");
    prompt(&engine, "second").await;

    let first_hears = tokio::time::timeout(DRAIN_PATIENCE, drain(&mut first))
        .await
        .expect("the resident subscriber drains the second turn");
    let second_hears = tokio::time::timeout(DRAIN_PATIENCE, drain(&mut second))
        .await
        .expect("the late subscriber drains the second turn");

    assert_eq!(
        opening_prompt(&second_hears).as_deref(),
        Some("second"),
        "the late subscriber's stream opens with the turn it registered before, \
         not with anything of the one it missed: {second_hears:?}"
    );
    assert_eq!(
        first_hears, second_hears,
        "the second turn reaches both subscribers frame for frame"
    );
}

/// The engine-birth queue buffers from construction, so a frontend may act
/// first and subscribe second: everything a turn published before anybody
/// subscribed is waiting for the first subscriber, whole and in order.
#[tokio::test]
async fn the_first_subscriber_inherits_the_events_queued_before_it_subscribed() {
    const FLOOD: usize = 4;

    let engine = flood_engine(FLOOD);
    // Nobody is subscribed while this whole turn runs.
    prompt(&engine, "unheard").await;
    // Wait until the engine is idle again before subscribing, so everything
    // below happens strictly after the whole turn was published: an `Undo` is
    // refused with `Busy` while a turn streams and with `NoSnapshots` once it
    // is over, and the finish event is queued before the busy slot opens. A
    // subscribe that raced the turn instead could be registered before the
    // events it must *inherit* were published, and would prove nothing.
    tokio::time::timeout(DRAIN_PATIENCE, async {
        loop {
            match engine.send(Command::Undo).await {
                Err(ganja_core::EngineError::Busy) => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(ganja_core::EngineError::NoSnapshots) => break,
                other => panic!("the idle probe should be refused, got {other:?}"),
            }
        }
    })
    .await
    .expect("the unheard turn finishes");

    let mut events = engine
        .subscribe()
        .await
        .expect("the first subscriber claims the birth queue");
    let seen = tokio::time::timeout(DRAIN_PATIENCE, drain(&mut events))
        .await
        .expect("the whole turn should be waiting in the birth queue");

    assert_eq!(
        opening_prompt(&seen).as_deref(),
        Some("unheard"),
        "the turn is inherited from its first frame: {seen:?}"
    );
    let fragments: Vec<&str> = seen
        .iter()
        .filter_map(|event| match event {
            Event::PartDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        fragments,
        ["0", "1", "2", "3"],
        "every fragment survived the wait, in order"
    );
    assert!(
        matches!(
            seen.last(),
            Some(Event::MessageFinished {
                reason: FinishReason::Completed,
                ..
            })
        ),
        "and the finish is the last thing inherited: {seen:?}"
    );
}

/// The specific ordering `ganja run` depends on, pinned where it can be: the
/// cli resumes before it subscribes, and a session left mid-undo announces
/// its revert during that resume — so the announcement must wait in the birth
/// queue for the first subscriber rather than needing one to exist.
#[tokio::test]
async fn a_resumes_revert_notice_waits_in_the_birth_queue_for_the_first_subscriber() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(directory.path().join("storage"));

    // A session left mid-undo: its record carries the revert, anchored on the
    // one stored message.
    let anchor = Message::user("the prompt the undo took back");
    let mut info = seeded_session_info(SessionId::ascending(), 0);
    info.revert = Some(RevertState {
        message_id: anchor.id.clone(),
        snapshot: None,
        files: Vec::new(),
    });
    storage.save_info(&info).expect("the seeded record writes");
    seed_message(&storage, &info.id, &anchor);

    let engine = Engine::persistent(
        Arc::new(FloodProvider {
            count: 1,
            produced: Arc::default(),
        }),
        "flood-model",
        Arc::new(ganja_core::tool::Registry::new(Vec::new())),
        ganja_core::permission::Permissions::default(),
        storage,
    );
    engine.resume(&info.id).await.expect("the session loads");

    let mut events = engine
        .subscribe()
        .await
        .expect("the first subscriber claims the birth queue");
    // The engine goes away first, so a first subscriber that was handed a
    // fresh queue instead of the birth one fails on an ended stream rather
    // than parking forever.
    drop(engine);

    let event = events
        .next()
        .await
        .expect("the resumed revert should be waiting in the birth queue");
    match event {
        Event::RevertChanged {
            session_id,
            revert: Some(revert),
            prompt: None,
        } => {
            assert_eq!(
                revert.message_id, anchor.id,
                "the notice names the message the revert anchored on"
            );
            assert_eq!(
                session_id, info.id,
                "the resumed revert is addressed to the resumed session"
            );
        }
        other => panic!("the first inherited event should be the resumed revert: {other:?}"),
    }
}

/// After a resume, a turn's events are addressed to the resumed session —
/// the engine's one current-session slot was replaced, and everything reads
/// it — not to the name the engine was born with.
#[tokio::test]
async fn after_a_resume_every_event_carries_the_resumed_sessions_id() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(directory.path().join("storage"));
    let seeded = seed_session(&storage, 0);
    seed_message(&storage, &seeded, &Message::user("what came before"));

    let engine = Engine::persistent(
        Arc::new(FloodProvider {
            count: 3,
            produced: Arc::default(),
        }),
        "flood-model",
        Arc::new(ganja_core::tool::Registry::new(Vec::new())),
        ganja_core::permission::Permissions::default(),
        storage,
    );
    let born_as = engine.session_id();
    engine.resume(&seeded).await.expect("the session loads");
    assert_ne!(
        born_as, seeded,
        "the pin is only a pin while the resumed id differs from the birth one"
    );

    let mut events = engine
        .subscribe()
        .await
        .expect("the first subscriber claims the birth queue");
    prompt(&engine, "next").await;
    let seen = tokio::time::timeout(DRAIN_PATIENCE, drain(&mut events))
        .await
        .expect("the resumed turn drains");

    assert!(!seen.is_empty(), "a turn reports something");
    for event in &seen {
        assert_eq!(
            event.session_id(),
            &seeded,
            "an event of the resumed conversation named another session: {event:?}"
        );
    }
}
