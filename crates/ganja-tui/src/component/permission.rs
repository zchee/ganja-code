//! The permission dialog: a centered modal blocking on the user's decision
//! about one pending tool call.
//!
//! Spec: upstream `packages/tui/src/routes/session/permission.tsx`, trimmed to
//! the one-shot shape [`ganja_protocol::PermissionReply`] offers today — no
//! "always" confirmation stage and no "reject with a message" stage, both of
//! which upstream's richer protocol supports and ganja's does not yet.
//!
//! The modal is bounded, so a call can be longer than it can draw. Everything
//! below about measuring rows exists for that case: `y` and `a` are consent,
//! and consent to a command whose tail was cut without a word is not consent.

use ganja_protocol::PermissionId;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Widget as _, Wrap};
use unicode_width::UnicodeWidthStr as _;

use super::chat::split_at_width;
use crate::component::modal;
use crate::theme::Theme;

/// Lines of pretty-printed JSON shown before the rest is clamped.
const ARGS_PREVIEW_LINES: usize = 8;

/// Widest the modal grows, whatever the terminal offers.
const MAX_WIDTH: u16 = 76;

/// Tallest the modal grows, whatever the terminal offers.
const MAX_HEIGHT: u16 = 20;

/// The keys that answer the dialog. Held apart from the rest of the text
/// because the layout keeps them out of the body's budget: a modal whose
/// answers were pushed off the bottom is one the user cannot leave, and the
/// pty suite waits on this exact line to know the dialog is up.
const REPLY_KEYS: &str = "[y] allow once   [a] always allow   [n]/[Esc] reject";

/// What introduces the directories a call would reach outside the project.
///
/// Said in terms of what the *answer* covers rather than of what the call
/// does: an "always" here is remembered per directory, so a dialog that showed
/// the command and not these would be asking about something narrower than
/// what it is about to grant.
const OUTSIDE: &str = "grants access outside the project:";

/// A tool call waiting on the user's decision, and what to show about it.
#[derive(Clone, Debug, PartialEq)]
pub struct Permission {
    id: PermissionId,
    tool: String,
    title: String,
    args: serde_json::Value,
    /// Directories outside the project this call would work in. Usually
    /// empty, and the dialog says nothing when it is.
    directories: Vec<String>,
}

impl Permission {
    /// Builds the dialog state for one `PermissionRequested` event.
    #[must_use]
    pub fn new(
        id: PermissionId,
        tool: String,
        title: String,
        args: serde_json::Value,
        directories: Vec<String>,
    ) -> Self {
        Self { id, tool, title, args, directories }
    }

    /// The request this dialog is showing, so a caller can tell whether an
    /// incoming `PermissionReplied` names it.
    #[must_use]
    pub fn id(&self) -> &PermissionId {
        &self.id
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        // This dialog wraps its own text into the block rather than laying
        // rows out itself, so the two sizes beside the box are not its
        // business — the block's `inner` is.
        let (popup, _, _) = modal(area, MAX_WIDTH, MAX_HEIGHT, 0);

        Clear.render(popup, buffer);

        let block = Block::bordered().title(" permission ");
        let inner = block.inner(popup);

        Paragraph::new(Text::from(self.lines(inner, theme)))
            .block(block)
            .style(theme.fg)
            .wrap(Wrap { trim: false })
            .render(popup, buffer);
    }

    /// The dialog's text, laid out against the room `inner` actually has.
    ///
    /// A `Paragraph` drops whatever runs past the bottom of its area without
    /// saying so, which on this screen would let a user approve the half of a
    /// command they could see. So the dialog wraps the text itself, counts the
    /// rows, and spends its budget in priority order: the reply keys first,
    /// then a marker admitting the cut, then as much of the call as is left.
    fn lines(&self, inner: Rect, theme: &Theme) -> Vec<Line<'static>> {
        let mut body =
            vec![(format!("tool: {}", self.tool), theme.accent), (self.title.clone(), theme.fg)];
        // Inside the body, so these rows are spent out of the same budget the
        // call itself is and the overflow count stays true of the whole
        // dialog. A call that stays in the checkout adds nothing here, which is
        // what keeps the common dialog drawing exactly as it always did.
        if !self.directories.is_empty() {
            body.push((String::new(), theme.fg));
            body.push((OUTSIDE.to_owned(), theme.warning));
            body.extend(
                self.directories.iter().map(|directory| (format!("  {directory}"), theme.dim)),
            );
        }
        body.push((String::new(), theme.fg));
        body.extend(self.args_preview().into_iter().map(|text| (text, theme.dim)));
        let tail = [(String::new(), theme.fg), (REPLY_KEYS.to_owned(), theme.dim)];

