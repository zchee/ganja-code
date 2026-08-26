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
#[path = "palette_tests.rs"]
mod tests;
