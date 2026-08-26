//! The rewind picker: the session's own checkpoints, newest first, and the
//! choice of how much of one to put back.
//!
//! **Semantics are upstream's, presentation is Claude Code's.** Upstream
//! v1.18.22 reverts to a message (`session/revert.ts:13-23`) from a dialog that
//! lists messages (`component/dialog-message.tsx:22-52`), and that is exactly
//! what [`ganja_protocol::Command::RevertTo`] asks the engine for. What this
//! draws is Claude Code's rewind panel (screenshot 2026-08-11): checkpoint rows
//! carrying what each turn changed, a `(Current)` row for where the session
//! already stands, and — on Enter — a second step asking whether to restore the
//! code, the conversation, or both (**D451**, declared at
//! [`ganja_protocol::RevertScope`]).
//!
//! Two things the screenshot has that this deliberately does not:
//!
//! - **Line-level stats** (`+400 -100`). They need a `git diff --numstat`
//!   between two tree hashes, which is an engine addition the rewind wave kept
//!   out of scope. [`Checkpoint::files`] is the row's whole annotation for now,
//!   and the row shape has room for the rest.
//! - **Part-level anchors.** Upstream's `RevertInput` also carries a `partID`,
//!   so a revert can stop inside a turn. Ganja's checkpoints are whole user
//!   messages; narrower, not different.
//!
//! The dialog owns which row and which option are under the cursor and nothing
//! else: sending the command and closing the picker are [`crate::app::App`]'s,
//! the same split every other dialog here uses.

use ganja_protocol::{MessageId, RevertScope};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    component::{action_row, body_rows, chat::clip, clamped, first_visible},
    theme::Theme,
};

/// What marks the row the cursor is on, and what pads every other row so the
/// titles stay in one column.
const MARKER: &str = "> ";

/// Columns between a row's title and its annotation.
const GAP: usize = 2;

/// Widest the modal grows.
const MAX_WIDTH: u16 = 76;

/// Tallest the modal grows.
const MAX_HEIGHT: u16 = 20;

/// Rows the checkpoint step spends on chrome: a blank line and the hints.
const CHROME: usize = 2;

/// The row for where the session already stands. Claude Code's own wording.
const CURRENT: &str = "(Current)";

/// What a checkpoint row says when the turns it covers changed no file at all
/// — Claude Code's annotation, warning glyph included, because a rewind to
/// such a checkpoint has no code to put back however the scope is answered.
const NO_CODE: &str = "\u{26a0} No code restore";

/// The keys the checkpoint step answers to.
const HINTS: &str = "[j/k] [up/down] move   [Enter] continue   [Esc] cancel";

/// The line above the scope options, which is the screenshot's own subtitle.
const SUBTITLE: &str = "Restore the code and/or conversation to the point before this message";

/// One checkpoint the picker offers: a user message, and what the turns
/// between it and the next one changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The user message a rewind to this row anchors on.
    pub message_id: MessageId,
    /// The prompt's first line, clipped to the row at render time.
    pub title: String,
    /// How many distinct files the patch parts in this checkpoint's span
    /// named. Zero renders `NO_CODE`.
    pub files: usize,
}

/// Which of the picker's two steps is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Choosing a checkpoint.
    Checkpoints,
    /// Choosing how much of it to restore, by index into [`SCOPES`].
    Scope(usize),
}

/// The three answers to "how much of this checkpoint", in the order the
/// screenshot lists them.
const SCOPES: [(RevertScope, &str, &str); 3] = [
    (
        RevertScope::Both,
        "Code and conversation",
        "Put the files back and take the messages back with them",
    ),
    (
        RevertScope::Conversation,
        "Conversation only",
        "Take the messages back and leave the working tree exactly as it is",
    ),
    (
        RevertScope::Files,
        "Code only",
        "Put the files back and keep every message",
    ),
];

/// The checkpoints, which one is under the cursor, and which step is showing.
#[derive(Clone, Debug)]
pub struct Rewind {
    /// Newest first, as [`crate::component::chat::Chat::checkpoints`] hands
    /// them over.
    checkpoints: Vec<Checkpoint>,
    /// Index into the rendered rows: zero is [`CURRENT`], and everything after
    /// it indexes [`Rewind::checkpoints`] one lower.
    selected: usize,
    step: Step,
}

impl Rewind {
    /// Opens the picker over `checkpoints`, newest first.
    #[must_use]
    pub fn new(checkpoints: Vec<Checkpoint>) -> Self {
        Self {
            checkpoints,
            // On `(Current)` rather than on the newest checkpoint: the row that
            // does nothing is the safe place for a destructive dialog to open,
            // and it is where the session already is.
            selected: 0,
            step: Step::Checkpoints,
        }
    }

    /// Whether the scope step is the one on screen.
    #[must_use]
    pub fn is_choosing_scope(&self) -> bool {
        matches!(self.step, Step::Scope(_))
    }

    /// The checkpoint under the cursor, or [`None`] on the `(Current)` row.
    #[must_use]
    pub fn selected(&self) -> Option<&Checkpoint> {
        self.checkpoints.get(self.selected.checked_sub(1)?)
    }

