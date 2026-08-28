//! The Ctrl+R history search: a fuzzy filter over remembered prompts, shown
//! newest-first with a relative age and a full preview of the one under the
//! cursor.
//!
//! No upstream counterpart (deviation **D447**, at the keybind row in
//! `keybind.rs`): upstream's ctrl+r is `session_rename`
//! (`packages/tui/src/config/keybind.ts:93`), a command ganja has never had,
//! so the chord was free. The whole feature and its layout are Claude Code's
//! (screenshot 2026-08-11, the Ctrl+R panel): a match list on top, a preview
//! of the selected entry below it, and the typed query along the bottom edge.
//!
//! Matching decides membership with [`nucleo_matcher`], the same fuzzy
//! ranking `command.rs`'s palette narrowing already uses, but the
//! *ordering* stays newest-first regardless of score — a fuzzy filter over a
//! shell-style reverse-search, not a ranked search result list. An empty
//! query lists every entry.
//!
//! Claude Code's `ctrl+s` scope toggle ("everywhere" vs "this project") is
//! **not built**: ganja keeps one 50-entry per-project prompt history (P8's
//! pinned shape), so there is nothing to toggle between — raising retention
//! or adding scopes is a follow-up, not this wave.
//!
//! The dialog owns the query, the match list and which row is under the
//! cursor — and, so [`crate::app::App`]'s Esc has something to put back
//! without duplicating the capture, the exact composer buffer it opened
//! over. Filling the composer and closing the dialog are `App`'s, the same
//! split every other dialog here uses.

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Widget as _};
use unicode_width::UnicodeWidthStr as _;

use crate::component::chat::clip;
use crate::component::sessions::age;
use crate::component::{clamped, first_visible};
use crate::history::Recalled;
use crate::theme::Theme;

/// What marks the row the cursor is on, and what pads every other row.
const MARKER: &str = "> ";

/// Rows the dialog spends on chrome rather than the list or the preview: one
/// blank line above the preview, one blank line above the query, the query
/// line itself, and the hint line.
const CHROME: usize = 4;

/// Widest the modal grows. Prompts run long; the preview pane is the reason
/// to read one, so this is wider than the other pickers.
const MAX_WIDTH: u16 = 76;

/// Tallest the modal grows.
const MAX_HEIGHT: u16 = 24;

/// Gap between a row's title and its age.
const GAP: usize = 2;

/// The keys the dialog answers to, shown along its bottom edge.
const HINTS: &str = "[type] search   [up/down] move   [Enter] fill   [Esc] close";

/// What the query line says while nothing has been typed.
const PLACEHOLDER: &str = "search history";

/// What the match list says when the store holds nothing at all.
const NO_HISTORY: &str = "no prompts remembered yet";

/// What the match list says when a query matches nothing.
const NO_MATCHES: &str = "no matches";

/// The remembered prompts, the query narrowing them, and which one is under
/// the cursor.
#[derive(Clone, Debug)]
pub struct HistorySearch {
    /// Every remembered submission, newest first, as [`crate::history::History::entries`]
    /// hands them over.
    entries: Vec<Recalled>,
    /// What has been typed into the query line.
    query: String,
    /// Indices into [`HistorySearch::entries`] the current query matches,
    /// still newest-first — a fuzzy *filter*, not a ranked re-sort.
    matched: Vec<usize>,
    /// Index into [`HistorySearch::matched`]; always in range while it is
    /// non-empty.
    selected: usize,
    /// The wall clock every row is aged against, captured once at open so the
    /// list does not silently re-age between frames — the sessions picker's
    /// convention.
    opened: u64,
    /// The composer buffer this dialog opened over, and where the cursor sat
    /// in it — what an Esc puts back, byte for byte.
    origin_text: String,
    /// See [`HistorySearch::origin_text`].
    origin_cursor: (usize, usize),
}

impl HistorySearch {
    /// Opens the search over `entries`, remembering the composer's current
    /// `origin_text` and `origin_cursor` for an Esc to restore.
    #[must_use]
    pub fn new(
        entries: Vec<Recalled>,
        now: u64,
        origin_text: impl Into<String>,
        origin_cursor: (usize, usize),
    ) -> Self {
        let mut search = Self {
            entries,
            query: String::new(),
            matched: Vec::new(),
            selected: 0,
            opened: now,
            origin_text: origin_text.into(),
            origin_cursor,
        };
        search.refresh();

        search
    }

    /// The buffer an Esc restores.
    #[must_use]
    pub fn origin_text(&self) -> &str {
        &self.origin_text
    }

    /// The cursor position an Esc restores.
    #[must_use]
    pub fn origin_cursor(&self) -> (usize, usize) {
        self.origin_cursor
    }

    /// What has been typed into the query line.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Adds `character` to the query and re-narrows the matches.
    pub fn push(&mut self, character: char) {
        self.query.push(character);
        self.refresh();
    }

