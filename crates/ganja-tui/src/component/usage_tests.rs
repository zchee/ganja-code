use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ganja_protocol::Message;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{Data, PlanWindow, RateWindow, TokenUsage, TurnUsage, Usage, compact_minutes};
use crate::component::status::Totals;
use crate::theme::Theme;

/// Wide enough for the pinned per-model line to render whole, and two
/// taller than [`super::MAX_HEIGHT`] so the modal is never the terminal's
/// prisoner: what these tests assert is what the panel *chose* to draw,
/// not what a short terminal cut off. Grown with that constant in P17.
const AREA: Rect = Rect { x: 0, y: 0, width: 100, height: 32 };

fn data() -> Data {
    Data {
        totals: Totals { input_tokens: 16, output_tokens: 4, cost_usd: Some(0.5) },
        splits: TokenUsage {
            input_tokens: 3,
            output_tokens: 4,
            reasoning_tokens: 5,
            cache_read_tokens: 6,
            cache_write_tokens: 7,
        },
        context: Some((50_000, 200_000)),
        turns: vec![TurnUsage {
            message_id: Message::assistant("claude-sonnet-5").id,
            model: "claude-sonnet-5".to_owned(),
            usage: TokenUsage {
                input_tokens: 3,
                output_tokens: 4,
                reasoning_tokens: 5,
                cache_read_tokens: 6,
                cache_write_tokens: 7,
            },
        }],
        duration: None,
        // The headerless case every existing assertion in this module is
        // written against, kept explicit so the regression pin is visible:
        // a fake-provider session renders exactly what it always did
        // (**D484**, AC7's last clause).
        rates: Vec::new(),
        // The same pin for the plan meters (**D485**): a credential that
        // served none renders the honest tail and no section.
        plans: Vec::new(),
        now: None,
    }
}

/// One plan window, `used_percent` spent, over a `minutes`-long window,
/// refilling `in_secs` from [`NOW`] — negative for one already past its
/// reset, and [`None`] for a vendor that dated it not at all.
fn plan(name: &str, used_percent: f64, minutes: Option<u64>, in_secs: Option<i64>) -> PlanWindow {
    PlanWindow {
        name: name.to_owned(),
        used_percent,
        window_minutes: minutes,
        resets_at: in_secs.map(|seconds| {
            let offset = Duration::from_secs(seconds.unsigned_abs());
            if seconds < 0 { NOW - offset } else { NOW + offset }
        }),
        limit_name: None,
    }
}

/// One rate-limit window, `remaining` of `limit` left, refilling `in_secs`
/// from [`NOW`] — negative for one whose reset has already gone by, and
/// [`None`] for a vendor that dated it not at all (grok's shape, D484 as
/// P22 amended it).
fn window(kind: &str, limit: u64, remaining: u64, in_secs: Option<i64>) -> RateWindow {
    RateWindow {
        kind: kind.to_owned(),
        limit,
        remaining,
        reset: in_secs.map(|seconds| {
            let offset = Duration::from_secs(seconds.unsigned_abs());
            if seconds < 0 { NOW - offset } else { NOW + offset }
        }),
    }
}

/// A fixed "now" the panel judges expiry against, so an expired bucket is
/// manufactured rather than waited for.
const NOW: SystemTime = UNIX_EPOCH;

/// AC5's "same formatter" clause, pinned as a regression test now that
/// the sharing is literal: the panel's turn row spells its id and cost
/// cells through the inspector's own functions, so the two tables cannot
/// drift apart without this test noticing.
#[test]
fn a_turn_row_is_spelled_by_the_inspectors_own_formatter() {
    use crate::component::inspector::{short_id, turn_cost};

    let data = data();
    let row = &data.turns[0];
    let screen = rendered(&Usage::new(data.clone()), AREA);

    assert!(
        screen.contains(&short_id(&row.message_id)),
        "the turn row carries the inspector's own id spelling; got:\n{screen}"
    );
    assert!(
        screen.contains(&turn_cost(row)),
        "the turn row carries the inspector's own cost cell ({}); got:\n{screen}",
        turn_cost(row)
    );
}

