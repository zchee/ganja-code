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
    command,
    component::{
        chat::{Chat, WHEEL_LINES},
        dropdown::{self, Dropdown},
        editor::{self, Editor},
        help::Help,
        list::{self, ListDialog},
        palette::Palette,
        permission::Permission,
        sessions::{self, Sessions},
        status::{Activity, Status, Totals},
        themes::ThemeList,
    },
    event::AppEvent,
    keybind::{self, Keybinds},
    theme::{Theme, Themes},
};

/// Shortest gap between frames: roughly 60 FPS.
pub const FRAME: Duration = Duration::from_millis(16);

/// Modifiers that turn Enter into a line break. Terminals disagree about which
/// of these they can report, so all of them mean the same thing.
const NEWLINE_MODIFIERS: KeyModifiers = KeyModifiers::SHIFT
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::CONTROL);

/// Modifiers that stop a printable key from reaching a filter line: they mean
/// the key is a shortcut rather than a character.
const SHORTCUT_MODIFIERS: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);

/// Which list a dialog is showing, and therefore what choosing a row sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chooser {
    /// The provider's catalog models.
    Models,
    /// The agents this session may run as.
    Agents,
}

/// Whether the editor itself would do something with `key`.
///
/// The one exit binding that is also an editing key. Upstream gates its whole
/// exit binding on an empty-or-unfocused prompt; ganja gates the half that
/// would otherwise take a keystroke away from typing, and leaves Ctrl-C and
/// Ctrl-Q the unconditional interrupts they have always been here
/// (deviation: exit-gate-only-for-editing-keys).
fn edits(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL
}

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
    /// Provider this session runs on, which is what the model list is narrowed
    /// to: switching model is same-provider only, so offering the rest of the
    /// catalog would be offering refusals.
    provider: String,
    /// Model the engine asks for, kept here because pricing a turn needs it and
    /// the engine's copy is not the frontend's business.
    model: String,
    /// Agent the next turn runs as, [`None`] on a session built without a
    /// registry. Tracked here rather than read back per frame because this is
    /// the side that issues the switches.
    agent: Option<String>,
    chat: Chat,
    editor: Editor,
    status: Status,
    /// Which keys reach which actions this run.
    keys: Keybinds,
    /// The tool call currently waiting on the user's decision, if any.
    permission: Option<Permission>,
    /// The stored sessions the user is choosing between, while the picker is
    /// open.
    sessions: Option<Sessions>,
    /// The themes the user is choosing between, while that picker is open.
    theme_list: Option<ThemeList>,
    /// The models or agents the user is choosing between, and which of the two
    /// the list is.
    chooser: Option<(Chooser, ListDialog)>,
    /// The command palette, while it is open.
    palette: Option<Palette>,
    /// What was typed into the palette's filter, kept across a close so that
    /// reopening does not mean typing it again.
    palette_filter: String,
    /// The reference card, while it is open.
    help: Option<Help>,
    /// The inline command menu, while the buffer is a command being typed.
    dropdown: Option<Dropdown>,
    /// Every theme this run can switch to, and which one is active.
    themes: Themes,
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
    /// `notice` in the status bar, drawn in whichever of `themes` is active.
    ///
    /// The registry is handed in rather than loaded here so that the disk —
    /// the user's theme directory and their stored pick — is read on the one
    /// startup path that should read it, and so that the lane wiring
    /// configuration in has somewhere to put a configured theme.
    #[must_use]
    pub fn new(
        engine: Engine,
        model: impl Into<String>,
        notice: Option<String>,
        mut themes: Themes,
    ) -> Self {
        let theme = themes.theme();
        let agent = engine.agent();
        let mut status = Status::new(notice);
        status.set_agent(agent.clone());

        Self {
            engine,
            provider: String::new(),
            model: model.into(),
            agent,
            chat: Chat::default(),
            editor: Editor::new(&theme),
            status,
            keys: Keybinds::defaults(),
            permission: None,
            sessions: None,
            theme_list: None,
            chooser: None,
            palette: None,
            palette_filter: String::new(),
            help: None,
            dropdown: None,
            themes,
            theme,
            totals: Totals::default(),
            dirty: true,
            urgent: true,
            last_draw: Instant::now(),
            quit: false,
        }
    }

    /// Names the provider the engine was built on.
    ///
    /// A builder rather than a constructor argument because it is exactly one
    /// dialog's business — the model list — and every test that does not open
    /// that dialog should not have to answer for it.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();

        self
    }

    /// Uses `keys` instead of the compiled-in bindings.
    #[must_use]
    pub fn with_keybinds(mut self, keys: Keybinds) -> Self {
        self.keys = keys;

        self
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
                let area = frame.area();
                let [transcript, prompt, status] = Layout::vertical([
                    Constraint::Min(1),
                    Constraint::Length(editor::HEIGHT),
                    Constraint::Length(1),
                ])
                .areas(area);

                let buffer = frame.buffer_mut();
                // The theme's surface goes down first and everything is drawn
                // over it, which is how a theme reaches the cells no component
                // writes to. A theme with no background of its own patches
                // nothing, leaving the terminal's — image, transparency and
                // all — showing (upstream `context/theme.tsx:269`).
                buffer.set_style(area, self.theme.background);
                self.chat.render(transcript, buffer, &self.theme);
                // Anchored to the editor and drawn over the transcript, which
                // is what makes it read as part of what is being typed rather
                // than as another dialog.
                if let Some(dropdown) = &self.dropdown {
                    dropdown.render(prompt, buffer, &self.theme);
                }
                // The permission dialog draws last so that it is on top: it is
                // the one modal a turn is blocked on.
                if let Some(sessions) = &self.sessions {
                    sessions.render(transcript, buffer, &self.theme);
                }
                if let Some(themes) = &self.theme_list {
                    themes.render(transcript, buffer, &self.theme);
                }
                if let Some((_, chooser)) = &self.chooser {
                    chooser.render(transcript, buffer, &self.theme);
                }
                if let Some(palette) = &self.palette {
                    palette.render(transcript, buffer, &self.theme);
                }
                if let Some(help) = &self.help {
                    help.render(transcript, buffer, &self.theme);
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
        if self.exits(key) {
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

        if self.help.is_some() {
            // Nothing to choose, so both of the keys that mean "done" close
            // it and everything else is swallowed like any other modal.
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.help = None;
            }

            return Ok(());
        }

        if self.sessions.is_some() {
            self.handle_picker_key(key.code).await;

            return Ok(());
        }

        if self.theme_list.is_some() {
            self.handle_theme_key(key.code);

            return Ok(());
        }

        if self.chooser.is_some() {
            self.handle_chooser_key(key.code).await;

            return Ok(());
        }

        if self.palette.is_some() {
            self.handle_palette_key(key).await;

            return Ok(());
        }

        // Not a modal: the menu sits over the transcript while the editor
        // keeps the cursor, so it claims only the keys that steer it and lets
        // every other one through to carry on typing.
        if self.dropdown.is_some() && self.handle_dropdown_key(key).await {
            return Ok(());
        }

        match self.keys.action(key) {
            Some(keybind::Action::PaletteOpen) => {
                self.open_palette();
                return Ok(());
            }
            Some(keybind::Action::SessionsOpen) => {
                self.open_picker().await;
                return Ok(());
            }
            Some(keybind::Action::ThemesOpen) => {
                self.open_themes();
                return Ok(());
            }
            // Tab means "next agent" on an empty buffer only; with something
            // typed it is the editor's own key, as it is in every editor.
            Some(keybind::Action::AgentCycle) if self.editor.is_empty() => {
                self.cycle_agent().await;
                return Ok(());
            }
            // Including an exit binding whose gate said no, which falls
            // through to the editor below and deletes forward there.
            _ => {}
        }

        match key.code {
            // A no-op while idle, which is exactly what Esc should do there.
            KeyCode::Esc => self.engine.send(Command::CancelTurn).await?,
            KeyCode::Enter if key.modifiers.intersects(NEWLINE_MODIFIERS) => {
                self.editor.insert_newline();
                self.sync_dropdown();
            }
            KeyCode::Enter => self.submit().await,
            KeyCode::PageUp => self.chat.scroll_pages(-1),
            KeyCode::PageDown => self.chat.scroll_pages(1),
            // The two ends of the line while there is a line, and the two ends
            // of the conversation while there is not. Upstream layers these on
            // whether the composer has focus; ganja's composer always has it,
            // so what is left of the distinction is whether it holds anything.
            KeyCode::Home if self.editor.is_empty() => self.chat.scroll_to_top(),
            KeyCode::Home => self.editor.line_home(),
            KeyCode::End if self.editor.is_empty() => self.chat.follow_tail(),
            KeyCode::End => self.editor.line_end(),
            _ => {
                self.editor.input(key);
                self.sync_dropdown();
            }
        }

        Ok(())
    }

    /// Whether `key` quits.
    ///
    /// A bound key the editor also uses only quits on an empty buffer, so
    /// Ctrl-D deletes forward while there is something to delete and leaves
    /// once there is not.
    fn exits(&self, key: KeyEvent) -> bool {
        self.keys.binds(keybind::Action::AppExit, key) && (!edits(key) || self.editor.is_empty())
    }

    /// Whether a modal is claiming the keys and the wheel.
    fn modal_open(&self) -> bool {
        self.permission.is_some()
            || self.sessions.is_some()
            || self.theme_list.is_some()
            || self.chooser.is_some()
            || self.palette.is_some()
            || self.help.is_some()
    }

    /// Runs the command a palette row or a menu row named.
    async fn run_command(&mut self, action: command::Action) {
        match action {
            command::Action::Sessions => self.open_picker().await,
            command::Action::Models => self.open_models(),
            command::Action::Agents => self.open_agents(),
            command::Action::Themes => self.open_themes(),
            command::Action::Help => self.help = Some(Help::new(self.keys.clone())),
            command::Action::Exit => self.quit = true,
        }
    }

    /// Opens the palette on whatever filter it was last closed with.
    fn open_palette(&mut self) {
        self.palette = Some(Palette::reopened(
            self.keys.clone(),
            self.palette_filter.clone(),
        ));
    }

    /// Closes the palette, remembering what had been typed into it.
    fn close_palette(&mut self) {
        if let Some(palette) = self.palette.take() {
            self.palette_filter = palette.filter().to_owned();
        }
    }

    /// One keypress while the palette is open, which owns every key: its
    /// filter line is what the keyboard is pointed at.
    async fn handle_palette_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_palette(),
            KeyCode::Up => self.move_palette(-1),
            KeyCode::Down => self.move_palette(1),
            KeyCode::Backspace => {
                if let Some(palette) = &mut self.palette {
                    palette.backspace();
                }
            }
            KeyCode::Enter => {
                let action = self.palette.as_ref().and_then(Palette::selected);
                self.close_palette();
                if let Some(action) = action {
                    self.run_command(action).await;
                }
            }
            // Everything printable is filter text — j and k included, which is
            // why this dialog does not take them as movement the way the ones
            // without a filter line do.
            KeyCode::Char(character) if !key.modifiers.intersects(SHORTCUT_MODIFIERS) => {
                if let Some(palette) = &mut self.palette {
                    palette.push(character);
                }
            }
            _ => {}
        }
    }

    /// Moves the palette's cursor by `delta` commands.
    fn move_palette(&mut self, delta: isize) {
        if let Some(palette) = &mut self.palette {
            palette.move_selection(delta);
        }
    }

    /// One keypress while the model or agent list is open.
    async fn handle_chooser_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.chooser = None,
            KeyCode::Up | KeyCode::Char('k') => self.move_chooser(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_chooser(1),
            KeyCode::Enter => self.apply_chooser().await,
            _ => {}
        }
    }

    /// Moves the open list's cursor by `delta` rows.
    fn move_chooser(&mut self, delta: isize) {
        if let Some((_, chooser)) = &mut self.chooser {
            chooser.move_selection(delta);
        }
    }

    /// Sends what the open list has under its cursor.
    async fn apply_chooser(&mut self) {
        let Some((kind, value)) = self
            .chooser
            .as_ref()
            .and_then(|(kind, chooser)| chooser.selected().map(|value| (*kind, value.to_owned())))
        else {
            // An empty list has nothing under the cursor; Enter means nothing.
            return;
        };

        match kind {
            Chooser::Models => self.switch_model(value).await,
            Chooser::Agents => self.switch_agent(value).await,
        }
    }

    /// One keypress while the command menu is up, and whether it was one of
    /// the menu's own.
    async fn handle_dropdown_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            // Closes the menu and **keeps what was typed**. Upstream deletes
            // the whole `/xyz` here, which is the one sharp edge in its
            // autocomplete that nobody would ask for (**D11**).
            KeyCode::Esc => {
                self.dropdown = None;

                true
            }
            KeyCode::Up => {
                if let Some(dropdown) = &mut self.dropdown {
                    dropdown.move_selection(-1);
                }

                true
            }
            KeyCode::Down => {
                if let Some(dropdown) = &mut self.dropdown {
                    dropdown.move_selection(1);
                }

                true
            }
            KeyCode::Enter if !key.modifiers.intersects(NEWLINE_MODIFIERS) => {
                let action = self.dropdown.as_ref().and_then(Dropdown::selected);
                self.dropdown = None;

                if let Some(action) = action {
                    // The command runs, so the text that named it has done its
                    // job; leaving it would mean the next Enter sent the
                    // command's own name to the model.
                    self.editor.clear();
                    self.run_command(action).await;
                }

                true
            }
            _ => false,
        }
    }

    /// Opens, re-narrows or closes the command menu after the buffer changed.
    fn sync_dropdown(&mut self) {
        let text = self.editor.text();
        if !dropdown::triggered(&text, self.editor.cursor()) {
            self.dropdown = None;
            return;
        }

        match &mut self.dropdown {
            Some(dropdown) => dropdown.refresh(&text),
            None => self.dropdown = Some(Dropdown::new(&text)),
        }
    }

    /// Opens the model list over this provider's catalog entries.
    ///
    /// This provider's only: a switch is same-provider by construction, so a
    /// row for anything else would be a refusal with a nice label on it.
    fn open_models(&mut self) {
        let rows: Vec<list::Row> = catalog::models()
            .filter(|model| model.provider_id == self.provider)
            .map(|model| list::Row {
                value: model.id.to_owned(),
                label: model.id.to_owned(),
                detail: Some(model.name.to_owned()),
                active: model.id == self.model,
            })
            .collect();

        self.chooser = Some((Chooser::Models, ListDialog::new(" models ", rows)));
    }

    /// Opens the agent list over the agents a user may switch to.
    ///
    /// Subagents and hidden agents are left out: the first are the task tool's
    /// to spawn and the second exist precisely so as not to be offered.
    fn open_agents(&mut self) {
        let rows: Vec<list::Row> = self
            .engine
            .agents()
            .map(|registry| {
                registry
                    .agents()
                    .iter()
                    .filter(|agent| agent.selectable())
                    .map(|agent| list::Row {
                        value: agent.name.clone(),
                        label: agent.name.clone(),
                        detail: agent.description.clone(),
                        active: self.agent.as_deref() == Some(agent.name.as_str()),
                    })
                    .collect()
            })
            .unwrap_or_default();

        self.chooser = Some((Chooser::Agents, ListDialog::new(" agents ", rows)));
    }

    /// Moves to the next agent a user may switch to, wrapping.
    ///
    /// Wrapping where every list here clamps, because this is not a cursor in
    /// a list: it is one key pressed repeatedly to get somewhere, and stopping
    /// at the end would mean reaching for the mouse.
    async fn cycle_agent(&mut self) {
        let names: Vec<String> = self
            .engine
            .agents()
            .map(|registry| {
                registry
                    .agents()
                    .iter()
                    .filter(|agent| agent.selectable())
                    .map(|agent| agent.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        if names.is_empty() {
            return;
        }

        let next = self
            .agent
            .as_deref()
            .and_then(|current| names.iter().position(|name| name == current))
            .map_or(0, |index| (index + 1) % names.len());

        self.switch_agent(names[next].clone()).await;
    }

    /// Runs the rest of the session as `name`.
    ///
    /// A refusal — a switch mid-turn, a name the registry does not hold —
    /// lands in the status bar and leaves the list open, so the user still has
    /// what they were choosing from.
    async fn switch_agent(&mut self, name: String) {
        match self
            .engine
            .send(Command::SwitchAgent { name: name.clone() })
            .await
        {
            Ok(()) => {
                self.agent = Some(name);
                self.status.set_agent(self.agent.clone());
                // An agent may name a model of its own, which the engine
                // adopts on the switch; pricing follows the engine's answer
                // rather than what the frontend last remembered.
                self.model = self.engine.model();
                self.chooser = None;
                self.status.set_notice(None);
            }
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// Asks the rest of the session of `model`.
    async fn switch_model(&mut self, model: String) {
        match self
            .engine
            .send(Command::SwitchModel {
                model: model.clone(),
            })
            .await
        {
            Ok(()) => {
                self.model = model;
                self.chooser = None;
                self.status.set_notice(None);
            }
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// Opens the theme picker with the cursor on the theme already in use.
    fn open_themes(&mut self) {
        self.theme_list = Some(ThemeList::new(self.themes.names(), self.themes.active()));
    }

    /// One keypress while the theme picker is open, which owns every key.
    fn handle_theme_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.close_themes(true),
            KeyCode::Up | KeyCode::Char('k') => self.move_theme(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_theme(1),
            KeyCode::Enter => self.close_themes(false),
            _ => {}
        }
    }

    /// Moves the cursor by `delta` rows and applies what it lands on.
    ///
    /// Applying on the way past is the point of the dialog: a theme is
    /// something you recognize on screen, not something you recognize by name.
    fn move_theme(&mut self, delta: isize) {
        let Some(list) = &mut self.theme_list else {
            return;
        };
        list.move_selection(delta);

        if let Some(name) = list.selected().map(str::to_owned) {
            self.apply_theme(&name);
        }
    }

    /// Closes the picker, either putting back the theme it opened on or
    /// keeping — and storing — the one under the cursor.
    ///
    /// A pick that cannot be stored still applies for this run; the notice says
    /// only that it will not survive it.
    fn close_themes(&mut self, revert: bool) {
        let Some(list) = self.theme_list.take() else {
            return;
        };

        if revert {
            self.apply_theme(list.initial());
            return;
        }
        if let Err(refusal) = self.themes.persist() {
            self.status.set_notice(Some(refusal.to_string()));
        }
    }

    /// Installs `name` everywhere the active theme is held.
    ///
    /// Not one assignment: the editor holds the styles it was built with, and
    /// the transcript caches lines with their styles baked in. The first is
    /// repainted here; the second notices at its next frame, because the theme
    /// carries a revision the cache compares against.
    fn apply_theme(&mut self, name: &str) {
        let Some(theme) = self.themes.select(name) else {
            return;
        };

        self.editor.restyle(&theme);
        self.theme = theme;
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
                // A stored session carries the agent and the model it was left
                // on, and the engine restores both; the bar would otherwise go
                // on naming whatever the previous session was using.
                self.agent = self.engine.agent();
                self.status.set_agent(self.agent.clone());
                self.model = self.engine.model();
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

        // Checked before the engine hears about it, as upstream checks it:
        // `exit` on its own is a person leaving, not a question about the word.
        if command::is_bare_exit(&prompt) {
            self.quit = true;
            return;
        }

        match self
            .engine
            .send(Command::SendPrompt {
                text: prompt,
                // The composer has no `@` mentions yet; that lands with the
                // rest of the prompt UI.
                mentions: Vec::new(),
            })
            .await
        {
            Ok(()) => {
                self.editor.clear();
                self.dropdown = None;
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
            *self.totals.cost_usd.get_or_insert(0.0) += catalog::cost(usage, &model).total_usd;
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
        style::{Color, Modifier},
    };
    use tempfile::TempDir;

    use super::{App, Chooser, Dropdown, FRAME, Palette, Permission, permission_reply};
    use crate::{
        component::sessions,
        event::AppEvent,
        theme::{DEFAULT_THEME, Themes},
    };

    fn engine() -> Engine {
        Engine::new(
            Arc::new(FakeProvider::default()),
            fake::MODEL,
            Arc::new(ganja_core::Registry::new(Vec::new())),
            ganja_core::Permissions::default(),
        )
    }

    /// An app over the builtin themes: no disk is read, so a test never
    /// sees the machine's own theme directory or stored pick.
    fn app() -> App {
        App::new(engine(), fake::MODEL, None, Themes::builtin())
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
            Themes::builtin(),
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
            agent: None,
            model: None,
            parent: None,
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

        (
            App::new(engine, fake::MODEL, None, Themes::builtin()),
            events,
        )
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

    /// A screen dump carrying style as well as text.
    ///
    /// [`screen`] reads `.symbol()` only, which is what lets every layout
    /// snapshot survive a change of palette — and what makes it useless for
    /// pinning one. This one emits each row as the runs of cells that share a
    /// style, with the colors and modifiers the backend was actually handed.
    fn styled_screen(terminal: &Terminal<TestBackend>) -> String {
        /// One run of same-styled cells.
        fn run(text: &str, (fg, bg, modifier): (Color, Color, Modifier)) -> String {
            let mut described = format!("{text:?} {fg:?} on {bg:?}");
            if !modifier.is_empty() {
                described.push_str(&format!(" {modifier:?}"));
            }

            format!("[{described}]")
        }

        let buffer = terminal.backend().buffer();
        let area = buffer.area();

        (0..area.height)
            .map(|row| {
                let mut runs = Vec::new();
                let mut text = String::new();
                let mut style: Option<(Color, Color, Modifier)> = None;

                for column in 0..area.width {
                    let cell = &buffer[(column, row)];
                    let cell_style = (cell.fg, cell.bg, cell.modifier);

                    if style != Some(cell_style) {
                        if let Some(previous) = style {
                            runs.push(run(&text, previous));
                        }
                        text.clear();
                        style = Some(cell_style);
                    }
                    text.push_str(cell.symbol());
                }
                if let Some(previous) = style {
                    runs.push(run(&text, previous));
                }

                format!("{row:>2} {}", runs.join(" "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// An app drawing in the builtin theme `name`.
    fn themed_app(name: &str) -> App {
        let mut themes = Themes::builtin();
        themes
            .select(name)
            .unwrap_or_else(|| panic!("{name} should be a builtin theme"));

        App::new(engine(), fake::MODEL, None, themes)
    }

    /// The transcript the per-theme snapshots are taken over: a prompt, a
    /// reply, an edit carrying a diff and a call that was refused — between
    /// them every role the transcript paints.
    fn palette_transcript(app: &mut App) {
        app.chat
            .start_message(Message::user("show me every color you have"));

        let mut reply = Message::assistant("canned");
        reply
            .parts
            .push(Part::text("One edit applied, one command refused."));
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "edit".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"filePath": "theme.rs"}),
                    output: "edited theme.rs".to_owned(),
                    title: "theme.rs".to_owned(),
                    metadata: serde_json::json!({
                        "diff": "@@ -1,2 +1,2 @@\n-let theme = Theme::default();\n+let theme = themes.theme();\n context"
                    }),
                    started: 0,
                    completed: 1,
                },
            },
        });
        reply.parts.push(Part {
            id: PartId::from("prt_2".to_owned()),
            body: PartBody::Tool {
                call_id: "call_2".to_owned(),
                tool: "bash".to_owned(),
                state: ToolState::Error {
                    input: serde_json::json!({"command": "rm -rf /"}),
                    error: "refused: destructive command".to_owned(),
                    started: 0,
                    completed: 1,
                },
            },
        });
        app.chat.start_message(reply);
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

        let mut app = App::new(engine(), MODEL, None, Themes::builtin());
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

        let mut app = App::new(engine(), MODEL, None, Themes::builtin());

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

        let mut app = App::new(engine(), MODEL, None, Themes::builtin());
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
            directories: Vec::new(),
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
                        metadata: serde_json::Value::Null,
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
                        metadata: serde_json::Value::Null,
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
        let mut app = App::new(engine, fake::MODEL, None, Themes::builtin());

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
                    metadata: serde_json::Value::Null,
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

    /// The modal is bounded, so a command can be longer than it can draw. What
    /// the user must never be handed is a call that simply stops mid-word with
    /// `y` sitting under it: here the tail that pipes a download into a shell
    /// is off the bottom, and the dialog says so rather than letting the
    /// visible half read as the whole thing.
    #[test]
    fn snapshot_permission_dialog_with_a_call_too_long_to_fit() {
        let command = format!(
            "cargo test --workspace --all-features --no-fail-fast -- --nocapture {}; \
             curl -fsSL http://ganja.example/install.sh | sh -",
            "--skip live --skip golden --skip pty --skip slow --skip flaky ".repeat(12),
        );
        let mut app = app();
        app.permission = Some(Permission::new(
            PermissionId::from("perm_1".to_owned()),
            "shell".to_owned(),
            command.clone(),
            serde_json::json!({ "command": command }),
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

    /// Live preview is what the dialog is for: the theme under the cursor is
    /// applied as the cursor reaches it, before anything is confirmed.
    #[tokio::test]
    async fn moving_the_cursor_in_the_theme_picker_applies_what_it_lands_on() {
        let mut app = app();

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("control-t is handled");

        assert!(app.theme_list.is_some(), "the picker should be open");
        assert_eq!(
            app.theme.name(),
            DEFAULT_THEME,
            "opening previews nothing: the cursor starts where the user is"
        );

        app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .await
            .expect("j is handled");

        assert_eq!(
            app.theme.name(),
            "terminal",
            "the row below opencode should already be applied"
        );
        assert_eq!(app.themes.active(), "terminal");
        assert!(app.theme_list.is_some(), "moving must not close the picker");
    }

    /// The other half of preview: browsing has to cost nothing.
    #[tokio::test]
    async fn cancelling_the_theme_picker_puts_back_the_theme_it_opened_on() {
        let mut app = app();
        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("control-t is handled");
        app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .await
            .expect("j is handled");
        assert_ne!(app.theme.name(), DEFAULT_THEME, "a preview must have run");

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert!(app.theme_list.is_none(), "escape closes the picker");
        assert_eq!(app.theme.name(), DEFAULT_THEME);
        assert_eq!(app.themes.active(), DEFAULT_THEME);
    }

    #[tokio::test]
    async fn keeping_a_theme_closes_the_picker_and_leaves_it_applied() {
        let mut app = app();
        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("control-t is handled");
        app.handle(key(KeyCode::Char('k'), KeyModifiers::NONE))
            .await
            .expect("k is handled");
        let previewed = app.theme.name().to_owned();

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.theme_list.is_none());
        assert_eq!(app.theme.name(), previewed);
    }

    /// The picker owns every key while it is open, exactly as the sessions one
    /// does — otherwise `j` would be typed into the prompt behind it.
    #[tokio::test]
    async fn keys_while_the_theme_picker_is_open_do_not_reach_the_editor() {
        let mut app = app();
        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("control-t is handled");

        for event in typing("jkx") {
            app.handle(event).await.expect("typing is handled");
        }

        assert!(app.editor.prompt().is_none());
        assert!(app.theme_list.is_some());
    }

    /// The wheel belongs to whatever modal is up, like it does for the other
    /// two: scrolling the transcript out from under a dialog is never what the
    /// notch meant.
    #[tokio::test]
    async fn the_wheel_does_not_reach_the_transcript_while_the_theme_picker_is_open() {
        let mut app = app();
        for index in 0..60 {
            app.chat
                .start_message(Message::user(format!("entry {index}")));
        }
        app.draw(&mut terminal(40, 12)).expect("a frame draws");

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("control-t is handled");
        app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })))
        .await
        .expect("a wheel event is handled");

        assert!(
            app.chat.is_following_tail(),
            "the wheel should be swallowed"
        );
    }

    /// A pick has to outlive the run that made it, and the theme it names has
    /// to be the one the next run opens on.
    #[tokio::test]
    async fn a_kept_theme_is_stored_and_reopened_next_run() {
        let directory = temporary();
        let store = directory.path().join("tui.json");

        let mut themes = Themes::builtin();
        themes.adopt_store(store.clone());
        let mut app = App::new(engine(), fake::MODEL, None, themes);

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("control-t is handled");
        app.handle(key(KeyCode::Char('k'), KeyModifiers::NONE))
            .await
            .expect("k is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        let kept = app.theme.name().to_owned();

        let mut reopened = Themes::builtin();
        reopened.adopt_store(store);
        let next_run = App::new(engine(), fake::MODEL, None, reopened);

        assert_eq!(next_run.theme.name(), kept);
        assert_ne!(kept, DEFAULT_THEME, "the fixture must have changed it");
    }

    /// A cancel must not leave the previewed name behind in the store.
    #[tokio::test]
    async fn a_cancelled_preview_is_never_written_down() {
        let directory = temporary();
        let store = directory.path().join("tui.json");

        let mut themes = Themes::builtin();
        themes.adopt_store(store.clone());
        let mut app = App::new(engine(), fake::MODEL, None, themes);

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("control-t is handled");
        app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .await
            .expect("j is handled");
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert!(
            !store.exists(),
            "nothing was confirmed, so nothing is stored"
        );
    }

    /// A theme switch has to reach the editor and the cached transcript, not
    /// just the panes redrawn from scratch every frame.
    #[tokio::test]
    async fn a_theme_switch_repaints_the_whole_screen() {
        let mut app = themed_app("aura");
        palette_transcript(&mut app);

        let mut screen_buffer = terminal(80, 24);
        app.draw(&mut screen_buffer).expect("a frame draws");
        let (glyphs_before, styles_before) =
            (screen(&screen_buffer), styled_screen(&screen_buffer));

        app.apply_theme("gruvbox");
        app.draw(&mut screen_buffer).expect("a frame draws");

        assert_eq!(
            glyphs_before,
            screen(&screen_buffer),
            "a theme switch must not move a single glyph"
        );
        assert_ne!(
            styles_before,
            styled_screen(&screen_buffer),
            "nothing was repainted"
        );
    }

    #[test]
    fn snapshot_theme_opencode() {
        let mut app = themed_app(DEFAULT_THEME);
        palette_transcript(&mut app);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    #[test]
    fn snapshot_theme_tokyonight() {
        let mut app = themed_app("tokyonight");
        palette_transcript(&mut app);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    #[test]
    fn snapshot_theme_gruvbox() {
        let mut app = themed_app("gruvbox");
        palette_transcript(&mut app);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    #[test]
    fn snapshot_theme_aura() {
        let mut app = themed_app("aura");
        palette_transcript(&mut app);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    /// Opened the way a user opens it, and dumped with styles: this is what
    /// pins the selected row's fill and the panel surface behind the list.
    #[tokio::test]
    async fn snapshot_themes_dialog_open() {
        let mut app = app();
        palette_transcript(&mut app);

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("control-t is handled");

        assert!(
            app.theme_list.is_some(),
            "the picker must be open, or the snapshot is of a bare screen"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
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

    /// An engine carrying the four builtin agents, which is what the agent
    /// list and Tab both read.
    fn agentic_app() -> App {
        let registry = Arc::new(
            ganja_core::AgentRegistry::build(&ganja_core::config::Config::default())
                .expect("the builtin agents resolve"),
        );
        let engine = Engine::new(
            Arc::new(FakeProvider::default()),
            fake::MODEL,
            Arc::new(ganja_core::Registry::new(Vec::new())),
            ganja_core::Permissions::default(),
        )
        .with_agents(registry);

        App::new(engine, fake::MODEL, None, Themes::builtin())
    }

    /// The whole point of `ctrl+p`: it reaches the list of everything else.
    #[tokio::test]
    async fn control_p_opens_the_palette_and_escape_closes_it() {
        let mut app = app();

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        assert!(app.palette.is_some(), "the palette should be open");

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.palette.is_none(), "escape should close it");
    }

    #[tokio::test]
    async fn typing_in_the_palette_filters_it_rather_than_reaching_the_editor() {
        let mut app = app();
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");

        // `j` and `k` are movement in the dialogs with no filter line; here
        // they have to be text, or half the alphabet cannot be searched for.
        for event in typing("jk") {
            app.handle(event).await.expect("typing is handled");
        }

        assert_eq!(
            app.palette.as_ref().map(Palette::filter),
            Some("jk"),
            "the keys should have reached the filter"
        );
        assert!(
            app.editor.prompt().is_none(),
            "and not the editor underneath"
        );
    }

    #[tokio::test]
    async fn the_palette_runs_the_command_under_its_cursor() {
        let mut app = app();
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        for event in typing("themes") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(
            app.palette.is_none(),
            "running a command closes the palette"
        );
        assert!(app.theme_list.is_some(), "and opens what it named");
    }

    /// Closing is not forgetting: a glance at the screen mid-search should not
    /// cost the search.
    #[tokio::test]
    async fn a_reopened_palette_still_holds_what_was_typed_into_it() {
        let mut app = app();
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        for event in typing("mo") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");

        assert_eq!(app.palette.as_ref().map(Palette::filter), Some("mo"));
    }

    #[tokio::test]
    async fn the_palette_reaches_every_command_it_lists() {
        let cases = [
            ("help", (|app: &App| app.help.is_some()) as fn(&App) -> bool),
            ("themes", |app: &App| app.theme_list.is_some()),
            ("models", |app: &App| app.chooser.is_some()),
            ("agents", |app: &App| app.chooser.is_some()),
            ("exit", |app: &App| app.quit),
        ];

        for (typed, opened) in cases {
            let mut app = agentic_app().with_provider("anthropic");
            app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
                .await
                .expect("control-p is handled");
            for event in typing(typed) {
                app.handle(event).await.expect("typing is handled");
            }
            app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .expect("enter is handled");

            assert!(opened(&app), "/{typed} should have done something");
        }
    }

    /// The trigger, at the level the user meets it: a slash that starts the
    /// buffer raises the menu and a slash anywhere else does not.
    #[tokio::test]
    async fn the_command_menu_opens_on_a_leading_slash_and_on_nothing_else() {
        let mut leading = app();
        for event in typing("/") {
            leading.handle(event).await.expect("typing is handled");
        }
        assert!(leading.dropdown.is_some(), "a leading slash should open it");

        let mut midway = app();
        for event in typing("what about /tmp") {
            midway.handle(event).await.expect("typing is handled");
        }
        assert!(
            midway.dropdown.is_none(),
            "a slash mid-sentence is a path, not a command"
        );
    }

    #[tokio::test]
    async fn the_command_menu_closes_once_the_slash_is_backspaced_away() {
        let mut app = app();
        for event in typing("/mo") {
            app.handle(event).await.expect("typing is handled");
        }
        assert!(app.dropdown.is_some());

        for _ in 0..3 {
            app.handle(key(KeyCode::Backspace, KeyModifiers::NONE))
                .await
                .expect("backspace is handled");
        }

        assert!(app.dropdown.is_none(), "an empty buffer is not a command");
    }

    /// **D11**: upstream deletes the typed `/xyz` whenever the menu closes.
    /// Reverting that divergence — wiping the buffer in `handle_dropdown_key`'s
    /// escape arm — fails this test.
    #[tokio::test]
    async fn escape_closes_the_command_menu_and_keeps_what_was_typed() {
        let mut app = app();
        for event in typing("/models") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert!(app.dropdown.is_none(), "the menu should have closed");
        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("/models"),
            "the text must survive, where upstream deletes it"
        );
    }

    #[tokio::test]
    async fn enter_on_the_command_menu_runs_the_command_and_empties_the_editor() {
        let mut app = app();
        for event in typing("/themes") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.theme_list.is_some(), "the command should have run");
        assert!(
            app.editor.prompt().is_none(),
            "the text that named it has done its job"
        );
    }

    #[tokio::test]
    async fn the_arrow_keys_steer_the_command_menu_instead_of_the_transcript() {
        let mut app = app();
        for event in typing("/") {
            app.handle(event).await.expect("typing is handled");
        }
        let first = app
            .dropdown
            .as_ref()
            .and_then(Dropdown::selected)
            .expect("a menu with rows");

        app.handle(key(KeyCode::Down, KeyModifiers::NONE))
            .await
            .expect("down is handled");

        assert_ne!(
            app.dropdown.as_ref().and_then(Dropdown::selected),
            Some(first),
            "down should have moved the cursor"
        );
    }

    #[tokio::test]
    async fn the_model_list_holds_the_running_providers_models_and_marks_the_active_one() {
        let mut served = app().with_provider("anthropic");
        served.open_models();

        let dialog = served.chooser.as_ref().expect("the list should be open");
        assert_eq!(dialog.0, Chooser::Models);
        assert!(
            !dialog.1.is_empty(),
            "anthropic has models in the compiled-in catalog"
        );

        let mut unknown = app().with_provider("a-provider-nothing-ships");
        unknown.open_models();
        assert!(
            unknown
                .chooser
                .as_ref()
                .is_some_and(|(_, list)| list.is_empty()),
            "a provider with no catalog entries has nothing to offer"
        );
    }

    #[tokio::test]
    async fn choosing_a_model_switches_the_session_to_it() {
        let mut app = app().with_provider("anthropic");
        app.open_models();

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.chooser.is_none(), "a switch that took closes the list");
        assert_eq!(
            app.model,
            app.engine.model(),
            "the frontend prices what the engine will ask for"
        );
        assert_ne!(app.model, fake::MODEL, "and it is no longer the old model");
    }

    /// A switch mid-turn is exactly what the engine refuses, and a refusal has
    /// one place to be seen.
    #[tokio::test]
    async fn a_refused_model_switch_reaches_the_status_bar_and_leaves_the_list_up() {
        let (mut app, mut events) = wired().await;
        app = app.with_provider("anthropic");
        for event in typing("a turn to be busy with") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        app.open_models();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(
            app.chooser.is_some(),
            "a refused switch keeps the list the user was choosing from"
        );
        let mut terminal = terminal(120, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("already streaming"),
            "the refusal should be readable:\n{}",
            screen(&terminal)
        );

        // Drain the turn so the test does not leave one streaming.
        pump(&mut app, &mut events, 1).await;
    }

    /// Subagents are the task tool's to spawn; a picker offering them would be
    /// offering a switch the engine refuses.
    #[tokio::test]
    async fn the_agent_list_holds_only_the_agents_a_user_may_switch_to() {
        let mut app = agentic_app();
        app.open_agents();

        let (kind, dialog) = app.chooser.as_ref().expect("the list should be open");
        assert_eq!(*kind, Chooser::Agents);

        let mut listed = Vec::new();
        let mut cursor = dialog.clone();
        cursor.move_selection(-99);
        for _ in 0..8 {
            if let Some(value) = cursor.selected() {
                listed.push(value.to_owned());
            }
            cursor.move_selection(1);
        }
        listed.dedup();

        assert_eq!(
            listed,
            vec!["build".to_owned(), "plan".to_owned()],
            "general and explore are subagents"
        );
    }

    #[tokio::test]
    async fn choosing_an_agent_switches_the_session_and_the_status_bar_says_so() {
        let mut app = agentic_app();
        assert_eq!(
            app.agent.as_deref(),
            Some("build"),
            "sessions start on build"
        );

        app.open_agents();
        app.handle(key(KeyCode::Down, KeyModifiers::NONE))
            .await
            .expect("down is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(app.agent.as_deref(), Some("plan"));
        assert_eq!(app.engine.agent().as_deref(), Some("plan"));

        let mut terminal = terminal(80, 8);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("plan"),
            "the bar should name the agent:\n{}",
            screen(&terminal)
        );
    }

    #[tokio::test]
    async fn tab_on_an_empty_editor_cycles_the_agents_and_wraps() {
        let mut app = agentic_app();
        let mut seen = vec![app.agent.clone()];

        for _ in 0..3 {
            app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
                .await
                .expect("tab is handled");
            seen.push(app.agent.clone());
        }

        assert_eq!(
            seen,
            vec![
                Some("build".to_owned()),
                Some("plan".to_owned()),
                Some("build".to_owned()),
                Some("plan".to_owned()),
            ],
            "two selectable agents, cycled and wrapped"
        );
    }

    #[tokio::test]
    async fn tab_with_something_typed_reaches_the_editor_instead_of_the_agents() {
        let mut app = agentic_app();
        for event in typing("half a thought") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .expect("tab is handled");

        assert_eq!(
            app.agent.as_deref(),
            Some("build"),
            "a Tab inside a sentence is not a request to change agent"
        );
        assert_ne!(
            app.editor.prompt().as_deref(),
            Some("half a thought"),
            "it should have reached the editor"
        );
    }

    /// The one key that means two things, and the gate that decides which.
    #[tokio::test]
    async fn control_d_exits_on_an_empty_editor_and_deletes_forward_otherwise() {
        let mut bare = app();
        bare.handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .await
            .expect("control-d is handled");
        assert!(bare.quit, "an empty editor has nothing to delete");

        let mut typed = app();
        for event in typing("abc") {
            typed.handle(event).await.expect("typing is handled");
        }
        typed
            .handle(key(KeyCode::Home, KeyModifiers::NONE))
            .await
            .expect("home is handled");
        typed
            .handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .await
            .expect("control-d is handled");

        assert!(!typed.quit, "there was something to delete");
        assert_eq!(
            typed.editor.prompt().as_deref(),
            Some("bc"),
            "and it was deleted"
        );
    }

    /// Ganja's own interrupts, which stay unconditional where Ctrl-D is gated.
    #[tokio::test]
    async fn control_c_and_control_q_quit_whatever_is_typed() {
        for code in [KeyCode::Char('c'), KeyCode::Char('q')] {
            let mut app = app();
            for event in typing("a draft nobody asked to keep") {
                app.handle(event).await.expect("typing is handled");
            }

            app.handle(key(code, KeyModifiers::CONTROL))
                .await
                .expect("a quit key is handled");

            assert!(app.quit, "{code:?} with Control should quit");
        }
    }

    #[tokio::test]
    async fn home_and_end_move_in_the_buffer_while_there_is_one_and_in_the_transcript_otherwise() {
        let mut written = app();
        for event in typing("a line to move around in") {
            written.handle(event).await.expect("typing is handled");
        }

        written
            .handle(key(KeyCode::Home, KeyModifiers::NONE))
            .await
            .expect("home is handled");
        assert_eq!(
            written.editor.cursor(),
            (0, 0),
            "home reached the line's start"
        );

        written
            .handle(key(KeyCode::End, KeyModifiers::NONE))
            .await
            .expect("end is handled");
        assert_eq!(
            written.editor.cursor(),
            (0, "a line to move around in".chars().count()),
            "end reached the line's end"
        );

        // Empty editor: the same keys are the transcript's.
        let mut empty = app();
        empty.seed(stored_transcript(80));
        let mut terminal = terminal(40, 12);
        empty.draw(&mut terminal).expect("a frame draws");

        empty
            .handle(key(KeyCode::Home, KeyModifiers::NONE))
            .await
            .expect("home is handled");
        assert!(
            !empty.chat.is_following_tail(),
            "home should have gone to the oldest message"
        );

        empty
            .handle(key(KeyCode::End, KeyModifiers::NONE))
            .await
            .expect("end is handled");
        assert!(
            empty.chat.is_following_tail(),
            "end should have come back to the newest"
        );
    }

    #[tokio::test]
    async fn a_bare_exit_word_submitted_on_its_own_quits() {
        for word in ["exit", "quit", ":q"] {
            let (mut app, _events) = wired().await;
            for event in typing(word) {
                app.handle(event).await.expect("typing is handled");
            }

            app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .expect("enter is handled");

            assert!(app.quit, "{word:?} on its own should quit");
        }
    }

    #[tokio::test]
    async fn an_exit_word_inside_a_sentence_is_a_prompt_like_any_other() {
        let (mut app, mut events) = wired().await;
        for event in typing("does exit mean anything here") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(!app.quit, "the word only quits when it is the whole prompt");
        pump(&mut app, &mut events, 1).await;
    }

    /// The keys the config file moved are the keys that work; the ones it
    /// replaced are not.
    #[tokio::test]
    async fn a_rebound_key_opens_what_it_was_bound_to_and_the_default_stops_working() {
        let configured: std::collections::BTreeMap<String, String> =
            [("palette_open".to_owned(), "f5".to_owned())].into();
        let keys = crate::keybind::Keybinds::from_config(&configured).expect("a legible binding");
        let mut app = app().with_keybinds(keys);

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        assert!(
            app.palette.is_none(),
            "the replaced default should be inert"
        );

        app.handle(key(KeyCode::F(5), KeyModifiers::NONE))
            .await
            .expect("f5 is handled");
        assert!(app.palette.is_some(), "and f5 should open it");
    }

    #[tokio::test]
    async fn the_help_card_opens_from_the_palette_and_closes_on_escape() {
        let mut app = app();
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        for event in typing("help") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert!(app.help.is_some());

        // Every other key is swallowed, like any modal here.
        for event in typing("x") {
            app.handle(event).await.expect("typing is handled");
        }
        assert!(app.help.is_some(), "typing should not close it");
        assert!(app.editor.prompt().is_none(), "nor reach the editor");

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.help.is_none());
    }

    #[tokio::test]
    async fn the_wheel_does_not_reach_the_transcript_while_the_palette_is_open() {
        let mut app = app();
        app.seed(stored_transcript(80));
        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })))
        .await
        .expect("the wheel is handled");

        assert!(
            app.chat.is_following_tail(),
            "a modal claims the wheel as well as the keys"
        );
    }

    #[tokio::test]
    async fn snapshot_palette_open() {
        let mut app = app();
        palette_transcript(&mut app);

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");

        assert!(
            app.palette.is_some(),
            "the palette must be open, or the snapshot is of a bare screen"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_palette_filtered() {
        let mut app = app();
        palette_transcript(&mut app);

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        for event in typing("s") {
            app.handle(event).await.expect("typing is handled");
        }

        assert_eq!(
            app.palette.as_ref().map(Palette::filter),
            Some("s"),
            "the fragment must have landed, or the snapshot is of an unfiltered list"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// The selected row is filled rather than tinted, which is the one part of
    /// the palette a symbol-only dump cannot pin.
    #[tokio::test]
    async fn snapshot_palette_selection_styling() {
        let mut app = themed_app("tokyonight");
        palette_transcript(&mut app);

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        app.handle(key(KeyCode::Down, KeyModifiers::NONE))
            .await
            .expect("down is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_command_menu_open() {
        let mut app = app();
        palette_transcript(&mut app);

        for event in typing("/s") {
            app.handle(event).await.expect("typing is handled");
        }

        assert!(
            app.dropdown.is_some(),
            "the menu must be open, or the snapshot is of a bare screen"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_agents_dialog_open() {
        let mut app = agentic_app();
        palette_transcript(&mut app);
        app.open_agents();

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_help_dialog_open() {
        let mut app = app();
        palette_transcript(&mut app);
        app.run_command(crate::command::Action::Help).await;

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }
}
