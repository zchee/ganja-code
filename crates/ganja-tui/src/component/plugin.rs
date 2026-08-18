//! The `/plugin` dialog: one row per installed plugin — enabled state, origin
//! marketplace, and a per-surface summary of what it contributes — with the
//! store's own row-independent actions listed beneath them: Add marketplace,
//! Install plugin, Reload. Enter on a plugin row opens a second step offering
//! Enable/Disable and Remove, the [`crate::component::mcp::Mcp`] dialog's own
//! two-step shape; Enter on Add or Install opens a third, TUI-local step — a
//! free-text line rendered the way the question dialog renders its typed
//! answer, but driven entirely by this frontend: the text never rides an
//! engine `question` round trip, because nothing about naming a marketplace
//! is the model's business (**D474**, declared at
//! [`crate::app::App`]'s reload action).
//!
//! Part of the plugin system that has no upstream opencode counterpart at all
//! (**D472**, `ganja-core/src/plugin.rs`); the dialog's surface is this
//! build's reading of Claude Code's `/plugin` panel, so nothing here cites an
//! upstream file.
//!
//! Running a chosen action and re-reading the store are
//! [`crate::app::App`]'s, not this component's — the same split every other
//! dialog keeps. What this component *does* own is the notice line: an
//! action's outcome (a failed clone's captured git stderr most of all)
//! surfaces here, in the dialog, never as a silent state.
//!
//! One piece of the app's state does cross into the dialog: whether a store
//! action it started is still running. A marketplace add is a `git clone`
//! and runs off the event loop, so the dialog can be keyed while one is in
//! flight — and the two actions that write the store are dimmed and refuse
//! to open while it is, because a second writer would race the first over
//! the same `plugins.json`.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    component::{
        ACTION_HINTS, CHROME, INPUT_HINTS, LIST_HINTS, MARKER, MAX_HEIGHT, MAX_WIDTH, TwoStep,
        body_rows, chat::clip, clamped, first_visible,
    },
    theme::Theme,
};

/// Columns between a row's fixed columns.
const GAP: usize = 2;

/// What is shown when nothing is installed.
const EMPTY: &str = "no plugins installed";

/// What a plugin with no discoverable components summarizes to.
const NO_COMPONENTS: &str = "no components";

/// What a store action is refused with while another one is still running.
///
/// Shown by this dialog when the refusal is one it can make itself — the two
/// store-writing top actions, which do not even open their input step while a
/// clone runs — and by [`crate::app::App`] for the ones only it can catch, so
/// both doors say the same thing.
pub const BUSY: &str =
    "a plugin action is already running \u{b7} wait for it to finish, or Esc to close";

/// The row-independent actions, in the order the list shows them beneath the
/// plugin rows.
const TOP_ACTIONS: [TopAction; 3] = [
    TopAction::AddMarketplace,
    TopAction::Install,
    TopAction::Reload,
];

/// One installed plugin, as the dialog shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// What a chosen action names to the store.
    pub name: String,
    /// Whether the load path reads it.
    pub enabled: bool,
    /// The marketplace it came from.
    pub marketplace: String,
    /// The per-surface summary [`summarize`] computed from the collector's
    /// component lines.
    pub summary: String,
}

/// An action that belongs to the store rather than to any one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopAction {
    /// Add a marketplace from a git URL or a local directory.
    AddMarketplace,
    /// Install one plugin, spelled `<plugin>@<marketplace>`.
    Install,
    /// Re-read the store and rebuild what can rebuild in-session.
    Reload,
}

impl TopAction {
    /// The label the list shows for it.
    #[must_use]
    fn label(self) -> &'static str {
        match self {
            Self::AddMarketplace => "Add marketplace",
            Self::Install => "Install plugin",
            Self::Reload => "Reload",
        }
    }

    /// Whether choosing it writes the store, and therefore whether a running
    /// action has to refuse it.
    #[must_use]
    fn writes_store(self) -> bool {
        match self {
            Self::AddMarketplace | Self::Install => true,
            // The reload re-reads what is there; it never stages, moves or
            // deletes, so a clone in flight is nothing for it to race.
            Self::Reload => false,
        }
    }

    /// What the free-text step asks for, for the two actions that need text.
    #[must_use]
    fn prompt(self) -> &'static str {
        match self {
            Self::AddMarketplace => "Add marketplace: a git URL or a local directory",
            Self::Install => "Install: <plugin>@<marketplace>",
            Self::Reload => "",
        }
    }
}

