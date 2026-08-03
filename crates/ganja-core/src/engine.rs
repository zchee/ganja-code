//! The engine frontends drive: commands in, an ordered event stream out.
//!
//! Delivery is lossless. Events travel a bounded channel, so a producer that
//! outruns its consumer waits instead of dropping fragments; backpressure lands
//! on the turn task and never on the render loop. A single subscriber is
//! supported through P6, after which fanout gets per-subscriber queues.
//!
//! The engine owns the transcript. A turn appends the user's message, streams
//! the reply into an assistant message, and reports both through the event
//! stream, so a frontend that applies every event holds exactly what the next
//! [`ChatRequest`] will carry.

use std::{ops::ControlFlow, sync::Arc};

use futures::{StreamExt as _, stream::BoxStream};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::{
    protocol::{Command, Event, FinishReason, Message, Part, PartId},
    provider::{ChatRequest, Provider, ProviderEvent},
};

/// Events the engine queues before a producer has to wait for the subscriber.
pub const EVENT_CAPACITY: usize = 1024;

/// A command the engine refused.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A turn is streaming and P2 runs one turn at a time.
    #[error("a turn is already streaming; cancel it before sending another prompt")]
    Busy,
    /// [`Engine::subscribe`] was called more than once.
    #[error("the engine already has a subscriber")]
    AlreadySubscribed,
}

/// Owns the turn lifecycle and publishes what happens during it.
pub struct Engine {
    provider: Arc<dyn Provider>,
    model: String,
    events: mpsc::Sender<Event>,
    unclaimed: Mutex<Option<mpsc::Receiver<Event>>>,
    /// Holds the cancellation handle of the turn in flight, and doubles as the
    /// idle/busy flag.
    turn: Arc<Mutex<Option<CancellationToken>>>,
    /// The conversation so far. P4 persists it; until then it lives and dies
    /// with the process.
    history: Arc<Mutex<Vec<Message>>>,
}

impl Engine {
    /// Builds an engine that answers through `provider`, asking it for `model`.
    #[must_use]
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);

        Self {
            provider,
            model: model.into(),
            events,
            unclaimed: Mutex::new(Some(receiver)),
            turn: Arc::default(),
            history: Arc::default(),
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

        // The task is deliberately not joined. `cancel` is what stops a turn,
        // and it now reaches the provider itself, so an aborted HTTP stream is
        // the provider's business rather than something the engine has to kill
        // from outside. Aborting the task instead would skip the cleanup below
        // that releases the busy slot and guarantees a terminal event.
        tokio::spawn(run_turn(Turn {
            provider: Arc::clone(&self.provider),
            model: self.model.clone(),
            prompt,
            cancel,
            events: self.events.clone(),
            slot: Arc::clone(&self.turn),
            history: Arc::clone(&self.history),
        }));

        Ok(())
    }

    async fn cancel_turn(&self) {
        if let Some(cancel) = self.turn.lock().await.as_ref() {
            cancel.cancel();
        }
    }
}

/// Everything one turn needs, gathered so the spawned task takes one argument.
struct Turn {
    provider: Arc<dyn Provider>,
    model: String,
    prompt: String,
    cancel: CancellationToken,
    events: mpsc::Sender<Event>,
    slot: Arc<Mutex<Option<CancellationToken>>>,
    history: Arc<Mutex<Vec<Message>>>,
}

/// Why a turn ended, and what to say about it.
struct Outcome {
    reason: FinishReason,
    error: Option<String>,
}

async fn run_turn(turn: Turn) {
    let mut assistant = Message::assistant(turn.model.clone());
    let outcome = stream_turn(&turn, &mut assistant).await;
    let completed = assistant.complete();

    // A turn that died before its first fragment leaves nothing worth sending
    // back as context — and some providers reject an empty assistant message.
    if assistant.has_content() {
        turn.history.lock().await.push(assistant.clone());
    }

    // Released before the finish event is queued so that a prompt sent in
    // reaction to it is never rejected as busy.
    *turn.slot.lock().await = None;

    if let Some(outcome) = outcome {
        let _ = turn
            .events
            .send(Event::MessageFinished {
                message_id: assistant.id,
                reason: outcome.reason,
                usage: assistant.usage,
                error: outcome.error,
                completed,
            })
            .await;
    }
}

