use ganja_core::engine::ContextBreakdown;
use ratatui::{buffer::Buffer, layout::Rect};

use super::{CELLS, Context};
use crate::theme::Theme;

/// Wide enough to seat the legend beside the grid, the pinned layout.
const WIDE: Rect = Rect {
    x: 0,
    y: 0,
    width: 120,
    height: 40,
};

/// An 80-column terminal: the house width cannot seat the column beside
/// the grid, so the panel stacks it below.
const NARROW: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 36,
};

/// A breakdown with something in every category over a small round
/// window, so shares are easy to reason about by hand.
fn sized() -> ContextBreakdown {
    ContextBreakdown {
        model: "claude-sonnet-5".to_owned(),
        system_prompt: 3_000,
        instructions: 2_000,
        tools_builtin: 11_000,
        tools_mcp: 1_000,
        tools_builtin_count: 12,
        tools_mcp_count: 3,
        skills: 500,
        conversation_user: 4_000,
        conversation_assistant: 8_500,
        window: Some(100_000),
        reserve: Some(10_000),
    }
}

fn unsized_model() -> ContextBreakdown {
    ContextBreakdown {
        window: None,
        reserve: None,
        ..sized()
    }
}

fn rendered(dialog: &Context, area: Rect) -> String {
    let mut buffer = Buffer::empty(area);
    dialog.render(area, &mut buffer, &Theme::default());

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

/// AC4's TUI half: the legend's used rows sum to exactly the accessor's
/// total, and the free-space row is exactly the accessor's window −
/// used − reserve — no figure in the panel is derived a second way.
#[test]
fn the_legend_total_is_the_accessors_total_and_free_space_its_free() {
    let breakdown = sized();
    let dialog = Context::new(None, breakdown.clone());

    let used: u64 = dialog.used().iter().map(|row| row.tokens).sum();
    assert_eq!(used, breakdown.total());

    let legend = dialog.legend();
    let free = legend
        .iter()
        .find(|row| row.label == "Free space")
        .expect("a sized window earns a free-space row");
    assert_eq!(Some(free.tokens), breakdown.free());
    let reserve = legend
        .iter()
        .find(|row| row.label == "Autocompact reserve")
        .expect("a sized window earns a reserve row");
    assert_eq!(Some(reserve.tokens), breakdown.reserve);
}

/// The grid never draws a hole and never draws a cell too many: whatever
/// the shares, the cells sum to exactly two hundred.
#[test]
fn the_grid_always_paints_exactly_two_hundred_cells() {
    for breakdown in [
        sized(),
        ContextBreakdown {
            system_prompt: 1,
            window: Some(1_000_000),
            reserve: Some(100_000),
            ..ContextBreakdown::default()
        },
    ] {
        let dialog = Context::new(None, breakdown.clone());
        let cells: usize = dialog
            .cells(breakdown.window.expect("both fixtures are sized"))
            .iter()
            .sum();
        assert_eq!(cells, CELLS, "{breakdown:?}");
    }
}

/// The panel carries the word Claude Code carries: these are estimates.
/// The sized panel says it in the pinned legend header, the degraded one
/// in its own header line (P14 pre-mortem 2).
#[test]
fn both_renderings_say_estimated() {
    let sized = rendered(&Context::new(None, sized()), WIDE);
    assert!(
        sized.contains("Estimated usage by category"),
        "got:\n{sized}"
    );

    let degraded = rendered(&Context::new(None, unsized_model()), NARROW);
    assert!(degraded.contains("estimated"), "got:\n{degraded}");
}

#[test]
fn a_sized_window_renders_the_title_the_grid_and_every_legend_row() {
    let screen = rendered(&Context::new(None, sized()), WIDE);

    assert!(screen.contains("Context Usage"), "the title:\n{screen}");
    for label in [
        "System prompt",
        "System tools",
        "MCP tools",
        "Memory files",
        "Skills",
        "Messages",
        "Free space",
        "Autocompact reserve",
    ] {
        assert!(screen.contains(label), "{label} missing:\n{screen}");
    }
    assert!(
        screen.contains('\u{26c1}'),
        "the grid draws used cells:\n{screen}"
    );
    assert!(screen.contains('\u{26f6}'), "and free ones:\n{screen}");
    assert!(
        screen.contains("30.0k/100.0k tokens (30%)"),
        "the window line names used over window:\n{screen}"
    );
}

/// The screenshot's free row alone drops the word "tokens"; every other
/// legend row keeps it.
#[test]
fn the_free_row_drops_the_word_tokens_and_the_reserve_row_keeps_it() {
    let screen = rendered(&Context::new(None, sized()), WIDE);

    assert!(
        screen.contains("Free space: 60.0k (60.0%)"),
        "got:\n{screen}"
    );
    assert!(
        !screen.contains("Free space: 60.0k tokens"),
        "the free row carries no unit:\n{screen}"
    );
    assert!(
        screen.contains("Autocompact reserve: 10.0k tokens (10.0%)"),
        "got:\n{screen}"
    );
}

/// The pinned layout seats the legend beside the grid where the panel is
/// wide enough — a free grid row and a legend label share a line — and
/// stacks it below where the house width cannot hold both.
#[test]
fn the_legend_sits_beside_the_grid_only_when_it_fits() {
    let beside = rendered(&Context::new(None, sized()), WIDE);
    assert!(
        beside
            .lines()
            .any(|line| line.contains("\u{26f6} \u{26f6}") && line.contains("System prompt")),
        "wide panels seat the legend beside the grid:\n{beside}"
    );

    let stacked = rendered(&Context::new(None, sized()), NARROW);
    assert!(
        !stacked
            .lines()
            .any(|line| line.contains("\u{26f6} \u{26f6}") && line.contains("System prompt")),
        "narrow panels stack it below:\n{stacked}"
    );
    assert!(
        stacked.contains("System prompt: 3.0k tokens (3.0%)"),
        "stacked legend rows stay whole:\n{stacked}"
    );
}

/// The pinned model pair: a catalog display name renders bold over the
/// dim model id, and a model the catalog cannot name renders its id
/// once — no fake display name is invented.
#[test]
fn a_display_name_renders_over_the_id_and_its_absence_renders_the_id_once() {
    let paired = rendered(
        &Context::new(Some("Claude Sonnet 5".to_owned()), sized()),
        WIDE,
    );
    assert!(paired.contains("Claude Sonnet 5"), "got:\n{paired}");
    assert!(paired.contains("claude-sonnet-5"), "got:\n{paired}");

    let bare = rendered(&Context::new(None, sized()), WIDE);
    assert_eq!(
        bare.matches("claude-sonnet-5").count(),
        1,
        "the id renders exactly once:\n{bare}"
    );
    let collapsed = rendered(
        &Context::new(Some("claude-sonnet-5".to_owned()), sized()),
        WIDE,
    );
    assert_eq!(
        collapsed.matches("claude-sonnet-5").count(),
        1,
        "a display name that is the id would stutter as a pair:\n{collapsed}"
    );
}

/// The detail sections render exactly the categories whose item counts
/// the breakdown carries — the two tool categories — and a zero count
/// earns no section at all.
#[test]
fn detail_sections_render_exactly_the_categories_whose_counts_exist() {
    let screen = rendered(&Context::new(None, sized()), WIDE);
    assert!(
        screen.contains("\u{2514} 12 tools \u{b7} 11.0k tokens"),
        "the builtin section:\n{screen}"
    );
    assert!(
        screen.contains("\u{2514} 3 tools \u{b7} 1.0k tokens"),
        "the MCP section:\n{screen}"
    );
    assert!(
        screen.contains("MCP tools \u{b7} /mcp"),
        "the hint names ganja's own door:\n{screen}"
    );

    let uncounted = rendered(
        &Context::new(
            None,
            ContextBreakdown {
                tools_builtin_count: 0,
                tools_mcp_count: 0,
                ..sized()
            },
        ),
        WIDE,
    );
    // Corner-plus-space: the bare corner is also the dialog border's
    // bottom-left glyph, which every rendering carries.
    assert!(
        !uncounted.contains("\u{2514} "),
        "no count, no section:\n{uncounted}"
    );
}

/// A panel too short for everything drops the detail sections first and
/// the pair's id line second — the close hint always survives, and the
/// grid and legend a person opened the panel for outrank the metadata.
#[test]
fn the_details_and_the_id_line_yield_before_the_close_hint_does() {
    let short = Rect::new(0, 0, 80, 30);
    let screen = rendered(
        &Context::new(Some("Claude Sonnet 5".to_owned()), sized()),
        short,
    );

    assert!(screen.contains("[Esc] close"), "got:\n{screen}");
    // Corner-plus-space, not the bare corner the border also draws.
    assert!(!screen.contains("\u{2514} "), "details yielded:\n{screen}");
    assert!(
        screen.contains("Claude Sonnet 5") && !screen.contains("claude-sonnet-5"),
        "the pair collapsed to the display name:\n{screen}"
    );
    assert!(
        screen.contains("Free space"),
        "the legend survived whole:\n{screen}"
    );
}

/// The degraded panel: totals, the honest sentence, and no invented
/// percentages anywhere.
#[test]
fn an_unsized_model_renders_totals_and_the_honest_sentence() {
    let screen = rendered(&Context::new(None, unsized_model()), NARROW);

    assert!(
        screen.contains("unsized model \u{2014} percentages unavailable"),
        "got:\n{screen}"
    );
    assert!(screen.contains("total"), "got:\n{screen}");
    assert!(
        !screen.contains('%'),
        "no denominator, no percentages:\n{screen}"
    );
    assert!(
        !screen.contains("Free space"),
        "free space needs a window:\n{screen}"
    );
}

#[test]
fn a_tiny_area_draws_without_panicking() {
    for (width, height) in [(1, 1), (4, 3), (20, 5)] {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);

        Context::new(None, sized()).render(area, &mut buffer, &Theme::default());
    }
}
