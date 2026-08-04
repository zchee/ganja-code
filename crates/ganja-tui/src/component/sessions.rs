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
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{component::chat::split_at_width, theme::Theme};

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
        let last = self.entries.len().saturating_sub(1);
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

        let width = area.width.saturating_sub(4).clamp(1, 76);
        let height = area.height.saturating_sub(2).clamp(1, 20);
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);

        // Inside the border on both axes.
        let inner_width = usize::from(width).saturating_sub(2);
        let rows = usize::from(height)
            .saturating_sub(2)
            .saturating_sub(CHROME)
            .max(1);

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
        let first = self.first_visible(rows);
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

    /// The first session on screen: far enough down to keep the selected one
    /// visible, and no further.
    fn first_visible(&self, rows: usize) -> usize {
        self.selected.saturating_sub(rows.saturating_sub(1))
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
fn age(now: u64, updated: u64) -> String {
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

/// `text` cut to `width` display columns.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }

    split_at_width(text, width).0.to_owned()
}

#[cfg(test)]
mod tests {
    use ganja_core::{SessionId, SessionInfo, Usage, storage::VERSION};
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{DAY, HOUR, MINUTE, Sessions, age};
    use crate::theme::Theme;

    /// The moment every fixture is aged against; sessions are placed relative
    /// to it so a test asserts on the interval it asked for.
    const NOW: u64 = 1_000 * DAY;

    fn info(id: &str, title: Option<&str>, updated: u64, tokens: u64) -> SessionInfo {
        SessionInfo {
            id: SessionId::from(id.to_owned()),
            version: VERSION,
            title: title.map(str::to_owned),
            created: 0,
            updated,
            usage: Usage {
                input_tokens: tokens,
                ..Usage::default()
            },
            context_tokens: 0,
            summary: None,
            agent: None,
            model: None,
            parent: None,
            revert: None,
        }
    }

    /// Two sessions, newest first, as the engine lists them.
    fn sessions() -> Sessions {
        Sessions::new(
            vec![
                info(
                    "ses_newer",
                    Some("porting storage"),
                    NOW - 5 * MINUTE,
                    1_234,
                ),
                info("ses_older", None, NOW - 3 * HOUR, 42),
            ],
            NOW,
        )
    }

    fn rendered(sessions: &Sessions, area: Rect) -> String {
        let mut buffer = Buffer::empty(area);
        sessions.render(area, &mut buffer, &Theme::default());

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
    fn the_list_shows_what_a_person_chooses_by() {
        let screen = rendered(&sessions(), Rect::new(0, 0, 80, 20));

        assert!(screen.contains("porting storage"), "got:\n{screen}");
        assert!(screen.contains("5m ago"), "got:\n{screen}");
        assert!(screen.contains("3h ago"), "got:\n{screen}");
        assert!(screen.contains("1.2k tokens"), "got:\n{screen}");
        assert!(screen.contains("42 tokens"), "got:\n{screen}");
    }

    /// A session that never earned a title still has to be identifiable, and
    /// by the same string `--session` would take.
    #[test]
    fn an_untitled_session_is_listed_by_its_id() {
        let screen = rendered(&sessions(), Rect::new(0, 0, 80, 20));

        assert!(screen.contains("ses_older"), "got:\n{screen}");
    }

    #[test]
    fn a_title_of_only_whitespace_falls_back_to_the_id_too() {
        let sessions = Sessions::new(vec![info("ses_blank", Some("   "), NOW, 0)], NOW);

        assert!(
            rendered(&sessions, Rect::new(0, 0, 80, 20)).contains("ses_blank"),
            "a blank title is no title"
        );
    }

    #[test]
    fn the_selection_starts_on_the_newest_and_moves_within_the_list() {
        let mut sessions = sessions();
        assert_eq!(
            sessions.selected().map(|info| info.id.as_str()),
            Some("ses_newer")
        );

        sessions.move_selection(1);
        assert_eq!(
            sessions.selected().map(|info| info.id.as_str()),
            Some("ses_older")
        );

        // Clamped at both ends rather than wrapping around.
        sessions.move_selection(9);
        assert_eq!(
            sessions.selected().map(|info| info.id.as_str()),
            Some("ses_older")
        );
        sessions.move_selection(-9);
        assert_eq!(
            sessions.selected().map(|info| info.id.as_str()),
            Some("ses_newer")
        );
    }

    #[test]
    fn the_marker_follows_the_selection() {
        let mut sessions = sessions();
        let first = rendered(&sessions, Rect::new(0, 0, 80, 20));
        sessions.move_selection(1);
        let second = rendered(&sessions, Rect::new(0, 0, 80, 20));

        assert!(first.contains("> porting storage"), "got:\n{first}");
        assert!(second.contains("> ses_older"), "got:\n{second}");
        assert!(
            !second.contains("> porting storage"),
            "only one row is selected:\n{second}"
        );
    }

    /// More sessions than rows: the list has to move under the selection, or
    /// the user cannot reach what they are selecting.
    #[test]
    fn a_selection_below_the_fold_scrolls_the_list_to_it() {
        let entries = (0..40_u32)
            .map(|index| {
                info(
                    &format!("ses_{index:02}"),
                    Some(&format!("session number {index:02}")),
                    NOW - u64::from(index) * MINUTE,
                    0,
                )
            })
            .collect();
        let mut sessions = Sessions::new(entries, NOW);
        let area = Rect::new(0, 0, 80, 20);

        let top = rendered(&sessions, area);
        assert!(top.contains("session number 00"), "got:\n{top}");
        assert!(!top.contains("session number 39"), "got:\n{top}");

        sessions.move_selection(39);
        let bottom = rendered(&sessions, area);

        assert!(
            bottom.contains("> session number 39"),
            "the selection must be on screen:\n{bottom}"
        );
        assert!(
            !bottom.contains("session number 00"),
            "the list should have scrolled:\n{bottom}"
        );
    }

    #[test]
    fn a_title_too_wide_for_the_column_is_cut_rather_than_wrapped() {
        let sessions = Sessions::new(vec![info("ses_1", Some(&"wide ".repeat(40)), NOW, 0)], NOW);

        let screen = rendered(&sessions, Rect::new(0, 0, 60, 20));

        for line in screen.lines() {
            assert!(
                line.chars().count() <= 60,
                "a row must not overflow the dialog: {line:?}"
            );
        }
        assert!(screen.contains("wide"), "got:\n{screen}");
    }

    #[test]
    fn ages_round_to_the_unit_they_are_reported_in() {
        assert_eq!(age(NOW, NOW), "just now");
        assert_eq!(age(NOW, NOW - 59 * 1_000), "just now");
        assert_eq!(age(NOW, NOW - 5 * MINUTE), "5m ago");
        assert_eq!(age(NOW, NOW - 3 * HOUR), "3h ago");
        assert_eq!(age(NOW, NOW - 2 * DAY), "2d ago");
        // A clock that moved backwards between runs.
        assert_eq!(age(NOW, NOW + DAY), "just now");
    }

    #[test]
    fn an_empty_list_has_nothing_selected_and_does_not_panic() {
        let sessions = Sessions::new(Vec::new(), NOW);

        assert!(sessions.is_empty());
        assert!(sessions.selected().is_none());
        rendered(&sessions, Rect::new(0, 0, 80, 20));
    }

    #[test]
    fn a_zero_area_draws_nothing_and_does_not_panic() {
        rendered(&sessions(), Rect::new(0, 0, 0, 0));
    }

    /// The same protection the permission dialog is pinned for, on the other
    /// piece of chrome that renders text the model chose: a session's title is
    /// written by a titling request, so it must not be able to repaint the
    /// screen the user is choosing from.
    #[test]
    fn an_escape_sequence_in_a_title_never_reaches_the_buffer() {
        let sessions = Sessions::new(
            vec![info(
                "ses_1",
                Some("\u{1b}[2J\u{1b}[31mchoose me\u{7}"),
                NOW,
                0,
            )],
            NOW,
        );

        let screen = rendered(&sessions, Rect::new(0, 0, 80, 20));
        let leaked: Vec<char> = screen
            .chars()
            .filter(|character| *character != '\n' && character.is_control())
            .collect();

        assert!(
            leaked.is_empty(),
            "control characters reached the buffer: {leaked:?}\n{screen}"
        );
        assert!(
            screen.contains("choose me"),
            "the printable remainder still has to render:\n{screen}"
        );
    }
}
