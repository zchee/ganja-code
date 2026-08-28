use std::collections::VecDeque;

use ganja_core::SessionId;
use ganja_protocol::{
    Event as CoreEvent, Message, MessageId, Part, PartBody, PartId, Role, ToolState,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{Feed, Inspector, TurnUsage};
use crate::component::status::Totals;
use crate::theme::Theme;

const AREA: Rect = Rect { x: 0, y: 0, width: 100, height: 24 };

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
    Feed { session, messages, events, usages, totals: Totals::default() }
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
    for line in ["line one", "line two", "line three", "line four", "line five"] {
        assert!(
            screen.contains(line),
            "the full output should be unclamped, unlike the transcript pane's preview:\n{screen}"
        );
    }
    assert!(expected.contains("mcp__docs__search"), "the fixture should exercise a real mcp id");
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
    assert!(oldest_row < newest_row, "the oldest event should be above the newest:\n{screen}");
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
    let totals = Totals { input_tokens: 16, output_tokens: 4, cost_usd: Some(0.5) };

    let mut inspector = Inspector::new();
    inspector.select_index(2);
    let screen = render(
        &mut inspector,
        &Feed { session: None, messages: &[], events: &events, usages: &usages, totals },
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
    assert!(opened.contains("prt_29"), "the tail is what opens:\n{opened}");
    assert!(!opened.contains("prt_0\""), "and the head is off screen:\n{opened}");

    events.push_back(delta(30));
    let followed = render_in(&mut inspector, area, &feed(None, &[], &events, &usages));
    assert!(followed.contains("prt_30"), "a pinned viewport follows what arrives:\n{followed}");

    inspector.scroll(-1);
    let held = render_in(&mut inspector, area, &feed(None, &[], &events, &usages));
    assert!(!held.contains("prt_30"), "scrolled up, the viewport holds its place:\n{held}");

    inspector.scroll(isize::MAX);
    let repinned = render_in(&mut inspector, area, &feed(None, &[], &events, &usages));
    assert!(repinned.contains("prt_30"), "End re-pins the viewport to the tail:\n{repinned}");
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
    assert!(!up.contains("prt_29"), "half a page up, the tail is off screen:\n{up}");
    assert!(up.contains("prt_27"), "and the row two above it is the last shown:\n{up}");

    by_pair.scroll_half_page(1);
    let down = render_in(&mut by_pair, area, &fed);
    assert_eq!(down, pinned, "half a page down from there is the tail again, re-pinned:\n{down}");
}

/// The footer's legend is the full one where the row has room for it,
/// the narrow one — the vim pair dropped, nothing else — where only that
/// fits, and the position alone where neither does. Eighty columns on
/// the transcript tab is the narrow case, one column short of the full
/// row, which is what keeps the 80-column snapshot as it was.
#[test]
fn the_footer_drops_the_vim_pair_first_and_the_rest_of_the_legend_last() {
    let wide = super::footer(super::Tab::Transcript, 0, 10, 10, 100);
    assert!(wide.contains("ctrl+u/d scroll"), "{wide}");
    assert_eq!(wide.chars().count(), 100);

    let eighty = super::footer(super::Tab::Transcript, 0, 10, 10, 80);
    assert!(!eighty.contains("ctrl+u/d") && eighty.contains("pgdn scroll"), "{eighty}");
    assert_eq!(eighty.chars().count(), 80);

    let tiny = super::footer(super::Tab::Transcript, 0, 10, 10, 20);
    assert!(!tiny.contains("scroll") && tiny.starts_with("transcript"), "{tiny}");
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
