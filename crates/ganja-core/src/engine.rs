//! The engine frontends drive: commands in, an ordered event stream out.
//!
//! Delivery is lossless. Events travel a bounded channel, so a producer that
//! outruns its consumer waits instead of dropping fragments; backpressure lands
//! on the turn task and never on the render loop. A single subscriber is
//! supported through P6, after which fanout gets per-subscriber queues.
//!
//! The engine owns the transcript. A turn appends the user's message, runs the
//! agent loop in [`crate::session`] — streaming the reply, executing the tool
//! calls it asks for, asking again until a request ends without any — and
//! reports every part of it through the event stream, so a frontend that
//! applies every event holds exactly what the next
//! [`ChatRequest`](crate::provider::ChatRequest) will carry.
//!
//! A **persistent** engine ([`Engine::persistent`]) additionally writes every
//! turn through to a [`Storage`] as it streams, and exposes the session
//! operations — [`Engine::sessions`], [`Engine::resume`],
//! [`Engine::current_session`] — as plain request/response methods, the
//! in-process analog of upstream's REST routes. They are deliberately not bus
//! events: the wire protocol is pinned, and P7 owns the transport. An engine
//! built with [`Engine::new`] has none of this — no store, no auto-title, no
//! compaction — which is what keeps golden, scripted and PTY runs
//! deterministic.

use std::{path::PathBuf, sync::Arc};

use futures::{StreamExt as _, stream::BoxStream};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::{
    permission::Permissions,
    protocol::{Command, Event, Message, PartBody, Role, ToolState, Usage, now},
    provider::Provider,
    session::{LiveSession, Persist, SessionState, Turn, TurnHandle, run_turn},
    storage::{self, SessionId, SessionInfo, Storage, StorageError},
    tool::{FileTimes, Registry},
};

/// Events the engine queues before a producer has to wait for the subscriber.
pub const EVENT_CAPACITY: usize = 1024;

/// A command the engine refused.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A turn is streaming — or waiting on a permission — and the engine runs
    /// one turn at a time. Session switches wait for the same reason: the
    /// turn in flight is writing into the session it started on.
    #[error("a turn is already streaming; cancel it before sending another prompt")]
    Busy,
    /// [`Engine::subscribe`] was called more than once.
    #[error("the engine already has a subscriber")]
    AlreadySubscribed,
    /// A session operation reached an engine built with [`Engine::new`],
    /// which keeps no sessions: its transcript lives and dies with the
    /// process.
    #[error("this engine keeps no sessions; it was built without storage")]
    Ephemeral,
    /// [`Engine::resume`] named a session the store does not hold — never
    /// created, or quarantined as corrupt.
    #[error("no stored session named {}", id.as_str())]
    SessionNotFound {
        /// The id nothing answers to.
        id: SessionId,
    },
    /// The storage layer refused to act. Reads never fail on content — a
    /// corrupt file is quarantined and skipped — so this is the filesystem
    /// itself refusing.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Owns the turn lifecycle and publishes what happens during it.
pub struct Engine {
    provider: Arc<dyn Provider>,
    model: String,
    /// What the model is told before it is told anything else, composed once
    /// by [`crate::instruction::system_prompt`]. [`None`] is an engine nobody
    /// configured, which every scripted and golden run relies on.
    system: Option<String>,
    /// Tools the model is offered, and the agent loop executes.
    tools: Arc<Registry>,
    /// Rules deciding which tool calls wait for the user.
    permissions: Arc<std::sync::Mutex<Permissions>>,
    /// Directory tool calls resolve relative paths against, captured once so
    /// every call in a session agrees on where it is.
    cwd: PathBuf,
    /// Which files this session has read, shared by every tool call in it.
    files: Arc<FileTimes>,
    events: mpsc::Sender<Event>,
    unclaimed: Mutex<Option<mpsc::Receiver<Event>>>,
    /// Holds the handle of the turn in flight, and doubles as the idle/busy
    /// flag. The handle carries the turn's cancellation token and the
    /// permission wait a [`Command::ReplyPermission`] routes into.
    turn: Arc<Mutex<Option<TurnHandle>>>,
    /// The conversation the next request carries. On a persistent engine this
    /// is the live window — everything from the compaction summary onward —
    /// rather than the whole stored transcript.
    history: Arc<Mutex<Vec<Message>>>,
    /// The store and the live session, when this engine persists. [`None`]
    /// is [`Engine::new`]'s in-memory engine, and with it every P4 behaviour
    /// is absent: no write-through, no auto-title, no compaction.
    persistence: Option<Arc<SessionState>>,
}

