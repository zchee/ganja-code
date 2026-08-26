//! A centered modal for answering the first question in one protocol request.
//! The protocol permits a batch, but this first frontend answers only its first
//! question, with either one selected option or upstream's free-text row. That
//! deliberately small surface keeps a `question` call from hanging the TUI
//! without pretending this frontend can collect a multi-question form. Spec:
//! upstream `packages/tui/src/routes/session/question.tsx`.

use ganja_protocol::{QuestionId, QuestionInfo};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    component::{chat::clip, clamped, modal},
    theme::Theme,
};

/// What marks the answer row the cursor is on, and what pads every other row.
const MARKER: &str = "> ";

/// Rows spent outside the answer list: the question, two gaps, and the keys.
const CHROME: usize = 4;

/// The keys the selection answers to, shown along its bottom edge.
const HINTS: &str = "[j/k] [up/down] move   [Enter] select   [Esc] dismiss";

/// The keys that matter once the free-text row owns the keyboard.
const HINTS_EDITING: &str = "[type/backspace] edit   [Enter] reply   [Esc] cancel edit";

/// What a question with neither options nor a free-text row says.
const EMPTY: &str = "no selectable options; Esc dismisses";

/// Upstream's label for the answer row outside the supplied options.
const CUSTOM_LABEL: &str = "Type your own answer";

/// Widest the modal grows, whatever the terminal offers.
const MAX_WIDTH: u16 = 72;

/// Tallest the modal grows, whatever the terminal offers.
const MAX_HEIGHT: u16 = 20;

/// Gap between an answer's label and its detail.
const GAP: usize = 2;

/// One open question and the option currently under the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    id: QuestionId,
    question: String,
    header: String,
    options: Vec<ganja_protocol::QuestionOption>,
    /// Whether the free-text row is offered; the protocol makes an absent
    /// `custom` flag opt in (`question.custom !== false`).
    custom: bool,
    /// What has been typed into the free-text row, retained so reopening the
    /// editor restores it like upstream's per-tab store.
    input: String,
    /// Whether the free-text row currently owns the keyboard.
    editing: bool,
    /// Index into the options plus the free-text row, when either exists.
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
            custom: question.custom != Some(false),
            input: String::new(),
            editing: false,
            selected: 0,
        }
    }

    /// The request this dialog is showing, so a terminal event can retire it.
    #[must_use]
    pub fn id(&self) -> &QuestionId {
        &self.id
    }

    /// Whether the free-text row currently owns the keyboard.
    #[must_use]
    pub fn is_editing(&self) -> bool {
        self.editing
    }

    /// Whether the protocol permits drawing the free-text row at all.
    #[cfg(test)]
    #[must_use]
    pub fn offers_custom(&self) -> bool {
        self.custom
    }

    /// Whether the cursor is on the free-text row after the supplied options.
    #[must_use]
    pub fn on_custom_row(&self) -> bool {
        self.custom && self.selected == self.options.len()
    }

    /// What has been typed into the free-text row, including text kept after a
    /// cancelled edit so reopening does not discard work.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The real option label under the cursor, or [`None`] when it is on the
    /// free-text row or there are no answer rows.
    #[must_use]
    pub fn selected_label(&self) -> Option<&str> {
        self.options
            .get(self.selected)
            .map(|option| option.label.as_str())
    }

    /// Moves the cursor by `delta` answer rows, clamped at both ends.
    pub fn move_selection(&mut self, delta: isize) {
        let rows = self.options.len() + usize::from(self.custom);
        self.selected = clamped(self.selected, delta, rows);
    }

    /// Adds `character` while the free-text row owns the keyboard.
    pub fn push(&mut self, character: char) {
        if self.editing {
            self.input.push(character);
        }
    }

    /// Takes the last character back off while the free-text row owns the
    /// keyboard.
    pub fn backspace(&mut self) {
        if self.editing {
            self.input.pop();
        }
    }

    /// Leaves the editor without discarding text that upstream would seed back
    /// into it when the row is reopened.
    pub fn cancel_edit(&mut self) {
        self.editing = false;
    }

    /// Resolves Enter without letting the app disagree about whether it opens
    /// the editor or produces an answer.
    #[must_use]
    pub fn submit(&mut self) -> Option<String> {
        if self.editing {
            self.editing = false;
            let answer = self.input.trim().to_owned();
            if answer.is_empty() {
                self.input.clear();
                return self.selected_label().map(str::to_owned);
            }

            return Some(answer);
        }

        if self.on_custom_row() {
            self.editing = true;
            return None;
        }

        self.selected_label().map(str::to_owned)
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let (popup, inner_width, rows) = modal(area, MAX_WIDTH, MAX_HEIGHT, CHROME);

        Clear.render(popup, buffer);
        let mut lines = vec![Line::styled(clip(&self.question, inner_width), theme.fg)];
        lines.push(Line::raw(""));
        lines.extend(self.answer_lines(inner_width, rows, theme));
        lines.push(Line::raw(""));
        let hints = if self.editing { HINTS_EDITING } else { HINTS };
        lines.push(Line::styled(clip(hints, inner_width), theme.dim));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(format!(" {} ", self.header)))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// The visible slice of every row the cursor can reach.
    fn answer_lines(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.options.is_empty() && !self.custom {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        }

        let row_count = self.options.len() + usize::from(self.custom);
        let first = self.selected.saturating_sub(rows.saturating_sub(1));
        let label_width = self
            .options
            .iter()
            .map(|option| option.label.width())
            .chain(self.custom.then_some(CUSTOM_LABEL.width()))
            .max()
            .unwrap_or(0);

        (first..row_count)
            .take(rows)
            .map(|index| {
                let option = self.options.get(index);
                let label = option.map_or(CUSTOM_LABEL, |option| option.label.as_str());
                let head = format!(
                    "{marker}{label:<label_width$}",
                    marker = if index == self.selected { MARKER } else { "  " },
                );
                let detail_width = width.saturating_sub(head.width() + GAP).max(1);
                let detail = if let Some(option) = option {
                    clip(&option.description, detail_width)
                } else if self.editing {
                    format!("{}█", clip(&self.input, detail_width.saturating_sub(1)))
                } else {
                    clip(&self.input, detail_width)
                };
                let line = clip(
                    &format!("{head}{gap}{detail}", gap = " ".repeat(GAP),),
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

#[cfg(test)]
#[path = "question_tests.rs"]
mod tests;
