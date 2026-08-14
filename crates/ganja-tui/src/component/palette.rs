//! The command palette: a centered modal listing every command, narrowed by
//! whatever is typed into its filter line.
//!
//! Spec: upstream `packages/tui/src/component/command-palette.tsx` over
//! `ui/dialog-select.tsx`. Three behaviors are ported deliberately:
//!
//! - commands are **grouped by category**, with the heading in the accent
//!   colour, because a flat list of six is a list and a flat list of sixty is
//!   a wall;
//! - a **suggested** block is pinned above the groups while the filter is
//!   empty, and vanishes the moment anything is typed — upstream lists those
//!   commands twice on purpose, once where they belong and once where a hand
//!   reaching for them lands;
//! - closing keeps the filter, so reopening the palette after a glance at the
//!   screen does not mean typing the fragment again.
//!
//! The dialog owns the filter and which row is under the cursor. Running a
//! command is [`crate::app::App`]'s, like every other dialog here.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    command::{self, Action, Entry, Surface},
    component::{chat::clip, first_visible, modal},
    keybind::Keybinds,
    theme::Theme,
};

/// What marks the row the cursor is on, and what pads every other row.
const MARKER: &str = "> ";

/// Rows the dialog spends on something other than the list: the filter line, a
/// blank line and the key reminders.
const CHROME: usize = 3;

/// The keys the dialog answers to, shown along its bottom edge.
const HINTS: &str = "[type] filter   [up/down] move   [Enter] run   [Esc] close";

/// What the filter line says while nothing has been typed.
const PLACEHOLDER: &str = "search commands";

/// What is shown when the fragment matches nothing.
const EMPTY: &str = "no commands match";

/// The heading over the block pinned above the groups.
const SUGGESTED: &str = "suggested";

/// Widest the modal grows. A command row is a short name, a short title and a
/// key; a box the width of the screen would only spread them apart.
const MAX_WIDTH: u16 = 64;

/// Tallest the modal grows, whatever the terminal offers.
const MAX_HEIGHT: u16 = 20;

/// Gap between the columns of a row.
const GAP: usize = 2;

/// One line of the list: either a group heading or a command.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    /// A category name, or `suggested`.
    Heading(&'static str),
    /// A command, with the key that reaches it where it has one.
    Command {
        /// The command this row runs.
        entry: &'static Entry,
        /// How its binding is spelled, or [`None`] when it has none.
        hint: Option<String>,
    },
}

impl Row {
    /// The command this row runs, or [`None`] for a heading.
    fn entry(&self) -> Option<&'static Entry> {
        match self {
            Self::Heading(_) => None,
            Self::Command { entry, .. } => Some(entry),
        }
    }
}

/// The commands, the fragment narrowing them, and which one is under the
/// cursor.
#[derive(Clone, Debug)]
pub struct Palette {
    /// What has been typed into the filter line.
    filter: String,
    /// The keys each command is reached by, resolved once when the palette
    /// opens: the bindings cannot change while it is up.
    keys: Keybinds,
    /// Headings and commands, in the order they are drawn.
    rows: Vec<Row>,
    /// Index into [`Palette::rows`]; always on a command while there is one.
    selected: usize,
}

impl Palette {
    /// Opens the palette over every command, with `keys` supplying the hints.
    #[must_use]
    pub fn new(keys: Keybinds) -> Self {
        let mut palette = Self {
            filter: String::new(),
            keys,
            rows: Vec::new(),
            selected: 0,
        };
        palette.refresh();

        palette
    }

    /// Reopens a palette that was closed, keeping what had been typed.
    ///
    /// Upstream's dialogs are recreated empty; keeping the fragment is the
    /// less destructive half of the same choice **D11** makes for the
    /// dropdown, and it costs nothing — the filter is one keystroke from
    /// empty either way (deviation: palette-filter-survives-close).
    #[must_use]
    pub fn reopened(keys: Keybinds, filter: String) -> Self {
        let mut palette = Self {
            filter,
            keys,
            rows: Vec::new(),
            selected: 0,
        };
        palette.refresh();

        palette
    }

    /// What has been typed into the filter line.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Adds `character` to the filter.
    pub fn push(&mut self, character: char) {
        self.filter.push(character);
        self.refresh();
    }

    /// Takes the last character back off the filter.
    pub fn backspace(&mut self) {
        self.filter.pop();
        self.refresh();
    }

