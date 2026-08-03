//! The event loop and the state it owns.
//!
//! One [`tokio::select!`] owns every mutable piece of UI state, so nothing is
//! shared with the engine but channels. No arm awaits work of unbounded
//! duration: a prompt is handed to the engine, which answers through the event
//! stream, and the loop goes straight back to drawing.
//!
//! Frames are coalesced. A burst of fragments redraws at most once per
//! [`FRAME`], while a keystroke always redraws immediately — the two rules that
//! keep streaming cheap without making typing feel laggy.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use ganja_core::{
    Command, Engine, Event as CoreEvent, FinishReason, Message, PartBody, PermissionReply, Role,
    ToolState, Usage, catalog,
};
use ratatui::{
    DefaultTerminal, Terminal,
    backend::Backend,
    crossterm::event::{
        Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    layout::{Constraint, Layout},
};

use crate::{
    component::{
        chat::{Chat, WHEEL_LINES},
        editor::{self, Editor},
        permission::Permission,
        sessions::{self, Sessions},
        status::{Activity, Status, Totals},
    },
    event::AppEvent,
    theme::Theme,
};

/// Shortest gap between frames: roughly 60 FPS.
pub const FRAME: Duration = Duration::from_millis(16);

/// Modifiers that turn Enter into a line break. Terminals disagree about which
/// of these they can report, so all of them mean the same thing.
const NEWLINE_MODIFIERS: KeyModifiers = KeyModifiers::SHIFT
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::CONTROL);

/// The reply a key sends while the permission dialog is open, or [`None`]
/// for a key the dialog swallows without acting on it. Pulled out of
/// [`App::handle_key`] so the mapping can be asserted on its own.
fn permission_reply(code: KeyCode) -> Option<PermissionReply> {
    match code {
        KeyCode::Char('y') => Some(PermissionReply::Once),
        KeyCode::Char('a') => Some(PermissionReply::Always),
        KeyCode::Char('n') | KeyCode::Esc => Some(PermissionReply::Reject),
        _ => None,
    }
}

/// The whole terminal application.
pub struct App {
    engine: Engine,
    /// Model the engine asks for, kept here because pricing a turn needs it and
    /// the engine's copy is not the frontend's business.
    model: String,
    chat: Chat,
    editor: Editor,
    status: Status,
    /// The tool call currently waiting on the user's decision, if any.
    permission: Option<Permission>,
    /// The stored sessions the user is choosing between, while the picker is
    /// open.
    sessions: Option<Sessions>,
    theme: Theme,
    /// What the session has spent, accumulated across turns.
    totals: Totals,
    /// State changed since the last frame.
    dirty: bool,
    /// The change came from the keyboard, which skips the coalescing gate.
    urgent: bool,
    last_draw: Instant,
    quit: bool,
}

impl App {
    /// Builds an app driven by `engine`, which is asking `model`, showing
    /// `notice` in the status bar.
    #[must_use]
    pub fn new(engine: Engine, model: impl Into<String>, notice: Option<String>) -> Self {
        let theme = Theme::default();

        Self {
            engine,
            model: model.into(),
            chat: Chat::default(),
            editor: Editor::new(&theme),
            status: Status::new(notice),
            permission: None,
            sessions: None,
            theme,
            totals: Totals::default(),
            dirty: true,
            urgent: true,
            last_draw: Instant::now(),
            quit: false,
        }
    }

    /// Fills the transcript from a resumed session's stored `transcript`.
    ///
    /// The messages take the same route into the viewport that a live
    /// `MessageStarted` takes, so a resumed session and a streamed one are the
    /// same entries built the same way. Nothing is invented on the way in: the
    /// engine hands over what it read back, including replies it never saw
    /// finish, which render as interrupted rather than as complete.
    ///
    /// The spend counters are deliberately left alone. What a stored session
    /// cost cannot be recomputed here — the store records the tokens but not
    /// which model spent them — so the status bar keeps meaning what it has
    /// always meant: what this run has spent. The picker is where a session's
    /// accumulated size is shown.
    pub fn seed(&mut self, transcript: Vec<Message>) {
        for message in transcript {
            self.chat.restore_message(message);
        }
    }

    /// Runs until the user quits or the terminal goes away.
    ///
    /// # Errors
    ///
    /// Returns an error if the engine refuses a subscription, or if the
    /// terminal cannot be read from or drawn to.
    pub async fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut core_events = self
            .engine
            .subscribe()
            .await
            .context("failed to subscribe to engine events")?;
        let mut term_events = EventStream::new();

        loop {
            if self.needs_draw() {
                self.draw(terminal)?;
            }

            let event = tokio::select! {
                incoming = term_events.next() => match incoming {
                    Some(incoming) => AppEvent::Term(
                        incoming.context("failed to read a terminal event")?,
                    ),
                    // The event source closed; there is nothing left to react to.
                    None => break,
                },
                incoming = core_events.next() => match incoming {
                    Some(incoming) => AppEvent::core(incoming),
                    None => break,
                },
                () = tokio::time::sleep(self.until_next_frame()), if self.wants_wakeup() => {
                    AppEvent::Tick
                }
                // Raw mode swallows Ctrl-C, so this arm only fires for a signal
                // raised from outside the terminal, such as `kill -INT`.
                _ = tokio::signal::ctrl_c() => break,
            };

            self.handle(event).await?;

            if self.quit {
                break;
            }
        }

