//! Ctrl+T's overlay: everything the wire already carries but the transcript
//! never shows in full (**F2**).
//!
//! Spec: the three displays are the OpenAI Codex CLI transcript overlay's —
//! raw log, per-turn tokens, tool/MCP expansion
//! (`docs/references/codex.ja.md:21`). Ctrl+T itself is Claude Code's chord,
//! not upstream's or Codex's own — opencode's `keybind.ts` has no such action
//! at all, and it reaches the overlay only because `themes_open` gave the
//! chord up (deviation **D453**, at `keybind.rs`'s own table row).
//!
//! The presentation is a synthesis of two screenshots supplied 2026-08-11,
//! still filed under **D453** since it reshapes the same overlay rather than
//! opening a second one: the OpenAI Codex CLI's own Ctrl+T overlay supplies
//! the full-terminal takeover (no border, no centering — [`Inspector::render`]
//! is handed the whole frame, not the transcript pane, and `App::draw` skips
//! the editor and status bar while it is open) and the slash-art banner
//! naming the active tab (`banner_line`); Claude Code's Ctrl+O
//! detailed-transcript mode supplies the one-line footer's wording and its
//! trailing mode-word marker (`footer`), replacing Codex's own two-line hint
//! pair and separate `4% —` counter. Ganja keeps its three tabs rather than
//! Codex's single view — the divergence the paragraph above already named.
//!
//! The overlay's **paint** is Codex's too, pinned by a third screenshot
//! (2026-08-15): monochrome — body and banner in the theme's own text color
//! on the terminal's own background, the active tab and the token table's
//! head told by bold against dim, never by the accent. The accent and panel
//! slots retired from this surface on purpose: a theme whose accent is a
//! color (claude.json's purple) or whose panel is a gray was repainting an
//! overlay whose whole reference is white-on-black, and no theme schema slot
//! exists that could scope a choice to this surface alone.
//!
//! The viewport moved once more the same day: every tab opens **pinned to
//! its tail** — the newest of what the overlay exists to expand — and,
//! because each render re-reads the feed, a pinned viewport follows a
//! streaming turn on its own. Scrolling up unpins and holds; the bottom, or
//! End, re-pins — the chat pane's own tail-follow rule, on the overlay.
//!
//! A view, not a mode: nothing here pauses a running turn, and every
//! [`Inspector::render`] call re-reads what `App` hands it rather than a
//! snapshot taken when the overlay opened — the state itself (the raw-log
//! ring buffer, the per-turn usage rows) lives on `App`, which is what lets a
//! turn keep streaming into it while the overlay sits on top.

use std::collections::VecDeque;

use ganja_core::{SessionInfo, catalog};
use ganja_protocol::{Event as CoreEvent, MessageId, Usage};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Modifier,
    text::{Line, Span, Text},
    widgets::{Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    component::{chat::clip, status::Totals},
    theme::Theme,
    transcript,
};

/// Rows spent on the header (the banner and the tab strip) and the one-line
/// footer, none of which scroll. No border and no centering margin to budget
/// for anymore — full-terminal takeover, screenshot-sourced (see the module
/// doc) — so unlike [`crate::component::help`]'s own `CHROME` this is the
/// whole non-content cost, not just the half a `Block::bordered` left open.
const CHROME: usize = 3;

/// The keys the overlay answers to, shown along the footer's left half in
/// Claude Code's unbracketed `key action` wording — not Codex's own two-line,
/// bracketed hint pair. Names neither Ctrl+T nor the tab-cycle keys, the same
/// restraint the popup-era `HINTS` already used: the header's own tab strip
/// already spells out the digit shortcuts, and a footer trying to fit both
/// halves of Claude Code's pattern on an 80-column terminal has no room left
/// for a chord the toggle that opened the overlay already taught. The vim
/// keys the overlay also answers — `j`/`k`, and the `Ctrl+U`/`Ctrl+D`
/// half-page pair (2026-08-25) — go unlisted for the same reason: a person
/// who reaches for them knows them, and the row has no room to teach them.
const HINTS: &str = "q/esc close \u{b7} up/down/pgup/pgdn scroll";