/// Runs one turn, accumulating the reply into `assistant` and returning why it
/// ended, or [`None`] once the subscriber is gone and there is nobody left to
/// tell.
async fn stream_turn(turn: &Turn, assistant: &mut Message) -> Option<Outcome> {
    let user = Message::user(turn.prompt.clone());
    let request = {
        let mut history = turn.history.lock().await;
        history.push(user.clone());

        ChatRequest {
            model: turn.model.clone(),
            system: None,
            messages: history.clone(),
        }
    };

    turn.events
        .send(Event::MessageStarted { message: user })
        .await
        .ok()?;
    turn.events
        .send(Event::MessageStarted {
            message: assistant.clone(),
        })
        .await
        .ok()?;

    let mut events = match turn.provider.stream(request, turn.cancel.clone()).await {
        Ok(events) => events,
        Err(error) => {
            return Some(Outcome {
                reason: FinishReason::Failed,
                error: Some(error.to_string()),
            });
        }
    };

    // The text part fragments accumulate into, once one is open.
    let mut open: Option<PartId> = None;

    loop {
        // Biased so that a cancel already in hand always wins the race against
        // a fragment that happens to be ready, which is what bounds how long a
        // cancelled turn can keep streaming.
        let event = tokio::select! {
            biased;
            () = turn.cancel.cancelled() => return Some(Outcome::cancelled()),
            event = events.next() => event,
        };

        let Some(event) = event else {
            // A stream that ends after a cancel ended because of it; one that
            // ends without a finish has said all it is going to.
            return Some(if turn.cancel.is_cancelled() {
                Outcome::cancelled()
            } else {
                Outcome::completed()
            });
        };

        match event {
            ProviderEvent::TextDelta(delta) => {
                let part_id = match &open {
                    Some(part_id) => part_id.clone(),
                    None => {
                        let part = Part::text(String::new());
                        let part_id = part.id.clone();
                        assistant.parts.push(part.clone());
                        open = Some(part_id.clone());

                        if let ControlFlow::Break(stop) = deliver(
                            turn,
                            Event::PartStarted {
                                message_id: assistant.id.clone(),
                                part,
                            },
                        )
                        .await
                        {
                            return stop;
                        }

                        part_id
                    }
                };

                // The open part is the newest one: nothing else appends parts
                // until P3's tools do.
                if let Some(text) = assistant.parts.last_mut().and_then(Part::as_text_mut) {
                    text.push_str(&delta);
                }

                if let ControlFlow::Break(stop) = deliver(
                    turn,
                    Event::PartDelta {
                        message_id: assistant.id.clone(),
                        part_id,
                        delta,
                    },
                )
                .await
                {
                    return stop;
                }
            }
            ProviderEvent::Usage(usage) => assistant.usage = Some(usage),
            // A provider that died mid-stream keeps whatever it already
            // streamed — the transcript is honest about how far it got — but
            // the turn is reported as failed, never as a model that stopped
            // talking on purpose.
            ProviderEvent::Failed(error) => {
                return Some(Outcome {
                    reason: FinishReason::Failed,
                    error: Some(error.to_string()),
                });
            }
            ProviderEvent::Finish(reason) => {
                return Some(Outcome {
                    reason,
                    error: None,
                });
            }
            // Reasoning and tool calls have no protocol part until P3 renders
            // and executes them. Dropping them keeps the transcript honest
            // instead of pasting raw arguments into the reply.
            event @ (ProviderEvent::ReasoningDelta(_)
            | ProviderEvent::ToolCallStart { .. }
            | ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::ToolCallEnd { .. }) => {
                tracing::debug!(?event, "provider event has no rendered part yet");
            }
        }
    }
}

