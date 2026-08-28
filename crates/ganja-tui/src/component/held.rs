//! The two review surfaces for held inbound peer messages (**D524**): the
//! per-message approval modal a parity hold raises, and the `/held` listing
//! every held entry is reviewed in.
//!
//! Neither is a port of upstream opencode, which has no cross-session
//! messaging at all. The approval modal renders exactly the five sanitized
//! items of v1-only supplementary fact (b) — reply address, claimed display
//! name, verified PID, a one-line preview, an expandable body preview —
//! mapped honestly onto what ganja actually holds: this transport carries no
//! reply address and the route keeps no process identity, so those two rows
//! say so rather than inventing one. The modal also states that the message
//! has not yet been delivered to the model, names the hold cause, and counts
//! its deadline down; approving delivers that one message, denying or
//! dismissing drops it (the same fact). The listing is the `/mcp` two-step
//! shape (`component/team.rs:11` names `component/mcp.rs` as the template)
//! and is the *only* review surface an explicit or mode-unknown hold has —
//! those raise no modal, because no deadline is racing anybody (v2
//! §"Cross-pass reconciliation", the expiry re-check).
//!
//! Which causes raise the modal, and what a keypress sends, are
//! [`crate::app::App`]'s; these components draw and hold cursor state, the
//! same split every dialog here keeps. The listing stays poll-driven:
//! [`HeldList::refresh`] is what a tick hands a fresh read of
//! `Engine::held_messages` to while the dialog is open.

use std::time::{Duration, Instant};

use ganja_protocol::team::cap_for_display;
use ganja_protocol::{HeldDecision, HeldId, HoldCause, PolicySource};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Widget as _};
use unicode_width::UnicodeWidthStr as _;

use super::permission::{overflow_marker, wrap_all};
use crate::component::chat::clip;
use crate::component::{
    ACTION_HINTS, CHROME, LIST_HINTS, MARKER, MAX_HEIGHT, MAX_WIDTH, action_row, body_rows,
    clamped, first_visible, modal,
};
use crate::theme::Theme;

/// What the reply-address row says: ganja's `from` is an identity, not a road
/// back, so the row states the absence instead of fabricating an address.
const REPLY_ADDRESS: &str = "reply address: none exists on this transport";

/// What the verified-PID row says: the route compares the peer's uid at
/// accept and drops it, so no process identity survives to show.
const VERIFIED_PID: &str = "verified pid: same-user socket, no process identity";

/// The statement the modal makes about where the message is: held means the
/// model has heard nothing, and will hear nothing unless somebody delivers it.
const NOT_DELIVERED: &str = "This message has not been delivered to the model.";

/// The keys that answer the approval modal. Deny and dismiss are one answer
/// on purpose: dismissing a review is deciding it, never deferring it.
const APPROVAL_KEYS: &str = "[y] deliver   [n]/[Esc] deny (drop)";

/// What the listing shows over an empty buffer.
const EMPTY: &str = "nothing is held for review";

/// Columns between a listing row's fixed columns and its preview.
const GAP: usize = 2;

/// The hold cause as the review surfaces name it, one spelling per surface
/// need — three surfaces render this enum (the modal, the listing rows, the
/// sender's held note), and the first two live here.
///
/// The vocabulary is v1-only supplementary fact (b)'s cause list, mapped onto
/// the causes ganja can hold for; the project tier keeps that fact's own name
/// for it, "repository tightening".
fn cause_sentence(cause: HoldCause) -> String {
    match cause {
        HoldCause::Explicit { source } => format!(
            "explicit settings: cross_session_inbound is \"hold\" ({})",
            match source {
                PolicySource::Global => "the global config",
                PolicySource::ExplicitFile => "the GANJA_CONFIG file",
                PolicySource::Project => "repository tightening",
            }
        ),
        HoldCause::ModeMismatch => {
            "mode mismatch: the sender and this session assert different permission classes"
                .to_owned()
        }
        HoldCause::NoModeAsserted => {
            "missing sender mode: this session runs bypassed and the sender asserted no class"
                .to_owned()
        }
        HoldCause::ModeUnknown => {
            "startup mode uncertainty: this session's own permission mode could not be read"
                .to_owned()
        }
    }
}