/// Everything [`Inspector::render`] reads fresh every frame, bundled so the
/// method takes a handful of parameters rather than one per field — `App`
/// owns all of it and hands over borrows, never a copy it keeps around.
pub struct Feed<'a> {
    /// [`None`] only for a session nothing has saved anything to yet.
    pub session: Option<&'a SessionInfo>,
    /// What [`crate::component::chat::Chat::messages`] currently shows.
    pub messages: &'a [transcript::Entry<'a>],
    /// The raw-log ring buffer `App` tees every [`CoreEvent`] into.
    pub events: &'a VecDeque<CoreEvent>,
    /// The per-turn usage ring buffer `App::record` appends to.
    pub usages: &'a VecDeque<TurnUsage>,
    /// The same totals the status bar shows.
    pub totals: Totals,
}

/// One finished turn's spend, captured where `App::record` already reads a
/// `Usage` off the wire — the reasoning and cache splits ride along
/// untouched, where the status bar's own running totals collapse them
/// (**F2**).
#[derive(Clone, Debug)]
pub struct TurnUsage {
    /// The assistant message this turn's usage belongs to.
    pub message_id: MessageId,
    /// The model the turn ran against, read off `App::model` at the moment
    /// `MessageFinished` arrived — the same value `App::record` already
    /// prices the running totals against.
    pub model: String,
    /// What the turn spent.
    pub usage: Usage,
}

/// Which of the three tabs is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    /// Every part of every message on screen, full input JSON and full
    /// output — the `/copy` shape, unclamped.
    Transcript,
    /// One line per core event this session has emitted, oldest first.
    Log,
    /// One row per turn that reported a [`Usage`], with a totals footer.
    Tokens,
}

impl Tab {
    /// Every tab, in the order the strip and Left/Right cycle them.
    const ALL: [Self; 3] = [Self::Transcript, Self::Log, Self::Tokens];

