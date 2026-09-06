use std::time::Duration;

use ganja_protocol::{Message, MessageId, Part, PartBody, PartId, ToolState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use super::{
    BULLET, COMPACT_BLUE, COMPACT_PERIWINKLE, Chat, Compaction, Instant, RESULT,
    WORKING_FRAME_STEP, WORKING_FRAMES, WORKING_VERBS, Working, compact_elapsed, compact_pulse,
    compact_tokens, elapsed, split_at_width, working_frame, wrap,
};
use crate::theme::{Theme, Themes};

/// A reply carrying one tool part in `state`, rendered wide enough that
/// nothing wraps.
fn tool_call(tool: &str, state: ToolState) -> Vec<String> {
    tool_call_at(tool, state, 80)
}

/// The same, laid out at `width`: a teammate spawn's header names three
/// arguments and does not fit the width every other row is measured at.
fn tool_call_at(tool: &str, state: ToolState, width: u16) -> Vec<String> {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool { call_id: "call_1".to_owned(), tool: tool.to_owned(), state },
    });
    chat.start_message(reply);

    rendered(&mut chat, Rect::new(0, 0, width, 20))
}

/// A tool the gateway ran on its own side, drawn in the same grammar a
/// local call is (**D489**): the marker, the name it came under, the
/// arguments condensed onto that line, and the result under `⎿`.
///
/// The name is deliberately the namespaced one — a row a reader could
/// mistake for a call this machine made would be the one wrong thing to
/// draw.
#[test]
fn a_provider_run_tool_draws_in_the_same_grammar_a_local_call_does() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::ServerTool {
            tool: "openrouter:web_search".to_owned(),
            input: serde_json::json!({"query": "rust 2024"}),
            output: "3 results".to_owned(),
        },
    });
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 80, 20));
    assert_eq!(
        &lines[..2],
        [
            // The leading capital is the grammar's own titlecase, which
            // every tool name on this pane gets — an `mcp__…` row reads
            // `Mcp__…` today. What matters is that the *namespace*
            // survives it: nobody should read this row as a call the
            // machine in front of them made.
            format!("{BULLET}Openrouter:web_search(query: \"rust 2024\")"),
            format!("{RESULT}3 results"),
        ],
        "got {lines:?}"
    );
}

const VIEWPORT: Rect = Rect { x: 0, y: 0, width: 20, height: 6 };

/// Fills a transcript the way the engine does: one complete user message
/// per entry.
fn transcript(chat: &mut Chat, entries: usize) {
    for index in 0..entries {
        chat.start_message(Message::user(format!("entry {index}")));
    }
}

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

/// The working strip as [`crate::app::App`] composes it: laid out at
/// `width`, drawn into exactly the rows it asked for.
fn strip(chat: &mut Chat, width: u16) -> Vec<String> {
    let height = chat.lay_out_working(width, &Theme::default());
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    chat.render_working(area, &mut buffer);

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

/// A file part renders as the token the user typed — range and all — and
/// names its mime when the bytes are not text.
#[test]
fn an_attached_file_renders_as_its_token_with_range_and_mime() {
    let mut chat = Chat::default();
    let mut message = Message::user("look");
    message.parts.push(Part {
        id: PartId::from("prt_f1".to_owned()),
        body: PartBody::File {
            path: "src/lib.rs".to_owned(),
            mime: "text/plain".to_owned(),
            start: Some(5),
            end: Some(9),
            content: None,
        },
    });
    message.parts.push(Part {
        id: PartId::from("prt_f2".to_owned()),
        body: PartBody::File {
            path: "shot.png".to_owned(),
            mime: "image/png".to_owned(),
            start: None,
            end: None,
            content: None,
        },
    });
    chat.start_message(message);

    let screen = rendered(&mut chat, Rect::new(0, 0, 40, 8)).join("\n");
    assert!(screen.contains("@src/lib.rs#5-9"), "{screen}");
    assert!(!screen.contains("@src/lib.rs#5-9 ("), "a text mention needs no mime label:\n{screen}");
    assert!(screen.contains("@shot.png (image/png)"), "{screen}");
}

/// With pixels available, an attached image's row gives way to a blank
/// box and the render asks for its cells; answered, the box fills with
/// kitty placeholder cells carrying the id in their color — the picture
/// in the transcript, not its path (2026-08-15). A text attachment
/// keeps its token row beside it, and a decode failure leaves the box
/// honestly blank forever.
#[test]
fn with_graphics_an_attached_image_asks_for_cells_and_then_draws_them() {
    let mut chat = Chat::default();
    chat.set_graphics(true);
    let mut message = Message::user("look");
    message.parts.push(Part {
        id: PartId::from("prt_f1".to_owned()),
        body: PartBody::File {
            path: "shot.png".to_owned(),
            mime: "image/png".to_owned(),
            start: None,
            end: None,
            content: None,
        },
    });
    message.parts.push(Part {
        id: PartId::from("prt_f2".to_owned()),
        body: PartBody::File {
            path: "src/lib.rs".to_owned(),
            mime: "text/plain".to_owned(),
            start: None,
            end: None,
            content: None,
        },
    });
    chat.start_message(message);

    let area = Rect::new(0, 0, 40, 12);
    let screen = rendered(&mut chat, area).join("\n");
    assert!(!screen.contains("shot.png"), "the image's path is off the screen:\n{screen}");
    assert!(screen.contains("@src/lib.rs"), "{screen}");
    assert_eq!(
        chat.images_wanting_cells(),
        &["shot.png".to_owned()],
        "the render asks for the cells it does not have"
    );

    chat.set_image_cell("shot.png", 7, 3);
    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer, &Theme::default());
    let cell = &buffer[(2, 1)];
    assert!(
        cell.symbol().starts_with('\u{10EEEE}'),
        "the box holds placeholder cells, got {:?}",
        cell.symbol()
    );
    assert_eq!(cell.style().fg, Some(crate::graphics::id_color(7)), "and the id rides the color");
    assert!(chat.images_wanting_cells().is_empty(), "an answered image is not asked for again");

    chat.set_image_cell("shot.png", 0, 0);
    let mut blank = Buffer::empty(area);
    chat.render(area, &mut blank, &Theme::default());
    assert_eq!(blank[(2, 1)].symbol(), " ", "a decode failure keeps the box blank");
    assert!(chat.images_wanting_cells().is_empty(), "and never re-asks");
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
    assert_eq!(wrap("one\n\ntwo", 10), vec!["one".to_owned(), String::new(), "two".to_owned()]);
}

#[test]
fn a_word_wider_than_the_viewport_is_chopped_not_dropped() {
    assert_eq!(wrap("abcdefghij", 4), vec!["abcd".to_owned(), "efgh".to_owned(), "ij".to_owned()]);
}

#[test]
fn wrapping_measures_display_width_not_bytes() {
    // Each of these is two columns wide, so only two fit on a five-column
    // line.
    assert_eq!(wrap("ああ ああ", 5), vec!["ああ".to_owned(), "ああ".to_owned()]);
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

/// A wrap lands between grapheme clusters and never inside one. Both
/// shapes here overflow a one-column budget on their first cluster, which
/// is exactly where a `char` walk used to cut: after the family's leading
/// emoji, and between the kana and the mark that voices it. Half a cluster
/// is not a glyph any terminal can draw back.
#[test]
fn a_wrap_never_splits_a_zwj_family_or_a_combining_sequence() {
    // Four emoji joined by three ZERO WIDTH JOINERs — 25 bytes, one glyph.
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    // "か" plus the combining voiced sound mark that makes it "が".
    let voiced = "\u{304b}\u{3099}";

    assert_eq!(
        split_at_width(&format!("{family}x"), 1),
        (family, "x"),
        "the family is consumed whole"
    );
    assert_eq!(
        split_at_width(&format!("{voiced}x"), 1),
        (voiced, "x"),
        "the mark stays with the kana it voices"
    );
}

#[test]
fn a_new_entry_scrolls_into_view() {
    let mut chat = Chat::default();
    transcript(&mut chat, 20);

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
    transcript(&mut chat, 20);
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
    transcript(&mut chat, 40);
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
    let reply = Message::assistant("canned");
    let part = Part::text("");
    chat.start_message(reply.clone());
    chat.start_part(&reply.id, part.clone());

    for fragment in ["hello ", "world"] {
        chat.append_delta(&reply.id, &part.id, fragment);
    }

    let lines = rendered(&mut chat, VIEWPORT);

    assert!(
        lines.iter().any(|line| line == "\u{25cf} hello world"),
        "streamed fragments should join into one entry, got {lines:?}"
    );
}

/// Every part of a message renders, which is what keeps P3's tool output
/// from displacing the text around it.
#[test]
fn a_message_renders_all_of_its_parts() {
    let mut chat = Chat::default();
    let reply = Message::assistant("canned");
    chat.start_message(reply.clone());
    for text in ["first", "second"] {
        chat.start_part(&reply.id, Part::text(text));
    }

    let lines = rendered(&mut chat, VIEWPORT);

    assert!(
        lines.iter().any(|line| line == "\u{25cf} first")
            && lines.iter().any(|line| line == "\u{25cf} second"),
        "both parts should render, each behind a bullet of its own, got {lines:?}"
    );
}

/// The invariant stated where this loop begins: a prompt is one block
/// however many parts it was built from. A peer part arriving beside what
/// the person typed opens its own `@` head under the caret that part
/// already drew, because two carets on one entry would claim two things
/// were said.
#[test]
fn a_prompt_carrying_a_peers_words_draws_one_caret_for_the_whole_entry() {
    // Wider and taller than `VIEWPORT`: this entry is four rows and the
    // question is which glyph leads each of them, so none may scroll off.
    const AREA: Rect = Rect { x: 0, y: 0, width: 30, height: 12 };

    let mut chat = Chat::default();
    let prompt = Message::user("what did w1 say");
    chat.start_message(prompt.clone());
    chat.start_part(
        &prompt.id,
        Part::peer("w1", Some("picked up W2".to_owned()), None, "on the protocol"),
    );
    chat.start_part(&prompt.id, Part::peer("w2", None, None, "and I have it"));

    let lines = rendered(&mut chat, AREA);
    let carets = lines.iter().filter(|line| line.starts_with("\u{3e} ")).count();

    assert_eq!(carets, 1, "one entry, one caret, got {lines:?}");
    assert!(
        lines.iter().any(|line| line == "\u{3e} what did w1 say"),
        "the caret leads what the person typed, got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "@ w1\u{276f} picked up W2")
            && lines.iter().any(|line| line == "@ w2\u{276f}"),
        "both peers head their own blocks under that caret, got {lines:?}"
    );
}

/// The same part on a reply is not something a person said, and not the
/// reply's own words either: it takes the `@` head there exactly as it
/// does on a prompt, claiming neither the caret nor the bullet.
#[test]
fn a_peers_words_on_a_reply_take_their_own_head_and_not_the_bullet() {
    let mut chat = Chat::default();
    let reply = Message::assistant("canned");
    chat.start_message(reply.clone());
    chat.start_part(&reply.id, Part::peer("w1", None, None, "relayed"));

    let lines = rendered(&mut chat, VIEWPORT);

    assert!(
        lines.iter().any(|line| line == "@ w1\u{276f}"),
        "a peer part on a reply heads its own block, got {lines:?}"
    );
    assert!(
        lines.iter().all(|line| !line.starts_with("\u{3e} ") && !line.starts_with("\u{25cf} ")),
        "neither the caret nor the bullet claims these words, got {lines:?}"
    );
}

/// **AC-7.** The whole of the teammate rendering in one frame: the
/// sender's `@ name\u{276f}` head painted `info` at the top of its block
/// with its own one-line summary dimmed beside it, what it said hanging
/// under that head in body text, and one caret for the entry however many
/// parts it arrived in.
///
/// The dump is symbols only, the palette-independent shape this crate's
/// snapshots use — so the two styles that carry the meaning here are
/// asserted beside it rather than left to a theme change to break.
///
/// It lives beside the pane it pins rather than in `app.rs`, because what
/// is under test is one component's own drawing; it writes into the crate's
/// one snapshot directory all the same.
#[test]
fn snapshot_teammate_message() {
    // Wide and tall enough for the whole entry: what this pins is which
    // glyph and which style leads each row, so no row may scroll off.
    const AREA: Rect = Rect { x: 0, y: 0, width: 46, height: 10 };

    let mut chat = Chat::default();
    let prompt = Message::user("what did w1 say");
    chat.start_message(prompt.clone());
    chat.start_part(
        &prompt.id,
        Part::peer(
            "w1",
            Some("picked up W2".to_owned()),
            None,
            "The protocol surface is mine.\nThe envelope is W6's.",
        ),
    );
    chat.start_part(&prompt.id, Part::peer("w2", None, None, "and I have it"));

    insta::with_settings!({snapshot_path => "../snapshots"}, {
        insta::assert_snapshot!(rendered(&mut chat, AREA).join("\n"));
    });

    let mut buffer = Buffer::empty(AREA);
    chat.render(AREA, &mut buffer, &Theme::default());
    let row_of = |needle: &str| {
        (0..AREA.height)
            .find(|row| {
                (0..AREA.width)
                    .map(|column| buffer[(column, *row)].symbol())
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("the frame holds {needle:?}"))
    };
    let theme = Theme::default();
    assert_eq!(
        buffer[(0, row_of("@ w1\u{276f} picked up W2"))].style().fg,
        theme.info.fg,
        "the head that says whose words these are is painted info"
    );
    assert_eq!(
        buffer[(6, row_of("@ w1\u{276f} picked up W2"))].style().fg,
        theme.dim.fg,
        "and its one-line summary recedes beside it"
    );
    assert_eq!(
        buffer[(2, row_of("The protocol surface"))].style().fg,
        theme.fg.fg,
        "and what it said is body text under it"
    );
}

#[test]
fn events_for_a_message_the_transcript_never_saw_are_ignored() {
    let mut chat = Chat::default();
    let orphan = Message::assistant("canned");
    let part = Part::text("");

    chat.start_part(&orphan.id, part.clone());
    chat.append_delta(&orphan.id, &part.id, "orphan");

    assert!(rendered(&mut chat, VIEWPORT).iter().all(String::is_empty));
}

#[test]
fn a_pending_tool_call_names_the_tool() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part::tool("call_1", "shell"));
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

    assert!(
        lines.iter().any(|line| line == "\u{25cf} Shell"),
        "a call whose arguments have not arrived names the tool alone, got {lines:?}"
    );
}

