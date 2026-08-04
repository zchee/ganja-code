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

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{component::chat::split_at_width, theme::Theme};

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

        Self {
            title: title.into(),
            rows,
            selected,
        }
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
        let last = self.rows.len().saturating_sub(1);
        let moved = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta.unsigned_abs())
        };

        self.selected = moved.min(last);
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let height = area.height.saturating_sub(2).clamp(1, 20);
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);

        // Inside the border on both axes.
        let inner_width = usize::from(width).saturating_sub(2);
        let rows = usize::from(height)
            .saturating_sub(2)
            .saturating_sub(CHROME)
            .max(1);

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

        let first = self.first_visible(rows);
        // Labels padded to the widest, so the details beside them sit in one
        // column instead of stepping in and out per row.
        let label_width = self
            .rows
            .iter()
            .map(|row| row.label.width())
            .max()
            .unwrap_or(0);

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
                    if index == self.selected {
                        theme.selection
                    } else {
                        theme.fg
                    },
                )
            })
            .collect()
    }

    /// The first row on screen: far enough down to keep the selected one
    /// visible, and no further.
    fn first_visible(&self, rows: usize) -> usize {
        self.selected.saturating_sub(rows.saturating_sub(1))
    }
}

/// `text` cut to `width` display columns.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }

    split_at_width(text, width).0.to_owned()
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{ListDialog, Row};
    use crate::theme::Theme;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 60,
        height: 14,
    };

    fn row(value: &str, active: bool) -> Row {
        Row {
            value: value.to_owned(),
            label: value.to_owned(),
            detail: Some(format!("what {value} is for")),
            active,
        }
    }

    fn dialog() -> ListDialog {
        ListDialog::new(
            " models ",
            vec![
                row("first", false),
                row("second", true),
                row("third", false),
            ],
        )
    }

    fn rendered(dialog: &ListDialog) -> String {
        let mut buffer = Buffer::empty(AREA);
        dialog.render(AREA, &mut buffer, &Theme::default());

        (0..AREA.height)
            .map(|line| {
                (0..AREA.width)
                    .map(|column| buffer[(column, line)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_dialog_opens_on_the_row_the_session_is_already_on() {
        assert_eq!(dialog().selected(), Some("second"));
    }

    #[test]
    fn a_list_with_nothing_active_opens_on_its_first_row() {
        let dialog = ListDialog::new(" models ", vec![row("first", false), row("second", false)]);

        assert_eq!(dialog.selected(), Some("first"));
    }

    #[test]
    fn the_cursor_clamps_at_both_ends() {
        let mut dialog = dialog();

        dialog.move_selection(-9);
        assert_eq!(dialog.selected(), Some("first"));

        dialog.move_selection(99);
        assert_eq!(dialog.selected(), Some("third"));
    }

    #[test]
    fn the_active_row_is_marked_so_the_list_says_where_the_session_stands() {
        let screen = rendered(&dialog());
        let marked: Vec<&str> = screen
            .lines()
            .filter(|line| line.contains('*'))
            .map(str::trim)
            .collect();

        assert_eq!(marked.len(), 1, "exactly one row is active:\n{screen}");
        assert!(marked[0].contains("second"), "{marked:?}");
    }

    #[test]
    fn an_empty_list_says_so_instead_of_drawing_an_empty_box() {
        let dialog = ListDialog::new(" agents ", Vec::new());

        assert!(dialog.is_empty());
        assert_eq!(dialog.selected(), None);
        assert!(
            rendered(&dialog).contains("nothing to choose from"),
            "{}",
            rendered(&dialog)
        );
    }

    #[test]
    fn a_detail_too_long_for_the_box_is_cut_rather_than_wrapped() {
        let dialog = ListDialog::new(
            " agents ",
            vec![Row {
                value: "explore".to_owned(),
                label: "explore".to_owned(),
                detail: Some("d".repeat(500)),
                active: false,
            }],
        );

        for line in rendered(&dialog).lines() {
            assert!(line.chars().count() <= usize::from(AREA.width));
        }
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        for (width, height) in [(1, 1), (3, 2), (8, 4)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);

            dialog().render(area, &mut buffer, &Theme::default());
        }
    }
}