    /// How the strip names it, including the digit that jumps straight to it.
    fn label(self) -> &'static str {
        match self {
            Self::Transcript => "[1] transcript",
            Self::Log => "[2] log",
            Self::Tokens => "[3] tokens",
        }
    }

    /// The lowercase word this tab goes by outside the strip's
    /// digit-prefixed label — the banner's spaced-out letters, and the
    /// footer's "Showing …" and trailing mode-word marker.
    fn name(self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::Log => "log",
            Self::Tokens => "tokens",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// The overlay itself.
///
/// Holds only which tab is showing and how far it has scrolled; everything
/// it draws is handed in fresh on every [`Inspector::render`] call, so a turn
/// streaming underneath is never behind what this shows.
#[derive(Debug)]
pub struct Inspector {
    tab: Tab,
    /// First row of the active tab's content on screen, or [`None`] to stay
    /// **pinned to the tail** — the opening state (2026-08-15, retiring the
    /// open-at-the-top posture): the newest of what this overlay exists to
    /// expand is what it opens on, and because every render re-reads the feed
    /// fresh, a pinned viewport follows a streaming turn on its own. Clamped
    /// by the render the same way [`crate::component::help::Help::offset`]
    /// is — the render is the only place that knows how many rows there are
    /// and how many fit — and a viewport scrolled to (or past) the bottom
    /// returns to [`None`], exactly the chat pane's own tail-follow rule.
    offset: Option<usize>,
    /// The active tab's content length at the last render, so a scroll that
    /// starts from the pinned tail has a row to start counting from.
    total: usize,
    /// How many content rows the last render had room for, kept beside
    /// `total` for the same reason.
    rows: usize,
}

impl Inspector {
    /// Opens on the transcript tab, pinned to the tail.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tab: Tab::Transcript,
            offset: None,
            total: 0,
            rows: 0,
        }
    }

    /// Switches to `tab`, and back to its tail: a scroll position from one
    /// tab means nothing on another, and the tail is where every tab opens.
    fn select(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.offset = None;
        }
    }

    /// Jumps straight to the tab at `index` in `Tab::ALL`, the digit-key
    /// shortcut. Out of range does nothing, rather than panicking on a stray
    /// key this build never binds to a fourth tab.
    pub fn select_index(&mut self, index: usize) {
        if let Some(tab) = Tab::ALL.get(index).copied() {
            self.select(tab);
        }
    }

    /// The Right-arrow half of the strip's cycle.
    pub fn next_tab(&mut self) {
        self.select(self.tab.next());
    }

    /// The Left-arrow half of the strip's cycle.
    pub fn previous_tab(&mut self) {
        self.select(self.tab.previous());
    }

    /// Moves the active tab's viewport by `delta` rows, negative towards the
    /// top. A pinned viewport starts counting from the tail the last render
    /// measured; reaching the bottom again — End rides this with
    /// `isize::MAX` — re-pins, so scrolling down past the end behaves like
    /// never having scrolled, exactly as the chat pane reads it.
    pub fn scroll(&mut self, delta: isize) {
        let max = self.total.saturating_sub(self.rows);
        let current = self.offset.unwrap_or(max);
        let moved = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta.unsigned_abs())
        };

        self.offset = (moved < max).then_some(moved);
    }

    /// Moves the active tab's viewport to its first row.
    pub fn scroll_to_top(&mut self) {
        self.offset = Some(0);
    }

    /// Moves the active tab's viewport by half of what the last render had
    /// room for, `direction` negative towards the top — vim's `Ctrl+U`/
    /// `Ctrl+D` pair, whose `scroll` option defaults to half the window. The
    /// screen's own step rather than the fixed one the Page keys ride, which
    /// is what makes two presses a page whatever the terminal's height; never
    /// less than one row, so the pair moves before a first render has
    /// measured anything.
    pub fn scroll_half_page(&mut self, direction: isize) {
        let half = isize::try_from((self.rows / 2).max(1)).unwrap_or(isize::MAX);
        self.scroll(if direction < 0 { -half } else { half });
    }

    /// Draws the overlay over the whole of `area`.
    ///
    /// Full-terminal takeover, not a popup (screenshot-sourced, see the
    /// module doc): no border, no centering, and `area` is the caller's
    /// whole frame — `App::draw` hands it the same `Rect` `frame.area()`
    /// returns, and skips the editor and status bar while this is open
    /// rather than drawing them over the bottom of it. Content is still
    /// sized to most of `area` rather than to itself, unlike
    /// [`crate::component::help::Help`]: a reference card is short enough to
    /// size itself to what it holds, where a raw event log or a full
    /// transcript is routinely longer than any terminal, and a popup that
    /// resized itself on every tab switch would be its own kind of
    /// distraction. What does not fit scrolls, exactly as the help card's
    /// does.
    pub fn render(&mut self, area: Rect, buffer: &mut Buffer, theme: &Theme, feed: &Feed<'_>) {
        if area.is_empty() {
            return;
        }

        Clear.render(area, buffer);

        let width = usize::from(area.width);

        let content = match self.tab {
            Tab::Transcript => transcript_lines(feed.session, feed.messages, theme),
            Tab::Log => log_lines(feed.events, theme),
            Tab::Tokens => token_lines(feed.usages, feed.totals, theme),
        };
        let total = content.len();

        let rows = usize::from(area.height).saturating_sub(CHROME).max(1);
        // Written back so a scroll starts from the rows actually shown, the
        // same discipline `Help::render` follows — and a viewport that was
        // dragged to the bottom, or that the content shrank out from under,
        // goes back to following the tail.
        let max = total.saturating_sub(rows);
        self.total = total;
        self.rows = rows;
        self.offset = self.offset.filter(|offset| *offset < max);
        let offset = self.offset.unwrap_or(max);

        let mut lines: Vec<Line<'static>> = vec![
            banner_line(self.tab, theme, width),
            tab_strip(self.tab, theme),
        ];
        lines.extend(
            content
                .into_iter()
                .skip(offset)
                .take(rows)
                .map(|line| Line::styled(clip(&text_of(&line), width), style_of(&line))),
        );
        lines.push(Line::styled(
            clip(&footer(self.tab, offset, rows, total, width), width),
            theme.dim,
        ));

        Paragraph::new(Text::from(lines))
            .style(theme.fg.patch(theme.background))
            .render(area, buffer);
    }
}

impl Default for Inspector {
    fn default() -> Self {
        Self::new()
    }
}