impl Engine {
    /// Builds an engine that answers through `provider`, asking it for
    /// `model`, executing calls to `tools` under `permissions`.
    ///
    /// The engine is in-memory: the transcript lives and dies with the
    /// process, and session operations answer [`EngineError::Ephemeral`].
    /// Tests and demos rely on that absence — nothing here ever touches a
    /// disk or spends a provider request on bookkeeping.
    #[must_use]
    pub fn new(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        tools: Arc<Registry>,
        permissions: Permissions,
    ) -> Self {
        Self::assemble(provider, model.into(), tools, permissions, None)
    }

    /// Builds an engine whose sessions live in `storage`.
    ///
    /// The first prompt creates a session (or [`Engine::resume`] installs a
    /// stored one), every turn writes itself through as it streams, a
    /// completed first turn earns the session a title, and a session whose
    /// last request filled 90% of its model's context window is compacted
    /// before the next turn.
    #[must_use]
    pub fn persistent(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        tools: Arc<Registry>,
        permissions: Permissions,
        storage: Storage,
    ) -> Self {
        Self::assemble(
            provider,
            model.into(),
            tools,
            permissions,
            Some(Arc::new(SessionState {
                storage,
                live: std::sync::Mutex::new(LiveSession::default()),
            })),
        )
    }

    fn assemble(
        provider: Arc<dyn Provider>,
        model: String,
        tools: Arc<Registry>,
        permissions: Permissions,
        persistence: Option<Arc<SessionState>>,
    ) -> Self {
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);