        let width = usize::from(inner.width);
        let height = usize::from(inner.height);
        let mut rows = wrap_all(&body, width);
        let tail_rows = wrap_all(&tail, width);

        // Under this the modal is a border and nothing else — there is no row
        // left to carry a warning on, either.
        if width > 0 && height > 0 {
            let room = height.saturating_sub(tail_rows.len());
            if rows.len() > room {
                // The marker outranks the body row it displaces: a call seen in
                // part is still worth something, a cut nobody mentions is not.
                // Reserving against the largest count this dialog could report
                // keeps that to one pass, since a smaller count never wraps to
                // more rows than a larger one.
                let reserved = wrap(&overflow_marker(rows.len()), width).len().min(room);
                let kept = room - reserved;
                let hidden = rows.len() - kept;
                rows.truncate(kept);

                let mut marker = wrap_all(&[(overflow_marker(hidden), theme.accent)], width);
                marker.truncate(reserved);
                rows.append(&mut marker);
            }
        }

        rows.extend(tail_rows);
        rows.into_iter().map(|(text, style)| Line::styled(text, style)).collect()
    }

    /// The call's arguments, pretty-printed and clamped to a few lines: the
    /// dialog needs enough to recognize the call, not the whole payload.
    fn args_preview(&self) -> Vec<String> {
        let pretty = serde_json::to_string_pretty(&self.args).unwrap_or_default();
        let mut shown: Vec<&str> = pretty.lines().collect();
        let clamped = shown.len() > ARGS_PREVIEW_LINES;
        shown.truncate(ARGS_PREVIEW_LINES);

        let mut preview: Vec<String> = shown.into_iter().map(str::to_owned).collect();
        if clamped {
            preview.push("...".to_owned());
        }

        preview
    }
}

/// The line the dialog adds when it runs out of room.
///
/// The count is in rows as the terminal would draw them, not source lines,
/// because a single argument can run for a screenful on its own and a source
/// count would report that as one. The marker always displaces at least one
/// body row of its own, so `hidden` is never less than two.
///
/// `pub(crate)` since the held-message dialog (**D524**) budgets its rows the
/// same way for the same consent reason: what `y` approves must be what was
/// shown, or flagged as cut.
pub(crate) fn overflow_marker(hidden: usize) -> String {
    format!("... +{hidden} lines not shown")
}

/// Splits `text` into chunks of at most `width` display columns, verbatim.
///
/// The transcript breaks on word boundaries; this deliberately does not. A
/// dialog asking whether to run a command has to show that command character
/// for character, and word wrapping swallows the whitespace it breaks on.
/// Chunking on width alone also keeps the row count exact, which is what the
/// overflow marker's honesty rests on.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut rows = Vec::new();
    let mut rest = text;
    while rest.width() > width {
        let (head, tail) = split_at_width(rest, width);
        rows.push(head.to_owned());
        rest = tail;
    }
    // A remainder of nothing earns a row only when it is the whole text: a
    // blank source line still takes a row, an exact fit does not add one.
    if !rest.is_empty() || rows.is_empty() {
        rows.push(rest.to_owned());
    }

    rows
}

/// [`wrap`] across a run of styled lines, carrying each line's style onto
/// every chunk it wrapped into. `pub(crate)` for [`overflow_marker`]'s reason.
pub(crate) fn wrap_all(lines: &[(String, Style)], width: usize) -> Vec<(String, Style)> {
    lines
        .iter()
        .flat_map(|(text, style)| {
            let style = *style;
            wrap(text, width).into_iter().map(move |row| (row, style))
        })
        .collect()
}

#[cfg(test)]
#[path = "permission_tests.rs"]
mod tests;