/// What Enter resolved to — everything the app has to act on. Movement and
/// step changes stay inside the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Mark the named plugin enabled again.
    Enable(String),
    /// Keep the named plugin installed but contributing nothing.
    Disable(String),
    /// Delete the named plugin from the store.
    Remove(String),
    /// Add a marketplace from what was typed.
    AddMarketplace(String),
    /// Install what was typed, spelled `<plugin>@<marketplace>`.
    Install(String),
    /// Re-read the store and rebuild what can rebuild in-session.
    Reload,
}

/// Which of the dialog's steps is on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// Choosing a plugin row or a top-level action.
    List,
    /// Choosing one of the selected plugin's actions, by index.
    Actions(usize),
    /// Typing the text a top-level action needs.
    Input {
        /// Which action the text is for — only the two that take text.
        action: TopAction,
        /// What has been typed so far.
        buffer: String,
    },
}

/// The rows, which one is under the cursor, which step is showing, and the
/// last action's outcome.
#[derive(Clone, Debug)]
pub struct Plugin {
    rows: Vec<Row>,
    /// Index over the plugin rows *and* the top-level actions after them;
    /// always in range, because the top-level actions make the list
    /// non-empty.
    selected: usize,
    step: Step,
    /// What the last action had to say — a failed clone's git stderr, an
    /// enable's confirmation, the reload's honest split, or what the action
    /// running right now is doing.
    notice: Option<String>,
    /// Whether a store action the app started is still running off the loop.
    busy: bool,
}

impl Plugin {
    /// Opens the dialog over `rows`, cursor on the first row — or on the
    /// first top-level action when nothing is installed.
    #[must_use]
    pub fn new(rows: Vec<Row>) -> Self {
        Self {
            rows,
            selected: 0,
            step: Step::List,
            notice: None,
            busy: false,
        }
    }

    /// Replaces the rows with a fresh store read, keeping the cursor where
    /// it was — reclamped, because a Remove shrinks the list under it.
    pub fn refresh(&mut self, rows: Vec<Row>) {
        self.rows = rows;
        self.selected = self.selected.min(self.total_rows().saturating_sub(1));
    }

    /// Sets the notice line the next frame shows.
    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// Says whether a store action the app started is still running, which is
    /// what dims the two store-writing actions and refuses a second one.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    /// Whether such an action is running.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Whether the free-text step currently owns the keyboard.
    #[must_use]
    pub fn is_typing(&self) -> bool {
        matches!(self.step, Step::Input { .. })
    }

    /// Whether the per-plugin action step is the one on screen.
    #[must_use]
    pub fn is_choosing_action(&self) -> bool {
        matches!(self.step, Step::Actions(_))
    }

    /// What has been typed into the free-text step, for the app's tests.
    #[must_use]
    pub fn input(&self) -> Option<&str> {
        match &self.step {
            Step::Input { buffer, .. } => Some(buffer.as_str()),
            Step::List | Step::Actions(_) => None,
        }
    }