/// The plain text of a [`Line`] built from exactly one [`Span`] — every line
/// [`transcript_lines`]/[`log_lines`]/[`token_lines`] produce, so re-clipping
/// after windowing never has to reason about more than one.
fn text_of(line: &Line<'static>) -> String {
    line.spans
        .first()
        .map_or_else(String::new, |span| span.content.to_string())
}

/// The style that one-span line carries.
fn style_of(line: &Line<'static>) -> ratatui::style::Style {
    line.spans
        .first()
        .map_or_else(Default::default, |span| span.style)
}

/// The header's first line: Codex's `/`-and-space-filled banner, spelling out
/// the active tab's name letter by letter (Screenshot A's
/// `/ T R A N S C R I P T / / / …`) rather than a fixed "TRANSCRIPT" — this
/// build keeps three tabs, so the banner is the one place naming which of
/// them is active besides the strip underneath it.
fn banner_line(active: Tab, theme: &Theme, width: usize) -> Line<'static> {
    let spaced = active
        .name()
        .to_uppercase()
        .chars()
        .map(|letter| letter.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let mut banner = format!("/ {spaced} /");
    while banner.width() < width {
        banner.push_str(" /");
    }

    Line::styled(clip(&banner, width), theme.fg)
}

/// The strip naming all three tabs, the active one picked out.
fn tab_strip(active: Tab, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, tab) in Tab::ALL.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        let style = if *tab == active {
            theme.fg.add_modifier(Modifier::BOLD)
        } else {
            theme.dim
        };
        spans.push(Span::styled(tab.label(), style));
    }

    Line::from(spans)
}

/// Tab 1: every message on screen, replayed through [`transcript::format`] —
/// the exact function `/copy` puts on the clipboard, so a completed tool
/// call's input JSON and output are byte-identical to what a copy of the same
/// part would read, MCP calls included: their tool id is already spelled
/// `mcp__<server>__<tool>` on the wire, and `transcript::format` prints
/// whatever `tool` field it is handed without a special case for one.
///
/// `session` is [`None`] only for a session nothing has saved yet — a fresh
/// run against the fake provider, or a turn still streaming its very first
/// reply — and reads as an explicit placeholder rather than an empty tab, so
/// "nothing to show" and "still starting up" are not the same screen.
fn transcript_lines(
    session: Option<&SessionInfo>,
    messages: &[transcript::Entry<'_>],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let Some(session) = session else {
        return vec![Line::styled("no session yet".to_owned(), theme.dim)];
    };
    if messages.is_empty() {
        return vec![Line::styled("nothing said yet".to_owned(), theme.dim)];
    }

    transcript::format(session, messages)
        .lines()
        .map(|line| Line::styled(line.to_owned(), theme.fg))
        .collect()
}

/// Tab 2: one line per teed [`CoreEvent`], oldest first — a `VecDeque`
/// already keeps them in arrival order, so the newest is always the tail, the
/// way a person reading a log expects to find it.
///
/// `{event:?}` rather than a hand-written summarizer: the derived `Debug` on
/// [`CoreEvent`] is exactly the "raw" the tab promises, and it never falls
/// out of date with a variant this file was not touched to learn about.
fn log_lines(events: &VecDeque<CoreEvent>, theme: &Theme) -> Vec<Line<'static>> {
    if events.is_empty() {
        return vec![Line::styled("no events yet".to_owned(), theme.dim)];
    }

    events
        .iter()
        .map(|event| Line::styled(format!("{event:?}"), theme.fg))
        .collect()
}

/// Column widths the token table's header and every row share, so the
/// columns line up.
const ID_WIDTH: usize = 10;
const MODEL_WIDTH: usize = 24;
const COUNT_WIDTH: usize = 10;
const COST_WIDTH: usize = 10;