/// Queues `event`, or breaks with the turn's report.
///
/// [`mpsc::Sender::send`] is cancel-safe: losing the race drops the event
/// without queueing it, which is what an abandoned turn wants. Waiting on a
/// full queue must not outlive a cancel, hence the race.
async fn deliver(turn: &Turn, event: Event) -> ControlFlow<Option<Outcome>> {
    tokio::select! {
        biased;
        () = turn.cancel.cancelled() => ControlFlow::Break(Some(Outcome::cancelled())),
        queued = turn.events.send(event) => match queued {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(None),
        },
    }
}

impl Outcome {
    fn completed() -> Self {
        Self {
            reason: FinishReason::Completed,
            error: None,
        }
    }

    fn cancelled() -> Self {
        Self {
            reason: FinishReason::Cancelled,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use futures::{
        StreamExt as _,
        stream::{self, BoxStream},
    };
    use tokio_util::sync::CancellationToken;

    use super::{Engine, EngineError};
    use crate::{
        protocol::{Command, Event, FinishReason, Message, Role, Usage},
        provider::{
            ChatRequest, FakeProvider, Provider, ProviderError, ProviderEvent, fake::MODEL,
        },
    };

    fn engine() -> Engine {
        Engine::new(
            Arc::new(FakeProvider::new(
                "one two",
                std::time::Duration::from_millis(1),
            )),
            MODEL,
        )
    }

    /// Records what it was asked and answers with a scripted stream.
    struct ScriptedProvider {
        events: Vec<ProviderEvent>,
        failure: Option<ProviderError>,
        seen: Arc<Mutex<Vec<ChatRequest>>>,
    }

    impl ScriptedProvider {
        fn new(events: Vec<ProviderEvent>) -> Self {
            Self {
                events,
                failure: None,
                seen: Arc::default(),
            }
        }

        fn failing(failure: ProviderError) -> Self {
            Self {
                events: Vec::new(),
                failure: Some(failure),
                seen: Arc::default(),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }

        async fn stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            self.seen
                .lock()
                .expect("the request log is never poisoned")
                .push(request);

            match &self.failure {
                Some(failure) => Err(failure.clone()),
                None => Ok(stream::iter(self.events.clone()).boxed()),
            }
        }
    }

    /// Drains events until the turn finishes, returning everything seen.
    async fn drain(events: &mut BoxStream<'static, Event>) -> Vec<Event> {
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

    /// The text a transcript rebuilt from `events` alone would show.
    fn replay(events: &[Event]) -> String {
        let mut messages: Vec<Message> = Vec::new();

        for event in events {
            match event {
                Event::MessageStarted { message } => messages.push(message.clone()),
                Event::PartStarted { message_id, part } => {
                    if let Some(message) = messages.iter_mut().find(|it| it.id == *message_id) {
                        message.parts.push(part.clone());
                    }
                }
                Event::PartDelta {
                    message_id,
                    part_id,
                    delta,
                } => {
                    if let Some(text) = messages
                        .iter_mut()
                        .find(|it| it.id == *message_id)
                        .and_then(|message| message.parts.iter_mut().find(|it| it.id == *part_id))
                        .and_then(crate::protocol::Part::as_text_mut)
                    {
                        text.push_str(delta);
                    }
                }
                Event::MessageFinished { .. } => {}
            }
        }

        messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(crate::protocol::Part::as_text)
            .collect()
    }

    #[tokio::test]
    async fn a_turn_reports_both_messages_and_streams_the_reply_into_one_part() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "hi".to_owned(),
            })
            .await
            .expect("an idle engine accepts a prompt");

        let seen = drain(&mut events).await;

        let Some(Event::MessageStarted { message: user }) = seen.first() else {
            panic!("a turn should open with the user's message, got {seen:?}");
        };
        assert_eq!(user.role, Role::User);
        assert_eq!(
            user.parts.first().and_then(|part| part.as_text()),
            Some("hi")
        );

        let Some(Event::MessageStarted { message: assistant }) = seen.get(1) else {
            panic!("the reply's envelope should follow, got {seen:?}");
        };
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.model.as_deref(), Some(MODEL));
        assert!(assistant.parts.is_empty(), "the reply starts empty");

