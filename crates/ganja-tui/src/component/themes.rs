//! The theme picker: a centered modal listing every theme this run can switch
//! to.
//!
//! Spec: upstream `packages/tui/src/component/dialog-theme-list.tsx`. Two of
//! its behaviors are the whole point of the dialog and are ported exactly:
//! moving the cursor **applies** the theme under it, so the choice is made by
//! looking at the screen rather than at a name; and cancelling puts back the
//! theme that was active when it opened, so browsing costs nothing.
//!
//! The dialog owns which name is under the cursor and nothing else. Applying,
//! reverting and storing are [`crate::app::App`]'s, because they touch state
//! the dialog does not own — the same split the sessions picker uses.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};

use crate::{
    component::{chat::clip, clamped, first_visible, modal},
    theme::Theme,
};

/// What marks the row the cursor is on, and what pads every other row so the
/// names stay in one column.
const MARKER: &str = "> ";

/// Rows the dialog spends on something other than the list: a blank line and
/// the key reminders.
const CHROME: usize = 2;

/// The keys the dialog answers to, shown along its bottom edge.
const HINTS: &str = "[j/k] [up/down] preview   [Enter] keep   [Esc] cancel";

/// Widest the modal grows, whatever the terminal offers. A theme name is one
/// short word; a list box the width of the screen would only be harder to read.
const MAX_WIDTH: u16 = 40;

/// Tallest the modal grows, whatever the terminal offers.
const MAX_HEIGHT: u16 = 20;

/// The themes to choose between, and which one the cursor is on.
#[derive(Clone, Debug)]
pub struct ThemeList {
    /// Case-insensitively sorted, as [`crate::theme::Themes::names`] answers.
    names: Vec<String>,
    /// Index into [`ThemeList::names`]; always in range while it is non-empty.
    selected: usize,
    /// What was active when the dialog opened, and what cancelling restores.
    initial: String,
}

impl ThemeList {
    /// Opens the list over `names`, with the cursor on `active`.
    ///
    /// Starting anywhere else would preview a theme the user never asked to
    /// see the moment the dialog opened.
    #[must_use]
    pub fn new(names: Vec<String>, active: &str) -> Self {
        let selected = names.iter().position(|name| name == active).unwrap_or(0);

        Self {
            names,
            selected,
            initial: active.to_owned(),
        }
    }

    /// The theme under the cursor, or [`None`] when there is nothing to pick.
    #[must_use]
    pub fn selected(&self) -> Option<&str> {
        self.names.get(self.selected).map(String::as_str)
    }

    /// The theme cancelling puts back.
    #[must_use]
    pub fn initial(&self) -> &str {
        &self.initial
    }

    /// Moves the cursor by `delta` rows.
    ///
    /// Clamped rather than wrapped, like the sessions picker: running off one
    /// end and landing on the other is never what the keypress meant.
    pub fn move_selection(&mut self, delta: isize) {
        self.selected = clamped(self.selected, delta, self.names.len());
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let (popup, inner_width, rows) = modal(area, MAX_WIDTH, MAX_HEIGHT, CHROME);

        Clear.render(popup, buffer);

        let mut lines = self.rows(inner_width, rows, theme);
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" themes "))
            // The panel surface is what makes a switch visible in the dialog
            // that caused it, rather than only behind it.
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// One line per visible theme.
    fn rows(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let first = first_visible(self.selected, rows);

        self.names
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(index, name)| {
                let marker = if index == self.selected { MARKER } else { "  " };
                let row = clip(&format!("{marker}{name}"), width);

                // The selected row is filled rather than tinted, which is what
                // gives the contrast rule something to answer.
                Line::styled(
                    format!("{row:<width$}"),
                    if index == self.selected {
                        theme.selection
                    } else {
                        theme.fg
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "themes_tests.rs"]
mod tests;