    /// The command under the cursor, or [`None`] when nothing matches.
    #[must_use]
    pub fn selected(&self) -> Option<Action> {
        self.rows
            .get(self.selected)
            .and_then(Row::entry)
            .map(|entry| entry.action)
    }

    /// Moves the cursor by `delta` commands, stepping over headings.
    ///
    /// Clamped rather than wrapped, like every other list here: running off
    /// one end and landing on the other is never what the keypress meant.
    pub fn move_selection(&mut self, delta: isize) {
        let commands: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.entry().is_some())
            .map(|(index, _)| index)
            .collect();
        let Some(last) = commands.len().checked_sub(1) else {
            return;
        };

        let current = commands
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let moved = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta.unsigned_abs())
        };

        self.selected = commands[moved.min(last)];
    }

    /// Rebuilds the rows for the current filter, putting the cursor on the
    /// first command.
    fn refresh(&mut self) {
        let matched = command::matches(&self.filter, Surface::Palette);
        let mut rows = Vec::new();

        // The pinned block, and only while nothing has been typed: upstream
        // drops it as soon as the filter says something, because a fragment is
        // already a statement about what the user is looking for.
        if self.filter.is_empty() {
            let suggested: Vec<&Entry> = matched
                .iter()
                .copied()
                .filter(|entry| entry.suggested)
                .collect();
            if !suggested.is_empty() {
                rows.push(Row::Heading(SUGGESTED));
                rows.extend(suggested.into_iter().map(|entry| self.row(entry)));
            }
        }

        let mut group = None;
        for entry in matched {
            if group != Some(entry.category) {
                rows.push(Row::Heading(entry.category.label()));
                group = Some(entry.category);
            }
            rows.push(self.row(entry));
        }

        self.rows = rows;
        self.selected = self
            .rows
            .iter()
            .position(|row| row.entry().is_some())
            .unwrap_or(0);
    }

    /// One command row, carrying whatever key reaches it.
    fn row(&self, entry: &'static Entry) -> Row {
        Row::Command {
            entry,
            hint: entry
                .action
                .keybind()
                .and_then(|action| self.keys.hint(action)),
        }
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let (popup, inner_width, rows) = modal(area, MAX_WIDTH, MAX_HEIGHT, CHROME);

        Clear.render(popup, buffer);

        let mut lines = vec![self.filter_line(inner_width, theme)];
        lines.extend(self.lines(inner_width, rows, theme));
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" commands "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// The line the fragment is typed on.
    fn filter_line(&self, width: usize, theme: &Theme) -> Line<'static> {
        if self.filter.is_empty() {
            return Line::styled(clip(PLACEHOLDER, width), theme.dim);
        }

        Line::styled(clip(&self.filter, width), theme.accent)
    }

    /// The visible slice of headings and commands.
    fn lines(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.rows.is_empty() {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        }

        let first = first_visible(self.selected, rows);
        // Names padded to the widest, so the titles beside them sit in one
        // column instead of stepping in and out per row.
        let name_width = self
            .rows
            .iter()
            .filter_map(Row::entry)
            .map(|entry| entry.slash().width())
            .max()
            .unwrap_or(0);

        self.rows
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(index, row)| match row {
                Row::Heading(label) => Line::styled(clip(label, width), theme.accent),
                Row::Command { entry, hint } => {
                    self.command_line(index, entry, hint.as_deref(), name_width, width, theme)
                }
            })
            .collect()
    }

    /// One command: marker, name, title, and the key that reaches it.
    fn command_line(
        &self,
        index: usize,
        entry: &Entry,
        hint: Option<&str>,
        name_width: usize,
        width: usize,
        theme: &Theme,
    ) -> Line<'static> {
        let hint = hint.unwrap_or_default();
        let head = format!(
            "{marker}{slash:<name_width$}",
            marker = if index == self.selected { MARKER } else { "  " },
            slash = entry.slash(),
        );
        // The key sits hard right, where the eye finds it without reading the
        // title; the title takes whatever is left.
        let title_width = width
            .saturating_sub(head.width() + hint.width() + GAP * 2)
            .max(1);
        let row = format!(
            "{head}{gap}{title:<title_width$}{gap}{hint}",
            gap = " ".repeat(GAP),
            title = clip(entry.title, title_width),
        );

        // Filled rather than tinted, so the contrast rule has something to
        // answer — the same choice the theme picker makes.
        Line::styled(
            format!("{row:<width$}"),
            if index == self.selected {
                theme.selection
            } else {
                theme.fg
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Palette, Row, SUGGESTED};
    use crate::{command::Action, keybind::Keybinds, theme::Theme};

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 64,
        height: 20,
    };

    fn palette() -> Palette {
        Palette::new(Keybinds::defaults())
    }

    fn typing(palette: &mut Palette, text: &str) {
        for character in text.chars() {
            palette.push(character);
        }
    }

    fn rendered(palette: &Palette) -> String {
        let mut buffer = Buffer::empty(AREA);
        palette.render(AREA, &mut buffer, &Theme::default());

        (0..AREA.height)
            .map(|row| {
                (0..AREA.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_fresh_palette_starts_on_a_command_rather_than_a_heading() {
        assert!(palette().selected().is_some());
    }

    #[test]
    fn the_suggested_block_is_pinned_while_nothing_is_typed_and_gone_once_something_is() {
        let mut palette = palette();
        assert!(
            palette.rows.contains(&Row::Heading(SUGGESTED)),
            "an empty filter should pin the suggested commands"
        );

        typing(&mut palette, "e");
        assert!(
            !palette.rows.contains(&Row::Heading(SUGGESTED)),
            "a fragment should drop the pinned block"
        );
    }

    #[test]
    fn typing_narrows_the_list_and_backspace_widens_it_again() {
        let mut palette = palette();
        let all = palette.rows.len();

        typing(&mut palette, "themes");
        assert!(palette.rows.len() < all, "the list should have narrowed");
        assert_eq!(palette.selected(), Some(Action::Themes));

        for _ in 0.."themes".len() {
            palette.backspace();
        }
        assert_eq!(palette.rows.len(), all, "backspacing should widen it back");
    }

    #[test]
    fn moving_the_cursor_steps_over_the_headings() {
        let mut palette = palette();
        let mut seen = Vec::new();

        for _ in 0..12 {
            seen.push(palette.selected());
            palette.move_selection(1);
        }

        assert!(
            seen.iter().all(Option::is_some),
            "the cursor should never rest on a heading: {seen:?}"
        );
    }

    #[test]
    fn the_cursor_clamps_at_both_ends() {
        let mut palette = palette();
        palette.move_selection(-5);
        let first = palette.selected();

        palette.move_selection(500);
        let last = palette.selected();
        palette.move_selection(500);

        assert_eq!(palette.selected(), last, "past the end should stay put");
        assert_ne!(first, last, "the list should have more than one command");
    }

    #[test]
    fn a_fragment_nothing_matches_says_so_instead_of_drawing_an_empty_box() {
        let mut palette = palette();
        typing(&mut palette, "zzzz");

        assert_eq!(palette.selected(), None);
        assert!(
            rendered(&palette).contains("no commands match"),
            "{}",
            rendered(&palette)
        );
    }

    #[test]
    fn a_reopened_palette_keeps_the_fragment_it_was_closed_on() {
        let mut palette = palette();
        typing(&mut palette, "the");

        let reopened = Palette::reopened(Keybinds::defaults(), palette.filter().to_owned());

        assert_eq!(reopened.filter(), "the");
        assert_eq!(reopened.selected(), Some(Action::Themes));
    }

    #[test]
    fn a_command_with_a_binding_shows_it_and_one_without_shows_nothing() {
        let screen = rendered(&palette());

        assert!(screen.contains("ctrl+s"), "/sessions has a key:\n{screen}");
        let models = screen
            .lines()
            .find(|line| line.contains("/models"))
            .unwrap_or_default();
        assert!(
            !models.contains("ctrl"),
            "/models has no key of its own: {models}"
        );
    }

    #[test]
    fn the_filter_line_shows_a_placeholder_until_something_is_typed() {
        let mut palette = palette();
        assert!(rendered(&palette).contains("search commands"));

        typing(&mut palette, "mo");
        let screen = rendered(&palette);
        assert!(!screen.contains("search commands"), "{screen}");
        assert!(screen.contains("mo"), "{screen}");
    }

    #[test]
    fn a_one_column_area_draws_without_panicking() {
        for (width, height) in [(1, 1), (2, 3), (5, 2), (64, 3)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);

            palette().render(area, &mut buffer, &Theme::default());
        }
    }
}