#[test]
fn a_running_tool_call_shows_a_title_derived_from_its_input() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "shell".to_owned(),
            state: ToolState::Running {
                input: serde_json::json!({"command": "cargo test"}),
                metadata: serde_json::Value::Null,
                started: 0,
            },
        },
    });
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

    assert!(
        lines.iter().any(|line| line == "\u{25cf} Shell(command: \"cargo test\")"),
        "got {lines:?}"
    );
}

/// An in-flight call's point winks — the full bullet bright through the
/// hold, shrinking and dimming to the small dot at the bottom — while
/// its words hold still (user directive, 2026-08-25): past the lead, the
/// two frames are the same text.
#[test]
fn an_in_flight_calls_point_winks_and_its_words_hold_still() {
    let area = Rect::new(0, 0, 60, 4);
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "shell".to_owned(),
            state: ToolState::Running {
                input: serde_json::json!({"command": "cargo test"}),
                metadata: serde_json::Value::Null,
                started: 0,
            },
        },
    });
    chat.start_message(reply);

    let frame = |chat: &mut Chat| {
        let mut buffer = Buffer::empty(area);
        chat.render(area, &mut buffer, &Theme::default());
        let words: Vec<String> = (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        (words, buffer[(0, 0)].style().fg)
    };

    chat.blink_epoch = Some(Instant::now());
    let (bright, lead) = frame(&mut chat);
    assert_eq!(
        bright[0], "\u{25cf} Shell(command: \"cargo test\")",
        "the crest wears the full bullet"
    );
    assert_eq!(lead, Theme::default().fg.fg, "bright through the hold");

    // 1600 ms into the wink: past the drop, flat at the bottom.
    chat.blink_epoch = Instant::now().checked_sub(Duration::from_millis(1_600));
    let (dim, lead) = frame(&mut chat);
    assert_eq!(dim[0], "\u{b7} Shell(command: \"cargo test\")", "the trough wears the small dot");
    assert_eq!(
        bright[0].chars().skip(2).collect::<String>(),
        dim[0].chars().skip(2).collect::<String>(),
        "past the lead the words hold still"
    );
    assert_eq!(bright[1..], dim[1..], "and every other row holds still");
    assert_eq!(lead, Theme::default().dim.fg, "the chrome's own dim at the bottom");
}

/// The wink's envelope, measured off the reference recording: bright
/// through the long hold, straight down past it, flat at the bottom,
/// easing back up to meet the next hold.
#[test]
fn the_points_wink_holds_drops_rests_and_rises() {
    let at = |ms: u64| super::point_level(Duration::from_millis(ms));

    assert_eq!(at(0), super::POINT_BRIGHT);
    assert_eq!(at(1_399), super::POINT_BRIGHT, "the hold runs long");
    let falling = at(1_470);
    assert!(falling > 0 && falling < super::POINT_BRIGHT, "down fast past the hold, got {falling}");
    assert_eq!(at(1_600), 0, "flat at the bottom");
    assert_eq!(at(1_850), 2, "easing back");
    assert_eq!(at(2_000), at(0), "one wink in, it starts over");

    let glyph = super::point_glyph;
    assert_eq!(glyph(super::POINT_BRIGHT), "\u{25cf} ", "biggest at the crest");
    assert_eq!(glyph(2), "\u{2022} ");
    assert_eq!(glyph(1), "\u{2219} ");
    assert_eq!(glyph(0), "\u{b7} ", "smallest at the trough");
}

/// Between its two ends the point actually fades — where the theme gives
/// both ends RGB values to fade between; the terminal theme's named
/// slots collapse to the nearer end instead.
#[test]
fn the_points_way_between_blends_where_the_theme_is_rgb() {
    use ratatui::style::{Color, Style};

    let mut theme = Theme::default();
    theme.dim = Style::new().fg(Color::Rgb(0, 0, 0));
    theme.fg = Style::new().fg(Color::Rgb(200, 100, 40));
    assert_eq!(
        super::point_style(&theme, 2).fg,
        Some(Color::Rgb(100, 50, 20)),
        "halfway up is halfway between"
    );
    assert_eq!(super::point_style(&theme, super::POINT_BRIGHT).fg, theme.fg.fg);
    assert_eq!(super::point_style(&theme, 0).fg, theme.dim.fg);

    let terminal = Theme::default();
    assert_eq!(
        super::point_style(&terminal, 3).fg,
        terminal.fg.fg,
        "a named palette's upper middle collapses to bright"
    );
    assert_eq!(super::point_style(&terminal, 1).fg, terminal.dim.fg, "and its lower middle to dim");
}

/// A thought renders its own markdown, folded into the thinking tone
/// (user directive, 2026-08-25): the `**` markers leave the screen, the
/// bold survives as bold beside the block's italic, and no markdown
/// color escapes the tone.
#[test]
fn a_thought_renders_its_markdown_bold_in_the_thinking_tone() {
    let area = Rect::new(0, 0, 60, 4);
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::ReasoningText {
            text: "**Confirming crate count** and planning".to_owned(),
        },
    });
    chat.start_message(reply);

    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer, &Theme::default());
    let row: String = (0..area.width)
        .map(|column| buffer[(column, 0)].symbol())
        .collect::<String>()
        .trim_end()
        .to_owned();
    assert_eq!(
        row, "\u{2234} Confirming crate count and planning",
        "the markers belong to the shape, not on the screen"
    );

    let theme = Theme::default();
    let bold = buffer[(2, 0)].style();
    assert!(bold.add_modifier.contains(Modifier::BOLD), "the heading keeps its bold");
    assert_eq!(bold.fg, theme.dim.fg, "and comes home to the dim");
    assert!(bold.add_modifier.contains(Modifier::ITALIC), "inside the block's own italic");
    let plain = buffer[(30, 0)].style();
    assert!(
        !plain.add_modifier.contains(Modifier::BOLD),
        "past the heading the thought is not bold"
    );
    assert_eq!(plain.fg, theme.dim.fg);
}

/// **AC1.** The whole grammar of a settled call in one screen: the bullet
/// and the condensed arguments on the header, the `⎿` marker carrying what
/// the tool called the work, the preview hanging under that marker's own
/// columns, and a hint naming what was cut and where the rest is.
#[test]
fn a_completed_tool_call_renders_as_a_bullet_a_result_marker_and_a_hanging_preview() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "grep".to_owned(),
            state: ToolState::Completed {
                input: serde_json::json!({"pattern": "fn main"}),
                output: "one\ntwo\nthree\nfour\nfive\nsix".to_owned(),
                title: "6 matches".to_owned(),
                metadata: serde_json::json!({}),
                started: 0,
                completed: 1,
            },
        },
    });
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));
    let drawn: Vec<&str> =
        lines.iter().map(String::as_str).filter(|line| !line.is_empty()).collect();

    assert_eq!(
        drawn,
        vec![
            "\u{25cf} Grep(pattern: \"fn main\")",
            "  \u{23bf} 6 matches",
            "    one",
            "    two",
            "    three",
            "    four",
            "    \u{2026} +2 lines (ctrl+t to expand)",
        ],
        "got {lines:?}"
    );
}

/// A settled `read`, as the screenshot pins it: the path bare and absolute
/// on the header, and a count as the whole of the result — no preview, and
/// none of the envelope the tool writes for the model.
#[test]
fn a_settled_read_is_a_path_and_a_count_and_nothing_else() {
    let lines = tool_call(
        "read",
        ToolState::Completed {
            input: serde_json::json!({"filePath": "/repo/src/lib.rs"}),
            output: "<path>/repo/src/lib.rs</path>\n<content>\n1: fn main() {}\n</content>"
                .to_owned(),
            title: "src/lib.rs".to_owned(),
            metadata: serde_json::json!({
                "display": {
                    "type": "file",
                    "path": "/repo/src/lib.rs",
                    "lineStart": 1,
                    "lineEnd": 77,
                    "totalLines": 77,
                },
            }),
            started: 0,
            completed: 1,
        },
    );
    let drawn: Vec<&str> =
        lines.iter().map(String::as_str).filter(|line| !line.is_empty()).collect();

    assert_eq!(
        drawn,
        vec!["\u{25cf} Read(/repo/src/lib.rs)", "  \u{23bf} Read 77 lines"],
        "got {lines:?}"
    );
}