        Ok(())
    }

    /// Applies one event to the UI state.
    ///
    /// # Errors
    ///
    /// Returns an error only if the engine fails for a reason the status bar
    /// cannot explain; refused commands are shown, not propagated.
    pub async fn handle(&mut self, event: AppEvent) -> Result<()> {
        match event {
            AppEvent::Term(event) => {
                self.handle_terminal(event).await?;
                self.dirty = true;
                self.urgent = true;
            }
            AppEvent::Core(event) => {
                self.handle_core(*event);
                self.dirty = true;
            }
            AppEvent::Tick => {}
        }

        Ok(())
    }

    /// Renders one frame.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot be written to.
    pub fn draw<B>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        terminal
            .draw(|frame| {
                let [transcript, prompt, status] = Layout::vertical([
                    Constraint::Min(1),
                    Constraint::Length(editor::HEIGHT),
                    Constraint::Length(1),
                ])
                .areas(frame.area());

                let buffer = frame.buffer_mut();
                self.chat.render(transcript, buffer, &self.theme);
                // The permission dialog draws last so that it is on top: it is
                // the one modal a turn is blocked on.
                if let Some(sessions) = &self.sessions {
                    sessions.render(transcript, buffer, &self.theme);
                }
                if let Some(permission) = &self.permission {
                    permission.render(transcript, buffer, &self.theme);
                }
                self.editor.render(prompt, buffer);
                self.status.render(status, buffer, &self.theme);
            })
            .context("failed to draw a frame")?;

        self.dirty = false;
        self.urgent = false;
        self.last_draw = Instant::now();

        Ok(())
    }

    async fn handle_terminal(&mut self, event: TermEvent) -> Result<()> {
        match event {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                self.handle_key(key).await?
            }
            TermEvent::Mouse(mouse) if !self.modal_open() => match mouse.kind {
                MouseEventKind::ScrollUp => self.chat.scroll_lines(-WHEEL_LINES),
                MouseEventKind::ScrollDown => self.chat.scroll_lines(WHEEL_LINES),
                _ => {}
            },
            _ => {}
        }

        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if matches!(key.code, KeyCode::Char('c' | 'q'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.quit = true;
            return Ok(());
        }

        if let Some(permission) = &self.permission {
            // Every other key is swallowed while the modal is open: the
            // editor and the transcript beneath it are not what the user is
            // acting on right now.
            if let Some(reply) = permission_reply(key.code) {
                let id = permission.id().clone();
                self.engine
                    .send(Command::ReplyPermission { id, reply })
                    .await?;
            }

            return Ok(());
        }

        if self.sessions.is_some() {
            self.handle_picker_key(key.code).await;

            return Ok(());
        }

        match key.code {
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_picker().await;
            }
            // A no-op while idle, which is exactly what Esc should do there.
            KeyCode::Esc => self.engine.send(Command::CancelTurn).await?,
            KeyCode::Enter if key.modifiers.intersects(NEWLINE_MODIFIERS) => {
                self.editor.insert_newline();
            }
            KeyCode::Enter => self.submit().await,
            KeyCode::PageUp => self.chat.scroll_pages(-1),
            KeyCode::PageDown => self.chat.scroll_pages(1),
            KeyCode::End => self.chat.follow_tail(),
            _ => self.editor.input(key),
        }

        Ok(())
    }

    /// Whether a modal is claiming the keys and the wheel.
    fn modal_open(&self) -> bool {
        self.permission.is_some() || self.sessions.is_some()
    }

    /// Opens the sessions picker over what this project's store holds.
    ///
    /// A refusal — an engine running without storage, a store that will not
    /// read — lands in the status bar: there is nothing to pick from, so a
    /// dialog would have nothing to say that the notice does not.
    async fn open_picker(&mut self) {
        match self.engine.sessions().await {
            Ok(entries) => self.sessions = Some(Sessions::new(entries, sessions::now())),
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// One keypress while the picker is open, which owns every key: the
    /// editor and the transcript beneath it are not what the user is acting
    /// on right now.
    async fn handle_picker_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.sessions = None,
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(sessions) = &mut self.sessions {
                    sessions.move_selection(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(sessions) = &mut self.sessions {
                    sessions.move_selection(1);
                }
            }
            KeyCode::Enter => self.resume_selected().await,
            _ => {}
        }
    }

    /// Switches to the session the picker is on.
    ///
    /// The screen changes only once the engine has actually resumed: a resume
    /// it refuses — mid-turn, or a session that vanished between listing and
    /// choosing — leaves the current conversation up, with the refusal in the
    /// status bar. The picker stays open so the user still has the list they
    /// were choosing from.
    async fn resume_selected(&mut self) {
        let Some(id) = self
            .sessions
            .as_ref()
            .and_then(|sessions| sessions.selected())
            .map(|info| info.id.clone())
        else {
            // An empty list has nothing under the cursor; Enter means nothing.
            return;
        };

        match self.engine.resume(&id).await {
            Ok(transcript) => {
                self.sessions = None;
                self.chat.clear();
                self.seed(transcript);
                self.status.set_notice(None);
            }
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// Hands the editor's contents to the engine.
    ///
    /// The prompt reaches the transcript as an engine event rather than being
    /// pushed here, so what the screen shows is exactly what the engine will
    /// send back to the model.
    async fn submit(&mut self) {
        let Some(prompt) = self.editor.prompt() else {
            return;
        };

        match self.engine.send(Command::SendPrompt { text: prompt }).await {
            Ok(()) => {
                self.editor.clear();
                self.status.set_notice(None);
            }
            // The editor keeps the text, so a refused prompt is never lost.
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    fn handle_core(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::MessageStarted { message } => {
                if message.role == Role::Assistant {
                    self.status.set_activity(Activity::Streaming);
                }
                self.chat.start_message(message);
            }
            CoreEvent::PartStarted { message_id, part } => self.chat.start_part(&message_id, part),
            CoreEvent::PartDelta {
                message_id,
                part_id,
                delta,
            } => self.chat.append_delta(&message_id, &part_id, &delta),
            CoreEvent::PartUpdated { message_id, part } => {
                if let PartBody::Tool { tool, state, .. } = &part.body {
                    self.status.set_activity(match state {
                        ToolState::Pending | ToolState::Running { .. } => {
                            Activity::Tool(tool.clone())
                        }
                        ToolState::Completed { .. } | ToolState::Error { .. } => {
                            Activity::Streaming
                        }
                    });
                }
                self.chat.update_part(&message_id, part);
            }
            CoreEvent::PermissionRequested {
                id,
                tool,
                title,
                args,
                ..
            } => {
                self.permission = Some(Permission::new(id, tool, title, args));
                self.status.set_activity(Activity::Permission);
            }
            CoreEvent::PermissionReplied { id, .. } => {
                let names_open_request = self
                    .permission
                    .as_ref()
                    .is_some_and(|permission| *permission.id() == id);
                if names_open_request {
                    self.permission = None;
                    self.status.set_activity(Activity::Streaming);
                }
            }
            CoreEvent::MessageFinished {
                reason,
                usage,
                error,
                ..
            } => {
                self.status.set_activity(match reason {
                    FinishReason::Completed => Activity::Ready,
                    FinishReason::Cancelled => Activity::Stopped,
                    FinishReason::Failed => Activity::Failed,
                });
                if let Some(usage) = usage {
                    self.record(&usage);
                }
                if error.is_some() {
                    self.status.set_notice(error);
                }
            }
        }
    }

    /// Adds what a turn spent to the session totals.
    ///
    /// Tokens accumulate whatever the model is, so a run against the fake
    /// provider still shows counts; dollars only appear once the catalog can
    /// price the model, because a made-up figure is worse than none.
    fn record(&mut self, usage: &Usage) {
        // The three input counters are disjoint — `Usage` says so, and each
        // provider is what normalizes to it — so what a turn spent on the way
        // in is their sum rather than `input_tokens` alone.
        let input = usage
            .input_tokens
            .saturating_add(usage.cache_read_tokens)
            .saturating_add(usage.cache_write_tokens);

        self.totals.input_tokens = self.totals.input_tokens.saturating_add(input);
        self.totals.output_tokens = self
            .totals
            .output_tokens
            .saturating_add(usage.output_tokens);

        if let Some(model) = catalog::model(&self.model) {
            *self.totals.cost_usd.get_or_insert(0.0) += catalog::cost(usage, model).total_usd;
        }

        self.status.set_totals(self.totals);
    }

    fn needs_draw(&self) -> bool {
        if self.dirty {
            self.urgent || self.last_draw.elapsed() >= FRAME
        } else {
            // The spinner animates on its own while a turn streams.
            self.status.is_streaming() && self.last_draw.elapsed() >= FRAME
        }
    }

    fn wants_wakeup(&self) -> bool {
        self.dirty || self.status.is_streaming()
    }

    fn until_next_frame(&self) -> Duration {
        FRAME.saturating_sub(self.last_draw.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use futures::{StreamExt as _, stream::BoxStream};
    use ganja_core::{
        Engine, Event as CoreEvent, FinishReason, Message, Part, PartBody, PartId, PermissionId,
        PermissionReply, SessionId, SessionInfo, Storage, ToolState, Usage,
        provider::{FakeProvider, fake},
        storage::VERSION,
    };
    use ratatui::{
        Terminal,
        backend::TestBackend,
        crossterm::event::{
            Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
            MouseEvent, MouseEventKind,
        },
    };
    use tempfile::TempDir;

    use super::{App, FRAME, Permission, permission_reply};
    use crate::{component::sessions, event::AppEvent};

    fn engine() -> Engine {
        Engine::new(
            Arc::new(FakeProvider::default()),
            fake::MODEL,
            Arc::new(ganja_core::Registry::new(Vec::new())),
            ganja_core::Permissions::default(),
        )
    }

    fn app() -> App {
        App::new(engine(), fake::MODEL, None)
    }

    /// An app whose engine writes into, and lists from, a store in `directory`.
    ///
    /// The picker asks the engine what it holds, so a picker test needs the
    /// real storage path rather than a stub: what it renders is what
    /// [`Engine::sessions`] read off the disk.
    fn persistent_app(directory: &TempDir) -> App {
        App::new(
            Engine::persistent(
                Arc::new(FakeProvider::default()),
                fake::MODEL,
                Arc::new(ganja_core::Registry::new(Vec::new())),
                ganja_core::Permissions::default(),
                Storage::open(directory.path().join("storage")),
            ),
            fake::MODEL,
            None,
        )
    }

    /// Stores one session under `id`, last touched `ago` milliseconds before
    /// `now`, carrying `tokens` of accumulated input and one message.
    fn store_session(
        directory: &TempDir,
        id: &str,
        title: Option<&str>,
        now: u64,
        ago: u64,
        tokens: u64,
    ) {
        let storage = Storage::open(directory.path().join("storage"));
        let updated = now.saturating_sub(ago);
        let info = SessionInfo {
            id: SessionId::from(id.to_owned()),
            version: VERSION,
            title: title.map(str::to_owned),
            created: updated,
            updated,
            usage: Usage {
                input_tokens: tokens,
                ..Usage::default()
            },
            context_tokens: 0,
            summary: None,
        };
        let message = Message::user("what the picker is choosing between");

        storage.save_info(&info).expect("the info stores");
        storage
            .save_message(&info.id, &message)
            .expect("the envelope stores");
        for part in &message.parts {
            storage
                .save_part(&info.id, &message.id, part)
                .expect("the part stores");
        }
    }

    /// The three sessions the picker snapshots render: one titled and recent,
    /// one that never earned a title, and one old enough to be listed in
    /// hours.
    ///
    /// Ages are written as offsets from the clock the picker itself reads when
    /// it opens, because a row says how long ago a session was touched. Fixed
    /// timestamps would re-age every day and rot the snapshot; an offset
    /// renders the same interval forever.
    fn store_pickable_sessions(directory: &TempDir) {
        const MINUTE: u64 = 60 * 1_000;
        const HOUR: u64 = 60 * MINUTE;

        let now = sessions::now();

        store_session(
            directory,
            "ses_newest",
            Some("porting the session store"),
            now,
            30 * 1_000,
            12_400,
        );
        store_session(directory, "ses_middle", None, now, 5 * MINUTE, 1_234);
        store_session(
            directory,
            "ses_oldest",
            Some("a first look at the tool registry"),
            now,
            3 * HOUR,
            42,
        );
    }

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
    }

    /// An app plus the engine stream its own loop would read, for the tests
    /// that need a prompt to travel the whole way and come back.
    async fn wired() -> (App, BoxStream<'static, CoreEvent>) {
        let engine = engine();
        let events = engine.subscribe().await.expect("the test subscribes first");

        (App::new(engine, fake::MODEL, None), events)
    }

    /// Feeds the app the next `count` engine events.
    async fn pump(app: &mut App, events: &mut BoxStream<'static, CoreEvent>, count: usize) {
        for _ in 0..count {
            let event = events.next().await.expect("the engine keeps reporting");
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
        }
    }

    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(width, height)).expect("a test terminal is buildable")
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area();

        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
        AppEvent::Term(TermEvent::Key(KeyEvent::new(code, modifiers)))
    }

    fn typing(text: &str) -> impl Iterator<Item = AppEvent> + use<'_> {
        text.chars()
            .map(|character| key(KeyCode::Char(character), KeyModifiers::NONE))
    }

    /// The prompt reaches the transcript through the engine, not through the
    /// editor: the frontend never invents an entry.
    #[tokio::test]
    async fn enter_submits_the_prompt_and_the_engine_puts_it_in_the_transcript() {
        let (mut app, mut events) = wired().await;
        for event in typing("hello") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.editor.prompt().is_none(), "the editor should be empty");

        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            !screen(&terminal).contains("hello"),
            "the transcript should wait for the engine:\n{}",
            screen(&terminal)
        );

        pump(&mut app, &mut events, 1).await;

        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("hello"),
            "the prompt should be in the transcript:\n{}",
            screen(&terminal)
        );
    }

    #[tokio::test]
    async fn modified_enter_inserts_a_newline_instead_of_submitting() {
        for modifier in [
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
        ] {
            let mut app = app();
            for event in typing("one") {
                app.handle(event).await.expect("typing is handled");
            }
            app.handle(key(KeyCode::Enter, modifier))
                .await
                .expect("modified enter is handled");
            for event in typing("two") {
                app.handle(event).await.expect("typing is handled");
            }

            assert_eq!(
                app.editor.prompt().as_deref(),
                Some("one\ntwo"),
                "{modifier:?}+Enter should break the line"
            );
        }
    }

    #[tokio::test]
    async fn a_bare_q_types_instead_of_quitting() {
        let mut app = app();

        app.handle(key(KeyCode::Char('q'), KeyModifiers::NONE))
            .await
            .expect("typing is handled");

        assert!(!app.quit);
        assert_eq!(app.editor.prompt().as_deref(), Some("q"));
    }

    #[tokio::test]
    async fn control_c_and_control_q_quit() {
        for code in [KeyCode::Char('c'), KeyCode::Char('q')] {
            let mut app = app();

            app.handle(key(code, KeyModifiers::CONTROL))
                .await
                .expect("a quit key is handled");

            assert!(app.quit, "{code:?} with Control should quit");
        }
    }

    #[tokio::test]
    async fn a_second_prompt_mid_turn_is_refused_without_losing_the_text() {
        let (mut app, mut events) = wired().await;
        for event in typing("first") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        // Both message envelopes, so the turn is visibly under way.
        pump(&mut app, &mut events, 2).await;

        for event in typing("second") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("second"),
            "a refused prompt must stay in the editor"
        );

        let mut terminal = terminal(100, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("already streaming"),
            "the refusal should be explained:\n{}",
            screen(&terminal)
        );
    }

    /// Drives the real engine, so this covers the whole path a keystroke takes:
    /// Esc becomes a command, the engine stops the turn, and the finish event
    /// turns the status bar over.
    #[tokio::test]
    async fn escape_stops_a_streaming_turn_inside_the_budget() {
        const CANCEL_BUDGET: Duration = Duration::from_millis(100);

        let (mut app, mut events) = wired().await;

        for event in typing("hello") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        // Both envelopes, the reply's part, and a fragment in it: the turn is
        // actually streaming before it gets interrupted.
        pump(&mut app, &mut events, 4).await;
        assert!(app.status.is_streaming());

        let issued = Instant::now();
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        while app.status.is_streaming() {
            let event = events.next().await.expect("the engine keeps reporting");
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
        }
        let elapsed = issued.elapsed();

        assert!(
            elapsed < CANCEL_BUDGET,
            "the turn took {elapsed:?} to stop, budget is {CANCEL_BUDGET:?}"
        );

        let mut terminal = terminal(80, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("stopped"),
            "the status bar should report the cancel:\n{}",
            screen(&terminal)
        );
    }

    #[tokio::test]
    async fn escape_while_idle_changes_nothing() {
        let mut app = app();

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert!(!app.status.is_streaming());
        assert!(!app.quit);
        assert!(app.editor.prompt().is_none());
    }

    #[tokio::test]
    async fn streamed_fragments_land_in_one_entry() {
        let mut app = app();
        let reply = Message::assistant("canned");
        let part = Part::text("");

        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::core(CoreEvent::PartStarted {
            message_id: reply.id.clone(),
            part: part.clone(),
        }))
        .await
        .expect("a part start is handled");
        for fragment in ["stream", "ed ", "reply"] {
            app.handle(AppEvent::core(CoreEvent::PartDelta {
                message_id: reply.id.clone(),
                part_id: part.id.clone(),
                delta: fragment.to_owned(),
            }))
            .await
            .expect("a fragment is handled");
        }

        assert!(app.status.is_streaming());

        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("streamed reply"),
            "fragments should join:\n{}",
            screen(&terminal)
        );

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            message_id: reply.id,
            reason: FinishReason::Cancelled,
            usage: None,
            error: None,
            completed: 0,
        }))
        .await
        .expect("a turn end is handled");
        assert!(!app.status.is_streaming());
    }

    /// A turn the provider refused ends the same way any other does, with the
    /// reason on screen.
    #[tokio::test]
    async fn a_failed_turn_reports_why_in_the_status_bar() {
        let mut app = app();

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            message_id: Message::assistant("canned").id,
            reason: FinishReason::Failed,
            usage: None,
            error: Some("no usable credentials".to_owned()),
            completed: 0,
        }))
        .await
        .expect("a turn end is handled");

        let mut terminal = terminal(80, 12);
        app.draw(&mut terminal).expect("a frame draws");

        assert!(!app.status.is_streaming());
        assert!(
            screen(&terminal).contains("no usable credentials"),
            "the failure should be explained:\n{}",
            screen(&terminal)
        );
    }

    /// Builds the finish event a provider that reported its usage produces.
    fn finished(model: &str, usage: Usage) -> AppEvent {
        AppEvent::core(CoreEvent::MessageFinished {
            message_id: Message::assistant(model).id,
            reason: FinishReason::Completed,
            usage: Some(usage),
            error: None,
            completed: 0,
        })
    }

    /// Spend is per session, not per turn, so two turns add up — and the sum
    /// counts cache traffic, which is billed and is otherwise invisible.
    #[tokio::test]
    async fn usage_accumulates_across_turns_and_reaches_the_status_bar() {
        const MODEL: &str = "claude-sonnet-5";

        let mut app = App::new(engine(), MODEL, None);
        let usage = Usage {
            input_tokens: 6_000,
            output_tokens: 400,
            reasoning_tokens: 200,
            cache_read_tokens: 6_000,
            cache_write_tokens: 300,
        };

        for _ in 0..2 {
            app.handle(finished(MODEL, usage))
                .await
                .expect("a turn end is handled");
        }

        assert_eq!(
            app.totals.input_tokens, 24_600,
            "6,000 + 6,000 + 300, twice"
        );
        assert_eq!(app.totals.output_tokens, 800);

        let mut terminal = terminal(100, 12);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        assert!(screen.contains("24.6k in"), "got:\n{screen}");
        assert!(screen.contains("800 out"), "got:\n{screen}");
        // Two turns of $0.01795: input 6k at $2, 6k cached at $0.20, 300
        // written at $2.50, output 400 at $10, all per million tokens.
        assert!(screen.contains("$0.0359"), "got:\n{screen}");
    }

    /// The fake provider reports usage against a model with no price, which is
    /// exactly the case that must show counts and no dollars.
    #[tokio::test]
    async fn an_unpriced_model_reports_its_tokens_and_invents_no_price() {
        let mut app = app();

        app.handle(finished(
            fake::MODEL,
            Usage {
                input_tokens: 40,
                output_tokens: 7,
                ..Usage::default()
            },
        ))
        .await
        .expect("a turn end is handled");

        assert_eq!(app.totals.cost_usd, None);

        let mut terminal = terminal(100, 12);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        assert!(screen.contains("40 in"), "got:\n{screen}");
        assert!(screen.contains("7 out"), "got:\n{screen}");
        assert!(!screen.contains('$'), "got:\n{screen}");
    }

    /// A turn that died part-way through still spent what it spent. Since a
    /// provider failure became a terminal `Failed` event, a finish can carry
    /// both an error and the usage reported before the stream broke, and the
    /// bill has to survive the failure rather than being written off with it.
    #[tokio::test]
    async fn a_failed_turn_still_bills_for_what_it_spent() {
        const MODEL: &str = "claude-sonnet-5";

        let mut app = App::new(engine(), MODEL, None);

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            message_id: Message::assistant(MODEL).id,
            reason: FinishReason::Failed,
            usage: Some(Usage {
                input_tokens: 2_000,
                output_tokens: 150,
                ..Usage::default()
            }),
            error: Some("the provider answered 500: overloaded".to_owned()),
            completed: 0,
        }))
        .await
        .expect("a failed turn is handled");

        assert_eq!(app.totals.input_tokens, 2_000);
        assert_eq!(app.totals.output_tokens, 150);

        let mut terminal = terminal(120, 12);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        assert!(screen.contains("failed"), "got:\n{screen}");
        assert!(screen.contains("2.0k in"), "got:\n{screen}");
        // 2,000 in at $2 plus 150 out at $10, per million tokens.
        assert!(screen.contains("$0.0055"), "got:\n{screen}");
        assert!(
            screen.contains("overloaded"),
            "the reason must survive beside the bill:\n{screen}"
        );
    }

    /// A turn that ends without usage — a cancel, or a provider that reports
    /// none — leaves the totals alone rather than resetting them.
    #[tokio::test]
    async fn a_turn_without_usage_does_not_disturb_the_totals() {
        const MODEL: &str = "claude-sonnet-5";

        let mut app = App::new(engine(), MODEL, None);
        app.handle(finished(
            MODEL,
            Usage {
                input_tokens: 1_000,
                output_tokens: 100,
                ..Usage::default()
            },
        ))
        .await
        .expect("a turn end is handled");

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            message_id: Message::assistant(MODEL).id,
            reason: FinishReason::Cancelled,
            usage: None,
            error: None,
            completed: 0,
        }))
        .await
        .expect("a cancel is handled");

        assert_eq!(app.totals.input_tokens, 1_000);
        assert_eq!(app.totals.output_tokens, 100);
    }

    #[tokio::test]
    async fn the_wheel_and_the_page_keys_move_the_viewport() {
        let mut app = app();
        for index in 0..60 {
            app.chat
                .start_message(Message::user(format!("entry {index}")));
        }
        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");

        app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })))
        .await
        .expect("a wheel event is handled");
        assert!(!app.chat.is_following_tail());

        app.handle(key(KeyCode::End, KeyModifiers::NONE))
            .await
            .expect("End is handled");
        assert!(app.chat.is_following_tail());

        app.handle(key(KeyCode::PageUp, KeyModifiers::NONE))
            .await
            .expect("PageUp is handled");
        assert!(!app.chat.is_following_tail());

        app.handle(key(KeyCode::PageDown, KeyModifiers::NONE))
            .await
            .expect("PageDown is handled");
        assert!(app.chat.is_following_tail());
    }

    #[tokio::test]
    async fn unrelated_events_do_not_disturb_the_editor() {
        let mut app = app();
        for event in typing("draft") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })))
        .await
        .expect("a click is handled");
        app.handle(AppEvent::Term(TermEvent::Key(KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ))))
        .await
        .expect("a key release is handled");
        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert_eq!(app.editor.prompt().as_deref(), Some("draft"));
    }

    #[tokio::test]
    async fn engine_bursts_are_coalesced_but_keystrokes_are_not() {
        let mut app = app();
        let reply = Message::assistant("canned");
        let part = Part::text("");
        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::core(CoreEvent::PartStarted {
            message_id: reply.id.clone(),
            part: part.clone(),
        }))
        .await
        .expect("a part start is handled");

        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");

        app.handle(AppEvent::core(CoreEvent::PartDelta {
            message_id: reply.id,
            part_id: part.id,
            delta: "burst".to_owned(),
        }))
        .await
        .expect("a fragment is handled");
        assert!(
            !app.needs_draw(),
            "a fragment arriving inside the frame budget should wait"
        );
        assert!(app.wants_wakeup(), "the pending frame must be woken up for");
        assert!(app.until_next_frame() <= FRAME);

        app.handle(key(KeyCode::Char('a'), KeyModifiers::NONE))
            .await
            .expect("typing is handled");
        assert!(app.needs_draw(), "a keystroke should redraw immediately");
    }

    #[test]
    fn a_resize_storm_rewraps_without_panicking() {
        let mut app = app();
        for index in 0..40 {
            app.chat.start_message(Message::user(format!(
                "entry {index} carries enough words to wrap at every width tried here"
            )));
        }

        let mut terminal = terminal(80, 24);
        for width in [80_u16, 12, 200, 1, 61, 3, 120, 40] {
            terminal.backend_mut().resize(width, 24);
            app.draw(&mut terminal)
                .expect("a frame draws after a resize");

            assert!(
                app.chat
                    .cached_widths()
                    .iter()
                    .all(|cached| *cached == Some(width)),
                "the wrap cache should be invalidated by a resize to {width}"
            );
        }

        terminal.backend_mut().resize(1, 1);
        app.draw(&mut terminal)
            .expect("a frame draws into a one-cell terminal");
    }

    #[test]
    #[ignore = "timing-sensitive: run at phase gates with `cargo test -- --ignored`"]
    fn a_five_thousand_line_transcript_draws_inside_the_frame_budget() {
        const ENTRIES: usize = 250;
        const LINES_PER_ENTRY: usize = 20;
        const FRAMES: usize = 200;

        let mut app = app();
        for entry in 0..ENTRIES {
            let body = (0..LINES_PER_ENTRY)
                .map(|line| format!("entry {entry} line {line} of plain streamed transcript text"))
                .collect::<Vec<_>>()
                .join("\n");
            app.chat.start_message(Message::user(body));
        }

        let mut terminal = terminal(120, 40);
        // The first frame wraps everything; the budget is about the rest.
        app.draw(&mut terminal).expect("the warm-up frame draws");
        assert!(
            app.chat.line_count() >= 5_000,
            "the transcript should be at least 5,000 lines, got {}",
            app.chat.line_count()
        );

        let mut samples = Vec::with_capacity(FRAMES);
        for frame in 0..FRAMES {
            app.chat
                .scroll_lines(if frame.is_multiple_of(2) { -7 } else { 5 });

            let started = Instant::now();
            app.draw(&mut terminal).expect("a frame draws");
            samples.push(started.elapsed());
        }

        samples.sort_unstable();
        let p50 = samples[FRAMES / 2];
        let p95 = samples[FRAMES * 95 / 100];
        let worst = samples.last().copied().unwrap_or_default();

        // The gate runner reads these numbers off the test output.
        println!(
            "{} lines, {FRAMES} frames: p50 {p50:?}, p95 {p95:?}, worst {worst:?}",
            app.chat.line_count()
        );

        assert!(
            p95 < FRAME,
            "p95 frame time was {p95:?} (worst {worst:?}), budget is {FRAME:?}"
        );
    }

    /// A stored conversation of `count` messages with the shape a real one
    /// has: prompts of a few lines, replies of a paragraph or two, and every
    /// fifth reply carrying a finished tool call between its text parts.
    ///
    /// Deliberately not made of one-word messages. The cost the frontend pays
    /// on a resume is wrapping every line of every entry, so a transcript of
    /// stubs would measure nothing.
    fn stored_transcript(count: usize) -> Vec<Message> {
        (0..count)
            .map(|index| {
                if index.is_multiple_of(2) {
                    return Message::user(format!(
                        "turn {index}: each part of a message is written to its own file, so a\n\
                         streaming text part can be rewritten as it grows without rewriting the\n\
                         whole envelope. Walk me through what a resume has to rebuild from that."
                    ));
                }

                let mut reply = Message::assistant(fake::MODEL);
                reply.parts.push(Part::text(format!(
                    "turn {index}: a resume reads the info file first, because it names the\n\
                     summary the live window starts from. Everything from that message onward\n\
                     is read back envelope by envelope, and each envelope's parts are read from\n\
                     the part directory keyed by the message id.\n\
                     \n\
                     Assistant messages that carry no content stay in the transcript but are\n\
                     left out of the request window, since some providers reject an empty\n\
                     message; they render as interrupted rather than as complete."
                )));

                if index.is_multiple_of(5) {
                    reply.parts.push(Part {
                        id: PartId::from(format!("prt_{index}")),
                        body: PartBody::Tool {
                            call_id: format!("call_{index}"),
                            tool: "read".to_owned(),
                            state: ToolState::Completed {
                                input: serde_json::json!({"filePath": "crates/ganja-core/src/storage.rs"}),
                                output: "read 412 lines".to_owned(),
                                title: "storage.rs".to_owned(),
                                metadata: serde_json::json!({}),
                                started: 0,
                                completed: 1,
                            },
                        },
                    });
                    reply.parts.push(Part::text(
                        "The tool call above is closed as an error on resume when the previous\n\
                         process died before it finished, because the next request has to answer\n\
                         every call the model opened.",
                    ));
                }

                reply.complete();
                reply
            })
            .collect()
    }

    /// P4 acceptance: a 200-message session reaches its first frame inside
    /// 150ms.
    ///
    /// The measured window is [`App::seed`] plus the first [`App::draw`], over
    /// messages already in memory. Reading them off the disk is the engine's
    /// cost and is timed where it happens; what the frontend owes is turning a
    /// resumed transcript into a screen. The first frame is the expensive one
    /// because it wraps every entry — later frames reuse the cache.
    #[test]
    fn a_two_hundred_message_session_reaches_its_first_frame_inside_the_budget() {
        const BUDGET: Duration = Duration::from_millis(150);
        const MESSAGES: usize = 200;

        // Built outside the window: composing fixtures is not what a resume
        // pays for.
        let transcript = stored_transcript(MESSAGES);
        let mut app = app();
        let mut terminal = terminal(100, 30);

        let started = Instant::now();
        app.seed(transcript);
        app.draw(&mut terminal).expect("the first frame draws");
        let elapsed = started.elapsed();

        // The gate runner reads this number off the test log rather than
        // trusting a green assertion to mean it was measured.
        eprintln!(
            "{MESSAGES} messages, {} wrapped lines: first frame in {elapsed:?} (budget {BUDGET:?})",
            app.chat.line_count()
        );

        assert!(
            app.chat.line_count() >= 1_000,
            "a transcript this budget is worth measuring should be at least 1,000 lines, got {}",
            app.chat.line_count()
        );
        assert!(
            elapsed < BUDGET,
            "the first frame took {elapsed:?}, budget is {BUDGET:?}"
        );
    }

    #[test]
    fn a_fresh_app_wants_its_first_frame() {
        let app = app();

        assert!(app.needs_draw());
        assert!(app.until_next_frame() <= FRAME);
    }

    fn permission_event(id: &str) -> CoreEvent {
        CoreEvent::PermissionRequested {
            id: PermissionId::from(id.to_owned()),
            call_id: "call_1".to_owned(),
            tool: "shell".to_owned(),
            title: "cargo test".to_owned(),
            args: serde_json::json!({"command": "cargo test"}),
        }
    }

    #[tokio::test]
    async fn a_tool_call_moves_through_its_lifecycle_on_screen() {
        let mut app = app();
        let reply = Message::assistant("canned");
        let part = Part::tool("call_1", "shell");

        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::core(CoreEvent::PartStarted {
            message_id: reply.id.clone(),
            part: part.clone(),
        }))
        .await
        .expect("a part start is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("[running] shell"),
            "got:\n{}",
            screen(&terminal)
        );

        app.handle(AppEvent::core(CoreEvent::PartUpdated {
            message_id: reply.id.clone(),
            part: Part {
                id: part.id.clone(),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "shell".to_owned(),
                    state: ToolState::Running {
                        input: serde_json::json!({"command": "cargo test"}),
                        started: 0,
                    },
                },
            },
        }))
        .await
        .expect("a running update is handled");
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("cargo test"),
            "got:\n{}",
            screen(&terminal)
        );

        app.handle(AppEvent::core(CoreEvent::PartUpdated {
            message_id: reply.id.clone(),
            part: Part {
                id: part.id.clone(),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "shell".to_owned(),
                    state: ToolState::Completed {
                        input: serde_json::json!({"command": "cargo test"}),
                        output: "ok".to_owned(),
                        title: "cargo test".to_owned(),
                        metadata: serde_json::json!({}),
                        started: 0,
                        completed: 1,
                    },
                },
            },
        }))
        .await
        .expect("a completed update is handled");
        app.draw(&mut terminal).expect("a frame draws");
        let screen_text = screen(&terminal);
        assert!(screen_text.contains("[done] shell"), "got:\n{screen_text}");
        assert!(screen_text.contains("ok"), "got:\n{screen_text}");
    }

    #[tokio::test]
    async fn a_part_updated_for_an_unseen_id_is_appended_not_dropped() {
        let mut app = app();
        let reply = Message::assistant("canned");
        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");

        // No PartStarted for this id: a frontend that joined mid-stream still
        // has to converge on the same transcript.
        app.handle(AppEvent::core(CoreEvent::PartUpdated {
            message_id: reply.id.clone(),
            part: Part {
                id: PartId::from("prt_orphan".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "read".to_owned(),
                    state: ToolState::Running {
                        input: serde_json::json!({"filePath": "a.rs"}),
                        started: 0,
                    },
                },
            },
        }))
        .await
        .expect("an update for an unseen id is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("a.rs"),
            "an update for an id the transcript never saw start should still append, got:\n{}",
            screen(&terminal)
        );
    }

    #[test]
    fn permission_keys_map_to_the_right_reply() {
        assert_eq!(
            permission_reply(KeyCode::Char('y')),
            Some(PermissionReply::Once)
        );
        assert_eq!(
            permission_reply(KeyCode::Char('a')),
            Some(PermissionReply::Always)
        );
        assert_eq!(
            permission_reply(KeyCode::Char('n')),
            Some(PermissionReply::Reject)
        );
        assert_eq!(
            permission_reply(KeyCode::Esc),
            Some(PermissionReply::Reject)
        );
        assert_eq!(permission_reply(KeyCode::Char('x')), None);
    }

    #[tokio::test]
    async fn keys_while_the_dialog_is_open_are_sent_as_replies_not_typed() {
        let mut app = app();
        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");

        app.handle(key(KeyCode::Char('y'), KeyModifiers::NONE))
            .await
            .expect("y is handled");

        assert!(
            app.permission.is_some(),
            "the dialog waits for PermissionReplied before closing"
        );
        assert!(
            app.editor.prompt().is_none(),
            "the keystroke must not reach the editor"
        );
    }

    #[tokio::test]
    async fn a_permission_request_opens_the_dialog() {
        let mut app = app();
        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");

        assert!(app.permission.is_some());

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("shell"), "got:\n{screen}");
        assert!(screen.contains("cargo test"), "got:\n{screen}");
    }

    #[tokio::test]
    async fn a_matching_reply_closes_the_dialog_but_a_stray_one_does_not() {
        let mut app = app();
        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");
        assert!(app.permission.is_some());

        app.handle(AppEvent::core(CoreEvent::PermissionReplied {
            id: PermissionId::from("perm_other".to_owned()),
            reply: PermissionReply::Reject,
        }))
        .await
        .expect("a stray reply is handled");
        assert!(
            app.permission.is_some(),
            "a reply naming a different request must not close this dialog"
        );

        app.handle(AppEvent::core(CoreEvent::PermissionReplied {
            id: PermissionId::from("perm_1".to_owned()),
            reply: PermissionReply::Once,
        }))
        .await
        .expect("the matching reply is handled");
        assert!(app.permission.is_none());
    }

    #[tokio::test]
    async fn control_c_still_quits_while_the_dialog_is_open() {
        let mut app = app();
        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");

        app.handle(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .expect("control-c is handled");

        assert!(app.quit);
    }

    /// Drives a real turn to a streaming state, opens the dialog by hand
    /// (nothing in this build yet gates a real tool call on one), presses
    /// Esc, and proves the turn was never cancelled: it runs to a natural
    /// `Completed` finish rather than the `Cancelled` the sibling escape test
    /// gets when Esc is allowed to reach `CancelTurn`.
    #[tokio::test]
    async fn escape_with_the_dialog_open_does_not_cancel_the_turn() {
        let engine = Engine::new(
            Arc::new(FakeProvider::new("one two three", Duration::from_millis(2))),
            fake::MODEL,
            Arc::new(ganja_core::Registry::new(Vec::new())),
            ganja_core::Permissions::default(),
        );
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, fake::MODEL, None);

        for event in typing("hello") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        pump(&mut app, &mut events, 4).await;
        assert!(app.status.is_streaming());

        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");
        assert!(app.permission.is_some());

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(
            app.permission.is_some(),
            "Esc should reject the dialog, not close it before PermissionReplied"
        );

        let mut finished = None;
        while finished.is_none() {
            let event = events.next().await.expect("the engine keeps reporting");
            if let CoreEvent::MessageFinished { reason, .. } = &event {
                finished = Some(*reason);
            }
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
        }

        assert_eq!(
            finished,
            Some(FinishReason::Completed),
            "Esc must not have cancelled the turn"
        );
        assert!(
            app.permission.is_some(),
            "the dialog should not self-close on an unrelated event"
        );
    }

    #[test]
    fn snapshot_tool_pending() {
        let mut app = app();
        let mut message = Message::assistant("canned");
        message.parts.push(Part::tool("call_1", "shell"));
        app.chat.start_message(message);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn snapshot_tool_running() {
        let mut app = app();
        let mut message = Message::assistant("canned");
        message.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                state: ToolState::Running {
                    input: serde_json::json!({"command": "cargo test"}),
                    started: 0,
                },
            },
        });
        app.chat.start_message(message);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn snapshot_tool_completed_with_a_diff() {
        let mut app = app();
        let mut message = Message::assistant("canned");
        message.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "edit".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"filePath": "a.rs"}),
                    output: "edited a.rs".to_owned(),
                    title: "a.rs".to_owned(),
                    metadata: serde_json::json!({
                        "diff": "@@ -1,2 +1,2 @@\n-old line\n+new line\n context line"
                    }),
                    started: 0,
                    completed: 1,
                },
            },
        });
        app.chat.start_message(message);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn snapshot_tool_error() {
        let mut app = app();
        let mut message = Message::assistant("canned");
        message.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                state: ToolState::Error {
                    input: serde_json::json!({"command": "rm -rf /"}),
                    error: "refused: destructive command\nsee policy for details".to_owned(),
                    started: 0,
                    completed: 1,
                },
            },
        });
        app.chat.start_message(message);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn snapshot_permission_dialog_open() {
        let mut app = app();
        app.permission = Some(Permission::new(
            PermissionId::from("perm_1".to_owned()),
            "shell".to_owned(),
            "cargo test".to_owned(),
            serde_json::json!({"command": "cargo test"}),
        ));

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// Opens the picker the way a user does — Ctrl-S, through `App::handle` —
    /// so what is snapshotted is the dialog over the list the engine actually
    /// read back, not a `Sessions` assembled by the test.
    #[tokio::test]
    async fn snapshot_sessions_picker_open() {
        let directory = temporary();
        store_pickable_sessions(&directory);
        let mut app = persistent_app(&directory);

        app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .expect("control-s is handled");

        assert!(
            app.sessions.is_some(),
            "the picker must be open, or the snapshot is of a bare screen"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_sessions_picker_after_moving_the_selection() {
        let directory = temporary();
        store_pickable_sessions(&directory);
        let mut app = persistent_app(&directory);

        app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .expect("control-s is handled");
        app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .await
            .expect("j is handled");

        assert_eq!(
            app.sessions
                .as_ref()
                .and_then(|sessions| sessions.selected())
                .map(|info| info.id.as_str()),
            Some("ses_middle"),
            "j should move down one row rather than reaching the editor"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }
}