    /// Moves whichever list is showing by `delta` rows.
    ///
    /// Clamped rather than wrapped, like every other list here: running off one
    /// end and landing on the other is never what the keypress meant.
    pub fn move_selection(&mut self, delta: isize) {
        match self.step {
            Step::Checkpoints => {
                self.selected = clamped(self.selected, delta, self.checkpoints.len() + 1);
            }
            Step::Scope(option) => self.step = Step::Scope(clamped(option, delta, SCOPES.len())),
        }
    }

    /// Enter on the checkpoint step: opens the scope choice for the row under
    /// the cursor.
    ///
    /// Answers `false` for the `(Current)` row, which is a person choosing
    /// where the session already is — there is nothing to restore and nothing
    /// to ask about, so the caller closes the picker.
    pub fn advance(&mut self) -> bool {
        if self.selected == 0 {
            return false;
        }
        self.step = Step::Scope(0);

        true
    }

    /// Enter on the scope step: the checkpoint and the scope the rewind names.
    ///
    /// [`None`] while the checkpoint step is showing, which no caller reaches
    /// — [`Rewind::is_choosing_scope`] is what decides which Enter this is.
    #[must_use]
    pub fn chosen(&self) -> Option<(MessageId, RevertScope)> {
        let Step::Scope(option) = self.step else {
            return None;
        };
        let checkpoint = self.selected()?;
        let (scope, _, _) = SCOPES.get(option)?;

        Some((checkpoint.message_id.clone(), *scope))
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let available = area.height.saturating_sub(2).clamp(1, MAX_HEIGHT);

        // Inside the border on both axes.
        let inner_width = usize::from(width).saturating_sub(2);
        let rows = body_rows(available, CHROME);

        let mut lines = match self.step {
            Step::Checkpoints => self.checkpoint_rows(inner_width, rows, theme),
            Step::Scope(option) => self.scope_rows(inner_width, option, theme),
        };
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

        // The checkpoint step takes the screenful it was given, because its
        // list is as long as the conversation. The scope step is three answers
        // and never grows, so it takes exactly the rows it needs — a fixed
        // height that filled the same box would push its own hint line off the
        // bottom.
        let height = match self.step {
            Step::Checkpoints => available,
            Step::Scope(_) => u16::try_from(lines.len().saturating_add(2))
                .unwrap_or(available)
                .min(available),
        };
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" rewind "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// One line per visible checkpoint, `(Current)` first, each annotated with
    /// what its turns changed.
    fn checkpoint_rows(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let first = first_visible(self.selected, rows);
        let notes: Vec<String> = std::iter::once(String::new())
            .chain(self.checkpoints.iter().map(annotation))
            .collect();
        let titles: Vec<&str> = std::iter::once(CURRENT)
            .chain(self.checkpoints.iter().map(|point| point.title.as_str()))
            .collect();

        // The annotation column is as wide as its widest value, so the titles
        // beside it line up instead of jittering per row.
        let visible: Vec<usize> = (first..titles.len()).take(rows).collect();
        let note_width = visible
            .iter()
            .map(|&index| notes[index].width())
            .max()
            .unwrap_or(0);
        let title_width = width
            .saturating_sub(MARKER.width() + note_width + GAP)
            .max(1);

        visible
            .iter()
            .map(|&index| {
                let row = format!(
                    "{marker}{title:<title_width$}{gap}{note:>note_width$}",
                    marker = if index == self.selected { MARKER } else { "  " },
                    title = clip(titles[index], title_width),
                    gap = " ".repeat(GAP),
                    note = notes[index],
                );

                Line::styled(
                    row,
                    if index == self.selected {
                        theme.accent
                    } else {
                        theme.fg
                    },
                )
            })
            .collect()
    }

    /// The second step: what the chosen checkpoint is, the subtitle, and the
    /// three answers.
    ///
    /// No blank line under the subtitle: two header lines plus three two-line
    /// answers plus the hints is already eleven rows, and a twenty-row terminal
    /// has ten to give — a separator bought at the cost of the line that says
    /// how to get out is a bad trade.
    fn scope_rows(&self, width: usize, option: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled(
                clip(
                    self.selected()
                        .map_or(CURRENT, |point| point.title.as_str()),
                    width,
                ),
                theme.fg,
            ),
            Line::styled(clip(SUBTITLE, width), theme.dim),
        ];

        for (index, (_, label, description)) in SCOPES.iter().enumerate() {
            lines.push(action_row(index, option, label, width, theme));
            lines.push(Line::styled(
                clip(&format!("    {description}"), width),
                theme.dim,
            ));
        }

        lines
    }
}

/// What a checkpoint row says about the code its turns changed.
fn annotation(checkpoint: &Checkpoint) -> String {
    match checkpoint.files {
        0 => NO_CODE.to_owned(),
        1 => "1 file changed".to_owned(),
        files => format!("{files} files changed"),
    }
}

#[cfg(test)]
#[path = "rewind_tests.rs"]
mod tests;