        Self {
            provider,
            model,
            system: None,
            tools,
            permissions: Arc::new(std::sync::Mutex::new(permissions)),
            // Captured at construction so a process whose directory later
            // moves keeps resolving paths where the session began. A process
            // with no readable directory falls back to relative resolution.
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            files: Arc::new(FileTimes::default()),
            events,
            unclaimed: Mutex::new(Some(receiver)),
            turn: Arc::default(),
            history: Arc::default(),
            persistence,
        }
    }

    /// Sets what the model is told before it is told anything else.
    ///
    /// Consuming rather than a setter, so it composes with either constructor
    /// and cannot be called on an engine that is already streaming a turn. The
    /// prompt is captured once and carried by every request a turn makes —
    /// including the one that summarizes a conversation for compaction, which
    /// is what stops a compacted session from losing the instructions the rest
    /// of it was written under.
    ///
    /// [`None`] leaves the requests without one, which is what
    /// [`Engine::new`]'s scripted and golden runs depend on.
    #[must_use]
    pub fn with_system(mut self, system: Option<String>) -> Self {
        self.system = system;

        self
    }

    /// The permission rules this engine consults, shared with the agent loop
    /// and with whatever persists an "always" answer.
    #[must_use]
    pub fn permissions(&self) -> Arc<std::sync::Mutex<Permissions>> {
        Arc::clone(&self.permissions)
    }

    /// Every stored session, newest first — what a session picker lists.
    ///
    /// The listing runs on the blocking pool because it walks the store: a
    /// caller inside a render loop stays responsive however many sessions
    /// have accumulated.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Ephemeral`] on an engine built without storage,
    /// and [`EngineError::Storage`] when the filesystem refuses the listing.
    pub async fn sessions(&self) -> Result<Vec<SessionInfo>, EngineError> {
        let Some(state) = &self.persistence else {
            return Err(EngineError::Ephemeral);
        };

        let storage = state.storage.clone();
        let sessions = tokio::task::spawn_blocking(move || storage.list_sessions())
            .await
            .expect("the session listing neither panics nor is aborted")?;

        Ok(sessions)
    }

    /// The session the engine is writing into, or [`None`] before the first
    /// prompt or resume — and always [`None`] on an in-memory engine.
    #[must_use]
    pub fn current_session(&self) -> Option<SessionInfo> {
        self.persistence
            .as_ref()?
            .live
            .lock()
            .expect("the live session is never poisoned")
            .info
            .clone()
    }

    /// Installs the stored session `id` as the engine's current one and
    /// returns its **full transcript**, oldest first, for a frontend to seed
    /// its view from.
    ///
    /// The engine's own request history becomes the live window: messages
    /// from [`SessionInfo::summary`] onward, all of them when no compaction
    /// has happened. Assistant messages that carry no content are left out of
    /// the window — some providers reject an empty message — but stay in the
    /// returned transcript, `time.completed` still absent, which is how a
    /// frontend shows them as aborted.
    ///
    /// A tool call the previous process never finished is closed here as
    /// [`ToolState::Error`], in the returned transcript, in the installed
    /// window, and on disk: the next request must answer every call the
    /// model opened.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Busy`] while a turn is in flight — the turn is
    /// writing into the session it started on — [`EngineError::Ephemeral`]
    /// on an engine built without storage, [`EngineError::SessionNotFound`]
    /// for an id the store does not hold, and [`EngineError::Storage`] when
    /// the filesystem refuses a read.
    pub async fn resume(&self, id: &SessionId) -> Result<Vec<Message>, EngineError> {
        let Some(state) = &self.persistence else {
            return Err(EngineError::Ephemeral);
        };

        // The slot guard is held across the whole install, so a prompt that
        // arrives mid-resume waits and then lands on the freshly installed
        // session instead of racing the old one. No turn exists while it is
        // held, so nothing on the event path can contend for it.
        let slot = self.turn.lock().await;
        if slot.is_some() {
            return Err(EngineError::Busy);
        }

        let storage = state.storage.clone();
        let wanted = id.clone();
        let (info, transcript) = tokio::task::spawn_blocking(move || {
            let Some(info) = storage.load_info(&wanted)? else {
                return Err(EngineError::SessionNotFound { id: wanted });
            };
            let mut transcript = storage.load_transcript(&wanted)?;
            close_interrupted(&storage, &wanted, &mut transcript);

            Ok((info, transcript))
        })
        .await
        .expect("the session load neither panics nor is aborted")?;

        let start = match &info.summary {
            None => 0,
            Some(summary) => match transcript.iter().position(|m| m.id == *summary) {
                Some(index) => index,
                None => {
                    // The summary message was quarantined or lost; the whole
                    // transcript is the only honest window left.
                    tracing::warn!(
                        session = info.id.as_str(),
                        "the compaction summary is missing from the transcript; \
                         resuming with the full history"
                    );
                    0
                }
            },
        };
        let window: Vec<Message> = transcript[start..]
            .iter()
            .filter(|message| message.role == Role::User || message.has_content())
            .cloned()
            .collect();

        *self.history.lock().await = window;
        {
            let mut live = state
                .live
                .lock()
                .expect("the live session is never poisoned");
            live.info = Some(info);
            live.warned_uncataloged = false;
        }
        drop(slot);

        Ok(transcript)
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
    /// is still streaming, or still waiting on a permission.
    pub async fn send(&self, command: Command) -> Result<(), EngineError> {
        match command {
            Command::SendPrompt { text } => self.start_turn(text).await,
            Command::CancelTurn => {
                self.cancel_turn().await;
                Ok(())
            }
            Command::ReplyPermission { id, reply } => {
                self.reply_permission(&id, reply).await;
                Ok(())
            }
        }
    }

    async fn start_turn(&self, prompt: String) -> Result<(), EngineError> {
        let mut turn = self.turn.lock().await;
        if turn.is_some() {
            return Err(EngineError::Busy);
        }

        // The first prompt on a persistent engine mints the session, and its
        // record reaches the disk before the first byte streams: a crash
        // mid-turn must still leave something to resume. A store that
        // refuses is a warning, not a dead prompt.
        let persist = self.persistence.as_ref().map(|state| {
            let session = {
                let mut live = state
                    .live
                    .lock()
                    .expect("the live session is never poisoned");
                if live.info.is_none() {
                    live.warned_uncataloged = false;
                }
                live.info
                    .get_or_insert_with(|| fresh_session(&state.storage))
                    .id
                    .clone()
            };

            Persist::new(Arc::clone(state), session)
        });

        let cancel = CancellationToken::new();
        let pending = Arc::new(std::sync::Mutex::new(None));
        *turn = Some(TurnHandle {
            cancel: cancel.clone(),
            permission: Arc::clone(&pending),
        });
        drop(turn);

        // The task is deliberately not joined. `cancel` is what stops a turn,
        // and it reaches the provider and every running tool, so an aborted
        // HTTP stream is the provider's business rather than something the
        // engine has to kill from outside. Aborting the task instead would
        // skip the cleanup that releases the busy slot and guarantees a
        // terminal event.
        tokio::spawn(run_turn(Turn {
            provider: Arc::clone(&self.provider),
            model: self.model.clone(),
            system: self.system.clone(),
            tools: Arc::clone(&self.tools),
            permissions: Arc::clone(&self.permissions),
            cwd: self.cwd.clone(),
            files: Arc::clone(&self.files),
            prompt,
            cancel,
            pending,
            events: self.events.clone(),
            slot: Arc::clone(&self.turn),
            history: Arc::clone(&self.history),
            persist,
        }));

        Ok(())
    }

    async fn cancel_turn(&self) {
        if let Some(turn) = self.turn.lock().await.as_ref() {
            turn.cancel.cancel();
        }
    }

    /// Routes a reply to the permission wait that asked for it.
    ///
    /// A reply nothing is waiting for — the id is stale, the turn already
    /// ended, or a cancel raced it — is defined to be ignored: the turn task
    /// owns answering every request exactly once, so there is nothing here to
    /// repair.
    async fn reply_permission(
        &self,
        id: &crate::protocol::PermissionId,
        reply: crate::protocol::PermissionReply,
    ) {
        let delivered = self.turn.lock().await.as_ref().is_some_and(|turn| {
            let mut pending = turn
                .permission
                .lock()
                .expect("the pending permission is never poisoned");

            match pending.take_if(|waiting| waiting.id == *id) {
                // A closed receiver means the turn is already tearing down,
                // which is the same race as replying after the turn ended.
                Some(waiting) => waiting.sender.send(reply).is_ok(),
                None => false,
            }
        });

        if !delivered {
            tracing::debug!(id = id.as_str(), "no permission is waiting for this reply");
        }
    }
}