    /// The plugin row under the cursor, or [`None`] when the cursor is on a
    /// top-level action.
    #[must_use]
    pub fn selected_plugin(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Every position the list-step cursor can land on.
    fn total_rows(&self) -> usize {
        self.rows.len() + TOP_ACTIONS.len()
    }

    /// The per-plugin actions the selected row offers: the toggle that
    /// applies to its state, then Remove.
    fn plugin_actions(row: &Row) -> [&'static str; 2] {
        [if row.enabled { "Disable" } else { "Enable" }, "Remove"]
    }

    /// Moves whichever list is showing by `delta` rows. The free-text step
    /// has no rows to move.
    pub fn move_selection(&mut self, delta: isize) {
        match &self.step {
            Step::List => self.selected = clamped(self.selected, delta, self.total_rows()),
            Step::Actions(option) => {
                let count = self
                    .selected_plugin()
                    .map_or(0, |row| Self::plugin_actions(row).len());
                self.step = Step::Actions(clamped(*option, delta, count));
            }
            Step::Input { .. } => {}
        }
    }

    /// Adds `character` while the free-text step owns the keyboard.
    pub fn push(&mut self, character: char) {
        if let Step::Input { buffer, .. } = &mut self.step {
            buffer.push(character);
        }
    }

    /// Takes the last character back off while the free-text step owns the
    /// keyboard.
    pub fn backspace(&mut self) {
        if let Step::Input { buffer, .. } = &mut self.step {
            buffer.pop();
        }
    }

    /// Esc: leaves the free-text step for the list, keeping the dialog open
    /// and sending nothing anywhere — the typed text is abandoned, never
    /// submitted. Answers whether the key was consumed; `false` means the
    /// dialog itself should close, which is what Esc means on the other two
    /// steps, exactly as it does in the `/mcp` dialog.
    pub fn cancel(&mut self) -> bool {
        if self.is_typing() {
            self.step = Step::List;
            return true;
        }

        false
    }

    /// Enter, wherever the dialog is: steps forward where a step is what
    /// Enter means, and answers with the [`Effect`] the app has to run where
    /// a decision was made.
    ///
    /// While a store action is running ([`Plugin::set_busy`]), the two
    /// actions that write the store answer with [`BUSY`] on the notice line
    /// instead of opening their input step: a second clone would race the
    /// first, and letting a person type a path that is going to be refused
    /// anyway is worse than saying so at the keypress.
    pub fn submit(&mut self) -> Option<Effect> {
        match &mut self.step {
            Step::List => {
                if self.selected < self.rows.len() {
                    self.step = Step::Actions(0);
                    return None;
                }
                let action = TOP_ACTIONS[self.selected - self.rows.len()];
                if self.busy && action.writes_store() {
                    self.notice = Some(BUSY.to_owned());
                    return None;
                }
                match action {
                    TopAction::Reload => Some(Effect::Reload),
                    TopAction::AddMarketplace | TopAction::Install => {
                        self.step = Step::Input {
                            action,
                            buffer: String::new(),
                        };
                        None
                    }
                }
            }
            Step::Actions(option) => {
                let option = *option;
                let row = self.selected_plugin()?;
                let effect = match Self::plugin_actions(row)[option] {
                    "Enable" => Effect::Enable(row.name.clone()),
                    "Disable" => Effect::Disable(row.name.clone()),
                    _ => Effect::Remove(row.name.clone()),
                };
                // Back to the list so the outcome shows up on the row the
                // app's refresh repaints — the `/mcp` dialog's own posture.
                self.step = Step::List;

                Some(effect)
            }
            Step::Input { action, buffer } => {
                let typed = buffer.trim().to_owned();
                if typed.is_empty() {
                    // Nothing to submit is not a decision; the step stays.
                    return None;
                }
                let effect = match action {
                    TopAction::AddMarketplace => Effect::AddMarketplace(typed),
                    TopAction::Install => Effect::Install(typed),
                    TopAction::Reload => unreachable!("Reload never opens the input step"),
                };
                self.step = Step::List;

                Some(effect)
            }
        }
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let available = area.height.saturating_sub(2).clamp(1, MAX_HEIGHT);
        let inner_width = usize::from(width).saturating_sub(2);
        // The notice takes a row of the list's budget when it has something
        // to say.
        let rows = body_rows(available, CHROME + usize::from(self.notice.is_some()));

        let mut lines = match &self.step {
            Step::List => self.list_rows(inner_width, rows, theme),
            Step::Actions(option) => self.action_rows(inner_width, *option, theme),
            Step::Input { action, buffer } => Self::input_rows(inner_width, *action, buffer, theme),
        };
        if let Some(notice) = &self.notice {
            let first = notice.lines().next().unwrap_or(notice).trim();
            lines.push(Line::styled(clip(first, inner_width), theme.dim));
        }
        let hints = match &self.step {
            Step::List => LIST_HINTS,
            Step::Actions(_) => ACTION_HINTS,
            Step::Input { .. } => INPUT_HINTS,
        };
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(hints, inner_width), theme.dim));