/// A read that asked for a range says so, and says it the same way before
/// and after the answer arrives — the header is about what was asked.
#[test]
fn a_read_of_a_range_names_it_and_names_it_the_same_while_running() {
    let input = serde_json::json!({
        "filePath": "/repo/src/lib.rs",
        "offset": 1158,
        "limit": 60,
    });
    let running = tool_call(
        "read",
        ToolState::Running { input: input.clone(), metadata: serde_json::Value::Null, started: 0 },
    );
    let settled = tool_call(
        "read",
        ToolState::Completed {
            input,
            output: "the envelope".to_owned(),
            title: "src/lib.rs".to_owned(),
            metadata: serde_json::json!({
                "display": {
                    "type": "file",
                    "path": "/repo/src/lib.rs",
                    "lineStart": 1158,
                    "lineEnd": 1217,
                },
            }),
            started: 0,
            completed: 1,
        },
    );

    let header =
        |lines: &[String]| lines.iter().find(|line| !line.is_empty()).cloned().unwrap_or_default();
    assert_eq!(header(&running), "\u{25cf} Read(/repo/src/lib.rs \u{b7} lines 1158-1217)");
    assert_eq!(header(&settled), "\u{25cf} Read(/repo/src/lib.rs \u{b7} lines 1158-1217)");
    assert!(settled.iter().any(|line| line == "  \u{23bf} Read 60 lines"), "got {settled:?}");
    assert!(
        !settled.iter().any(|line| line.contains("envelope")),
        "the tool's output is the model's, not the transcript's: {settled:?}"
    );
}

/// An open-ended read is not a range: an `offset` with no `limit` stops
/// wherever the file does, which nothing can name before it is read.
#[test]
fn a_read_from_a_line_to_the_end_of_the_file_claims_no_range() {
    let lines = tool_call(
        "read",
        ToolState::Running {
            input: serde_json::json!({"filePath": "/repo/a.rs", "offset": 40}),
            metadata: serde_json::Value::Null,
            started: 0,
        },
    );

    assert!(lines.iter().any(|line| line == "\u{25cf} Read(/repo/a.rs)"), "got {lines:?}");
}

/// The count is of what was read, so an empty file reports none — and a
/// read that is not of a file at all keeps the rendering every other tool
/// has, because a line count is not what it did.
#[test]
fn a_read_that_is_not_of_a_files_lines_keeps_the_ordinary_shape() {
    let empty = tool_call(
        "read",
        ToolState::Completed {
            input: serde_json::json!({"filePath": "/repo/empty.rs"}),
            output: String::new(),
            title: "empty.rs".to_owned(),
            metadata: serde_json::json!({
                "display": {"type": "file", "lineStart": 1, "lineEnd": 0},
            }),
            started: 0,
            completed: 1,
        },
    );
    assert!(empty.iter().any(|line| line == "  \u{23bf} Read 0 lines"), "got {empty:?}");

    // A PDF is the kind of read that is neither a file's lines nor a
    // directory's entries: `ganja_tool::read` publishes no `display` block
    // for one at all, so there is nothing here to count.
    let pdf = tool_call(
        "read",
        ToolState::Completed {
            input: serde_json::json!({"filePath": "/repo/paper.pdf"}),
            output: "PDF read successfully. This tool cannot hand file bytes to the model yet."
                .to_owned(),
            title: "paper.pdf".to_owned(),
            metadata: serde_json::json!({
                "preview": "PDF read successfully",
                "truncated": false,
                "mime": "application/pdf",
            }),
            started: 0,
            completed: 1,
        },
    );
    assert!(
        pdf.iter().any(|line| line == "  \u{23bf} paper.pdf")
            && pdf.iter().any(|line| line.contains("PDF read successfully")),
        "a read that counted nothing keeps the ordinary shape, got {pdf:?}"
    );
}

/// A settled read of a **directory**, as the second screenshot pins it: the
/// path on the header and a count of what was listed as the whole of the
/// result — none of the envelope the tool writes for the model.
#[test]
fn a_settled_read_of_a_directory_is_a_count_and_no_envelope() {
    let lines = tool_call(
        "read",
        ToolState::Completed {
            input: serde_json::json!({"filePath": "/repo/src"}),
            output: "<path>/repo/src</path>\n<type>directory</type>\n<entries>\n\
                         lib.rs\nmain.rs\ncomponent/\n\n(3 entries)\n</entries>"
                .to_owned(),
            title: "src".to_owned(),
            metadata: serde_json::json!({
                "display": {
                    "type": "directory",
                    "path": "/repo/src",
                    "entries": ["component/", "lib.rs", "main.rs"],
                    "offset": 1,
                    "totalEntries": 3,
                    "truncated": false,
                },
            }),
            started: 0,
            completed: 1,
        },
    );
    let drawn: Vec<&str> =
        lines.iter().map(String::as_str).filter(|line| !line.is_empty()).collect();

    assert_eq!(
        drawn,
        vec!["\u{25cf} Read(/repo/src)", "  \u{23bf} Listed 3 entries"],
        "got {lines:?}"
    );
}

/// The header states the ask, on a directory as on a file: a listing that
/// asked for a window says which one, and the count under it is of what
/// that window actually held.
#[test]
fn a_read_of_a_range_of_a_directory_keeps_the_range_it_asked_for() {
    let lines = tool_call(
        "read",
        ToolState::Completed {
            input: serde_json::json!({
                "filePath": "/repo/src",
                "offset": 3,
                "limit": 2,
            }),
            output: "<path>/repo/src</path>\n<type>directory</type>\n<entries>\n\
                         lib.rs\nmain.rs\n\n(Showing 2 of 9 entries. \
                         Use 'offset' parameter to read beyond entry 5)\n</entries>"
                .to_owned(),
            title: "src".to_owned(),
            metadata: serde_json::json!({
                "display": {
                    "type": "directory",
                    "path": "/repo/src",
                    "entries": ["lib.rs", "main.rs"],
                    "offset": 3,
                    "totalEntries": 9,
                    "truncated": true,
                },
            }),
            started: 0,
            completed: 1,
        },
    );

    assert!(
        lines.iter().any(|line| line == "\u{25cf} Read(/repo/src \u{b7} lines 3-4)")
            && lines.iter().any(|line| line == "  \u{23bf} Listed 2 entries"),
        "got {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("<entries>")),
        "the envelope is the model's, not the transcript's: {lines:?}"
    );
}

/// A `todowrite` carrying `todos` in each state the tool defines.
fn todo_call(todos: serde_json::Value) -> ToolState {
    ToolState::Completed {
        input: serde_json::json!({ "todos": todos }),
        output: "[\n  {\n    \"content\": \"port cell.slang\"\n  }\n]".to_owned(),
        title: "2 todos".to_owned(),
        metadata: serde_json::json!({}),
        started: 0,
        completed: 1,
    }
}

/// The list every checklist test writes, one task per state that draws
/// differently.
fn todos() -> serde_json::Value {
    serde_json::json!([
        {"content": "port cell.slang", "status": "completed", "priority": "high"},
        {"content": "port graphics.slang", "status": "in_progress", "priority": "high"},
        {"content": "port bgimage.slang", "status": "pending", "priority": "medium"},
        {"content": "port the old shim", "status": "cancelled", "priority": "low"},
    ])
}

/// **The checklist screenshot.** A settled `todowrite` answers with the list
/// itself — a box per task, the first on the elbow and the rest hanging
/// under it — and never with the JSON the list travelled in.
#[test]
fn a_settled_todowrite_draws_its_list_as_a_checklist() {
    let lines = tool_call("todowrite", todo_call(todos()));
    let drawn: Vec<&str> =
        lines.iter().map(String::as_str).filter(|line| !line.is_empty()).collect();

    assert_eq!(
        drawn,
        vec![
            "\u{25cf} Todowrite(todos: [\u{2026}])",
            "  \u{23bf} \u{2612} port cell.slang",
            "    \u{2610} port graphics.slang",
            "    \u{2610} port bgimage.slang",
            "    \u{2612} port the old shim",
        ],
        "got {lines:?}"
    );
}

/// Each state is told by its box and by how the row is painted: the one
/// being worked on stands out, and the two nobody will work on again are
/// struck through.
#[test]
fn a_checklist_paints_the_task_in_hand_and_strikes_the_ones_that_are_done() {
    let theme = Theme::default();
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "todowrite".to_owned(),
            state: todo_call(todos()),
        },
    });
    chat.start_message(reply);

    let area = Rect::new(0, 0, 60, 10);
    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer, &theme);

    // Row 0 is the header, so the four tasks follow in the order written;
    // column 6 is the first column of each row's own words, past the
    // marker columns and the box.
    let done = buffer[(6, 1)].style();
    let in_hand = buffer[(6, 2)].style();
    let pending = buffer[(6, 3)].style();
    let cancelled = buffer[(6, 4)].style();

    assert!(
        done.add_modifier.contains(Modifier::CROSSED_OUT)
            && cancelled.add_modifier.contains(Modifier::CROSSED_OUT),
        "a finished task is struck through, got {done:?} and {cancelled:?}"
    );
    assert!(
        !buffer[(0, 1)].style().add_modifier.contains(Modifier::CROSSED_OUT),
        "the strike stays off the margin rather than ruling a line out to the left"
    );
    assert!(
        in_hand.add_modifier.contains(Modifier::BOLD) && in_hand.fg == theme.accent.fg,
        "the task in hand is the one the eye should land on, got {in_hand:?}"
    );
    assert!(
        !pending.add_modifier.contains(Modifier::CROSSED_OUT) && pending.fg == theme.fg.fg,
        "a task still to do is ordinary body text, got {pending:?}"
    );
}

/// An argument that is not a list this can draw keeps the preview every
/// other tool's result has: what really arrived is more use than a `⎿`
/// pointing at nothing.
#[test]
fn a_todowrite_whose_list_cannot_be_read_keeps_the_ordinary_preview() {
    for todos in [
        serde_json::json!("all of them"),
        serde_json::json!([]),
        serde_json::json!([{"status": "pending"}]),
    ] {
        let lines = tool_call("todowrite", todo_call(todos.clone()));

        assert!(
            lines.iter().any(|line| line == "  \u{23bf} 2 todos")
                && lines.iter().any(|line| line.contains("port cell.slang")),
            "the tool's own title and preview stand in for {todos}: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains(super::TODO_OPEN) || line.contains(super::TODO_DONE)),
            "nothing is drawn as a checklist it is not: {lines:?}"
        );
    }
}

