//! The transcript viewport.
//!
//! Each entry caches the lines it wrapped to at a given width, so a frame costs
//! one wrap per entry that actually changed plus a walk over the entries the
//! viewport crosses — never a reflow of the whole transcript. P1 renders plain
//! text; P6 slots markdown parsing in ahead of the wrap as a second, width-
//! independent cache stage.

use ratatui::{buffer::Buffer, layout::Rect, style::Style, text::Line};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::theme::Theme;

/// Lines one wheel notch moves the viewport.
pub const WHEEL_LINES: isize = 3;

/// Who produced an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Text the user submitted.
    User,
    /// Text the engine streamed back.
    Assistant,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::User => "you",
            Self::Assistant => "ganja",
        }
    }
}

/// A scrollable transcript of plain-text entries.
#[derive(Debug, Default)]
pub struct Chat {
    entries: Vec<Entry>,
    /// Where the viewport starts, or [`None`] to stay pinned to the tail.
    offset: Option<usize>,
    /// Height of the last viewport rendered; paging and clamping need it.
    height: usize,
}

#[derive(Debug)]
struct Entry {
    role: Role,
    text: String,
    wrapped: Option<Wrapped>,
}

#[derive(Debug)]
struct Wrapped {
    width: u16,
    lines: Vec<Line<'static>>,
}

impl Chat {
    /// Appends a new entry and returns to following the tail.
    pub fn push(&mut self, role: Role, text: String) {
        self.entries.push(Entry {
            role,
            text,
            wrapped: None,
        });
        self.follow_tail();
    }

    /// Extends the newest entry, which is how a streamed reply grows.
    ///
    /// Does nothing on an empty transcript: the engine always announces a turn
    /// before its first fragment.
    pub fn extend_last(&mut self, text: &str) {
        if let Some(entry) = self.entries.last_mut() {
            entry.text.push_str(text);
            entry.wrapped = None;
        }
    }

    /// Moves the viewport by `delta` lines, negative being towards the top.
    pub fn scroll_lines(&mut self, delta: isize) {
        let current = self.offset.unwrap_or_else(|| self.max_offset());
        let target = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta.unsigned_abs())
        };

        self.set_offset(target);
    }

    /// Moves the viewport by `delta` screenfuls, keeping one line of overlap so
    /// the reader has an anchor.
    pub fn scroll_pages(&mut self, delta: isize) {
        let page = self.height.saturating_sub(1).max(1);
        let lines = isize::try_from(page).unwrap_or(isize::MAX);

        self.scroll_lines(delta.saturating_mul(lines));
    }

    /// Pins the viewport to the newest line.
    pub fn follow_tail(&mut self) {
        self.offset = None;
    }

    /// Whether new text will scroll into view on its own.
    #[must_use]
    pub fn is_following_tail(&self) -> bool {
        self.offset.is_none()
    }

    /// Draws the visible slice of the transcript into `area`.
    pub fn render(&mut self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        self.height = usize::from(area.height);
        for entry in &mut self.entries {
            entry.wrap(area.width, theme);
        }

        let offset = self
            .offset
            .map_or_else(|| self.max_offset(), |offset| offset.min(self.max_offset()));
        self.offset = self.offset.map(|_| offset);

        for (row, line) in self.visible(offset).enumerate() {
            let Ok(row) = u16::try_from(row) else {
                break;
            };
            buffer.set_line(area.x, area.y + row, line, area.width);
        }
    }

    /// Lines the whole transcript wrapped to at the last rendered width.
    pub(crate) fn line_count(&self) -> usize {
        self.entries.iter().map(|entry| entry.lines().len()).sum()
    }

    /// Widths the entries are currently cached at, which is how a test tells
    /// that a resize actually invalidated the cache.
    #[cfg(test)]
    pub(crate) fn cached_widths(&self) -> Vec<Option<u16>> {
        self.entries
            .iter()
            .map(|entry| entry.wrapped.as_ref().map(|wrapped| wrapped.width))
            .collect()
    }

    fn max_offset(&self) -> usize {
        self.line_count().saturating_sub(self.height)
    }

    /// Follows the tail again once the viewport reaches the bottom, so that
    /// scrolling down past the end behaves like never having scrolled.
    fn set_offset(&mut self, offset: usize) {
        let max = self.max_offset();
        self.offset = if offset >= max { None } else { Some(offset) };
    }

    /// Yields the lines the viewport shows, skipping whole entries rather than
    /// stepping over every line above the offset.
    fn visible(&self, offset: usize) -> impl Iterator<Item = &Line<'static>> {
        let mut left_to_skip = offset;

        self.entries
            .iter()
            .flat_map(move |entry| {
                let lines = entry.lines();
                let skip = left_to_skip.min(lines.len());
                left_to_skip -= skip;
                &lines[skip..]
            })
            .take(self.height)
    }
}

impl Entry {
    fn lines(&self) -> &[Line<'static>] {
        self.wrapped
            .as_ref()
            .map_or(&[], |wrapped| wrapped.lines.as_slice())
    }

    fn wrap(&mut self, width: u16, theme: &Theme) {
        if self
            .wrapped
            .as_ref()
            .is_some_and(|wrapped| wrapped.width == width)
        {
            return;
        }

        let body_style = match self.role {
            Role::User => theme.accent,
            Role::Assistant => theme.fg,
        };

        let mut lines = vec![Line::styled(self.role.label().to_owned(), theme.dim)];
        lines.extend(
            wrap(&self.text, usize::from(width))
                .into_iter()
                .map(|line| Line::styled(line, body_style)),
        );
        // Breathing room before the next entry.
        lines.push(Line::styled(String::new(), Style::default()));

        self.wrapped = Some(Wrapped { width, lines });
    }
}

