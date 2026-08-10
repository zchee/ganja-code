//! A centered modal for answering one options-only question.
//!
//! The protocol permits a batch, but this first frontend answers only its
//! first question. It also has no custom text input: a custom-only question
//! with no options is Esc-only, because there is nothing to select and nowhere
//! to type. Shipping even that deliberately small surface stops an
//! options-only `question` call from hanging the TUI, which the previously
//! inert event arm did.

use ganja_protocol::{QuestionId, QuestionInfo};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{component::chat::split_at_width, theme::Theme};

/// What marks the option the cursor is on, and what pads every other row.
const MARKER: &str = "> ";

/// Rows spent outside the option list: the question, two gaps, and the keys.
const CHROME: usize = 4;

/// The keys the dialog answers to, shown along its bottom edge.
const HINTS: &str = "[j/k] [up/down] move   [Enter] answer   [Esc] dismiss";

/// What a custom-only question says where selectable options would be.
const EMPTY: &str = "no selectable options; Esc dismisses";

/// Widest the modal grows, whatever the terminal offers.
const MAX_WIDTH: u16 = 72;

/// Gap between an option's label and its description.
const GAP: usize = 2;

/// One open question and the option currently under the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    id: QuestionId,
    question: String,
    header: String,
    options: Vec<ganja_protocol::QuestionOption>,
    /// Index into [`Question::options`]; always in range while it is non-empty.
    selected: usize,
}

impl Question {
    /// Opens the dialog for the first question in one protocol request.
    #[must_use]
    pub fn new(id: QuestionId, question: QuestionInfo) -> Self {
        Self {
            id,
            question: question.question,
            header: question.header,
            options: question.options,
            selected: 0,
        }
    }

    /// The request this dialog is showing, so a terminal event can retire it.
    #[must_use]
    pub fn id(&self) -> &QuestionId {
        &self.id
    }

    /// The label under the cursor, or [`None`] for a custom-only question.
    #[must_use]
    pub fn selected_label(&self) -> Option<&str> {
        self.options
            .get(self.selected)
            .map(|option| option.label.as_str())
    }

    /// Moves the cursor by `delta` options, clamped at both ends.
    pub fn move_selection(&mut self, delta: isize) {
        let last = self.options.len().saturating_sub(1);
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

        let inner_width = usize::from(width).saturating_sub(2);
        let rows = usize::from(height)
            .saturating_sub(2)
            .saturating_sub(CHROME)
            .max(1);
        let mut lines = vec![Line::styled(clip(&self.question, inner_width), theme.fg)];
        lines.push(Line::raw(""));
        lines.extend(self.option_lines(inner_width, rows, theme));
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(format!(" {} ", self.header)))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// The visible slice of the options.
    fn option_lines(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.options.is_empty() {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        }

        let first = self.selected.saturating_sub(rows.saturating_sub(1));
        let label_width = self
            .options
            .iter()
            .map(|option| option.label.width())
            .max()
            .unwrap_or(0);

        self.options
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(index, option)| {
                let head = format!(
                    "{marker}{label:<label_width$}",
                    marker = if index == self.selected { MARKER } else { "  " },
                    label = option.label,
                );
                let detail_width = width.saturating_sub(head.width() + GAP).max(1);
                let line = clip(
                    &format!(
                        "{head}{gap}{detail}",
                        gap = " ".repeat(GAP),
                        detail = clip(&option.description, detail_width),
                    ),
                    width,
                );

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
    use ganja_protocol::{QuestionId, QuestionInfo, QuestionOption};
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::Question;
    use crate::theme::Theme;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 60,
        height: 14,
    };

    fn question(options: Vec<QuestionOption>) -> Question {
        Question::new(
            QuestionId::from("que_1".to_owned()),
            QuestionInfo {
                question: "Which database should the service use?".to_owned(),
                header: "Database".to_owned(),
                options,
                multiple: None,
                custom: None,
            },
        )
    }

    fn rendered(question: &Question) -> String {
        let mut buffer = Buffer::empty(AREA);
        question.render(AREA, &mut buffer, &Theme::default());

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
    fn the_dialog_renders_the_question_header_labels_and_descriptions() {
        let screen = rendered(&question(vec![
            QuestionOption {
                label: "Postgres".to_owned(),
                description: "Relational database".to_owned(),
            },
            QuestionOption {
                label: "SQLite".to_owned(),
                description: "One local file".to_owned(),
            },
        ]));

        for expected in [
            "Database",
            "Which database should the service use?",
            "Postgres",
            "Relational database",
            "SQLite",
            "One local file",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
        }
    }

    #[test]
    fn a_custom_only_question_is_dismissal_only() {
        let question = question(Vec::new());

        assert_eq!(question.selected_label(), None);
        assert!(
            rendered(&question).contains("no selectable options; Esc dismisses"),
            "{}",
            rendered(&question)
        );
    }
}