/// The same cause as a listing column: short enough to leave the row its
/// preview.
pub fn cause_label(cause: HoldCause) -> &'static str {
    match cause {
        HoldCause::Explicit { .. } => "explicit",
        HoldCause::ModeMismatch => "mode mismatch",
        HoldCause::NoModeAsserted => "no sender mode",
        HoldCause::ModeUnknown => "mode unknown",
    }
}

/// A deadline (or an age) in the coarsest unit that still moves: minutes
/// while minutes remain, seconds inside the last one. Ceiling rather than
/// floor so a deadline of five minutes reads "5m" the moment it is armed,
/// not "4m" one frame later.
fn coarse(duration: Duration) -> String {
    let seconds = duration.as_millis().div_ceil(1000);
    if seconds >= 60 { format!("{}m", seconds.div_ceil(60)) } else { format!("{seconds}s") }
}

/// One held message under a person's review: the approval modal's state.
#[derive(Clone, Debug)]
pub struct HeldApproval {
    id: HeldId,
    from: String,
    cause: HoldCause,
    /// The sender's own one-line summary, where it wrote one — display-capped
    /// here, since the engine caps the preview and deliberately not this.
    summary: Option<String>,
    /// The body's opening, pre-capped by the engine (8 lines / 1024 chars,
    /// control-stripped) — the expandable body preview of fact (b).
    preview: String,
    /// When the hold expires on its own. `Some` exactly for the parity
    /// causes, which are the only causes that raise this modal at all.
    deadline: Option<Instant>,
}

impl HeldApproval {
    /// Builds the modal's state for one `PeerHeld` event.
    #[must_use]
    pub fn new(
        id: HeldId,
        from: String,
        cause: HoldCause,
        summary: Option<String>,
        preview: String,
        expires_in_ms: Option<u64>,
    ) -> Self {
        Self {
            id,
            from,
            cause,
            summary: summary.map(|summary| cap_for_display(&summary).to_owned()),
            preview,
            deadline: expires_in_ms
                .map(|remaining| Instant::now() + Duration::from_millis(remaining)),
        }
    }

    /// The hold this modal is reviewing, so a caller can tell whether an
    /// incoming `PeerHoldSettled` names it, and settle it by id.
    #[must_use]
    pub fn id(&self) -> &HeldId {
        &self.id
    }

    /// Fact (b)'s one-line preview: the summary where the sender wrote one,
    /// else the body's first line, display-capped either way.
    fn one_line(&self) -> String {
        self.summary.clone().unwrap_or_else(|| {
            cap_for_display(self.preview.lines().next().unwrap_or_default()).to_owned()
        })
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let (popup, _, _) = modal(area, MAX_WIDTH, MAX_HEIGHT, 0);

        Clear.render(popup, buffer);

        let block = Block::bordered().title(" held message ");
        let inner = block.inner(popup);

        Paragraph::new(Text::from(self.lines(inner, theme)))
            .block(block)
            .style(theme.fg)
            .render(popup, buffer);
    }

    /// The modal's text against the room it actually has — the permission
    /// dialog's own budget discipline, because the consent problem is the
    /// same: `y` delivers foreign text into the conversation, and consent to
    /// a body whose tail was cut without a word is not consent.
    fn lines(&self, inner: Rect, theme: &Theme) -> Vec<Line<'static>> {
        // Fact (b)'s five items in fact (b)'s own order, under the claimed
        // identity's header row; the trust labels are the row text itself.
        let mut body = vec![
            (format!("from (claimed): {}", self.from), theme.accent),
            (format!("cause: {}", cause_sentence(self.cause)), theme.warning),
            (String::new(), theme.fg),
            (REPLY_ADDRESS.to_owned(), theme.dim),
            (VERIFIED_PID.to_owned(), theme.dim),
            (format!("preview: {}", self.one_line()), theme.fg),
            (String::new(), theme.fg),
        ];
        body.extend(self.preview.lines().map(|line| (line.to_owned(), theme.dim)));