        assert_eq!(
            seen.iter()
                .filter(|event| matches!(event, Event::PartStarted { .. }))
                .count(),
            1,
            "streamed text belongs to one part, got {seen:?}"
        );
        assert_eq!(replay(&seen), "hione two");

        let Some(Event::MessageFinished {
            message_id,
            reason,
            usage,
            error,
            completed,
        }) = seen.last()
        else {
            panic!("a turn always ends with a finish, got {seen:?}");
        };
        assert_eq!(*message_id, assistant.id);
        assert_eq!(*reason, FinishReason::Completed);
        assert_eq!(
            *usage,
            Some(Usage {
                input_tokens: 1,
                output_tokens: 2,
                ..Usage::default()
            })
        );
        assert!(error.is_none());
        assert!(*completed >= assistant.time.created);
    }

    #[tokio::test]
    async fn a_second_turn_carries_the_first_one_in_its_request() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            ProviderEvent::TextDelta("sure".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]));
        let seen = Arc::clone(&provider.seen);
        let engine = Engine::new(provider, "scripted-model");
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        for prompt in ["first", "second"] {
            engine
                .send(Command::SendPrompt {
                    text: prompt.to_owned(),
                })
                .await
                .expect("an idle engine accepts a prompt");
            drain(&mut events).await;
        }

        let requests = seen.lock().expect("the request log is never poisoned");
        let first = requests.first().expect("the first turn asked the provider");
        assert_eq!(first.model, "scripted-model");
        assert!(first.system.is_none());
        assert_eq!(first.messages.len(), 1, "the first turn has no history");

        let second = requests.get(1).expect("the second turn asked too");
        let transcript: Vec<(&str, Option<&str>)> = second
            .messages
            .iter()
            .map(|message| {
                (
                    message.model.as_deref().unwrap_or("user"),
                    message.parts.first().and_then(|part| part.as_text()),
                )
            })
            .collect();
        assert_eq!(
            transcript,
            vec![
                ("user", Some("first")),
                ("scripted-model", Some("sure")),
                ("user", Some("second")),
            ],
            "the second turn should carry the first one"
        );
    }

    #[tokio::test]
    async fn a_provider_that_cannot_answer_still_finishes_the_turn() {
        let engine = Engine::new(
            Arc::new(ScriptedProvider::failing(ProviderError::Auth(
                "ANTHROPIC_API_KEY is unset".to_owned(),
            ))),
            "scripted-model",
        );
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "hi".to_owned(),
            })
            .await
            .expect("an idle engine accepts a prompt");

        let seen = drain(&mut events).await;
        let Some(Event::MessageFinished { reason, error, .. }) = seen.last() else {
            panic!("a failed turn still finishes, got {seen:?}");
        };

        assert_eq!(*reason, FinishReason::Failed);
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("ANTHROPIC_API_KEY")),
            "the refusal should explain itself, got {error:?}"
        );

        engine
            .send(Command::SendPrompt {
                text: "again".to_owned(),
            })
            .await
            .expect("a failed turn leaves the engine idle");
    }

    #[tokio::test]
    async fn a_failed_turn_is_not_kept_as_context() {
        let provider = Arc::new(ScriptedProvider::failing(ProviderError::Transport(
            "connection reset".to_owned(),
        )));
        let seen = Arc::clone(&provider.seen);
        let engine = Engine::new(provider, "scripted-model");
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        for prompt in ["first", "second"] {
            engine
                .send(Command::SendPrompt {
                    text: prompt.to_owned(),
                })
                .await
                .expect("an idle engine accepts a prompt");
            drain(&mut events).await;
        }

        let requests = seen.lock().expect("the request log is never poisoned");
        let second = requests.get(1).expect("the second turn asked too");
        assert_eq!(
            second.messages.len(),
            2,
            "an empty reply should not enter the history, got {:?}",
            second.messages
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
        assert!(matches!(
            events.next().await,
            Some(Event::MessageStarted { .. })
        ));

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

        drain(&mut events).await;

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
