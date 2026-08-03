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
use ganja_core::{Command, Engine, Event as CoreEvent, FinishReason, Role};
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
        status::{Activity, Status},
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

/// The whole terminal application.
pub struct App {
    engine: Engine,
    chat: Chat,
    editor: Editor,
    status: Status,
    theme: Theme,
    /// State changed since the last frame.
    dirty: bool,
    /// The change came from the keyboard, which skips the coalescing gate.
    urgent: bool,
    last_draw: Instant,
    quit: bool,
}

impl App {
    /// Builds an app driven by `engine`, showing `notice` in the status bar.
    #[must_use]
    pub fn new(engine: Engine, notice: Option<String>) -> Self {
        let theme = Theme::default();

        Self {
            engine,
            chat: Chat::default(),
            editor: Editor::new(&theme),
            status: Status::new(notice),
            theme,
            dirty: true,
            urgent: true,
            last_draw: Instant::now(),
            quit: false,
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
                    Some(incoming) => AppEvent::Core(incoming),
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
                self.handle_core(event);
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
            TermEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.chat.scroll_lines(-WHEEL_LINES),
                MouseEventKind::ScrollDown => self.chat.scroll_lines(WHEEL_LINES),
                _ => {}
            },
            _ => {}
        }

        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('c' | 'q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true;
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
            CoreEvent::MessageFinished { reason, error, .. } => {
                self.status.set_activity(match reason {
                    FinishReason::Completed => Activity::Ready,
                    FinishReason::Cancelled => Activity::Stopped,
                    FinishReason::Failed => Activity::Failed,
                });
                if error.is_some() {
                    self.status.set_notice(error);
                }
            }
        }
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
        Engine, Event as CoreEvent, FinishReason, Message, Part,
        provider::{FakeProvider, fake},
    };
    use ratatui::{
        Terminal,
        backend::TestBackend,
        crossterm::event::{
            Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
            MouseEvent, MouseEventKind,
        },
    };

    use super::{App, FRAME};
    use crate::event::AppEvent;

    fn engine() -> Engine {
        Engine::new(Arc::new(FakeProvider::default()), fake::MODEL)
    }

    fn app() -> App {
        App::new(engine(), None)
    }

    /// An app plus the engine stream its own loop would read, for the tests
    /// that need a prompt to travel the whole way and come back.
    async fn wired() -> (App, BoxStream<'static, CoreEvent>) {
        let engine = engine();
        let events = engine.subscribe().await.expect("the test subscribes first");

        (App::new(engine, None), events)
    }

    /// Feeds the app the next `count` engine events.
    async fn pump(app: &mut App, events: &mut BoxStream<'static, CoreEvent>, count: usize) {
        for _ in 0..count {
            let event = events.next().await.expect("the engine keeps reporting");
            app.handle(AppEvent::Core(event))
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
            app.handle(AppEvent::Core(event))
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

        app.handle(AppEvent::Core(CoreEvent::MessageStarted {
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::Core(CoreEvent::PartStarted {
            message_id: reply.id.clone(),
            part: part.clone(),
        }))
        .await
        .expect("a part start is handled");
        for fragment in ["stream", "ed ", "reply"] {
            app.handle(AppEvent::Core(CoreEvent::PartDelta {
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

        app.handle(AppEvent::Core(CoreEvent::MessageFinished {
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

        app.handle(AppEvent::Core(CoreEvent::MessageFinished {
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
        app.handle(AppEvent::Core(CoreEvent::MessageStarted {
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::Core(CoreEvent::PartStarted {
            message_id: reply.id.clone(),
            part: part.clone(),
        }))
        .await
        .expect("a part start is handled");

        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");

        app.handle(AppEvent::Core(CoreEvent::PartDelta {
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

    #[test]
    fn a_fresh_app_wants_its_first_frame() {
        let app = app();

        assert!(app.needs_draw());
        assert!(app.until_next_frame() <= FRAME);
    }
}