/// **The checklist screenshot's other half.** While a turn runs, its newest
/// list hangs under the working line in the strip pinned above the
/// composer; when the turn settles the strip goes, and the call's own rows
/// stay where they are.
#[test]
fn the_working_line_carries_this_turns_checklist_and_drops_it_on_settle() {
    let mut chat = Chat::default();
    chat.start_message(Message::user("port the shaders"));
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "todowrite".to_owned(),
            state: todo_call(todos()),
        },
    });
    chat.start_message(reply);
    chat.set_working(Some(Working {
        started: Instant::now(),
        turn: 1,
        output_tokens: 0,
        compaction: None,
    }));

    let boxes = |lines: &[String]| {
        lines
            .iter()
            .filter(|line| line.contains(super::TODO_OPEN) || line.contains(super::TODO_DONE))
            .count()
    };
    let area = Rect::new(0, 0, 60, 24);
    let transcript = rendered(&mut chat, area);
    let running = strip(&mut chat, 60);

    assert!(
        !transcript.iter().any(|line| line.contains("\u{2026} (")),
        "the transcript itself no longer carries the line: {transcript:?}"
    );
    assert_eq!(boxes(&transcript), 4, "the call's own rows stay in the transcript: {transcript:?}");
    assert!(
        running.first().is_some_and(|line| line.contains("\u{2026} (")),
        "the strip opens on the working line, got {running:?}"
    );
    assert_eq!(boxes(&running), 4, "and carries this turn's list: {running:?}");
    assert_eq!(
        running[1], "  \u{23bf} \u{2612} port cell.slang",
        "the copy hangs off the working line's own elbow: {running:?}"
    );

    chat.set_working(None);
    let settled = strip(&mut chat, 60);

    assert!(settled.is_empty(), "a settled turn leaves no strip: {settled:?}");
    assert_eq!(
        boxes(&rendered(&mut chat, area)),
        4,
        "and the transcript keeps the rows it already drew"
    );
}

/// The copy under the working line is *this* turn's: a plan the last turn
/// wrote is not what the running one is working through.
#[test]
fn the_working_line_carries_no_checklist_from_a_turn_that_is_over() {
    let mut chat = Chat::default();
    chat.start_message(Message::user("port the shaders"));
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "todowrite".to_owned(),
            state: todo_call(todos()),
        },
    });
    chat.start_message(reply);
    chat.start_message(Message::user("now something else"));
    chat.start_message(Message::assistant("canned"));
    chat.set_working(Some(Working {
        started: Instant::now(),
        turn: 2,
        output_tokens: 0,
        compaction: None,
    }));

    let lines = strip(&mut chat, 60);

    assert!(
        lines.first().is_some_and(|line| line.contains("\u{2026} (")),
        "the strip opens on the working line: {lines:?}"
    );
    assert_eq!(
        lines.len(),
        1,
        "the new turn has written no list, so nothing hangs under it: {lines:?}"
    );
}

/// **AC2.** A call that is running and the same call once it has settled
/// are announced by the same words — at the wink's crest, by the very
/// same line: what tells them apart there is that the running lead
/// breathes while the settled one stands in its verdict color, not a
/// word in the text (2026-08-25).
#[test]
fn a_running_call_and_its_settled_self_share_their_header_words() {
    let input = serde_json::json!({"command": "cargo test"});
    let running = tool_call(
        "shell",
        ToolState::Running { input: input.clone(), metadata: serde_json::Value::Null, started: 0 },
    );
    let completed = tool_call(
        "shell",
        ToolState::Completed {
            input: input.clone(),
            output: String::new(),
            title: "cargo test".to_owned(),
            metadata: serde_json::json!({}),
            started: 0,
            completed: 1,
        },
    );
    let failed = tool_call(
        "shell",
        ToolState::Error { input, error: "no such command".to_owned(), started: 0, completed: 1 },
    );

    let header =
        |lines: &[String]| lines.iter().find(|line| !line.is_empty()).cloned().unwrap_or_default();
    assert_eq!(header(&running), "\u{25cf} Shell(command: \"cargo test\")");
    assert_eq!(header(&running), header(&completed));
    assert_eq!(header(&completed), header(&failed));
}

/// A header is one line, so the arguments on it are capped — and the cut
/// is admitted rather than left to look like the whole call.
#[test]
fn a_header_names_a_few_arguments_and_says_when_it_left_some_out() {
    let lines = tool_call(
        "grep",
        ToolState::Running {
            input: serde_json::json!({
                "include": "*.rs",
                "pattern": "fn main",
                "path": "src",
                "limit": 20,
                "todos": ["one", "two"],
            }),
            metadata: serde_json::Value::Null,
            started: 0,
        },
    );

    assert!(
        lines.iter().any(|line| line
            == "\u{25cf} Grep(path: \"src\", pattern: \"fn main\", include: \"*.rs\", \u{2026})"),
        "the recognizable fields come first and the cut is named, got {lines:?}"
    );
}

/// A value that would not fit a line — a whole file a `write` carries, a
/// command typed over several lines — is cut to something a header can
/// hold, and says so.
#[test]
fn a_header_cuts_an_argument_too_long_or_too_tall_to_draw() {
    let lines = tool_call(
        "write",
        ToolState::Running {
            input: serde_json::json!({
                "filePath": "a.rs",
                "content": "fn main() {\n    println!(\"hello\");\n}\n",
            }),
            metadata: serde_json::Value::Null,
            started: 0,
        },
    );

    assert!(
        lines
            .iter()
            .any(|line| line
                == "\u{25cf} Write(filePath: \"a.rs\", content: \"fn main() {\u{2026}\")"),
        "got {lines:?}"
    );
}

/// A nested payload is named by the shape it has rather than drawn: it is
/// still an argument the header must admit to, and still not one a single
/// line can carry.
#[test]
fn a_header_names_a_nested_argument_by_its_shape() {
    let lines = tool_call(
        "todowrite",
        ToolState::Running {
            input: serde_json::json!({"todos": [{"content": "one"}]}),
            metadata: serde_json::Value::Null,
            started: 0,
        },
    );

    assert!(
        lines.iter().any(|line| line == "\u{25cf} Todowrite(todos: [\u{2026}])"),
        "got {lines:?}"
    );
}

/// **Pre-mortem 1.** The marker's columns are measured, so a preview line
/// the viewport has to wrap keeps hanging under the marker instead of
/// sliding back to the margin.
#[test]
fn a_wrapped_preview_line_keeps_hanging_under_its_own_marker() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "read".to_owned(),
            state: ToolState::Completed {
                input: serde_json::json!({}),
                output: "alpha bravo charlie delta".to_owned(),
                title: String::new(),
                metadata: serde_json::json!({}),
                started: 0,
                completed: 1,
            },
        },
    });
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 18, 10));
    let drawn: Vec<&str> =
        lines.iter().map(String::as_str).filter(|line| !line.is_empty()).collect();

    assert_eq!(
        drawn,
        vec!["\u{25cf} Read", "  \u{23bf} alpha bravo", "    charlie delta",],
        "the wrapped remainder sits under what the marker introduced, got {lines:?}"
    );
}

#[test]
fn a_completed_tool_call_shows_its_title_and_a_clamped_output_preview() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "grep".to_owned(),
            state: ToolState::Completed {
                input: serde_json::json!({"pattern": "fn main"}),
                output: "one\ntwo\nthree\nfour\nfive".to_owned(),
                title: "5 matches".to_owned(),
                metadata: serde_json::json!({}),
                started: 0,
                completed: 1,
            },
        },
    });
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

    assert!(
        lines.iter().any(|line| line.contains("\u{25cf} Grep(pattern: \"fn main\")")),
        "got {lines:?}"
    );
    assert!(lines.iter().any(|line| line.contains("one")), "got {lines:?}");
    assert!(
        lines.iter().any(|line| line.contains("\u{2026} +1 line (ctrl+t to expand)")),
        "five lines should clamp to four plus a hint naming the one cut, got {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("five")),
        "the fifth line should have been clamped away, got {lines:?}"
    );
}

#[test]
fn a_completed_tool_call_prefers_its_diff_over_plain_output() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "edit".to_owned(),
            state: ToolState::Completed {
                input: serde_json::json!({"filePath": "a.rs"}),
                output: "PLAIN_OUTPUT_MARKER".to_owned(),
                title: "a.rs".to_owned(),
                metadata: serde_json::json!({
                    "diff": "+DIFF_ADDED_MARKER\n-DIFF_REMOVED_MARKER"
                }),
                started: 0,
                completed: 1,
            },
        },
    });
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

    assert!(lines.iter().any(|line| line.contains("DIFF_ADDED_MARKER")));
    assert!(lines.iter().any(|line| line.contains("DIFF_REMOVED_MARKER")));
    assert!(
        !lines.iter().any(|line| line.contains("PLAIN_OUTPUT_MARKER")),
        "a diff should be shown instead of the plain output, got {lines:?}"
    );
}

#[test]
fn an_errored_tool_call_shows_only_the_first_line_of_the_error() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_1".to_owned()),
        body: PartBody::Tool {
            call_id: "call_1".to_owned(),
            tool: "shell".to_owned(),
            state: ToolState::Error {
                input: serde_json::json!({"command": "rm -rf /"}),
                error: "refused: destructive command\nsecond line stays out of the transcript"
                    .to_owned(),
                started: 0,
                completed: 1,
            },
        },
    });
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

    assert!(
        lines.iter().any(|line| line.contains("\u{25cf} Shell(command: \"rm -rf /\")")),
        "got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "  \u{23bf} [error] refused: destructive command"),
        "got {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("second line")),
        "only the first line of the error should show, got {lines:?}"
    );
}

#[test]
fn update_part_replaces_a_known_id_and_appends_an_unknown_one() {
    let mut chat = Chat::default();
    let reply = Message::assistant("canned");
    let known = Part::tool("call_1", "shell");
    chat.start_message(reply.clone());
    chat.start_part(&reply.id, known.clone());

    chat.update_part(
        &reply.id,
        Part {
            id: known.id.clone(),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"command": "echo hi"}),
                    output: "hi".to_owned(),
                    title: "echo hi".to_owned(),
                    metadata: serde_json::json!({}),
                    started: 0,
                    completed: 1,
                },
            },
        },
    );
    chat.update_part(
        &reply.id,
        Part {
            id: PartId::from("prt_unseen".to_owned()),
            body: PartBody::Tool {
                call_id: "call_2".to_owned(),
                tool: "read".to_owned(),
                state: ToolState::Pending { input: None },
            },
        },
    );

    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

    assert!(
        lines.iter().any(|line| line.contains("\u{25cf} Shell(command: \"echo hi\")")),
        "the known id should be replaced in place, got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "\u{25cf} Read"),
        "an update for an id never started should still append, got {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line == "\u{25cf} Shell"),
        "the pending block should have been replaced, not kept alongside, got {lines:?}"
    );
}