        // The list step takes the screenful it was given; the other two are a
        // handful of rows and never grow, so they take exactly what they
        // need — the `/mcp` dialog's own two-height scheme.
        let height = match &self.step {
            Step::List => available,
            Step::Actions(_) | Step::Input { .. } => u16::try_from(lines.len().saturating_add(2))
                .unwrap_or(available)
                .min(available),
        };
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" plugin "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// The list step: one line per installed plugin, then the top-level
    /// actions.
    fn list_rows(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let name_width = self
            .rows
            .iter()
            .map(|row| row.name.width())
            .max()
            .unwrap_or(0);
        let market_width = self
            .rows
            .iter()
            .map(|row| row.marketplace.width())
            .max()
            .unwrap_or(0);

        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.rows.is_empty() {
            lines.push(Line::styled(clip(EMPTY, width), theme.dim));
        }
        for (index, row) in self.rows.iter().enumerate() {
            let state = if row.enabled { "Enabled" } else { "Disabled" };
            let head = format!(
                "{marker}{name:<name_width$}  {state:<8}  {market:<market_width$}",
                marker = if index == self.selected { MARKER } else { "  " },
                name = row.name,
                market = row.marketplace,
            );
            let summary_width = width.saturating_sub(head.width() + GAP).max(1);
            let line = clip(
                &format!(
                    "{head}{gap}{summary}",
                    gap = " ".repeat(GAP),
                    summary = clip(&row.summary, summary_width),
                ),
                width,
            );
            lines.push(Line::styled(
                format!("{line:<width$}"),
                if index == self.selected {
                    theme.selection
                } else {
                    theme.fg
                },
            ));
        }
        lines.push(Line::raw(""));
        for (offset, action) in TOP_ACTIONS.iter().enumerate() {
            let index = self.rows.len() + offset;
            let line = format!(
                "{marker}{label}",
                marker = if index == self.selected { MARKER } else { "  " },
                label = action.label(),
            );
            // A store action in flight dims the two that would race it, so
            // the refusal on the notice line is not the first a person hears
            // of it.
            let style = if self.busy && action.writes_store() {
                theme.dim
            } else if index == self.selected {
                theme.accent
            } else {
                theme.fg
            };
            lines.push(Line::styled(clip(&line, width), style));
        }

        // The scroll window slides over the composed lines: the selected
        // *line* — a plugin row directly, a top-level action past the blank
        // separator (and past the empty-store line when that is what is
        // above it) — is what has to stay visible, the same first-row answer
        // every scrolling list here gives.
        let selected_line = if self.selected < self.rows.len() {
            self.selected
        } else {
            self.rows.len().max(1) + 1 + (self.selected - self.rows.len())
        };
        let first = first_visible(selected_line, rows);

        lines.into_iter().skip(first).take(rows).collect()
    }

    /// The per-plugin action step: which plugin it is about, then Enable or
    /// Disable, then Remove.
    fn action_rows(&self, width: usize, option: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(row) = self.selected_plugin() else {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        };

        let mut lines = vec![Line::styled(clip(&row.name, width), theme.fg)];
        for (index, label) in Self::plugin_actions(row).iter().enumerate() {
            let line = format!(
                "{marker}{label}",
                marker = if index == option { MARKER } else { "  " },
            );
            lines.push(Line::styled(
                clip(&line, width),
                if index == option {
                    theme.accent
                } else {
                    theme.fg
                },
            ));
        }

        lines
    }

