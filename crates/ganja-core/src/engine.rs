//! The engine frontends drive: commands in, an ordered event stream out.
//!
//! Delivery is lossless. Events travel a bounded channel, so a producer that
//! outruns its consumer waits instead of dropping fragments; backpressure lands
//! on the turn task and never on the render loop. A single subscriber is
//! supported through P6, after which fanout gets per-subscriber queues.

use std::sync::Arc;

use futures::{StreamExt as _, stream::BoxStream};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::{
    protocol::{Command, Event, FinishReason},
    provider::Provider,
};

/// Events the engine queues before a producer has to wait for the subscriber.
pub const EVENT_CAPACITY: usize = 1024;

/// A command the engine refused.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A turn is streaming and P1 runs one turn at a time.
    #[error("a turn is already streaming; cancel it before sending another prompt")]
    Busy,
    /// [`Engine::subscribe`] was called more than once.
    #[error("the engine already has a subscriber")]
    AlreadySubscribed,
}

/// Owns the turn lifecycle and publishes what happens during it.
pub struct Engine {
    provider: Arc<dyn Provider>,
    events: mpsc::Sender<Event>,
    unclaimed: Mutex<Option<mpsc::Receiver<Event>>>,
    /// Holds the cancellation handle of the turn in flight, and doubles as the
    /// idle/busy flag.
    turn: Arc<Mutex<Option<CancellationToken>>>,
}

impl Engine {
    /// Builds an engine that answers through `provider`.
    #[must_use]
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);

        Self {
            provider,
            events,
            unclaimed: Mutex::new(Some(receiver)),
            turn: Arc::default(),
        }
    }

    /// Claims the event stream.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::AlreadySubscribed`] on every call after the
    /// first: splitting one lossless queue between two readers would hand each
    /// of them an arbitrary half of the transcript.
    pub async fn subscribe(&self) -> Result<BoxStream<'static, Event>, EngineError> {
        let receiver = self
            .unclaimed
            .lock()
            .await
            .take()
            .ok_or(EngineError::AlreadySubscribed)?;

        Ok(ReceiverStream::new(receiver).boxed())
    }

    /// Applies `command`.
    ///
    /// The call returns as soon as the command is accepted — a turn's work
    /// happens in a spawned task and is reported through the event stream — so
    /// a caller may await this from inside a render loop.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Busy`] when a prompt arrives while another turn
    /// is still streaming.
    pub async fn send(&self, command: Command) -> Result<(), EngineError> {
        match command {
            Command::SendPrompt { text } => self.start_turn(text).await,
            Command::CancelTurn => {
                self.cancel_turn().await;
                Ok(())
            }
        }
    }

    async fn start_turn(&self, prompt: String) -> Result<(), EngineError> {
        let mut turn = self.turn.lock().await;
        if turn.is_some() {
            return Err(EngineError::Busy);
        }

        let cancel = CancellationToken::new();
        *turn = Some(cancel.clone());
        drop(turn);

        tokio::spawn(run_turn(
            Arc::clone(&self.provider),
            prompt,
            cancel,
            self.events.clone(),
            Arc::clone(&self.turn),
        ));

        Ok(())
    }

    async fn cancel_turn(&self) {
        if let Some(cancel) = self.turn.lock().await.as_ref() {
            cancel.cancel();
        }
    }
}

async fn run_turn(
    provider: Arc<dyn Provider>,
    prompt: String,
    cancel: CancellationToken,
    events: mpsc::Sender<Event>,
    turn: Arc<Mutex<Option<CancellationToken>>>,
) {
    let outcome = stream_turn(provider.as_ref(), &prompt, &cancel, &events).await;

    // Released before the finish event is queued so that a prompt sent in
    // reaction to `TurnFinished` is never rejected as busy.
    *turn.lock().await = None;

    if let Some(reason) = outcome {
        let _ = events.send(Event::TurnFinished { reason }).await;
    }
}

/// Runs one turn, returning why it ended, or [`None`] once the subscriber is
/// gone and there is nobody left to tell.
async fn stream_turn(
    provider: &dyn Provider,
    prompt: &str,
    cancel: &CancellationToken,
    events: &mpsc::Sender<Event>,
) -> Option<FinishReason> {
    events.send(Event::TurnStarted).await.ok()?;

    let mut fragments = provider.stream(prompt);

    loop {
        let fragment = tokio::select! {
            () = cancel.cancelled() => return Some(FinishReason::Cancelled),
            fragment = fragments.next() => fragment,
        };

        let Some(text) = fragment else {
            return Some(FinishReason::Completed);
        };

        // `Sender::send` is cancel-safe: losing this race drops the fragment
        // without queueing it, which is what an abandoned turn wants. Waiting
        // on a full queue must not outlive a cancel, hence the select.
        tokio::select! {
            () = cancel.cancelled() => return Some(FinishReason::Cancelled),
            queued = events.send(Event::TextDelta { text }) => queued.ok()?,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt as _;

    use super::{Engine, EngineError};
    use crate::{
        protocol::{Command, Event, FinishReason},
        provider::FakeProvider,
    };

    fn engine() -> Engine {
        Engine::new(Arc::new(FakeProvider::new(
            "one two",
            std::time::Duration::from_millis(1),
        )))
    }

    #[tokio::test]
    async fn a_turn_reports_start_text_and_completion_in_order() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "hi".to_owned(),
            })
            .await
            .expect("an idle engine accepts a prompt");

        assert_eq!(events.next().await, Some(Event::TurnStarted));
        assert_eq!(
            events.next().await,
            Some(Event::TextDelta {
                text: "one ".to_owned()
            })
        );
        assert_eq!(
            events.next().await,
            Some(Event::TextDelta {
                text: "two".to_owned()
            })
        );
        assert_eq!(
            events.next().await,
            Some(Event::TurnFinished {
                reason: FinishReason::Completed
            })
        );
    }

    #[tokio::test]
    async fn a_second_subscriber_is_refused() {
        let engine = engine();
        let _first = engine.subscribe().await.expect("the first subscriber wins");

        assert!(matches!(
            engine.subscribe().await,
            Err(EngineError::AlreadySubscribed)
        ));
    }

    #[tokio::test]
    async fn a_prompt_sent_mid_turn_is_refused() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "first".to_owned(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        assert_eq!(events.next().await, Some(Event::TurnStarted));

        assert!(matches!(
            engine
                .send(Command::SendPrompt {
                    text: "second".to_owned()
                })
                .await,
            Err(EngineError::Busy)
        ));
    }

    #[tokio::test]
    async fn the_engine_accepts_a_prompt_again_once_the_turn_finished() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "first".to_owned(),
            })
            .await
            .expect("an idle engine accepts a prompt");

        while !matches!(events.next().await, Some(Event::TurnFinished { .. })) {}

        engine
            .send(Command::SendPrompt {
                text: "second".to_owned(),
            })
            .await
            .expect("a finished turn leaves the engine idle");
    }

    #[tokio::test]
    async fn cancelling_while_idle_does_nothing() {
        let engine = engine();
        let _events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::CancelTurn)
            .await
            .expect("an idle cancel is a no-op");
    }
}