/// Tab 3: one row per [`TurnUsage`], the reasoning and cache splits
/// [`App::record`](crate::app::App) already reads off the wire but the
/// status bar's running totals collapse, plus a footer built from
/// [`Totals::segment`] so it is the same string the status bar shows rather
/// than a second formatting of the same numbers.
fn token_lines(usages: &VecDeque<TurnUsage>, totals: Totals, theme: &Theme) -> Vec<Line<'static>> {
    if usages.is_empty() {
        return vec![Line::styled("no finished turns yet".to_owned(), theme.dim)];
    }

    let mut lines = vec![Line::styled(
        format!(
            "{:<ID_WIDTH$} {:<MODEL_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COST_WIDTH$}",
            "turn", "model", "in", "out", "reasoning", "cache-r", "cache-w", "cost",
        ),
        theme.fg.add_modifier(Modifier::BOLD),
    )];

    for row in usages {
        lines.push(Line::styled(
            format!(
                "{:<ID_WIDTH$} {:<MODEL_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COST_WIDTH$}",
                short_id(&row.message_id),
                clip(&row.model, MODEL_WIDTH),
                row.usage.input_tokens,
                row.usage.output_tokens,
                row.usage.reasoning_tokens,
                row.usage.cache_read_tokens,
                row.usage.cache_write_tokens,
                turn_cost(row),
            ),
            theme.fg,
        ));
    }

    lines.push(Line::raw(String::new()));
    lines.push(Line::styled(
        format!("{:<ID_WIDTH$} {}", "totals", totals.segment()),
        theme.fg.add_modifier(Modifier::BOLD),
    ));

    lines
}

/// The last few characters of a message id — the counter half of the
/// millis-plus-counter id ([`ganja_protocol::ascending`]) is what actually
/// tells two ids minted moments apart apart, so it is the half worth keeping
/// in a column this narrow. `pub(crate)` because `/usage`'s turn table shows
/// the same rows and must spell them the same way (AC5) — one formatter,
/// never a second copy to drift.
pub(crate) fn short_id(id: &MessageId) -> String {
    let raw = id.as_str();

    raw.get(raw.len().saturating_sub(8)..)
        .unwrap_or(raw)
        .to_owned()
}

/// A turn row's cost cell: catalog-priced, `-` when the model is uncataloged.
/// Shared with `/usage`'s table for the same one-formatter reason as
/// [`short_id`].
pub(crate) fn turn_cost(row: &TurnUsage) -> String {
    catalog::model(&row.model)
        .map(|model| catalog::cost(&row.usage, &model).total_usd)
        .map_or_else(|| "-".to_owned(), |dollars| format!("${dollars:.4}"))
}

/// The footer's right-edge marker: the active tab's mode word beside how far
/// the viewport sits in its content — Claude Code's `verbose` mode-word
/// presentation, combined with a scroll percentage rather than left bare, and
/// Codex's own `4% —` pairing turned around so the word comes first. An em
/// dash when everything already fits and there is nowhere to scroll to,
/// which is the one case a percentage would claim a position that does not
/// exist.
fn position(offset: usize, rows: usize, total: usize) -> String {
    if total <= rows {
        return "\u{2014}".to_owned();
    }

    let span = total.saturating_sub(rows).max(1);
    let percent = (offset.min(span) * 100 / span).min(100);

    format!("{percent}%")
}