/// A dead turn's reason belongs where the person is looking: the
/// provider's words land under the reply they ended, in the error style,
/// and a message the transcript never met says so instead of vanishing.
#[test]
fn a_failed_turns_error_is_painted_under_its_reply() {
    let mut chat = Chat::default();
    let reply = Message::assistant("canned");
    chat.start_message(reply.clone());

    assert!(
        chat.set_error(&reply.id, "Our servers are currently overloaded.".to_owned()),
        "the reply is on the transcript, so the error has a home"
    );
    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));
    assert!(
        lines.iter().any(|line| line.contains("[error] Our servers are currently overloaded.")),
        "the error paints under the reply, got {lines:?}"
    );

    assert!(
        !chat.set_error(&MessageId::from("msg_ghost".to_owned()), "lost".to_owned()),
        "an entry the transcript never met reports itself unplaceable"
    );
}

/// A reply whose process died mid-stream reads as a reply that simply
/// stopped talking. Saying so is the difference between a transcript that
/// is incomplete and one that is wrong.
#[test]
fn a_resumed_reply_the_store_never_saw_finish_says_it_was_interrupted() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part::text("half a thought"));
    assert_eq!(reply.time.completed, None, "the fixture must be unfinished");
    chat.restore_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 70, 20));

    assert!(
        lines.iter().any(|line| line.contains("[interrupted]")),
        "an unfinished stored reply should say so, got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("half a thought")),
        "what did reach the disk still has to render, got {lines:?}"
    );
}

#[test]
fn a_resumed_reply_that_finished_carries_no_interrupted_marker() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part::text("a whole thought"));
    reply.complete();
    chat.restore_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 70, 20));

    assert!(
        !lines.iter().any(|line| line.contains("[interrupted]")),
        "a completed reply must not be accused of dying, got {lines:?}"
    );
}

/// The same field is absent on a reply that is merely still arriving, so
/// the marker cannot key on it alone.
#[test]
fn a_streaming_reply_is_not_mistaken_for_an_interrupted_one() {
    let mut chat = Chat::default();
    let reply = Message::assistant("canned");
    let part = Part::text("");
    chat.start_message(reply.clone());
    chat.start_part(&reply.id, part.clone());
    chat.append_delta(&reply.id, &part.id, "still arriving");

    let lines = rendered(&mut chat, Rect::new(0, 0, 70, 20));

    assert!(
        !lines.iter().any(|line| line.contains("[interrupted]")),
        "a live reply is unfinished, not interrupted, got {lines:?}"
    );
}

/// Only a reply can be cut off mid-sentence: a user message is whole the
/// moment it is sent, whatever the store recorded about its clock.
#[test]
fn a_resumed_prompt_is_never_marked_interrupted() {
    let mut chat = Chat::default();
    let mut prompt = Message::user("what did I ask");
    prompt.time.completed = None;
    chat.restore_message(prompt);

    let lines = rendered(&mut chat, Rect::new(0, 0, 70, 20));

    assert!(!lines.iter().any(|line| line.contains("[interrupted]")), "got {lines:?}");
}

#[test]
fn clearing_leaves_nothing_of_the_previous_session_on_screen() {
    let mut chat = Chat::default();
    transcript(&mut chat, 20);
    rendered(&mut chat, VIEWPORT);

    chat.clear();

    assert!(rendered(&mut chat, VIEWPORT).iter().all(String::is_empty));
    assert!(chat.is_following_tail());
}

/// The `!` passthrough streams its output into a running part, so the
/// transcript has to show what has arrived rather than waiting for the
/// command to end.
#[test]
fn a_running_call_that_reports_as_it_goes_shows_the_newest_of_it() {
    let lines = tool_call(
        "bash",
        ToolState::Running {
            input: serde_json::json!({"command": "cargo test"}),
            metadata: serde_json::json!({
                "output": "compiling\nrunning 1 test\ntest a ... ok\ntest b ... ok\ntest c ... ok\nfinished"
            }),
            started: 0,
        },
    );

    assert!(
        lines.iter().any(|line| line.contains("finished")),
        "the newest line has to be on screen, got {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("compiling")),
        "the oldest lines are the ones that scroll off, got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "  \u{23bf} \u{2026} +2 lines (ctrl+t to expand)"),
        "and the cut has to be admitted, above what it cut, got {lines:?}"
    );
}

/// The common case has no such field, and its rows must not change.
#[test]
fn a_running_call_that_reports_nothing_is_one_line_as_it_always_was() {
    let lines = tool_call(
        "read",
        ToolState::Running {
            input: serde_json::json!({"filePath": "a.rs"}),
            metadata: serde_json::Value::Null,
            started: 0,
        },
    );
    let drawn: Vec<&String> = lines.iter().filter(|line| !line.is_empty()).collect();

    assert_eq!(drawn, vec![&"\u{25cf} Read(a.rs)".to_owned()], "got {lines:?}");
}

/// A call waiting its turn behind the step's earlier calls names its
/// arguments the moment the stream finishes saying them (2026-08-15), and
/// stays a bare name while they are still arriving.
#[test]
fn a_waiting_call_names_its_arguments_once_they_have_settled() {
    let named = tool_call(
        "shell",
        ToolState::Pending { input: Some(serde_json::json!({"command": "cargo test"})) },
    );
    assert!(
        named.iter().any(|line| line == "\u{25cf} Shell(command: \"cargo test\")"),
        "got {named:?}"
    );

    let streaming = tool_call("shell", ToolState::Pending { input: None });
    assert!(streaming.iter().any(|line| line == "\u{25cf} Shell"), "got {streaming:?}");
}

/// A delegated turn is one row: an icon, who is doing it, what they were
/// asked, and what they are doing about it now.
#[test]
fn a_running_task_names_the_agent_the_ask_and_the_tool_it_is_in() {
    let lines = tool_call(
        "task",
        ToolState::Running {
            input: serde_json::json!({
                "description": "find the parser",
                "subagent_type": "explore",
            }),
            metadata: serde_json::json!({"current_tool": "grep parser", "toolcalls": 3}),
            started: 0,
        },
    );

    assert!(
        lines
            .iter()
            .any(|line| line
                == "\u{25cf} Task(agent: \"explore\", description: \"find the parser\")"),
        "got {lines:?}"
    );
    assert!(lines.iter().any(|line| line == "  \u{23bf} grep parser"), "got {lines:?}");
}

/// Between tools there is no current one, so the count is what the row has
/// to say.
#[test]
fn a_running_task_between_tools_counts_them_instead() {
    let lines = tool_call(
        "task",
        ToolState::Running {
            input: serde_json::json!({"description": "find the parser"}),
            metadata: serde_json::json!({"toolcalls": 3}),
            started: 0,
        },
    );

    assert!(lines.iter().any(|line| line == "  \u{23bf} 3 toolcalls"), "got {lines:?}");
    assert!(
        lines.iter().any(|line| line == "\u{25cf} Task(description: \"find the parser\")"),
        "an agent nobody named is left off rather than invented, got {lines:?}"
    );
}

/// What runs inside a task is on the row, not behind a count
/// (2026-08-15): once the watcher's log arrives, the newest calls hang
/// under the heading in call order, the cut admitted above them off the
/// true total.
#[test]
fn a_running_task_expands_the_childs_recent_calls_and_admits_the_cut() {
    let lines = tool_call(
        "task",
        ToolState::Running {
            input: serde_json::json!({
                "description": "map it",
                "subagent_type": "explore",
            }),
            metadata: serde_json::json!({
                "toolcalls": 7,
                "current_tool": "grep five",
                "calls": ["grep one", "grep two", "grep three", "grep four", "grep five"],
            }),
            started: 0,
        },
    );

    assert!(
        lines.iter().any(|line| line == "  \u{23bf} \u{2026} +3 lines (ctrl+t to expand)"),
        "three of seven calls are off the row and said to be: {lines:?}"
    );
    let two = lines
        .iter()
        .position(|line| line == "    grep two")
        .expect("the oldest shown call is on screen");
    assert_eq!(
        lines[two + 3],
        "    grep five",
        "the newest call ends the block in call order: {lines:?}"
    );
    assert!(!lines.iter().any(|line| line.contains("grep one")), "the cut call is cut: {lines:?}");
}

/// What the child actually said is inside the tool result the model reads.
/// Printing it here would show the same work twice — once as the row, once
/// as prose the user never asked to see.
#[test]
fn a_finished_task_reports_its_shape_and_never_the_childs_answer() {
    let lines = tool_call(
        "task",
        ToolState::Completed {
            input: serde_json::json!({
                "description": "find the parser",
                "subagent_type": "explore",
            }),
            output: "<task id=\"tsk_1\" state=\"completed\"><task_result>\
                         THE CHILD'S OWN ANSWER</task_result></task>"
                .to_owned(),
            title: "find the parser".to_owned(),
            metadata: serde_json::json!({
                "session": "ses_child",
                "agent": "explore",
                "model": "fake",
                "toolcalls": 7,
            }),
            started: 1_000,
            completed: 13_400,
        },
    );

    assert!(
        lines
            .iter()
            .any(|line| line
                == "\u{25cf} Task(agent: \"explore\", description: \"find the parser\")"),
        "got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "  \u{23bf} 7 toolcalls \u{b7} 12.4s"),
        "got {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("THE CHILD'S OWN ANSWER")),
        "the child's answer belongs to the model, not to the row, got {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("task_result")),
        "and neither does the envelope it came in, got {lines:?}"
    );
}

/// A refused delegation is a refused call, and reads like every other one.
#[test]
fn a_failed_task_keeps_the_shape_every_other_failure_has() {
    let lines = tool_call(
        "task",
        ToolState::Error {
            input: serde_json::json!({"description": "find the parser"}),
            error: "no agent named parser-hunter".to_owned(),
            started: 0,
            completed: 1,
        },
    );

    assert!(
        lines.iter().any(|line| line.contains("\u{25cf} Task(description: \"find the parser\")")),
        "got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "  \u{23bf} [error] no agent named parser-hunter"),
        "got {lines:?}"
    );
}

/// A teammate spawn is not a delegated child, and the row may not read as
/// one (2026-09-03, bead `gaqe`): it names the teammate the roster and the
/// next `send_message` address, the backend it was seated on, and how long
/// the launch took. The `0 toolcalls` this row used to print was the absence
/// of a key rather than a count, and the `agent:` beside it named a role hint
/// as an agent that had run.
#[test]
fn a_finished_teammate_spawn_names_the_teammate_and_counts_nothing() {
    let lines = tool_call_at(
        "task",
        ToolState::Completed {
            input: serde_json::json!({
                "backend": "claude",
                "description": "Strict specification review",
                "name": "claude-review",
                "prompt": "review this branch strictly",
                "subagent_type": "critic",
            }),
            output: "Teammate started: claude-review on the claude backend. \
                     Send it work with send_message."
                .to_owned(),
            title: "Strict specification review".to_owned(),
            metadata: serde_json::json!({
                "teammate": "claude-review",
                "agent_id": "claude-review@session-01a06361",
                "backend": "claude",
            }),
            started: 1_788_373_839_733,
            completed: 1_788_373_839_913,
        },
        120,
    );

    assert!(
        lines.iter().any(|line| line
            == "\u{25cf} Task(teammate: \"claude-review\", backend: \"claude\", \
                description: \"Strict specification review\")"),
        "got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "  \u{23bf} teammate started \u{b7} 180ms"),
        "got {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("toolcalls")),
        "a count is a child's fact and a spawn has no child, got {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("agent:")),
        "and the role hint is not an agent that ran, got {lines:?}"
    );
}