        let mut tail = vec![(String::new(), theme.fg), (NOT_DELIVERED.to_owned(), theme.warning)];
        if let Some(deadline) = self.deadline {
            tail.push((
                format!(
                    "expires in {}",
                    coarse(deadline.saturating_duration_since(Instant::now()))
                ),
                theme.dim,
            ));
        }
        tail.push((APPROVAL_KEYS.to_owned(), theme.dim));

        let width = usize::from(inner.width);
        let height = usize::from(inner.height);
        let mut rows = wrap_all(&body, width);
        let tail_rows = wrap_all(&tail, width);

        // The permission dialog's overflow discipline, verbatim in shape: the
        // answers and the not-delivered statement outrank the body, and a cut
        // body is admitted rather than silently ended.
        if width > 0 && height > 0 {
            let room = height.saturating_sub(tail_rows.len());
            if rows.len() > room {
                let reserved =
                    wrap_all(&[(overflow_marker(rows.len()), theme.accent)], width).len().min(room);
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
}

/// What Enter on a listing row offers. Both actions belong on every row: a
/// hold's review is exactly this choice, whatever its cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Deliver it — subject to the engine's own policy re-check.
    Release,
    /// Drop it.
    Deny,
}

impl Action {
    /// The label the action step shows for it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Release => "Release (deliver it)",
            Self::Deny => "Deny (drop it)",
        }
    }

    /// The decision a chosen action sends.
    #[must_use]
    pub fn decision(self) -> HeldDecision {
        match self {
            Self::Release => HeldDecision::Release,
            Self::Deny => HeldDecision::Deny,
        }
    }
}

/// The two actions every row offers, in the order the step lists them.
const ACTIONS: [Action; 2] = [Action::Release, Action::Deny];

/// One held entry, as the listing shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// What a chosen action settles, never shown.
    pub id: HeldId,
    /// The sender's claimed identity.
    pub from: String,
    /// Why it is held, as [`cause_label`] spells it.
    pub cause: &'static str,
    /// How long it has been held, already in the countdown's own coarse
    /// format — minutes while minutes remain, seconds inside the last one.
    pub age: String,
    /// The one-line preview, display-capped by the caller.
    pub preview: String,
}

impl Row {
    /// The listing's row for one `Engine::held_messages` entry: the age
    /// coarsened, the cause shortened, the preview the summary where the
    /// sender wrote one and the body's first line otherwise.
    #[must_use]
    pub fn new(
        id: HeldId,
        from: String,
        cause: HoldCause,
        age: Duration,
        summary: Option<&str>,
        preview: &str,
    ) -> Self {
        let line = summary.unwrap_or_else(|| preview.lines().next().unwrap_or_default());

        Self {
            id,
            from,
            cause: cause_label(cause),
            age: coarse(age),
            preview: cap_for_display(line).to_owned(),
        }
    }
}

/// Which of the listing's two steps is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Choosing a held entry.
    Rows,
    /// Choosing what to do about it, by index into [`ACTIONS`].
    Actions(usize),
}

/// The held entries, which one is under the cursor, and which step is
/// showing — `/held`'s state.
#[derive(Clone, Debug)]
pub struct HeldList {
    rows: Vec<Row>,
    /// Index into [`HeldList::rows`]; clamped in range while it is non-empty.
    selected: usize,
    step: Step,
}

impl HeldList {
    /// Opens the listing over `rows`, cursor on the first one.
    #[must_use]
    pub fn new(rows: Vec<Row>) -> Self {
        Self { rows, selected: 0, step: Step::Rows }
    }