/// What a tool part that was still open when its process died says on resume.
const INTERRUPTED: &str = "the session was interrupted before this call finished";

/// A brand-new session record, already on disk by the time it is adopted.
fn fresh_session(storage: &Storage) -> SessionInfo {
    let created = now();
    let info = SessionInfo {
        id: SessionId::ascending(),
        version: storage::VERSION,
        title: None,
        created,
        updated: created,
        usage: Usage::default(),
        context_tokens: 0,
        summary: None,
    };

    if let Err(error) = storage.save_info(&info) {
        tracing::warn!(
            session = info.id.as_str(),
            %error,
            "could not create the session on disk; the conversation continues in memory"
        );
    }

    info
}

/// Closes every tool part a crash left `Pending` or `Running`, in the loaded
/// transcript and on disk. The stored input is kept — it is what the call was
/// going to run with — but both timestamps are the load's, per the P4
/// contract: nothing here pretends to know when the old process died.
///
/// An assistant envelope whose `time.completed` is absent is left exactly as
/// found: that absence is the abort marker a frontend renders, and inventing
/// parts for it would put words in a dead process's mouth.
fn close_interrupted(storage: &Storage, session: &SessionId, transcript: &mut [Message]) {
    for message in transcript.iter_mut() {
        let message_id = message.id.clone();
        for part in &mut message.parts {
            let PartBody::Tool { state, .. } = &mut part.body else {
                continue;
            };
            let input = match state {
                ToolState::Completed { .. } | ToolState::Error { .. } => continue,
                ToolState::Running { input, .. } => input.clone(),
                ToolState::Pending => serde_json::json!({}),
            };

            let stamp = now();
            *state = ToolState::Error {
                input,
                error: INTERRUPTED.to_owned(),
                started: stamp,
                completed: stamp,
            };

            // The closure must outlive this process too — the next request,
            // whenever it happens, has to answer this call. A store that
            // refuses re-closes on the next resume.
            if let Err(error) = storage.save_part(session, &message_id, part) {
                tracing::warn!(
                    session = session.as_str(),
                    part = part.id.as_str(),
                    %error,
                    "could not persist an interrupted call's closure"
                );
            }
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
        permission::Permissions,
        protocol::{Command, Event, FinishReason, Message, Role, Usage},
        provider::{
            ChatRequest, FakeProvider, Provider, ProviderError, ProviderEvent, fake::MODEL,
        },
        storage::{self, SessionId, SessionInfo, Storage},
        tool::Registry,
    };

    /// An engine over `provider` with no tools and default rules, which is
    /// all these tests need: they prove the turn lifecycle, not the loop.
    fn bare(provider: Arc<dyn Provider>, model: &str) -> Engine {
        Engine::new(
            provider,
            model,
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        )
    }

    fn engine() -> Engine {
        bare(
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
                Event::MessageFinished { .. }
                | Event::PartUpdated { .. }
                | Event::PermissionRequested { .. }
                | Event::PermissionReplied { .. } => {}
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
                .filter(|event| matches!(
                    event,
                    Event::PartStarted { part, .. } if part.as_text().is_some()
                ))
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
        let engine = bare(provider, "scripted-model");
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
        assert!(
            first.system.is_none(),
            "an engine nobody configured asks without a system prompt"
        );
        assert_eq!(first.messages.len(), 1, "the first turn has no history");

        let second = requests.get(1).expect("the second turn asked too");
        let transcript: Vec<(&str, Option<&str>)> = second
            .messages
            .iter()
            .map(|message| {
                (
                    message.model.as_deref().unwrap_or("user"),
                    // The first text part: an assistant message now opens
                    // with a step marker before anything it said.
                    message
                        .parts
                        .iter()
                        .find_map(crate::protocol::Part::as_text),
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
        let engine = bare(
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
        let engine = bare(provider, "scripted-model");
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

    /// Every request a turn makes carries the configured prompt — including
    /// the one that summarizes the conversation for compaction, which is what
    /// keeps a compacted session from being summarized under instructions the
    /// rest of it was never held under.
    #[tokio::test]
    async fn a_configured_system_prompt_reaches_the_agent_and_the_summarize_requests() {
        const SYSTEM: &str = "you are a canary";

        let provider = Arc::new(ScriptedProvider::new(vec![
            ProviderEvent::TextDelta("sure".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]));
        let seen = Arc::clone(&provider.seen);

        // A model the catalog knows, and a session already at its ceiling, so
        // the next turn compacts before it asks anything.
        let model = crate::catalog::default_model("anthropic")
            .expect("the catalog has a default for a provider this build ships");
        let window = crate::catalog::model(model)
            .expect("the default model is in the catalog")
            .context_window;

        let directory = tempfile::tempdir().expect("a temporary directory");
        let storage = Storage::open(directory.path().join("storage"));
        let session = SessionId::ascending();
        let info = SessionInfo {
            id: session.clone(),
            version: storage::VERSION,
            // Pre-titled, so the title machinery stays out of a test that is
            // not about it and cannot spend a request of its own.
            title: Some("seeded".to_owned()),
            created: 1,
            updated: 2,
            usage: Usage::default(),
            context_tokens: window,
            summary: None,
        };
        storage.save_info(&info).expect("the seeded record writes");
        let earlier = Message::user("the objective");
        storage
            .save_message(&session, &earlier)
            .expect("the seeded envelope writes");
        for part in &earlier.parts {
            storage
                .save_part(&session, &earlier.id, part)
                .expect("the seeded part writes");
        }

        let engine = Engine::persistent(
            provider,
            model,
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
            storage,
        )
        .with_system(Some(SYSTEM.to_owned()));
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        engine.resume(&session).await.expect("the session loads");

        engine
            .send(Command::SendPrompt {
                text: "next".to_owned(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;

        let requests = seen.lock().expect("the request log is never poisoned");
        assert_eq!(
            requests.len(),
            2,
            "a compacting turn asks twice: summarize, then the model itself"
        );
        assert!(
            requests[0].tools.is_empty(),
            "the summarize request is the toolless one, got {:?}",
            requests[0]
        );
        for request in requests.iter() {
            assert_eq!(request.system.as_deref(), Some(SYSTEM));
        }
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