/// While the launch is in flight nothing has answered yet, so the row says
/// exactly that — never the `0 toolcalls` a delegation would show between
/// tools, which for a spawn is a count of nothing that was ever coming.
#[test]
fn a_teammate_spawn_in_flight_says_it_is_starting() {
    let lines = tool_call_at(
        "task",
        ToolState::Running {
            input: serde_json::json!({
                "backend": "codex",
                "description": "Strict correctness review",
                "name": "codex-review",
                "subagent_type": "critic",
            }),
            metadata: serde_json::Value::Null,
            started: 0,
        },
        120,
    );

    assert!(
        lines.iter().any(|line| line
            == "\u{25cf} Task(teammate: \"codex-review\", backend: \"codex\", \
                description: \"Strict correctness review\")"),
        "got {lines:?}"
    );
    assert!(lines.iter().any(|line| line == "  \u{23bf} teammate starting"), "got {lines:?}");
    assert!(
        !lines.iter().any(|line| line.contains("toolcalls")),
        "nothing has run to be counted, got {lines:?}"
    );
}

/// Where the teammate was actually seated is the team's answer, not the
/// call's ask: a spawn that named no backend still says which one it got.
#[test]
fn a_finished_spawn_reads_its_backend_from_what_the_team_answered() {
    let lines = tool_call_at(
        "task",
        ToolState::Completed {
            input: serde_json::json!({
                "description": "Strict architecture review",
                "name": "grok-review",
                "subagent_type": "critic",
            }),
            output: "Teammate started: grok-review on the grok backend.".to_owned(),
            title: "Strict architecture review".to_owned(),
            metadata: serde_json::json!({
                "teammate": "grok-review",
                "agent_id": "grok-review@session-01a06361",
                "backend": "grok",
            }),
            started: 0,
            completed: 1_635,
        },
        120,
    );

    assert!(
        lines.iter().any(|line| line
            == "\u{25cf} Task(teammate: \"grok-review\", backend: \"grok\", \
                description: \"Strict architecture review\")"),
        "got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "  \u{23bf} teammate started \u{b7} 1.6s"),
        "got {lines:?}"
    );
}

/// A spawn that never started is a failed call and keeps every other failed
/// call's shape — which already names the teammate, off the raw arguments,
/// because that is the one thing the reader needs to know which member of the
/// roster is missing.
#[test]
fn a_refused_spawn_still_names_the_teammate_it_could_not_start() {
    let lines = tool_call_at(
        "task",
        ToolState::Error {
            input: serde_json::json!({
                "backend": "grok",
                "description": "Strict architecture review",
                "name": "reviewer-3",
                "prompt": "review this branch strictly",
                "subagent_type": "critic",
            }),
            error: "the grok backend is unavailable: grok exited before its composer was ready"
                .to_owned(),
            started: 0,
            completed: 12,
        },
        120,
    );

    assert!(
        lines.iter().any(|line| line.contains("name: \"reviewer-3\"")),
        "which member could not be started is the fact, got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line
            == "  \u{23bf} [error] the grok backend is unavailable: \
                grok exited before its composer was ready"),
        "got {lines:?}"
    );
}

/// The status bar's `N tasks running` means delegated children in flight
/// (**D462**). A spawn rides the same tool id and is not one of them, so
/// three teammates starting at once must not read as three turns the session
/// is waiting on (2026-09-03, bead `gaqe`).
#[test]
fn running_tasks_counts_delegated_children_and_not_teammate_spawns() {
    let delegated = ToolState::Running {
        input: serde_json::json!({"description": "find the parser", "subagent_type": "explore"}),
        metadata: serde_json::json!({"toolcalls": 3}),
        started: 0,
    };
    let spawn = ToolState::Running {
        input: serde_json::json!({
            "backend": "claude",
            "description": "Strict specification review",
            "name": "claude-review",
        }),
        metadata: serde_json::Value::Null,
        started: 0,
    };

    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    for (id, state) in [("prt_1", delegated), ("prt_2", spawn)] {
        reply.parts.push(Part {
            id: PartId::from(id.to_owned()),
            body: PartBody::Tool { call_id: id.to_owned(), tool: "task".to_owned(), state },
        });
    }
    chat.start_message(reply);

    assert_eq!(
        chat.running_tasks(),
        1,
        "the delegated child counts and the spawn beside it does not"
    );
}

#[test]
fn a_duration_is_reported_in_whatever_unit_reads_as_one() {
    let cases = [
        (0_u64, 1_u64, "1ms"),
        (0, 999, "999ms"),
        (0, 1_000, "1.0s"),
        (1_000, 13_400, "12.4s"),
        (0, 59_900, "59.9s"),
        (0, 60_000, "1m 0s"),
        (0, 3_723_000, "62m 3s"),
        // A clock that moved backwards between the two stamps.
        (5_000, 1_000, "0ms"),
    ];

    for (started, completed, expected) in cases {
        assert_eq!(elapsed(started, completed), expected, "{started}..{completed}");
    }
}

/// R12's scope, from the seam's side: a reply is markdown.
#[test]
fn an_assistant_reply_is_rendered_as_markdown() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part::text("# Heading\n\nand **loud** text"));
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 40, 10));

    assert!(
        lines.iter().any(|line| line == "\u{25cf} Heading"),
        "the heading's marker should be concealed, got {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "  and loud text"),
        "and so should the emphasis markers, under the bullet's own columns, got {lines:?}"
    );
}

/// The other half of the scope: what a person typed is never re-read as
/// markup, so their `#` and `**` stay on the screen — behind the caret
/// that says a person is who typed them.
#[test]
fn a_user_message_is_left_exactly_as_it_was_typed() {
    let mut chat = Chat::default();
    chat.start_message(Message::user("# Heading and **loud** text"));

    let lines = rendered(&mut chat, Rect::new(0, 0, 40, 10));

    assert!(lines.iter().any(|line| line == "> # Heading and **loud** text"), "got {lines:?}");
}

/// A prompt is one block however many parts it was built from: the caret
/// leads it once, and everything after hangs under it — so a prompt that
/// is nothing but an attachment is still marked as something a person
/// said.
#[test]
fn a_prompt_carries_one_caret_and_hangs_the_rest_of_itself_under_it() {
    let mut chat = Chat::default();
    let mut message = Message::user("look at this");
    message.parts.push(Part {
        id: PartId::from("prt_f1".to_owned()),
        body: PartBody::File {
            path: "src/lib.rs".to_owned(),
            mime: "text/plain".to_owned(),
            start: None,
            end: None,
            content: None,
        },
    });
    chat.start_message(message);

    let lines = rendered(&mut chat, Rect::new(0, 0, 40, 8));
    let drawn: Vec<&str> =
        lines.iter().map(String::as_str).filter(|line| !line.is_empty()).collect();

    assert_eq!(drawn, vec!["> look at this", "  @src/lib.rs"], "got {lines:?}");
}

/// The wrap cache holds styled lines, so it is as stale after a theme
/// switch as it is after a resize. Both frames here are the same width:
/// only the revision can invalidate the cache.
#[test]
fn a_theme_switch_restyles_the_lines_the_cache_already_holds() {
    let area = Rect::new(0, 0, 40, 6);
    let mut chat = Chat::default();
    chat.start_message(Message::user("what color am I"));

    let mut themes = Themes::builtin();
    let first = themes.select("aura").expect("aura is builtin");
    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer, &first);
    let before = buffer[(0, 0)].fg;

    let second = themes.select("gruvbox").expect("gruvbox is builtin");
    assert_ne!(
        first.revision(),
        second.revision(),
        "a switch has to change the revision, or nothing below is tested"
    );
    chat.render(area, &mut buffer, &second);

    assert_ne!(before, buffer[(0, 0)].fg, "the cached line kept the old palette");
}

/// A transcript of four entries, and the id of the third — which is what
/// an undo of the last exchange anchors on.
fn reverted_transcript() -> (Chat, MessageId) {
    /// A reply saying `text`, which is a part rather than the model name
    /// `Message::assistant` takes.
    fn reply(text: &str) -> Message {
        let mut message = Message::assistant("canned");
        message.parts.push(Part::text(text));

        message
    }

    let mut chat = Chat::default();
    chat.start_message(Message::user("the first question"));
    chat.start_message(reply("the first answer"));
    chat.start_message(Message::user("the second question"));
    chat.start_message(reply("the second answer"));

    let anchor = chat.entries[2].id.clone();

    (chat, anchor)
}

#[test]
fn a_revert_hides_the_anchor_and_everything_after_it() {
    let (mut chat, anchor) = reverted_transcript();

    chat.revert(anchor, vec!["src/lib.rs".to_owned()]);
    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));
    let screen = lines.join("\n");

    assert!(screen.contains("the first question"), "{screen}");
    assert!(screen.contains("the first answer"), "{screen}");
    assert!(!screen.contains("the second question"), "the anchor itself is hidden too:\n{screen}");
    assert!(!screen.contains("the second answer"), "{screen}");
}

/// The whole of the row's job: how much went away, and the way back.
#[test]
fn the_marker_row_counts_what_it_hides_and_names_the_files() {
    let (mut chat, anchor) = reverted_transcript();

    chat.revert(anchor, vec!["src/lib.rs".to_owned(), "src/app.rs".to_owned()]);
    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

    assert!(
        lines.iter().any(|line| line == "2 messages reverted \u{2014} /redo to restore"),
        "got {lines:?}"
    );
    for file in ["src/lib.rs", "src/app.rs"] {
        assert!(
            lines.iter().any(|line| line.contains(file)),
            "{file} should be named, got {lines:?}"
        );
    }
}

/// One hidden message is one message, not "1 messages".
#[test]
fn the_marker_row_counts_a_single_message_in_the_singular() {
    let mut chat = Chat::default();
    chat.start_message(Message::user("kept"));
    chat.start_message(Message::user("taken back"));
    let anchor = chat.entries[1].id.clone();

    chat.revert(anchor, Vec::new());
    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

    assert!(
        lines.iter().any(|line| line == "1 message reverted \u{2014} /redo to restore"),
        "got {lines:?}"
    );
}

/// A revert that put no file back is a revert of the conversation, and
/// still worth a row: the messages really are hidden.
#[test]
fn a_revert_that_moved_no_files_still_draws_its_row() {
    let (mut chat, anchor) = reverted_transcript();

    chat.revert(anchor, Vec::new());
    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 20));

    assert!(
        lines.iter().any(|line| line == "2 messages reverted \u{2014} /redo to restore"),
        "got {lines:?}"
    );
}

/// What a redo past the newest undone prompt gets: the entries were never
/// gone.
#[test]
fn unreverting_shows_the_hidden_entries_again_and_takes_the_row_away() {
    let (mut chat, anchor) = reverted_transcript();
    chat.revert(anchor, vec!["src/lib.rs".to_owned()]);
    rendered(&mut chat, Rect::new(0, 0, 60, 20));

    chat.unrevert();
    let screen = rendered(&mut chat, Rect::new(0, 0, 60, 20)).join("\n");

    assert!(!chat.is_reverted());
    assert!(screen.contains("the second question"), "{screen}");
    assert!(screen.contains("the second answer"), "{screen}");
    assert!(!screen.contains("reverted"), "{screen}");
}

