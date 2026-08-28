//! A centered modal for choosing one of a labelled set of things.
//!
//! Spec: upstream `packages/tui/src/component/dialog-model.tsx` and
//! `dialog-agent.tsx`, which are the same dialog over two lists. Ganja's are
//! too, so they are one component: what differs between "switch model" and
//! "switch agent" is the rows and the command they send, and neither of those
//! is drawing.
//!
//! Unlike the theme picker, moving the cursor here previews nothing — a model
//! is not something you recognize by looking at the screen, and asking the
//! provider on the way past would cost a request per keystroke. The choice is
//! made on Enter, and the row already in use is marked so that the list says
//! where the session currently stands.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Widget as _};
use unicode_width::UnicodeWidthStr as _;

use crate::component::chat::clip;
use crate::component::{clamped, first_visible, modal};
use crate::theme::Theme;

/// What marks the row the cursor is on, and what pads every other row.
const MARKER: &str = "> ";

/// What marks the row already in use.
const ACTIVE: &str = "*";

/// Rows the dialog spends on something other than the list: a blank line and
/// the key reminders.
const CHROME: usize = 2;

/// The keys the dialog answers to, shown along its bottom edge.
const HINTS: &str = "[j/k] [up/down] move   [Enter] switch   [Esc] close";

/// What is shown when there is nothing to choose from.
const EMPTY: &str = "nothing to choose from";

/// Widest the modal grows, whatever the terminal offers.
const MAX_WIDTH: u16 = 72;

/// Tallest the modal grows, whatever the terminal offers.
const MAX_HEIGHT: u16 = 20;

/// Gap between a row's label and its description.
const GAP: usize = 2;

/// One thing that can be chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// What choosing it sends — a model id, an agent name.
    pub value: String,
    /// What it is called on screen. Often the same string as the value, and
    /// deliberately separate: a catalog entry has a name a person reads and an
    /// id a provider wants.
    pub label: String,
    /// The line beside it, where there is one.
    pub detail: Option<String>,
    /// Whether this is the one the session is on.
    pub active: bool,
}

/// The things to choose between, and which one is under the cursor.
#[derive(Clone, Debug)]
pub struct ListDialog {
    /// What the border says, spaces included.
    title: String,
    rows: Vec<Row>,
    /// Index into [`ListDialog::rows`]; always in range while it is non-empty.
    selected: usize,
}

impl ListDialog {
    /// Opens the dialog over `rows`, with the cursor on whichever is active.
    ///
    /// Starting anywhere else would mean the first keypress is spent finding
    /// where the session already stands.
    #[must_use]
    pub fn new(title: impl Into<String>, rows: Vec<Row>) -> Self {
        let selected = rows.iter().position(|row| row.active).unwrap_or(0);

        Self { title: title.into(), rows, selected }
    }

    /// Whether there is nothing to choose from.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// What the row under the cursor sends, or [`None`] when the list is
    /// empty.
    #[must_use]
    pub fn selected(&self) -> Option<&str> {
        self.rows.get(self.selected).map(|row| row.value.as_str())
    }

    /// Moves the cursor by `delta` rows, clamped at both ends.
    pub fn move_selection(&mut self, delta: isize) {
        self.selected = clamped(self.selected, delta, self.rows.len());
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let (popup, inner_width, rows) = modal(area, MAX_WIDTH, MAX_HEIGHT, CHROME);

        Clear.render(popup, buffer);

        let mut lines = self.lines(inner_width, rows, theme);
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(self.title.clone()))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// The visible slice of the list.
    fn lines(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.rows.is_empty() {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        }

        let first = first_visible(self.selected, rows);
        // Labels padded to the widest, so the details beside them sit in one
        // column instead of stepping in and out per row.
        let label_width = self.rows.iter().map(|row| row.label.width()).max().unwrap_or(0);

        self.rows
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(index, row)| {
                let head = format!(
                    "{marker}{active} {label:<label_width$}",
                    marker = if index == self.selected { MARKER } else { "  " },
                    active = if row.active { ACTIVE } else { " " },
                    label = row.label,
                );
                let detail = row.detail.as_deref().unwrap_or_default();
                let detail_width = width.saturating_sub(head.width() + GAP).max(1);
                let line = if detail.is_empty() {
                    head
                } else {
                    format!(
                        "{head}{gap}{detail}",
                        gap = " ".repeat(GAP),
                        detail = clip(detail, detail_width),
                    )
                };
                let line = clip(&line, width);

                Line::styled(
                    format!("{line:<width$}"),
                    if index == self.selected { theme.selection } else { theme.fg },
                )
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
