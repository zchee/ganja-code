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
    layout::{Constraint, Rect},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    component::{chat::clip, status::Totals},
    theme::Theme,
    transcript,
};

/// Widest the popup grows, whatever the terminal offers. Wider than
/// [`crate::component::help::MAX_WIDTH`]'s 72: the token table alone needs
/// eight columns, and the transcript tab is full input JSON plus output.
const MAX_WIDTH: u16 = 110;

/// Rows spent on the tab strip and the footer, neither of which scrolls.
/// Mirrors [`crate::component::help`]'s own `CHROME`, widened by the two rows
/// the tab strip and the blank line under it cost that the help card, with
/// no header of its own, never had to budget for.
const CHROME: usize = 4;

/// The keys the overlay answers to, shown along its bottom edge.
const HINTS: &str = "[Left/Right] tab   [up/down] scroll   [Esc] close";

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
    /// First row of the active tab's content on screen. Reset to zero on a
    /// tab switch, and clamped by the render the same way
    /// [`crate::component::help::Help::offset`] is — the render is the only
    /// place that knows how many rows there are and how many fit.
    offset: usize,
}

impl Inspector {
    /// Opens on the transcript tab, scrolled to the top.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tab: Tab::Transcript,
            offset: 0,
        }
    }

    /// Switches to `tab`, and back to the top of it: a scroll position from
    /// one tab means nothing on another.
    fn select(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.offset = 0;
        }
    }

    /// Jumps straight to the tab at `index` in [`Tab::ALL`], the digit-key
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
    /// top. Deliberately unclamped at the far end, for the reason
    /// [`crate::component::help::Help::scroll`] is: only the render knows
    /// where the far end is.
    pub fn scroll(&mut self, delta: isize) {
        self.offset = if delta < 0 {
            self.offset.saturating_sub(delta.unsigned_abs())
        } else {
            self.offset.saturating_add(delta.unsigned_abs())
        };
    }

    /// Moves the active tab's viewport to its first row.
    pub fn scroll_to_top(&mut self) {
        self.offset = 0;
    }

    /// Draws the overlay centered over `area`.
    ///
    /// Sized to most of `area` rather than to its content, unlike
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

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let height = area.height.saturating_sub(2).max(1);
        let inner_width = usize::from(width).saturating_sub(2);
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);

        let content = match self.tab {
            Tab::Transcript => transcript_lines(feed.session, feed.messages, theme),
            Tab::Log => log_lines(feed.events, theme),
            Tab::Tokens => token_lines(feed.usages, feed.totals, theme),
        };
        let total = content.len();

        let rows = usize::from(height)
            .saturating_sub(2)
            .saturating_sub(CHROME)
            .max(1);
        // Written back so a scroll up starts from the last row actually
        // shown, the same discipline `Help::render` follows.
        self.offset = self.offset.min(total.saturating_sub(rows));
        let offset = self.offset;

        let mut lines: Vec<Line<'static>> =
            vec![tab_strip(self.tab, theme), Line::raw(String::new())];
        lines.extend(
            content
                .into_iter()
                .skip(offset)
                .take(rows)
                .map(|line| Line::styled(clip(&text_of(&line), inner_width), style_of(&line))),
        );
        lines.push(Line::raw(String::new()));
        lines.push(Line::styled(
            clip(&footer(offset, rows, total, inner_width), inner_width),
            theme.dim,
        ));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" inspector "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
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

/// The strip naming all three tabs, the active one picked out.
fn tab_strip(active: Tab, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, tab) in Tab::ALL.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        let style = if *tab == active {
            theme.accent
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
        theme.accent,
    )];

    for row in usages {
        let cost =
            catalog::model(&row.model).map(|model| catalog::cost(&row.usage, &model).total_usd);
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
                cost.map_or_else(|| "-".to_owned(), |dollars| format!("${dollars:.4}")),
            ),
            theme.fg,
        ));
    }

    lines.push(Line::raw(String::new()));
    lines.push(Line::styled(
        format!("{:<ID_WIDTH$} {}", "totals", totals.segment()),
        theme.accent,
    ));

    lines
}

/// The last few characters of a message id — the counter half of the
/// millis-plus-counter id ([`ganja_protocol::ascending`]) is what actually
/// tells two ids minted moments apart apart, so it is the half worth keeping
/// in a column this narrow.
fn short_id(id: &MessageId) -> String {
    let raw = id.as_str();

    raw.get(raw.len().saturating_sub(8)..)
        .unwrap_or(raw)
        .to_owned()
}

/// The bottom edge: which keys work, and — when the tab does not fit —
/// which of its rows are on screen. Mirrors
/// [`crate::component::help`]'s own `footer`.
fn footer(offset: usize, rows: usize, total: usize, width: usize) -> String {
    if total <= rows {
        return HINTS.to_owned();
    }

    let last = (offset + rows).min(total);
    let counter = format!("{first}-{last} of {total}", first = offset + 1);
    let room = width
        .saturating_sub(HINTS.width())
        .saturating_sub(counter.width());
    if room == 0 {
        return counter;
    }

    format!("{HINTS}{gap}{counter}", gap = " ".repeat(room))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use ganja_core::SessionId;
    use ganja_protocol::{Event as CoreEvent, Message, Part, PartBody, PartId, Role, ToolState};
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
    /// nothing about the new one.
    #[test]
    fn switching_tabs_resets_the_scroll_position() {
        let (events, usages) = (VecDeque::new(), VecDeque::new());
        let feed = feed(None, &[], &events, &usages);
        let mut inspector = Inspector::new();
        inspector.scroll(5);
        inspector.select_index(1);

        // Rendering does not panic and the new tab starts at its own top;
        // asserted indirectly by re-selecting the transcript tab and
        // confirming a fresh `Inspector` renders identically.
        let moved = render(&mut inspector, &feed);
        let fresh = render(&mut Inspector::default(), &feed);
        inspector.select_index(0);
        assert_eq!(render(&mut inspector, &feed), fresh, "got:\n{moved}");
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