fn rendered(dialog: &Usage, area: Rect) -> String {
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

/// AC5's core: the `Total cost:` value is the status bar's own
/// accumulated cost, and the per-model line carries the turn rows' own
/// numbers — nothing in the Session section is summed a second way.
#[test]
fn the_session_section_renders_the_totals_own_cost_and_the_turns_own_numbers() {
    let screen = rendered(&Usage::new(data()), AREA);

    assert!(screen.contains("Total cost:"), "got:\n{screen}");
    assert!(screen.contains("$0.50"), "the totals' 0.5 USD:\n{screen}");
    assert!(
        screen.contains(
            "claude-sonnet-5:  3 input, 4 output, 5 reasoning, 6 cache read, 7 cache write"
        ),
        "the turn's own split:\n{screen}"
    );
}

/// Two turns on the same model fold into one `Usage by model:` line
/// whose numbers are the turns' sums.
#[test]
fn turns_on_the_same_model_fold_into_one_summed_line() {
    let mut data = data();
    data.turns.push(data.turns[0].clone());

    let screen = rendered(&Usage::new(data), AREA);
    assert!(
        screen.contains(
            "claude-sonnet-5:  6 input, 8 output, 10 reasoning, 12 cache read, 14 cache write"
        ),
        "got:\n{screen}"
    );
}

#[test]
fn every_section_and_the_turn_table_are_shown() {
    let screen = rendered(&Usage::new(data()), AREA);

    for section in ["Session", "Context", "Cache", "Turns"] {
        assert!(screen.contains(section), "{section} missing:\n{screen}");
    }
    assert!(screen.contains("Usage by model:"), "got:\n{screen}");
    assert!(screen.contains("estimated"), "the context sub-line says estimated:\n{screen}");
}

/// The pinned block bar replaced the ASCII meter: filled cells, dim
/// remainder, the percentage after — 50k of 200k is a quarter.
#[test]
fn the_context_bar_is_block_cells_followed_by_the_percentage() {
    let screen = rendered(&Usage::new(data()), AREA);

    assert!(
        screen.contains("\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2591}"),
        "five filled cells then the remainder:\n{screen}"
    );
    assert!(screen.contains("25% used"), "got:\n{screen}");
    assert!(
        !screen.contains("[#") && !screen.contains("#-"),
        "the ASCII meter is gone from this panel:\n{screen}"
    );
}

/// Cache reads over everything that went in: 6 of 3+6+7 = 37.5%.
#[test]
fn the_cache_hit_rate_is_reads_over_all_input() {
    let rate = Usage::new(data()).cache_hit_rate().expect("the fixture sent input");

    assert!((rate - 0.375).abs() < f64::EPSILON, "got {rate}");
}

/// The pinned `Total duration` row renders the opener's own wall clock
/// in the compact spelling, and its absence renders no row at all — an
/// invented duration would be worse than none.
#[test]
fn the_duration_row_formats_compactly_and_only_over_a_measured_duration() {
    use std::time::Duration;

    for (duration, spelled) in [
        (Duration::from_secs(3 * 3_600 + 9 * 60), "3h 9m"),
        (Duration::from_secs(2 * 60), "2m"),
        (Duration::from_secs(45), "45s"),
    ] {
        let screen = rendered(&Usage::new(Data { duration: Some(duration), ..data() }), AREA);
        assert!(
            screen.contains(&format!("Total duration:  {spelled}")),
            "want {spelled} in:\n{screen}"
        );
    }

    let unmeasured = rendered(&Usage::new(data()), AREA);
    assert!(!unmeasured.contains("Total duration"), "no measured duration, no row:\n{unmeasured}");
}

/// The plan-limit meters Claude Code leads with are explicitly absent on a
/// credential that served none, with the honest lines naming which
/// credentials are silent and why — never drawn empty, never faked.
#[test]
fn plan_limits_are_absent_with_the_honest_line() {
    let screen = rendered(&Usage::new(data()), AREA);

    assert!(
        screen.contains("plan limits unavailable on this credential (probed 2026-08-14):"),
        "got:\n{screen}"
    );
    // The P17 narrowing (**D485**): the line no longer says the meters are
    // unbuildable — it names who is still quiet and cites where that was
    // established, the vendor's own documentation included.
    assert!(
        screen.contains("an openai API key's backend sends none")
            && screen.contains("anthropic's needs an"),
        "the still-silent credentials are named:\n{screen}"
    );
    assert!(
        screen.contains("platform.claude.com/docs/en/manage-claude/usage-cost-api"),
        "and the vendor's own page is kept:\n{screen}"
    );
    assert!(
        !screen.contains("Plan limits"),
        "no plan-limit meter may be drawn over nothing:\n{screen}"
    );
    // The P16 narrowing (**D484**): with no windows heard, that section is
    // absent too — the honest-absence line is not standing in for it.
    assert!(
        !screen.contains("Current window"),
        "a vendor that said nothing gets no section:\n{screen}"
    );
}

/// The `Plan limits` section renders what the credential really served,
/// in the panel's own block-bar shape (**D485**).
#[test]
fn the_plan_limits_section_meters_each_bucket_the_credential_served() {
    let screen = rendered(
        &Usage::new(Data {
            plans: vec![
                plan("primary", 12.0, Some(300), Some(90)),
                plan("secondary", 40.0, Some(10_080), Some(86_400)),
            ],
            now: Some(NOW),
            ..data()
        }),
        AREA,
    );

    assert!(screen.contains("Plan limits"), "got:\n{screen}");
    assert!(
        screen.contains("secondary \u{b7} 7d window, resets in 24h 0m"),
        "the weekly bucket names its window and its countdown:\n{screen}"
    );
    assert!(
        screen.contains("primary \u{b7} 5h window, resets in 1m"),
        "and so does the short one:\n{screen}"
    );
    assert!(
        screen.contains("40%") && screen.contains("12%"),
        "each bar carries its own percentage:\n{screen}"
    );
}

/// The panel never says both things: a session that drew a real meter does
/// not also print the line saying it could not (**D485**).
#[test]
fn a_drawn_plan_meter_replaces_the_absence_line_rather_than_joining_it() {
    let screen = rendered(
        &Usage::new(Data {
            plans: vec![plan("primary", 12.0, Some(300), Some(90))],
            now: Some(NOW),
            ..data()
        }),
        AREA,
    );

    assert!(screen.contains("Plan limits"), "got:\n{screen}");
    assert!(
        !screen.contains("plan limits unavailable"),
        "a panel that drew one may not deny having one:\n{screen}"
    );
    assert!(screen.contains("[Esc] close"), "and the footer still fits:\n{screen}");
}

/// A copilot snapshot arrives with no window length and may arrive with no
/// reset at all. Both absences are said plainly rather than filled in.
#[test]
fn a_plan_window_the_vendor_never_dated_says_so_instead_of_counting_down() {
    let screen = rendered(
        &Usage::new(Data {
            plans: vec![plan("premium_interactions", 11.5, None, None)],
            now: Some(NOW),
            ..data()
        }),
        AREA,
    );

    assert!(screen.contains("premium_interactions \u{b7} no reset reported"), "got:\n{screen}");
    assert!(
        !screen.contains("resets in") && !screen.contains("window,"),
        "nothing may invent a clock or a window length:\n{screen}"
    );
}

/// The staleness guard on the sibling shape: a dated plan window past its
/// reset says so, exactly as a rate bucket does.
#[test]
fn a_plan_window_past_its_reset_renders_as_expired() {
    let screen = rendered(
        &Usage::new(Data {
            plans: vec![plan("primary", 99.0, Some(300), Some(-1))],
            now: Some(NOW),
            ..data()
        }),
        AREA,
    );

    assert!(screen.contains("expired \u{2014} refreshes on the next request"), "got:\n{screen}");
}

/// More plan windows than the section draws are counted, never dropped in
/// silence — and the ones kept are the tightest.
#[test]
fn plan_windows_past_the_sections_cap_are_counted_and_the_tightest_are_kept() {
    let screen = rendered(
        &Usage::new(Data {
            plans: vec![
                plan("primary", 5.0, Some(300), Some(60)),
                plan("secondary", 80.0, Some(10_080), Some(60)),
                plan("bengalfox primary", 50.0, None, Some(60)),
            ],
            now: Some(NOW),
            ..data()
        }),
        AREA,
    );

    assert!(
        screen.contains("secondary \u{b7}") && screen.contains("bengalfox primary \u{b7}"),
        "the two tightest are the ones drawn:\n{screen}"
    );
    assert!(
        screen.contains("(1 roomier plan windows not shown)"),
        "the rest is counted rather than dropped:\n{screen}"
    );
}

/// A window's own length reads in the unit a person thinks about it in.
#[test]
fn a_rolling_windows_length_reads_as_hours_or_days_rather_than_minutes() {
    for (minutes, spelled) in
        [(45, "45m"), (300, "5h"), (90, "1h 30m"), (10_080, "7d"), (1_500, "1d 1h")]
    {
        assert_eq!(compact_minutes(minutes), spelled, "for {minutes} minutes");
    }
}

/// The `Current window` section renders what the vendor really said, in
/// the panel's own block-bar shape (**D484**).
#[test]
fn the_current_window_section_meters_each_bucket_the_vendor_reported() {
    let screen = rendered(
        &Usage::new(Data {
            rates: vec![
                window("requests", 1_000, 900, Some(90)),
                window("input-tokens", 80_000, 20_000, Some(30)),
            ],
            now: Some(NOW),
            ..data()
        }),
        AREA,
    );

    assert!(screen.contains("Current window"), "got:\n{screen}");
    assert!(
        screen.contains("requests \u{b7} 900 of 1.0k left, resets in 1m"),
        "the requests bucket renders its own name, counts and reset:\n{screen}"
    );
    assert!(
        screen.contains("input-tokens \u{b7} 20.0k of 80.0k left, resets in 30s"),
        "and so does the token bucket:\n{screen}"
    );
    // The honest tail still stands beside it: this fixture served no plan
    // window, so what is missing is the plan meters and only those.
    assert!(screen.contains("plan limits unavailable on this credential"), "got:\n{screen}");
}

/// P16 pre-mortem 4, on the panel: a window past its reset says so instead
/// of presenting a number as current.
#[test]
fn a_window_past_its_reset_renders_as_expired_rather_than_as_a_live_number() {
    let screen = rendered(
        &Usage::new(Data {
            rates: vec![window("requests", 1_000, 4, Some(-1))],
            now: Some(NOW),
            ..data()
        }),
        AREA,
    );

    assert!(screen.contains("expired \u{2014} refreshes on the next request"), "got:\n{screen}");
    assert!(
        !screen.contains("resets in"),
        "nothing may claim the window is still counting down:\n{screen}"
    );
    // The counts stay: they were true when the vendor said them, and the
    // panel's job is to date them, not to erase them.
    assert!(screen.contains("4 of 1.0k left"), "got:\n{screen}");
}

/// AC3, the other half of that guard (`53v`): a bucket its vendor never
/// dated is drawn with an empty reset slot — never as expired, and never
/// with an instant nobody sent. Grok's shape, which before P22 the parser
/// dropped whole.
#[test]
fn a_window_its_vendor_never_dated_says_so_instead_of_borrowing_a_clock() {
    let screen = rendered(
        &Usage::new(Data {
            rates: vec![window("requests", 1_000, 900, None)],
            now: Some(NOW),
            ..data()
        }),
        AREA,
    );

    assert!(
        screen.contains("requests \u{b7} 900 of 1.0k left, resets: \u{2014}"),
        "the counts are the vendor's and the clock is nobody's:\n{screen}"
    );
    assert!(
        !screen.contains("expired"),
        "the staleness marker may not fire for a window nothing dated:\n{screen}"
    );
    assert!(
        !screen.contains("resets in"),
        "and no countdown may be invented for it either:\n{screen}"
    );
}

/// More windows than the section draws are counted, never dropped in
/// silence — and the ones kept are the tightest (**D484**).
#[test]
fn windows_past_the_sections_cap_are_counted_and_the_tightest_are_kept() {
    let screen = rendered(
        &Usage::new(Data {
            rates: vec![
                window("requests", 100, 99, Some(60)),
                window("input-tokens", 100, 5, Some(60)),
                window("output-tokens", 100, 50, Some(60)),
                window("tokens", 100, 90, Some(60)),
            ],
            now: Some(NOW),
            ..data()
        }),
        AREA,
    );

    assert!(
        screen.contains("input-tokens \u{b7} 5 of 100 left")
            && screen.contains("output-tokens \u{b7} 50 of 100 left"),
        "the two tightest windows are the ones drawn:\n{screen}"
    );
    assert!(
        screen.contains("(2 roomier windows not shown)"),
        "the rest are counted rather than dropped:\n{screen}"
    );
    assert!(
        screen.contains("plan limits unavailable on this credential"),
        "and the footer still fits:\n{screen}"
    );
}

#[test]
fn an_empty_session_names_its_own_empty_states() {
    let screen = rendered(&Usage::new(Data::default()), AREA);

    assert!(screen.contains("nothing sent yet"), "got:\n{screen}");
    assert!(screen.contains("no finished turns yet"), "got:\n{screen}");
    assert!(screen.contains("none yet"), "got:\n{screen}");
    assert!(
        screen.contains("unpriced \u{2014} uncataloged model"),
        "an unpriced session says so on the cost row:\n{screen}"
    );
}

#[test]
fn an_unsized_model_meters_nothing_rather_than_inventing_a_window() {
    let screen = rendered(&Usage::new(Data { context: None, ..data() }), AREA);

    assert!(screen.contains("unsized model \u{2014} no window to meter"), "got:\n{screen}");
}

#[test]
fn a_tiny_area_draws_without_panicking() {
    for (width, height) in [(1, 1), (4, 3), (20, 5)] {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);

        Usage::new(data()).render(area, &mut buffer, &Theme::default());
    }
}