/// What a prompt after an undo gets: the engine deleted those messages, so
/// there is nothing left for a later redo to bring back.
#[test]
fn dropping_a_revert_removes_the_hidden_entries_for_good() {
    let (mut chat, anchor) = reverted_transcript();
    chat.revert(anchor, vec!["src/lib.rs".to_owned()]);

    chat.drop_reverted();
    chat.unrevert();
    let screen = rendered(&mut chat, Rect::new(0, 0, 60, 20)).join("\n");

    assert_eq!(chat.entries.len(), 2, "the hidden tail should be gone");
    assert!(screen.contains("the first answer"), "{screen}");
    assert!(
        !screen.contains("the second question"),
        "an unrevert after a drop has nothing to put back:\n{screen}"
    );
}

/// A copy is of the conversation on screen, and the hidden tail is not on
/// it — nor in what the next request will carry.
#[test]
fn the_copy_surfaces_read_the_visible_transcript_and_not_the_hidden_tail() {
    let (mut chat, anchor) = reverted_transcript();

    assert_eq!(chat.messages().len(), 4);
    chat.revert(anchor, Vec::new());
    assert_eq!(chat.messages().len(), 2);
}

/// Scrolling has to agree with what was drawn, so the row it stands in for
/// counts as lines like everything else.
#[test]
fn the_marker_rows_lines_are_part_of_what_the_viewport_can_scroll() {
    let (mut chat, anchor) = reverted_transcript();
    rendered(&mut chat, Rect::new(0, 0, 60, 20));
    let whole = chat.line_count();

    chat.revert(anchor, vec!["src/lib.rs".to_owned()]);
    rendered(&mut chat, Rect::new(0, 0, 60, 20));

    // Two entries' lines are gone — two apiece, a caret or bullet row and
    // the blank every entry ends with — and the marker's three — a
    // headline, one file and a blank of its own — took their place.
    assert_eq!(chat.line_count(), whole - 4 + 3);
}

/// Starting a fresh conversation ends the revert with it: the session the
/// undo happened in is not the one on screen any more.
#[test]
fn clearing_the_transcript_ends_the_revert_too() {
    let (mut chat, anchor) = reverted_transcript();
    chat.revert(anchor, Vec::new());

    chat.clear();

    assert!(!chat.is_reverted());
    assert_eq!(chat.line_count(), 0);
}

/// A reply carrying one patch part naming `files`.
fn patched(files: &[&str]) -> Message {
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part {
        id: PartId::from("prt_patch".to_owned()),
        body: PartBody::Patch {
            hash: "4b825dc".to_owned(),
            files: files.iter().map(|file| (*file).to_owned()).collect(),
        },
    });

    reply
}

/// **F7.** One checkpoint per prompt, newest first, each counting the
/// distinct files its own span changed — a file two steps of one turn both
/// touched is one file, and a span with no patches counts none.
#[test]
fn checkpoints_are_the_prompts_newest_first_with_their_spans_file_counts() {
    let mut chat = Chat::default();
    chat.start_message(Message::user("change two files"));
    chat.start_message(patched(&["src/lib.rs", "src/app.rs"]));
    // A second step of the same turn, touching one of them again.
    chat.start_message(patched(&["src/lib.rs"]));
    chat.start_message(Message::user("now just explain it"));
    chat.start_message(Message::assistant("canned"));

    let checkpoints = chat.checkpoints();

    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0].title, "now just explain it");
    assert_eq!(checkpoints[0].files, 0, "that turn changed nothing");
    assert_eq!(checkpoints[1].title, "change two files");
    assert_eq!(checkpoints[1].files, 2, "two distinct files, however many patches named them");
    assert_eq!(checkpoints[1].message_id, chat.entries[0].id);
}

/// The picker offers what the screen offers: a reverted tail is not on
/// screen, so it is not something to rewind to either.
#[test]
fn checkpoints_leave_out_what_a_revert_is_already_hiding() {
    let (mut chat, anchor) = reverted_transcript();
    assert_eq!(chat.checkpoints().len(), 2);

    chat.revert(anchor, Vec::new());

    let checkpoints = chat.checkpoints();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].title, "the first question");
}

/// A prompt is one line on a checkpoint row however many it was typed
/// over, and a prompt with no text at all is still identifiable.
#[test]
fn a_checkpoint_is_titled_by_the_prompts_first_line() {
    let mut chat = Chat::default();
    chat.start_message(Message::user("\n\n  the first real line  \nand more"));
    let mut wordless = Message::user("");
    wordless.parts.clear();
    let id = wordless.id.clone();
    chat.start_message(wordless);

    let checkpoints = chat.checkpoints();

    assert_eq!(checkpoints[1].title, "the first real line");
    assert_eq!(checkpoints[0].title, id.as_str());
}

/// A turn that started `seconds` ago, having spent `output_tokens`.
fn working(turn: u64, seconds: u64, output_tokens: u64) -> Working {
    Working {
        started: Instant::now()
            .checked_sub(Duration::from_secs(seconds))
            .expect("the test clock is well past the epoch"),
        turn,
        output_tokens,
        compaction: None,
    }
}

/// A turn that has been compacting for `seconds`, its summary `tokens`
/// into a `budget`.
fn compacting(seconds: u64, tokens: u64, budget: u64) -> Working {
    Working {
        compaction: Some(Compaction { tokens, budget, done: false }),
        ..working(0, seconds, 0)
    }
}

/// The summary's whole arrival snaps the gauge full, and the settled
/// turn holds it on screen for the settle window instead of taking the
/// strip back in the same frame — then a layout past the window clears
/// it like any settled turn's.
#[test]
fn a_finished_compaction_snaps_full_and_lingers_before_settling() {
    let mut chat = Chat::default();
    chat.set_compacting(500, 4_096);
    assert!(chat.finish_compacting(), "there was a compaction to finish");

    let snapped = strip(&mut chat, 60);
    assert_eq!(
        snapped[2],
        format!("  {} 100%", "\u{25b0}".repeat(40)),
        "the gauge is full the moment the summary lands"
    );

    chat.settle_working();
    assert!(!strip(&mut chat, 60).is_empty(), "the full gauge is held past the turn's end");

    chat.settling = Instant::now().checked_sub(super::COMPACT_SETTLE);
    assert!(strip(&mut chat, 60).is_empty(), "and a layout past the hold takes the strip back");
}

/// Only an arrival is held: a turn that ends mid-stream — a cancel, a
/// dead provider — clears at once, and so does an ordinary turn's end.
#[test]
fn settling_holds_nothing_that_never_finished() {
    let mut chat = Chat::default();
    chat.set_compacting(500, 4_096);
    chat.settle_working();
    assert!(strip(&mut chat, 60).is_empty(), "an unfinished compaction has no arrival to show");

    chat.set_working(Some(working(1, 3, 0)));
    assert!(!chat.finish_compacting(), "no compaction, nothing to finish");
    chat.settle_working();
    assert!(strip(&mut chat, 60).is_empty(), "an ordinary turn settles the way it always did");
}

/// The strip in its compacting dress (the 2026-08-25 reference
/// screenshots): the spinner glyph on the headline, the clock in minutes
/// and the streamed estimate abbreviated beside it, and under a blank
/// row the forty-segment gauge with its percentage.
#[test]
fn a_compacting_turn_wears_its_own_dress_and_gauge() {
    let mut chat = Chat::default();
    chat.set_working(Some(compacting(121, 2_500, 4_096)));

    let lines = strip(&mut chat, 60);

    assert_eq!(
        lines[0],
        format!(
            "{} Compacting conversation\u{2026} (2m 1s \u{b7} \u{2193} 2.5k tokens)",
            working_frame(Duration::from_secs(121))
        ),
        "got {lines:?}"
    );
    assert_eq!(lines[1], "", "a blank row of air before the gauge");
    assert_eq!(
        lines[2],
        format!("  {}{} 61%", "\u{25b0}".repeat(24), "\u{25b1}".repeat(16)),
        "2500 of 4096 is 61%, and 61% of forty segments is 24"
    );
}

/// The gauge never claims the end while the stream is still coming, and
/// a compaction that has streamed nothing yet shows a bare clock rather
/// than a zero.
#[test]
fn the_compacting_gauge_opens_bare_and_clamps_at_ninety_nine() {
    let mut chat = Chat::default();
    chat.set_working(Some(compacting(3, 0, 4_096)));
    let opened = strip(&mut chat, 60);
    assert_eq!(
        opened[0],
        format!("{} Compacting conversation\u{2026} (3s)", working_frame(Duration::from_secs(3))),
        "no token clause before anything streamed"
    );
    assert_eq!(opened[2], format!("  {} 0%", "\u{25b1}".repeat(40)));

    chat.set_working(Some(compacting(3, 8_192, 4_096)));
    let overrun = strip(&mut chat, 60);
    assert_eq!(
        overrun[2],
        format!("  {}\u{25b1} 99%", "\u{25b0}".repeat(39)),
        "a stream past the budget is 99%, never a claimed end"
    );
}

/// The first progress event arms the strip on its own — the automatic
/// trigger fires before any message opens — and later ones update it in
/// place, keeping the compaction's own clock.
#[test]
fn compaction_progress_arms_the_strip_and_updates_it_in_place() {
    let mut chat = Chat::default();
    chat.set_compacting(0, 4_096);

    let armed = strip(&mut chat, 60);
    assert!(
        armed.first().is_some_and(|line| line.contains("Compacting conversation\u{2026}")),
        "the first event arms the strip: {armed:?}"
    );

    chat.set_compacting(2_048, 4_096);
    let updated = strip(&mut chat, 60);
    assert!(
        updated.iter().any(|line| line.ends_with(" 50%")),
        "a later event moves the gauge: {updated:?}"
    );

    chat.set_working(None);
    assert!(strip(&mut chat, 60).is_empty(), "the strip settles the way every turn's does");
}

/// The headline's paint rides the glyph's own clock — blue at the
/// cycle's ends, periwinkle at its far frame, between the two on the way
/// — so the icon and the color change together, as the reference's two
/// frames show.
#[test]
fn the_compacting_pulse_swaps_paints_on_the_spinner_clock() {
    let step = u64::try_from(WORKING_FRAME_STEP).expect("a step fits in u64");
    let at = |steps: u64| compact_pulse(Duration::from_millis(steps * step));
    let paint = |(r, g, b): (u8, u8, u8)| ratatui::style::Color::Rgb(r, g, b);

    assert_eq!(at(0), paint(COMPACT_BLUE));
    assert_eq!(at(5), paint(COMPACT_PERIWINKLE), "the far frame");
    assert_eq!(at(10), at(0), "the whole cycle in, it starts over");
    assert_ne!(at(2), at(0), "the way there passes between the two");
    assert_ne!(at(2), at(5));
}