    /// The free-text step: what is being asked for, and the line being
    /// typed — the question dialog's own editing rendering, block cursor
    /// included, with no engine on the other end of it.
    fn input_rows(
        width: usize,
        action: TopAction,
        buffer: &str,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        vec![
            Line::styled(clip(action.prompt(), width), theme.fg),
            Line::styled(
                clip(
                    &format!("{MARKER}{buffer}\u{2588}"),
                    width.saturating_sub(1).max(1),
                ),
                theme.selection,
            ),
        ]
    }
}

/// The shared key surface, every method the inherent one: what the trait
/// exists for is the one driver in `app.rs`, and what stays this dialog's own
/// is everything `submit` decides.
impl TwoStep for Plugin {
    type Effect = Effect;

    fn is_typing(&self) -> bool {
        Self::is_typing(self)
    }

    fn cancel(&mut self) -> bool {
        Self::cancel(self)
    }

    fn backspace(&mut self) {
        Self::backspace(self);
    }

    fn push(&mut self, character: char) {
        Self::push(self, character);
    }

    fn move_selection(&mut self, delta: isize) {
        Self::move_selection(self, delta);
    }

    fn submit(&mut self) -> Option<Effect> {
        Self::submit(self)
    }
}

/// The per-surface summary a plugin row carries, computed from the component
/// lines `ganja_core::plugin::Listing` holds — the collector's own account,
/// which is what keeps this dialog and `ganja plugin list` two views of one
/// answer.
#[must_use]
pub fn summarize(components: &[String]) -> String {
    let mut hooks = 0usize;
    let mut mcp = 0usize;
    let mut skills = false;
    let mut agents = 0usize;
    let mut lsp = 0usize;
    for line in components {
        if line.starts_with("hook ") {
            hooks += 1;
        } else if line.starts_with("mcp ") {
            mcp += 1;
        } else if line == "skills" {
            skills = true;
        } else if line.starts_with("agent ") {
            agents += 1;
        } else if line.starts_with("lsp ") {
            lsp += 1;
        }
    }

    let mut parts = Vec::new();
    if hooks > 0 {
        parts.push(format!("{hooks} hook{}", if hooks == 1 { "" } else { "s" }));
    }
    if mcp > 0 {
        parts.push(format!("{mcp} mcp"));
    }
    if skills {
        parts.push("skills".to_owned());
    }
    if agents > 0 {
        parts.push(format!(
            "{agents} agent{}",
            if agents == 1 { "" } else { "s" }
        ));
    }
    if lsp > 0 {
        parts.push(format!("{lsp} lsp"));
    }

    if parts.is_empty() {
        NO_COMPONENTS.to_owned()
    } else {
        parts.join(" \u{b7} ")
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Effect, Plugin, Row, summarize};
    use crate::theme::Theme;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 76,
        height: 20,
    };

    fn row(name: &str, enabled: bool) -> Row {
        Row {
            name: name.to_owned(),
            enabled,
            marketplace: "company-tools".to_owned(),
            summary: "1 hook \u{b7} skills".to_owned(),
        }
    }

    fn dialog() -> Plugin {
        Plugin::new(vec![row("formatter", true), row("deployer", false)])
    }

    fn rendered(dialog: &Plugin, area: Rect) -> String {
        let mut buffer = Buffer::empty(area);
        dialog.render(area, &mut buffer, &Theme::default());

        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_plugin_lists_with_its_state_marketplace_and_summary() {
        let screen = rendered(&dialog(), AREA);

        assert!(screen.contains("formatter"), "got:\n{screen}");
        assert!(screen.contains("Enabled"), "got:\n{screen}");
        assert!(screen.contains("deployer"), "got:\n{screen}");
        assert!(screen.contains("Disabled"), "got:\n{screen}");
        assert!(screen.contains("company-tools"), "got:\n{screen}");
        assert!(screen.contains("1 hook \u{b7} skills"), "got:\n{screen}");
    }

    #[test]
    fn the_top_level_actions_are_always_offered_even_over_an_empty_store() {
        for dialog in [dialog(), Plugin::new(Vec::new())] {
            let screen = rendered(&dialog, AREA);

            assert!(screen.contains("Add marketplace"), "got:\n{screen}");
            assert!(screen.contains("Install plugin"), "got:\n{screen}");
            assert!(screen.contains("Reload"), "got:\n{screen}");
        }
        assert!(
            rendered(&Plugin::new(Vec::new()), AREA).contains("no plugins installed"),
            "an empty store says so"
        );
    }

    /// Enter on an enabled plugin offers Disable; on a disabled one, Enable —
    /// the toggle that applies, never both.
    #[test]
    fn enter_on_a_plugin_row_offers_the_applicable_toggle_and_remove() {
        let mut dialog = dialog();

        assert_eq!(dialog.submit(), None, "Enter opens the action step");
        assert!(dialog.is_choosing_action());
        let screen = rendered(&dialog, AREA);
        assert!(screen.contains("Disable"), "got:\n{screen}");
        assert!(screen.contains("Remove"), "got:\n{screen}");
        assert!(!screen.contains("> Enable\n"), "got:\n{screen}");

        assert_eq!(
            dialog.submit(),
            Some(Effect::Disable("formatter".to_owned())),
            "Enter on the toggle answers with it"
        );
        assert!(
            !dialog.is_choosing_action(),
            "running an action returns to the list"
        );
    }

    #[test]
    fn enter_on_a_disabled_plugin_offers_enable() {
        let mut dialog = dialog();
        dialog.move_selection(1);

        assert_eq!(dialog.submit(), None);
        assert_eq!(dialog.submit(), Some(Effect::Enable("deployer".to_owned())));
    }

    #[test]
    fn remove_is_the_action_after_the_toggle() {
        let mut dialog = dialog();
        dialog.submit();
        dialog.move_selection(1);

        assert_eq!(
            dialog.submit(),
            Some(Effect::Remove("formatter".to_owned()))
        );
    }

    /// The free-text step is TUI-local: Enter with text answers with an
    /// [`Effect`] for the app to run, and nothing here has an engine to ask.
    #[test]
    fn the_add_input_takes_text_and_submits_it_on_enter() {
        let mut dialog = dialog();
        dialog.move_selection(2);
        assert_eq!(dialog.submit(), None, "Add marketplace opens the input");
        assert!(dialog.is_typing());

        for character in "/tmp/market".chars() {
            dialog.push(character);
        }
        dialog.backspace();
        assert_eq!(dialog.input(), Some("/tmp/marke"));

        assert_eq!(
            dialog.submit(),
            Some(Effect::AddMarketplace("/tmp/marke".to_owned()))
        );
        assert!(!dialog.is_typing(), "a submit leaves the input step");
    }

    #[test]
    fn the_install_input_spells_the_claude_spec_spelling() {
        let mut dialog = Plugin::new(Vec::new());
        dialog.move_selection(1);
        assert_eq!(dialog.submit(), None);
        let screen = rendered(&dialog, AREA);
        assert!(
            screen.contains("<plugin>@<marketplace>"),
            "the input says what spelling it wants:\n{screen}"
        );

        for character in "formatter@company-tools".chars() {
            dialog.push(character);
        }
        assert_eq!(
            dialog.submit(),
            Some(Effect::Install("formatter@company-tools".to_owned()))
        );
    }

    /// Esc on the input step cancels the edit and keeps the dialog open;
    /// on the other steps it is not consumed, so the app closes the dialog —
    /// the `/mcp` dialog's own Esc.
    #[test]
    fn esc_cancels_the_input_step_and_closes_from_anywhere_else() {
        let mut dialog = dialog();
        dialog.move_selection(2);
        dialog.submit();
        dialog.push('x');

        assert!(dialog.cancel(), "the input step consumes the Esc");
        assert!(!dialog.is_typing());
        assert!(!dialog.cancel(), "the list step leaves Esc to the app");

        dialog.move_selection(-2);
        dialog.submit();
        assert!(dialog.is_choosing_action());
        assert!(
            !dialog.cancel(),
            "the action step leaves Esc to the app too"
        );
    }

    #[test]
    fn an_empty_input_submits_nothing_and_stays_where_it_is() {
        let mut dialog = Plugin::new(Vec::new());
        dialog.submit();
        assert!(dialog.is_typing());

        assert_eq!(dialog.submit(), None);
        assert!(dialog.is_typing(), "nothing typed is not a decision");
    }

    #[test]
    fn enter_on_reload_answers_with_the_reload_effect() {
        let mut dialog = dialog();
        dialog.move_selection(4);

        assert_eq!(dialog.submit(), Some(Effect::Reload));
    }

    #[test]
    fn the_notice_line_surfaces_an_actions_outcome() {
        let mut dialog = dialog();
        dialog.set_notice("git clone failed: repository not found");

        assert!(
            rendered(&dialog, AREA).contains("git clone failed: repository not found"),
            "got:\n{}",
            rendered(&dialog, AREA)
        );
    }

    /// `zus`: while the app has a store action running off the loop, the two
    /// actions that would write the store are refused where they are chosen —
    /// the input step never opens — and the notice line says why. Reload is
    /// untouched: it writes nothing there is to race.
    #[test]
    fn a_running_store_action_refuses_the_two_that_would_race_it() {
        let mut dialog = dialog();
        dialog.set_busy(true);
        assert!(dialog.is_busy());

        for offset in 0..2 {
            let mut dialog = dialog.clone();
            dialog.move_selection(2 + offset);

            assert_eq!(dialog.submit(), None, "the store action is refused");
            assert!(
                !dialog.is_typing(),
                "and the input step it would have opened stays shut"
            );
            let screen = rendered(&dialog, AREA);
            assert!(screen.contains("already running"), "got:\n{screen}");
        }

        let mut reload = dialog.clone();
        reload.move_selection(4);
        assert_eq!(
            reload.submit(),
            Some(Effect::Reload),
            "the reload races nothing and stays live"
        );
    }

    /// The refusal lasts exactly as long as the action does: the app clears
    /// the flag when it reaps the task, and the input opens again.
    #[test]
    fn clearing_the_running_flag_opens_the_add_again() {
        let mut dialog = dialog();
        dialog.set_busy(true);
        dialog.move_selection(2);
        assert_eq!(dialog.submit(), None);

        dialog.set_busy(false);
        assert_eq!(dialog.submit(), None, "Add marketplace opens the input");
        assert!(dialog.is_typing());
        dialog.push('x');
        assert_eq!(
            dialog.submit(),
            Some(Effect::AddMarketplace("x".to_owned()))
        );
    }

    #[test]
    fn a_refresh_reclamps_the_cursor_after_a_remove_shrinks_the_list() {
        let mut dialog = dialog();
        dialog.move_selection(4);

        dialog.refresh(Vec::new());
        // Three top-level rows are all that is left; the cursor holds inside
        // them rather than pointing past the end.
        assert_eq!(dialog.submit(), Some(Effect::Reload));
    }

    #[test]
    fn the_summary_counts_components_by_surface() {
        let components = [
            "hook PreToolUse".to_owned(),
            "hook Stop".to_owned(),
            "mcp db".to_owned(),
            "skills".to_owned(),
            "agent reviewer".to_owned(),
            "lsp go".to_owned(),
        ];

        assert_eq!(
            summarize(&components),
            "2 hooks \u{b7} 1 mcp \u{b7} skills \u{b7} 1 agent \u{b7} 1 lsp"
        );
        assert_eq!(summarize(&[]), "no components");
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        for (width, height) in [(1, 1), (3, 2), (8, 4)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);

            dialog().render(area, &mut buffer, &Theme::default());
        }
    }

    #[test]
    fn a_zero_area_draws_nothing_and_does_not_panic() {
        let screen = rendered(&dialog(), Rect::new(0, 0, 0, 0));

        assert!(screen.is_empty(), "a zero area has no cell: {screen}");
    }
}