/// Greedily wraps `text` to `width` display columns, preserving blank lines and
/// chopping words too wide to ever fit.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();

    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0;

        for word in paragraph.split_whitespace() {
            let mut rest = word;

            while !rest.is_empty() {
                let separator = usize::from(line_width != 0);
                let room = width.saturating_sub(line_width + separator);

                if rest.width() <= room {
                    if separator == 1 {
                        line.push(' ');
                        line_width += 1;
                    }
                    line.push_str(rest);
                    line_width += rest.width();
                    break;
                }

                if line_width != 0 {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0;
                    continue;
                }

                let (head, tail) = split_at_width(rest, width);
                lines.push(head.to_owned());
                rest = tail;
            }
        }

        lines.push(line);
    }

    lines
}

/// Splits `text` at the last boundary that fits in `width` columns, always
/// consuming at least one character so callers cannot loop forever.
fn split_at_width(text: &str, width: usize) -> (&str, &str) {
    let mut used = 0;

    for (index, character) in text.char_indices() {
        let advance = character.width().unwrap_or(0);
        if index > 0 && used + advance > width {
            return text.split_at(index);
        }
        used += advance;
    }

    (text, "")
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Chat, Role, split_at_width, wrap};
    use crate::theme::Theme;

    const VIEWPORT: Rect = Rect {
        x: 0,
        y: 0,
        width: 20,
        height: 6,
    };

    fn rendered(chat: &mut Chat, area: Rect) -> Vec<String> {
        let mut buffer = Buffer::empty(area);
        chat.render(area, &mut buffer, &Theme::default());

        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn wrapping_breaks_on_word_boundaries() {
        assert_eq!(
            wrap("the quick brown fox", 10),
            vec!["the quick".to_owned(), "brown fox".to_owned()]
        );
    }

    #[test]
    fn wrapping_preserves_blank_lines_between_paragraphs() {
        assert_eq!(
            wrap("one\n\ntwo", 10),
            vec!["one".to_owned(), String::new(), "two".to_owned()]
        );
    }

    #[test]
    fn a_word_wider_than_the_viewport_is_chopped_not_dropped() {
        assert_eq!(
            wrap("abcdefghij", 4),
            vec!["abcd".to_owned(), "efgh".to_owned(), "ij".to_owned()]
        );
    }

    #[test]
    fn wrapping_measures_display_width_not_bytes() {
        // Each of these is two columns wide, so only two fit on a five-column
        // line.
        assert_eq!(
            wrap("ああ ああ", 5),
            vec!["ああ".to_owned(), "ああ".to_owned()]
        );
    }

    #[test]
    fn a_zero_width_viewport_wraps_to_nothing() {
        assert!(wrap("anything", 0).is_empty());
    }

    #[test]
    fn splitting_always_consumes_a_character() {
        // A double-width character cannot fit in one column, but returning an
        // empty head would spin the caller forever.
        assert_eq!(split_at_width("ああ", 1), ("あ", "あ"));
    }

    #[test]
    fn a_new_entry_scrolls_into_view() {
        let mut chat = Chat::default();
        for index in 0..20 {
            chat.push(Role::Assistant, format!("entry {index}"));
        }

        let lines = rendered(&mut chat, VIEWPORT);

        assert!(
            lines.iter().any(|line| line.contains("entry 19")),
            "the newest entry should be visible, got {lines:?}"
        );
        assert!(chat.is_following_tail());
    }

    #[test]
    fn scrolling_up_pins_the_viewport_and_scrolling_back_down_releases_it() {
        let mut chat = Chat::default();
        for index in 0..20 {
            chat.push(Role::Assistant, format!("entry {index}"));
        }
        rendered(&mut chat, VIEWPORT);

        chat.scroll_lines(-9);
        assert!(!chat.is_following_tail());
        let lines = rendered(&mut chat, VIEWPORT);
        assert!(
            !lines.iter().any(|line| line.contains("entry 19")),
            "a pinned viewport should not show the tail, got {lines:?}"
        );

        chat.scroll_lines(100);
        assert!(chat.is_following_tail());
    }

    #[test]
    fn paging_moves_about_a_screenful() {
        let mut chat = Chat::default();
        for index in 0..40 {
            chat.push(Role::Assistant, format!("entry {index}"));
        }
        rendered(&mut chat, VIEWPORT);

        chat.scroll_pages(-4);
        let after_paging_up = rendered(&mut chat, VIEWPORT);
        chat.scroll_pages(1);
        let after_paging_down = rendered(&mut chat, VIEWPORT);

        assert_ne!(after_paging_up, after_paging_down);
        chat.follow_tail();
        assert!(chat.is_following_tail());
    }

    #[test]
    fn a_streamed_entry_grows_in_place() {
        let mut chat = Chat::default();
        chat.push(Role::Assistant, String::new());
        chat.extend_last("hello ");
        chat.extend_last("world");

        let lines = rendered(&mut chat, VIEWPORT);

        assert!(
            lines.iter().any(|line| line == "hello world"),
            "streamed fragments should join into one entry, got {lines:?}"
        );
    }

    #[test]
    fn extending_an_empty_transcript_is_a_no_op() {
        let mut chat = Chat::default();
        chat.extend_last("orphan");

        assert!(rendered(&mut chat, VIEWPORT).iter().all(String::is_empty));
    }
}