/// The two figures spell themselves the reference's way: minutes past a
/// minute, `k` past a thousand, the tenth dropped when it is zero.
#[test]
fn the_compacting_figures_abbreviate_the_reference_way() {
    assert_eq!(compact_elapsed(Duration::from_secs(59)), "59s");
    assert_eq!(compact_elapsed(Duration::from_secs(121)), "2m 1s");
    assert_eq!(compact_tokens(840), "840");
    assert_eq!(compact_tokens(2_500), "2.5k");
    assert_eq!(compact_tokens(4_000), "4k");
}

/// **AC4.** While a turn runs the strip says so, with what it has spent;
/// when the turn settles the strip is gone — and the viewport's own count
/// never counted it, because the strip is not the transcript's to scroll.
#[test]
fn the_strip_says_a_turn_is_working_and_takes_it_back_when_the_turn_settles() {
    let mut chat = Chat::default();
    chat.start_message(Message::user("go on then"));
    let area = Rect::new(0, 0, 60, 10);
    rendered(&mut chat, area);
    let settled = chat.line_count();

    chat.set_working(Some(working(1, 12, 431)));
    let lines = strip(&mut chat, 60);

    assert!(
        lines.iter().any(|line| {
            *line
                == format!(
                    "{} Thinking\u{2026} (12s \u{b7} \u{2193} 431 tokens)",
                    working_frame(Duration::from_secs(12))
                )
        }),
        "got {lines:?}"
    );
    assert_eq!(chat.line_count(), settled, "the strip is not the transcript's to scroll");

    chat.set_working(None);

    assert!(strip(&mut chat, 60).is_empty(), "a settled turn leaves no strip");
    assert_eq!(chat.line_count(), settled);
}

/// A session that has spent nothing yet says nothing about tokens, rather
/// than claiming a zero the screen would contradict.
#[test]
fn a_working_line_with_nothing_spent_yet_draws_no_token_segment() {
    let mut chat = Chat::default();
    chat.set_working(Some(working(1, 3, 0)));

    let lines = strip(&mut chat, 60);

    assert!(
        lines.iter().any(|line| {
            *line == format!("{} Thinking\u{2026} (3s)", working_frame(Duration::from_secs(3)))
        }),
        "got {lines:?}"
    );
}

/// The glyph turns through Claude Code's spinner frames forward and back,
/// one per step, off the turn's own clock — the first frame at the start,
/// the far one at the fifth step, the first again after the tenth — so a
/// line drawn twice at one instant is the same line, and nothing keeps a
/// phase of its own.
#[test]
fn the_working_glyph_turns_through_the_frames_and_back_on_the_turns_clock() {
    let step = u64::try_from(WORKING_FRAME_STEP).expect("a step fits in u64");
    let at = |steps: u64| working_frame(Duration::from_millis(steps * step));
    assert_eq!(at(0), "\u{b7}");
    assert_eq!(at(5), "\u{273d}", "the far frame at the fifth step");
    assert_eq!(at(6), "\u{273b}", "and back the way it came");
    assert_eq!(at(10), at(0), "the whole cycle in, it starts over");
    // Within a step the frame holds: the same instant read twice.
    assert_eq!(working_frame(Duration::from_millis(step * 3 + step / 2)), at(3));
    let forward: Vec<&str> = WORKING_FRAMES[..6].to_vec();
    let mut back = WORKING_FRAMES[6..].to_vec();
    back.reverse();
    assert_eq!(
        forward[1..5].to_vec(),
        back,
        "the way back is the way forward reversed, minus its ends"
    );
}

/// The verb rotates with the turn and repeats around the list, so the same
/// transcript replayed twice reads the same both times.
#[test]
fn the_working_verb_rotates_with_the_turn_and_wraps_around() {
    let verb = |turn: u64| {
        let mut chat = Chat::default();
        chat.set_working(Some(working(turn, 0, 0)));
        strip(&mut chat, 40).into_iter().find(|line| !line.is_empty()).unwrap_or_default()
    };

    assert_eq!(verb(0), "\u{b7} Working\u{2026} (0s)");
    assert_eq!(verb(1), "\u{b7} Thinking\u{2026} (0s)");
    assert_ne!(verb(1), verb(2));
    let len = u64::try_from(WORKING_VERBS.len()).expect("verb count fits in u64");
    assert_eq!(verb(len), verb(0), "the whole list in, it starts over rather than running out");
}

/// **Pre-mortem 2.** The working block lives outside the scroll entirely
/// (2026-08-15): it can neither break the tail-follow nor move a viewport
/// somebody pinned, because the transcript's own lines are the same with
/// and without it.
#[test]
fn the_working_line_disturbs_neither_the_tail_nor_a_pinned_viewport() {
    let mut chat = Chat::default();
    transcript(&mut chat, 20);
    let tail = rendered(&mut chat, VIEWPORT);
    assert!(chat.is_following_tail());

    chat.set_working(Some(working(1, 5, 0)));
    assert_eq!(rendered(&mut chat, VIEWPORT), tail, "the strip is not the viewport's to show");
    assert!(chat.is_following_tail(), "the tail is still followed");

    chat.set_working(None);
    chat.scroll_lines(-9);
    let pinned = rendered(&mut chat, VIEWPORT);
    assert!(!chat.is_following_tail());

    chat.set_working(Some(working(1, 6, 0)));

    assert_eq!(
        rendered(&mut chat, VIEWPORT),
        pinned,
        "a strip appearing at the bottom must not move a reader who is not there"
    );
}

/// **AC3.** Thinking a person can read renders behind its own marker, dim
/// and italic so it never competes with the answer it is on the way to,
/// and hangs its continuation under the marker's own columns.
#[test]
fn readable_thinking_renders_dim_and_italic_behind_its_own_marker() {
    let theme = Theme::default();
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part::reasoning_text("A greeting is enough, so keep it short"));
    reply.parts.push(Part::text("Hello, world!"));
    chat.start_message(reply);

    let area = Rect::new(0, 0, 24, 10);
    let lines = rendered(&mut chat, area);
    let drawn: Vec<&str> =
        lines.iter().map(String::as_str).filter(|line| !line.is_empty()).collect();

    assert_eq!(
        drawn,
        vec!["\u{2234} A greeting is enough,", "  so keep it short", "\u{25cf} Hello, world!",],
        "got {lines:?}"
    );

    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer, &theme);
    let marker = buffer[(0u16, 0u16)].style();
    assert_eq!(marker.fg, theme.dim.fg, "thinking recedes");
    assert!(
        marker.add_modifier.contains(Modifier::ITALIC),
        "and is set apart from the reply by more than its color"
    );
}

/// A think renders whole, first line to last, with its paragraph breaks
/// kept — the user's screenshot ruling (2026-08-14) that retired the tail
/// clamp: a long thought scrolls back the way a long reply does, and no
/// hint row stands where its opening lines used to be cut away.
#[test]
fn a_long_think_renders_whole_with_its_paragraphs() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part::reasoning_text("one\ntwo\nthree\nfour\n\nfive\nsix\nseven"));
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 14));
    let think_and_gap = &lines[..8];

    assert_eq!(
        think_and_gap,
        ["\u{2234} one", "  two", "  three", "  four", "", "  five", "  six", "  seven",],
        "got {lines:?}"
    );
}

/// A part the provider opened and has not filled is not a thought yet, so
/// it draws no marker standing on its own.
#[test]
fn an_empty_thinking_part_draws_nothing_at_all() {
    let mut chat = Chat::default();
    let reply = Message::assistant("canned");
    let part = Part::reasoning_text(String::new());
    chat.start_message(reply.clone());
    chat.start_part(&reply.id, part.clone());

    assert!(
        rendered(&mut chat, Rect::new(0, 0, 40, 6)).iter().all(String::is_empty),
        "an unfilled part is a marker about nothing"
    );

    // And the same part grows on a delta, which is how it arrives at all:
    // the event names an id and a fragment, never which kind of text.
    chat.append_delta(&reply.id, &part.id, "now there is a thought");
    let screen = rendered(&mut chat, Rect::new(0, 0, 40, 6)).join("\n");
    assert!(screen.contains("\u{2234} now there is a thought"), "got:\n{screen}");
}

/// **AC3, the half this build can answer.** Sealed reasoning is a blob only
/// the provider can open; there is nothing in it for a `∴` line to say, so
/// the part draws nothing at all — and the reply around it is untouched.
#[test]
fn sealed_reasoning_draws_nothing_and_leaves_the_reply_alone() {
    let mut chat = Chat::default();
    let mut reply = Message::assistant("canned");
    reply.parts.push(Part::reasoning("anthropic", "rs_1", Some("OPAQUE".to_owned())));
    reply.parts.push(Part::text("the answer itself"));
    chat.start_message(reply);

    let lines = rendered(&mut chat, Rect::new(0, 0, 60, 10));
    let drawn: Vec<&str> =
        lines.iter().map(String::as_str).filter(|line| !line.is_empty()).collect();

    assert_eq!(
        drawn,
        vec!["\u{25cf} the answer itself"],
        "a blob is not a thought anybody can read, got {lines:?}"
    );
}

/// **D467.** The highlighted message's rows carry the selection style,
/// its breathing-room blank stays unpainted, and every other row keeps
/// its own colors.
#[test]
fn the_backtrack_highlight_paints_the_anchors_rows_only() {
    let theme = Theme::default();
    let mut chat = Chat::default();
    let first = Message::user("first prompt");
    let anchor = first.id.clone();
    chat.start_message(first);
    chat.start_message(Message::user("second prompt"));
    chat.set_backtrack(Some(anchor));

    let area = Rect::new(0, 0, 20, 10);
    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer, &theme);

    // Rows 0-1 are the first entry (its one caret row, then blank), 2-3
    // the second.
    let style = |row: u16| buffer[(0u16, row)].style();
    assert_eq!(style(0).fg, theme.selection.fg, "the prompt row is painted");
    assert_ne!(style(1), style(0), "the breathing-room blank is not");
    assert_ne!(style(2).fg, theme.selection.fg, "the next message is not");
}

/// Stepping the highlight to a message above the viewport scrolls it into
/// view — once, on the frame after the step, so a later scroll stays
/// where the reader put it.
#[test]
fn the_backtrack_highlight_scrolls_into_view_once() {
    let mut chat = Chat::default();
    let first = Message::user("the oldest prompt");
    let anchor = first.id.clone();
    chat.start_message(first);
    transcript(&mut chat, 12);

    let screen = rendered(&mut chat, VIEWPORT).join("\n");
    assert!(
        !screen.contains("the oldest prompt"),
        "the fixture starts with the anchor scrolled away:\n{screen}"
    );

    chat.set_backtrack(Some(anchor));
    let screen = rendered(&mut chat, VIEWPORT).join("\n");
    assert!(screen.contains("the oldest prompt"), "the highlight is brought into view:\n{screen}");

    chat.scroll_lines(isize::try_from(chat.line_count()).unwrap_or(isize::MAX));
    let screen = rendered(&mut chat, VIEWPORT).join("\n");
    assert!(!screen.contains("the oldest prompt"), "a later scroll is not snapped back:\n{screen}");
}
