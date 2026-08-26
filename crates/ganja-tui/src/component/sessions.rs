//! The sessions picker: a centered modal listing what this project has stored,
//! for the user to choose one to resume.
//!
//! Spec: upstream `packages/tui/src/routes/session/list.tsx`. The columns are
//! what a person actually picks by — what the session was about, how long ago
//! they touched it, and how big it got — rather than everything
//! [`SessionInfo`] happens to carry.
//!
//! Size is shown in tokens rather than in dollars on purpose: a stored session
//! does not record which model answered it, and pricing one session's tokens
//! against whichever model this run happens to be configured for would put a
//! number on the screen that nobody was ever charged.
//!
//! Ages are relative to a moment captured when the dialog opened, not to the
//! clock at render time. A list whose rows silently re-age between frames
//! would be a list that never renders the same way twice.

use ganja_core::{SessionInfo, catalog::compact_tokens};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    component::{chat::clip, clamped, first_visible, modal},
    theme::Theme,
};

/// Milliseconds in each unit an age is rounded to.
const SECOND: u64 = 1_000;
/// See [`SECOND`].
const MINUTE: u64 = 60 * SECOND;
/// See [`SECOND`].
const HOUR: u64 = 60 * MINUTE;
/// See [`SECOND`].
const DAY: u64 = 24 * HOUR;

/// Columns between the list's three fields.
const GAP: usize = 2;

/// Widest the modal grows, whatever the terminal offers.
const MAX_WIDTH: u16 = 76;

/// Tallest the modal grows, whatever the terminal offers.
const MAX_HEIGHT: u16 = 20;

/// What marks the row the user is on, and what pads every other row so the
/// titles stay in one column.
const MARKER: &str = "> ";

/// Rows the dialog spends on something other than the list: a blank line and
/// the key reminders.
const CHROME: usize = 2;

/// The keys the dialog answers to, shown along its bottom edge.
const HINTS: &str = "[j/k] [up/down] move   [Enter] resume   [Esc] close";

/// The wall clock a freshly opened picker ages its rows against.
///
/// Milliseconds since the Unix epoch, saturating rather than failing when the
/// machine's clock is set before 1970 — the same convention
/// [`ganja_core`]'s own timestamps follow.
#[must_use]
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Stored sessions, and which one the user is on.
#[derive(Clone, Debug)]
pub struct Sessions {
    /// Newest first, as [`ganja_core::Engine::sessions`] answers.
    entries: Vec<SessionInfo>,
    /// Index into [`Sessions::entries`]; always in range while it is non-empty.
    selected: usize,
    /// Milliseconds since the Unix epoch when the dialog opened.
    opened: u64,
}

impl Sessions {
    /// Builds the picker over `entries`, ageing them against `now`.
    #[must_use]
    pub fn new(entries: Vec<SessionInfo>, now: u64) -> Self {
        Self {
            entries,
            selected: 0,
            opened: now,
        }
    }

    /// Whether there is nothing to choose from.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The session the user is on, or [`None`] when the list is empty.
    #[must_use]
    pub fn selected(&self) -> Option<&SessionInfo> {
        self.entries.get(self.selected)
    }

    /// Moves the selection by `delta` rows.
    ///
    /// Clamped rather than wrapped: the list is ordered by recency, so running
    /// off the newest end and landing on the oldest session is never what the
    /// keypress meant.
    pub fn move_selection(&mut self, delta: isize) {
        self.selected = clamped(self.selected, delta, self.entries.len());
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
        lines.push(Line::styled(HINTS, theme.dim));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" sessions "))
            .style(theme.fg)
            .render(popup, buffer);
    }

    /// One line per visible session, aligned into three columns.
    fn rows(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let first = first_visible(self.selected, rows);
        let visible = self
            .entries
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .collect::<Vec<_>>();

        // The two right-hand columns are as wide as their widest value, so the
        // titles beside them line up instead of jittering per row.
        let ages: Vec<String> = visible
            .iter()
            .map(|(_, info)| age(self.opened, info.updated))
            .collect();
        let sizes: Vec<String> = visible.iter().map(|(_, info)| size(info)).collect();
        let age_width = ages.iter().map(|age| age.width()).max().unwrap_or(0);
        let size_width = sizes.iter().map(|size| size.width()).max().unwrap_or(0);
        let title_width = width
            .saturating_sub(MARKER.width() + age_width + size_width + GAP * 2)
            .max(1);

        visible
            .iter()
            .zip(ages.iter().zip(sizes.iter()))
            .map(|((index, info), (age, size))| {
                let title = clip(title(info), title_width);
                let row = format!(
                    "{marker}{title:<title_width$}{gap}{age:>age_width$}{gap}{size:>size_width$}",
                    marker = if *index == self.selected {
                        MARKER
                    } else {
                        "  "
                    },
                    gap = " ".repeat(GAP),
                );

                Line::styled(
                    row,
                    if *index == self.selected {
                        theme.accent
                    } else {
                        theme.fg
                    },
                )
            })
            .collect()
    }
}

/// What a session is called in the list.
///
/// Sessions earn a title from their first completed turn, so one that crashed
/// before that — or that ran against a provider which never titles — has none.
/// The id stands in because it is also the thing the user would have to type
/// to open that session by name.
fn title(info: &SessionInfo) -> &str {
    info.title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| info.id.as_str())
}

/// How much a session has spent, as one figure.
///
/// Every counted token, cache traffic included: what makes a session expensive
/// to reopen is the whole of what it has accumulated, not the fresh half.
fn size(info: &SessionInfo) -> String {
    let usage = &info.usage;
    let total = usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens)
        .saturating_add(usage.output_tokens);

    format!("{} tokens", compact_tokens(total))
}

/// How long ago `updated` was, seen from `now`.
///
/// A stored session whose timestamp is in the future — a clock that moved
/// backwards between runs — reads as current rather than as negative.
///
/// `pub(crate)`: the history search modal ages its rows by the same idiom
/// (`component::search`), and a second copy of four bucket comparisons would
/// be the kind of duplication worth a one-line visibility change instead.
pub(crate) fn age(now: u64, updated: u64) -> String {
    let elapsed = now.saturating_sub(updated);

    if elapsed < MINUTE {
        return "just now".to_owned();
    }
    if elapsed < HOUR {
        return format!("{}m ago", elapsed / MINUTE);
    }
    if elapsed < DAY {
        return format!("{}h ago", elapsed / HOUR);
    }

    format!("{}d ago", elapsed / DAY)
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;