    /// Takes the last character back off the query and re-narrows.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.refresh();
    }

    /// The prompt under the cursor, or [`None`] when nothing matches.
    #[must_use]
    pub fn selected(&self) -> Option<&crate::history::PromptInfo> {
        self.matched.get(self.selected).map(|&index| &self.entries[index].prompt)
    }

    /// Moves the cursor by `delta` rows.
    ///
    /// Clamped rather than wrapped, like every other list here: running off
    /// one end and landing on the other is never what the keypress meant.
    pub fn move_selection(&mut self, delta: isize) {
        self.selected = clamped(self.selected, delta, self.matched.len());
    }

    /// Re-narrows [`HistorySearch::matched`] to whatever the query fuzzy-
    /// matches, keeping the newest-first order [`HistorySearch::entries`]
    /// already has.
    fn refresh(&mut self) {
        let needle = self.query.trim();
        self.matched = if needle.is_empty() {
            (0..self.entries.len()).collect()
        } else {
            let mut matcher = Matcher::new(Config::DEFAULT);
            let atom = Atom::new(
                needle,
                CaseMatching::Ignore,
                Normalization::Smart,
                AtomKind::Fuzzy,
                false,
            );

            self.entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| fuzzy_matches(&atom, &mut matcher, &entry.prompt.input))
                .map(|(index, _)| index)
                .collect()
        };
        self.selected = 0;
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let height = area.height.saturating_sub(2).clamp(1, MAX_HEIGHT);
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);

        // Inside the border on both axes.
        let inner_width = usize::from(width).saturating_sub(2);
        let content_rows = usize::from(height).saturating_sub(2).saturating_sub(CHROME).max(2);
        // The list and the preview split what is left roughly in half; the
        // list rounds up so a short terminal still shows at least one row of
        // each rather than starving the preview outright.
        let list_rows = content_rows.div_ceil(2).max(1);
        let preview_rows = content_rows.saturating_sub(list_rows).max(1);

        let mut lines = self.rows(inner_width, list_rows, theme);
        lines.push(Line::raw(""));
        lines.extend(self.preview(inner_width, preview_rows, theme));
        lines.push(Line::raw(""));
        lines.push(self.query_line(inner_width, theme));
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" history search "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// The line the query is typed on.
    fn query_line(&self, width: usize, theme: &Theme) -> Line<'static> {
        if self.query.is_empty() {
            return Line::styled(clip(PLACEHOLDER, width), theme.dim);
        }

        Line::styled(clip(&self.query, width), theme.accent)
    }

    /// One line per visible match, aligned into two columns: the title and
    /// its age.
    fn rows(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.entries.is_empty() {
            return vec![Line::styled(clip(NO_HISTORY, width), theme.dim)];
        }
        if self.matched.is_empty() {
            return vec![Line::styled(clip(NO_MATCHES, width), theme.dim)];
        }

        let first = first_visible(self.selected, rows);
        let visible: Vec<(usize, &Recalled)> = self
            .matched
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(position, &index)| (position, &self.entries[index]))
            .collect();

        let ages: Vec<String> =
            visible.iter().map(|(_, entry)| age(self.opened, entry.at)).collect();
        let age_width = ages.iter().map(|age| age.width()).max().unwrap_or(0);
        let title_width = width.saturating_sub(MARKER.width() + age_width + GAP).max(1);

        visible
            .iter()
            .zip(ages.iter())
            .map(|((position, entry), age)| {
                let title = clip(title(&entry.prompt.input), title_width);
                let row = format!(
                    "{marker}{title:<title_width$}{gap}{age:>age_width$}",
                    marker = if *position == self.selected { MARKER } else { "  " },
                    gap = " ".repeat(GAP),
                );

                Line::styled(
                    row,
                    if *position == self.selected { theme.selection } else { theme.fg },
                )
            })
            .collect()
    }

    /// The selected entry's full text, or nothing when there is none.
    fn preview(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(&index) = self.matched.get(self.selected) else {
            return Vec::new();
        };

        preview_lines(&self.entries[index].prompt.input, rows, width, theme)
    }
}

/// Whether `atom` fuzzy-matches `text` at all.
///
/// Only membership is asked for; [`HistorySearch::refresh`] keeps the
/// newest-first order regardless of the score nucleo would rank it at — a
/// reverse-search filter, not a ranked result list.
fn fuzzy_matches(atom: &Atom, matcher: &mut Matcher, text: &str) -> bool {
    let mut buffer = Vec::new();

    atom.score(Utf32Str::new(text, &mut buffer), matcher).is_some()
}

/// What a match row shows for the prompt: its first line, trailing carriage
/// returns trimmed for a file that crossed from a CRLF machine.
fn title(input: &str) -> &str {
    input.lines().next().unwrap_or(input).trim_end_matches('\r')
}

/// `text` rendered whole across up to `rows` lines, or truncated with a
/// trailing `+N lines` marker when it does not fit.
///
/// The marker replaces one content line rather than appending past `rows`,
/// so the preview never grows past the space it was given.
fn preview_lines(text: &str, rows: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    if rows == 0 {
        return Vec::new();
    }

    let all: Vec<&str> = text.lines().collect();
    if all.len() <= rows {
        return all.iter().map(|line| Line::styled(clip(line, width), theme.fg)).collect();
    }

    let shown = rows - 1;
    let mut lines: Vec<Line<'static>> =
        all[..shown].iter().map(|line| Line::styled(clip(line, width), theme.fg)).collect();
    lines.push(Line::styled(format!("+{} lines", all.len() - shown), theme.dim));

    lines
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