    /// Replaces the rows with a fresh poll, keeping the cursor where it was.
    /// Unlike the `/mcp` list this one shrinks under a person — a settlement
    /// retires its row — so the reclamp is load-bearing, not defensive.
    pub fn refresh(&mut self, rows: Vec<Row>) {
        self.rows = rows;
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        if self.rows.is_empty() {
            self.step = Step::Rows;
        }
    }

    /// Whether the action step is the one on screen.
    #[must_use]
    pub fn is_choosing_action(&self) -> bool {
        matches!(self.step, Step::Actions(_))
    }

    /// The entry under the cursor, or [`None`] over an empty buffer.
    #[must_use]
    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Moves whichever list is showing by `delta` rows.
    pub fn move_selection(&mut self, delta: isize) {
        match self.step {
            Step::Rows => self.selected = clamped(self.selected, delta, self.rows.len()),
            Step::Actions(option) => {
                self.step = Step::Actions(clamped(option, delta, ACTIONS.len()))
            }
        }
    }

    /// Enter on the row step: opens the action choice for the entry under the
    /// cursor. `false` over an empty buffer, where there is nothing to act on.
    pub fn advance(&mut self) -> bool {
        if self.selected().is_none() {
            return false;
        }
        self.step = Step::Actions(0);

        true
    }

    /// Enter on the action step: the hold and the chosen action. [`None`]
    /// while the row step is showing.
    #[must_use]
    pub fn chosen(&self) -> Option<(&HeldId, Action)> {
        let Step::Actions(option) = self.step else {
            return None;
        };
        let row = self.selected()?;

        Some((&row.id, ACTIONS[option]))
    }

    /// Leaves the action step for the rows without closing the dialog, so a
    /// settlement's outcome shows up as the row the next poll retires.
    pub fn back_to_rows(&mut self) {
        self.step = Step::Rows;
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let available = area.height.saturating_sub(2).clamp(1, MAX_HEIGHT);
        let inner_width = usize::from(width).saturating_sub(2);
        let rows = body_rows(available, CHROME);

        let mut lines = match self.step {
            Step::Rows => self.held_rows(inner_width, rows, theme),
            Step::Actions(option) => self.action_rows(inner_width, option, theme),
        };
        let hints = match self.step {
            Step::Rows => LIST_HINTS,
            Step::Actions(_) => ACTION_HINTS,
        };
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(hints, inner_width), theme.dim));

        // The row step takes the screenful, the action step exactly its two
        // answers — the `/mcp` dialog's own two-height scheme.
        let height = match self.step {
            Step::Rows => available,
            Step::Actions(_) => {
                u16::try_from(lines.len().saturating_add(2)).unwrap_or(available).min(available)
            }
        };
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" held "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// One line per visible held entry: sender, cause, age, preview.
    fn held_rows(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.rows.is_empty() {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        }

        let first = first_visible(self.selected, rows);
        let from_width = self.rows.iter().map(|row| row.from.width()).max().unwrap_or(0);
        let cause_width = self.rows.iter().map(|row| row.cause.width()).max().unwrap_or(0);

        self.rows
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(index, row)| {
                let head = format!(
                    "{marker}{from:<from_width$}  {cause:<cause_width$}  {age:>4}",
                    marker = if index == self.selected { MARKER } else { "  " },
                    from = row.from,
                    cause = row.cause,
                    age = row.age,
                );
                let preview_width = width.saturating_sub(head.width() + GAP).max(1);
                let line = if row.preview.is_empty() {
                    head
                } else {
                    format!(
                        "{head}{gap}{preview}",
                        gap = " ".repeat(GAP),
                        preview = clip(&row.preview, preview_width),
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

    /// The action step: which entry it is about, and the two answers.
    fn action_rows(&self, width: usize, option: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(row) = self.selected() else {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        };

        let mut lines =
            vec![Line::styled(clip(&format!("{} ({})", row.from, row.cause), width), theme.fg)];
        for (index, action) in ACTIONS.iter().enumerate() {
            lines.push(action_row(index, option, action.label(), width, theme));
        }

        lines
    }
}

#[cfg(test)]
#[path = "held_tests.rs"]
mod tests;
