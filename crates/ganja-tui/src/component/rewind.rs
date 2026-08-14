//! The rewind picker: the session's own checkpoints, newest first, and the
//! choice of how much of one to put back.
//!
//! **Semantics are upstream's, presentation is Claude Code's.** Upstream
//! v1.18.13 reverts to a message (`session/revert.ts:13-23`) from a dialog that
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
    component::{body_rows, chat::clip, clamped, first_visible},
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
    /// named. Zero renders [`NO_CODE`].
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
            let row = format!(
                "{marker}{label}",
                marker = if index == option { MARKER } else { "  " },
            );
            lines.push(Line::styled(
                clip(&row, width),
                if index == option {
                    theme.accent
                } else {
                    theme.fg
                },
            ));
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
mod tests {
    use ganja_protocol::{MessageId, RevertScope};
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Checkpoint, Rewind};
    use crate::theme::Theme;

    fn checkpoint(id: &str, title: &str, files: usize) -> Checkpoint {
        Checkpoint {
            message_id: MessageId::from(id.to_owned()),
            title: title.to_owned(),
            files,
        }
    }

    /// Two checkpoints, newest first, one of which changed nothing.
    fn rewind() -> Rewind {
        Rewind::new(vec![
            checkpoint("msg_3", "rename the thing", 2),
            checkpoint("msg_1", "what does this crate do", 0),
        ])
    }

    fn rendered(rewind: &Rewind, area: Rect) -> String {
        let mut buffer = Buffer::empty(area);
        rewind.render(area, &mut buffer, &Theme::default());

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
    fn the_list_shows_every_checkpoint_and_what_its_turns_changed() {
        let screen = rendered(&rewind(), Rect::new(0, 0, 80, 20));

        assert!(screen.contains("(Current)"), "got:\n{screen}");
        assert!(screen.contains("rename the thing"), "got:\n{screen}");
        assert!(screen.contains("2 files changed"), "got:\n{screen}");
        assert!(screen.contains("what does this crate do"), "got:\n{screen}");
        assert!(
            screen.contains("\u{26a0} No code restore"),
            "a span with no patches says so:\n{screen}"
        );
    }

    /// One file is one file, not "1 files".
    #[test]
    fn a_single_changed_file_is_counted_in_the_singular() {
        let rewind = Rewind::new(vec![checkpoint("msg_1", "touch one thing", 1)]);

        assert!(
            rendered(&rewind, Rect::new(0, 0, 80, 20)).contains("1 file changed"),
            "one file is not plural"
        );
    }

    /// The cursor opens on the row that does nothing, and the checkpoints are
    /// under it.
    #[test]
    fn the_picker_opens_on_current_and_moves_into_the_checkpoints() {
        let mut rewind = rewind();
        assert!(rewind.selected().is_none(), "(Current) is not a checkpoint");

        rewind.move_selection(1);
        assert_eq!(
            rewind.selected().map(|point| point.message_id.as_str()),
            Some("msg_3")
        );

        // Clamped at both ends rather than wrapping.
        rewind.move_selection(9);
        assert_eq!(
            rewind.selected().map(|point| point.message_id.as_str()),
            Some("msg_1")
        );
        rewind.move_selection(-9);
        assert!(rewind.selected().is_none());
    }

    /// Enter on `(Current)` has nothing to ask about: the caller reads the
    /// `false` as "close, having done nothing".
    #[test]
    fn enter_on_current_advances_to_nothing() {
        let mut rewind = rewind();

        assert!(!rewind.advance(), "there is nothing to restore");
        assert!(!rewind.is_choosing_scope(), "and no question to ask");
    }

    #[test]
    fn enter_on_a_checkpoint_opens_the_scope_choice_and_answers_with_it() {
        let mut rewind = rewind();
        rewind.move_selection(1);

        assert!(rewind.advance());
        assert!(rewind.is_choosing_scope());
        assert_eq!(
            rewind.chosen(),
            Some((MessageId::from("msg_3".to_owned()), RevertScope::Both)),
            "the first option is the whole checkpoint"
        );

        rewind.move_selection(1);
        assert_eq!(
            rewind.chosen(),
            Some((
                MessageId::from("msg_3".to_owned()),
                RevertScope::Conversation
            ))
        );

        rewind.move_selection(1);
        assert_eq!(
            rewind.chosen(),
            Some((MessageId::from("msg_3".to_owned()), RevertScope::Files))
        );

        // The scope list is clamped too.
        rewind.move_selection(9);
        assert_eq!(
            rewind.chosen(),
            Some((MessageId::from("msg_3".to_owned()), RevertScope::Files))
        );
    }

    /// The second step says which checkpoint it is about, in the screenshot's
    /// own words, and offers all three answers.
    #[test]
    fn the_scope_step_names_the_checkpoint_and_the_three_answers() {
        let mut rewind = rewind();
        rewind.move_selection(1);
        rewind.advance();

        let screen = rendered(&rewind, Rect::new(0, 0, 80, 20));

        assert!(screen.contains("rename the thing"), "got:\n{screen}");
        assert!(
            screen.contains("Restore the code and/or conversation"),
            "got:\n{screen}"
        );
        assert!(screen.contains("Code and conversation"), "got:\n{screen}");
        assert!(screen.contains("Conversation only"), "got:\n{screen}");
        assert!(screen.contains("Code only"), "got:\n{screen}");
        assert!(screen.contains("[Esc] cancel"), "got:\n{screen}");
    }

    /// Nothing under the cursor means nothing to choose: the scope step is
    /// unreachable and an Enter there answers with no rewind.
    #[test]
    fn a_session_with_no_checkpoints_still_opens_and_chooses_nothing() {
        let mut rewind = Rewind::new(Vec::new());

        assert!(!rewind.advance());
        assert_eq!(rewind.chosen(), None);
        assert!(
            rendered(&rewind, Rect::new(0, 0, 80, 20)).contains("(Current)"),
            "the row for where the session stands is always there"
        );
    }

    #[test]
    fn a_row_too_wide_for_the_column_is_cut_rather_than_wrapped() {
        let rewind = Rewind::new(vec![checkpoint("msg_1", &"wide ".repeat(40), 3)]);

        let screen = rendered(&rewind, Rect::new(0, 0, 60, 20));

        for line in screen.lines() {
            assert!(
                line.chars().count() <= 60,
                "a row must not overflow the dialog: {line:?}"
            );
        }
        assert!(screen.contains("3 files changed"), "got:\n{screen}");
    }

    /// More checkpoints than rows: the list has to move under the selection,
    /// or the user cannot reach what they are selecting.
    #[test]
    fn a_selection_below_the_fold_scrolls_the_list_to_it() {
        let checkpoints = (0..40)
            .map(|index| checkpoint(&format!("msg_{index:02}"), &format!("prompt {index:02}"), 1))
            .collect();
        let mut rewind = Rewind::new(checkpoints);
        let area = Rect::new(0, 0, 80, 20);

        let top = rendered(&rewind, area);
        assert!(top.contains("prompt 00"), "got:\n{top}");
        assert!(!top.contains("prompt 39"), "got:\n{top}");

        rewind.move_selection(40);
        let bottom = rendered(&rewind, area);

        assert!(
            bottom.contains("> prompt 39"),
            "the selection must be on screen:\n{bottom}"
        );
        assert!(
            !bottom.contains("prompt 00"),
            "the list should have scrolled:\n{bottom}"
        );
    }

    #[test]
    fn a_zero_area_draws_nothing_and_does_not_panic() {
        let screen = rendered(&rewind(), Rect::new(0, 0, 0, 0));

        assert!(
            screen.is_empty(),
            "a zero area has no cell to hold: {screen}"
        );
    }

    /// The same protection every other dialog rendering text somebody else
    /// wrote is pinned for: a prompt is the user's own bytes, and a control
    /// sequence in one must not repaint the screen they are choosing from.
    #[test]
    fn an_escape_sequence_in_a_prompt_never_reaches_the_buffer() {
        let rewind = Rewind::new(vec![checkpoint("msg_1", "\u{1b}[2Jrewind to me\u{7}", 1)]);

        let screen = rendered(&rewind, Rect::new(0, 0, 80, 20));
        let leaked: Vec<char> = screen
            .chars()
            .filter(|character| *character != '\n' && character.is_control())
            .collect();

        assert!(
            leaked.is_empty(),
            "control characters reached the buffer: {leaked:?}\n{screen}"
        );
        assert!(screen.contains("rewind to me"), "got:\n{screen}");
    }
}