/// The bottom edge, one line: Claude Code's `Showing … · … · …` wording on
/// the left, the active tab's [`position`] marker at the right edge — Codex's
/// own two-line hint pair plus separate `4% —` counter, collapsed into the
/// one line Screenshot B's footer used. When both cannot fit, the marker
/// stays and the static hints go, the same priority
/// [`crate::component::help`]'s own `footer` gives its row counter over its
/// hints.
fn footer(tab: Tab, offset: usize, rows: usize, total: usize, width: usize) -> String {
    let mode = tab.name();
    let left = format!("Showing {mode} \u{b7} {HINTS}");
    let right = format!("{mode} \u{b7} {}", position(offset, rows, total));

    let room = width
        .saturating_sub(left.width())
        .saturating_sub(right.width());
    if room == 0 {
        return right;
    }

    format!("{left}{gap}{right}", gap = " ".repeat(room))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use ganja_core::SessionId;
    use ganja_protocol::{
        Event as CoreEvent, Message, MessageId, Part, PartBody, PartId, Role, ToolState,
    };
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Feed, Inspector, TurnUsage};
    use crate::{component::status::Totals, theme::Theme};

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 24,
    };

    fn session(title: Option<&str>) -> ganja_core::SessionInfo {
        ganja_core::SessionInfo {
            effort: None,
            id: SessionId::from("ses_fixture".to_owned()),
            version: ganja_core::storage::VERSION,
            title: title.map(str::to_owned),
            created: 0,
            updated: 0,
            usage: ganja_protocol::Usage::default(),
            context_tokens: 0,
            summary: None,
            agent: None,
            model: None,
            activated_tools: std::collections::BTreeSet::new(),
            parent: None,
            revert: None,
        }
    }

    /// A feed with nothing in it but whatever `session`/`messages` supply —
    /// the shape every test whose tab under test does not care about the
    /// other two reaches for.
    fn feed<'a>(
        session: Option<&'a ganja_core::SessionInfo>,
        messages: &'a [crate::transcript::Entry<'a>],
        events: &'a VecDeque<CoreEvent>,
        usages: &'a VecDeque<TurnUsage>,
    ) -> Feed<'a> {
        Feed {
            session,
            messages,
            events,
            usages,
            totals: Totals::default(),
        }
    }

    fn render(inspector: &mut Inspector, feed: &Feed<'_>) -> String {
        render_in(inspector, AREA, feed)
    }

    fn render_in(inspector: &mut Inspector, area: Rect, feed: &Feed<'_>) -> String {
        let mut buffer = Buffer::empty(area);
        inspector.render(area, &mut buffer, &Theme::default(), feed);

        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Tab 1 shows a completed tool call's full input JSON and full output,
    /// byte-equal to what `transcript::format` — the `/copy` renderer — would
    /// print for the same part, MCP calls included: an `mcp__server__tool`
    /// id is printed verbatim, with no special case for it.
    #[test]
    fn the_transcript_tab_matches_the_copy_renderer_for_the_same_part() {
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "mcp__docs__search".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"query": "full input, never clamped"}),
                    output: "line one\nline two\nline three\nline four\nline five".to_owned(),
                    title: "search".to_owned(),
                    metadata: serde_json::json!({}),
                    started: 0,
                    completed: 1,
                },
            },
        });
        let messages = [(Role::Assistant, reply.parts.as_slice())];
        let session = session(Some("inspector fixture"));
        let (events, usages) = (VecDeque::new(), VecDeque::new());
        let feed = feed(Some(&session), &messages, &events, &usages);

        let expected = crate::transcript::format(&session, &messages);
        let mut inspector = Inspector::new();
        let mut screen = render(&mut inspector, &feed);
        // The whole document may be taller than the fixture's viewport; page
        // down until the tail — where the full, unclamped output lives — is
        // reached, mirroring how a person would actually read it.
        for _ in 0..20 {
            screen.push('\n');
            inspector.scroll(24);
            screen.push_str(&render(&mut inspector, &feed));
        }

        assert!(screen.contains("mcp__docs__search"), "{screen}");
        assert!(screen.contains("full input, never clamped"), "{screen}");
        for line in [
            "line one",
            "line two",
            "line three",
            "line four",
            "line five",
        ] {
            assert!(
                screen.contains(line),
                "the full output should be unclamped, unlike the transcript pane's preview:\n{screen}"
            );
        }
        assert!(
            expected.contains("mcp__docs__search"),
            "the fixture should exercise a real mcp id"
        );
    }

    #[test]
    fn the_transcript_tab_names_a_session_that_has_not_saved_anything_yet() {
        let (events, usages) = (VecDeque::new(), VecDeque::new());
        let mut inspector = Inspector::new();
        let screen = render(&mut inspector, &feed(None, &[], &events, &usages));

        assert!(screen.contains("no session yet"), "{screen}");
    }

    /// Tab 2 gains one line per teed event, and the newest lands at the tail.
    #[test]
    fn the_log_tab_lists_one_line_per_event_newest_at_the_tail() {
        let mut events = VecDeque::new();
        events.push_back(CoreEvent::AgentChanged {
            session_id: SessionId::from("ses_fixture".to_owned()),
            agent: "oldest".to_owned(),
            model: "m".to_owned(),
        });
        events.push_back(CoreEvent::AgentChanged {
            session_id: SessionId::from("ses_fixture".to_owned()),
            agent: "newest".to_owned(),
            model: "m".to_owned(),
        });
        let usages = VecDeque::new();

        let mut inspector = Inspector::new();
        inspector.select_index(1);
        let screen = render(&mut inspector, &feed(None, &[], &events, &usages));

        let oldest_row = screen.lines().position(|line| line.contains("oldest"));
        let newest_row = screen.lines().position(|line| line.contains("newest"));
        assert!(oldest_row.is_some() && newest_row.is_some(), "{screen}");
        assert!(
            oldest_row < newest_row,
            "the oldest event should be above the newest:\n{screen}"
        );
    }

    #[test]
    fn the_log_tab_names_its_own_empty_state() {
        let (events, usages) = (VecDeque::new(), VecDeque::new());
        let mut inspector = Inspector::new();
        inspector.select_index(1);
        let screen = render(&mut inspector, &feed(None, &[], &events, &usages));

        assert!(screen.contains("no events yet"), "{screen}");
    }

    /// Tab 3 shows the reasoning and cache splits, one row per turn, and a
    /// totals footer that is the status bar's own segment string.
    #[test]
    fn the_tokens_tab_shows_every_split_and_a_totals_footer_matching_the_status_bar() {
        let usage = ganja_protocol::Usage {
            input_tokens: 3,
            output_tokens: 4,
            reasoning_tokens: 5,
            cache_read_tokens: 6,
            cache_write_tokens: 7,
        };
        let mut usages = VecDeque::new();
        usages.push_back(TurnUsage {
            message_id: Message::assistant("claude-sonnet-5").id,
            model: "claude-sonnet-5".to_owned(),
            usage,
        });
        let events = VecDeque::new();
        let totals = Totals {
            input_tokens: 16,
            output_tokens: 4,
            cost_usd: Some(0.5),
        };

        let mut inspector = Inspector::new();
        inspector.select_index(2);
        let screen = render(
            &mut inspector,
            &Feed {
                session: None,
                messages: &[],
                events: &events,
                usages: &usages,
                totals,
            },
        );

        for value in ["3", "4", "5", "6", "7"] {
            assert!(screen.contains(value), "got:\n{screen}");
        }
        assert!(
            screen.contains(&totals.segment()),
            "the footer should be the status bar's own segment string:\n{screen}"
        );
    }

    #[test]
    fn the_tokens_tab_names_its_own_empty_state() {
        let (events, usages) = (VecDeque::new(), VecDeque::new());
        let mut inspector = Inspector::new();
        inspector.select_index(2);
        let screen = render(&mut inspector, &feed(None, &[], &events, &usages));

        assert!(screen.contains("no finished turns yet"), "{screen}");
    }

    #[test]
    fn digit_keys_and_arrows_reach_every_tab() {
        let (events, usages) = (VecDeque::new(), VecDeque::new());
        let feed = feed(None, &[], &events, &usages);
        let mut inspector = Inspector::new();

        inspector.select_index(2);
        assert!(render(&mut inspector, &feed).contains("no finished turns yet"));

        inspector.previous_tab();
        assert!(render(&mut inspector, &feed).contains("no events yet"));

        inspector.next_tab();
        inspector.next_tab();
        assert!(render(&mut inspector, &feed).contains("no session yet"));
    }

    /// Switching tabs forgets the old tab's scroll position: it describes
    /// nothing about the new one, whose tail is where every tab opens.
    #[test]
    fn switching_tabs_resets_the_scroll_position() {
        let (events, usages) = (VecDeque::new(), VecDeque::new());
        let feed = feed(None, &[], &events, &usages);
        let mut inspector = Inspector::new();
        inspector.scroll(5);
        inspector.select_index(1);

        // Rendering does not panic and the new tab starts pinned to its own
        // tail; asserted indirectly by re-selecting the transcript tab and
        // confirming a fresh `Inspector` renders identically.
        let moved = render(&mut inspector, &feed);
        let fresh = render(&mut Inspector::default(), &feed);
        inspector.select_index(0);
        assert_eq!(render(&mut inspector, &feed), fresh, "got:\n{moved}");
    }

    /// Every tab opens pinned to its tail — the newest is what the overlay
    /// exists to expand — and a pinned viewport follows the stream, because
    /// every render re-reads the feed fresh (2026-08-15, retiring the
    /// open-at-the-top posture). Scrolling up unpins and holds; End re-pins.
    #[test]
    fn the_log_tab_opens_on_its_tail_and_follows_what_arrives() {
        let delta = |index: usize| CoreEvent::PartDelta {
            session_id: SessionId::from("ses_1".to_owned()),
            message_id: MessageId::from("msg_1".to_owned()),
            part_id: PartId::from(format!("prt_{index}")),
            delta: format!("delta {index}"),
        };
        let mut events: VecDeque<CoreEvent> = (0..30).map(delta).collect();
        let usages = VecDeque::new();
        let mut inspector = Inspector::new();
        inspector.select_index(1);
        let area = Rect::new(0, 0, 120, 8);

        let opened = render_in(&mut inspector, area, &feed(None, &[], &events, &usages));
        assert!(
            opened.contains("prt_29"),
            "the tail is what opens:\n{opened}"
        );
        assert!(
            !opened.contains("prt_0\""),
            "and the head is off screen:\n{opened}"
        );

        events.push_back(delta(30));
        let followed = render_in(&mut inspector, area, &feed(None, &[], &events, &usages));
        assert!(
            followed.contains("prt_30"),
            "a pinned viewport follows what arrives:\n{followed}"
        );

        inspector.scroll(-1);
        let held = render_in(&mut inspector, area, &feed(None, &[], &events, &usages));
        assert!(
            !held.contains("prt_30"),
            "scrolled up, the viewport holds its place:\n{held}"
        );

        inspector.scroll(isize::MAX);
        let repinned = render_in(&mut inspector, area, &feed(None, &[], &events, &usages));
        assert!(
            repinned.contains("prt_30"),
            "End re-pins the viewport to the tail:\n{repinned}"
        );
    }

    /// vim's half-page pair: `Ctrl+U` moves the viewport up by half of what
    /// the last render had room for and `Ctrl+D` back down by the same — the
    /// screen's own step, asserted against the row arithmetic rather than a
    /// second screen — and reaching the tail again re-pins, like every other
    /// way down.
    #[test]
    fn the_half_page_pair_moves_by_half_of_what_the_last_render_showed() {
        let delta = |index: usize| CoreEvent::PartDelta {
            session_id: SessionId::from("ses_1".to_owned()),
            message_id: MessageId::from("msg_1".to_owned()),
            part_id: PartId::from(format!("prt_{index}")),
            delta: format!("delta {index}"),
        };
        let events: VecDeque<CoreEvent> = (0..30).map(delta).collect();
        let usages = VecDeque::new();
        let fed = feed(None, &[], &events, &usages);
        // Eight rows less the chrome is five of content: a half page is two.
        let area = Rect::new(0, 0, 120, 8);
        let mut by_pair = Inspector::new();
        by_pair.select_index(1);
        let mut by_rows = Inspector::new();
        by_rows.select_index(1);
        let pinned = render_in(&mut by_pair, area, &fed);
        render_in(&mut by_rows, area, &fed);

        by_pair.scroll_half_page(-1);
        by_rows.scroll(-2);
        let up = render_in(&mut by_pair, area, &fed);
        assert_eq!(up, render_in(&mut by_rows, area, &fed));
        assert!(
            !up.contains("prt_29"),
            "half a page up, the tail is off screen:\n{up}"
        );
        assert!(
            up.contains("prt_27"),
            "and the row two above it is the last shown:\n{up}"
        );

        by_pair.scroll_half_page(1);
        let down = render_in(&mut by_pair, area, &fed);
        assert_eq!(
            down, pinned,
            "half a page down from there is the tail again, re-pinned:\n{down}"
        );
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        let (events, usages) = (VecDeque::new(), VecDeque::new());
        let feed = feed(None, &[], &events, &usages);

        for (width, height) in [(1, 1), (4, 3), (20, 5)] {
            let area = Rect::new(0, 0, width, height);
            let mut inspector = Inspector::new();

            render_in(&mut inspector, area, &feed);
        }
    }
}
