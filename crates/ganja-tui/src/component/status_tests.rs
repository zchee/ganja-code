use std::cell::RefCell;
use std::fs;

use ganja_core::config::StatuslineElement;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr as _;

use super::{
    Activity, Duration, PlanWindow, RateWindow, SHELL_HINTS, Severity, Status, SystemTime, Todos,
    Totals, discover_git, head_name, meter_fill, meter_severity, truncate_spans,
};
use crate::theme::Theme;

fn rendered(status: &Status, width: u16) -> String {
    let area = Rect::new(0, 0, width, 1);
    let mut buffer = Buffer::empty(area);
    status.render(area, &mut buffer, &Theme::default());

    (0..width).map(|column| buffer[(column, 0)].symbol()).collect::<String>().trim_end().to_owned()
}

/// Every row of a taller render, for the rosters that earn extra lines.
fn rendered_rows(status: &Status, width: u16, height: u16) -> Vec<String> {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    status.render(area, &mut buffer, &Theme::default());

    (0..height)
        .map(|row| {
            (0..width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// A status bar rendering `elements`, without going through a config
/// file.
fn roster(elements: &[StatuslineElement]) -> Status {
    let mut status = Status::new(None);
    status.elements = Some(elements.to_vec());
    status
}

/// One rate-limit window, `remaining` of `limit` left, refilling `in_secs`
/// from now — negative for one whose reset has already gone by, and
/// [`None`] for a vendor that dated it not at all (grok's shape, D484 as
/// P22 amended it).
fn window(kind: &str, limit: u64, remaining: u64, in_secs: Option<i64>) -> RateWindow {
    let now = SystemTime::now();

    RateWindow {
        kind: kind.to_owned(),
        limit,
        remaining,
        reset: in_secs.map(|seconds| {
            let offset = Duration::from_secs(seconds.unsigned_abs());
            if seconds < 0 { now - offset } else { now + offset }
        }),
    }
}

/// One plan window, `used_percent` spent, refilling `in_secs` from now —
/// negative for one already past its reset, and [`None`] for a vendor that
/// dated it not at all (**D485**).
fn plan(name: &str, used_percent: f64, in_secs: Option<i64>) -> PlanWindow {
    let now = SystemTime::now();

    PlanWindow {
        name: name.to_owned(),
        used_percent,
        window_minutes: None,
        resets_at: in_secs.map(|seconds| {
            let offset = Duration::from_secs(seconds.unsigned_abs());
            if seconds < 0 { now - offset } else { now + offset }
        }),
        limit_name: None,
    }
}

/// The idle bar is its left-hand segments and nothing else: no key
/// reminders, and therefore no padding out to a right edge that would hold
/// them.
#[test]
fn an_idle_bar_shows_the_state_and_no_key_hints() {
    let line = rendered(&Status::new(None), 100);

    assert_eq!(line, "ready");
}

#[test]
fn a_streaming_bar_leads_with_a_spinner() {
    let mut status = Status::new(None);
    status.set_activity(Activity::Streaming);

    let line = rendered(&status, 100);

    assert!(status.is_streaming());
    assert!(line.contains("streaming"), "got {line:?}");
    assert!(!line.starts_with("streaming"), "got {line:?}");
}

/// A session with no agent registry — every scripted and golden run — says
/// nothing rather than saying "none", which is what keeps the layout
/// snapshots taken over those runs unchanged.
#[test]
fn the_bar_names_the_agent_only_once_there_is_one() {
    let mut status = Status::new(None);
    assert!(rendered(&status, 100).starts_with("ready"));

    status.set_agent(Some("plan".to_owned()));

    assert!(rendered(&status, 100).starts_with("plan"), "got {:?}", rendered(&status, 100));
}

/// The bypass is standing, so its marker is too (**D479**) — and a
/// session that did not ask for it renders the bar this build always drew,
/// which is what the rest of this file's expectations are written against.
#[test]
fn the_bar_carries_the_yolo_marker_only_in_a_bypassed_session() {
    let mut status = Status::new(None);
    let gated = rendered(&status, 100);
    assert!(!gated.contains("yolo"), "got {gated:?}");

    status.set_yolo(true);

    let bypassed = rendered(&status, 100);
    assert!(bypassed.starts_with("yolo"), "the marker is the first thing on the bar: {bypassed:?}");

    status.set_yolo(false);
    assert_eq!(rendered(&status, 100), gated, "turning it off restores the bar cell for cell");
}

/// The marker is not a roster element, so it does not depend on a config
/// naming it: a `tui.statusline` table that leaves everything else out
/// still says this session is bypassed (**D479**).
#[test]
fn a_configured_roster_carries_the_marker_too() {
    let mut status = roster(&[StatuslineElement::Activity]);
    assert!(!rendered(&status, 100).contains("yolo"));

    status.set_yolo(true);

    let line = rendered(&status, 100);
    assert!(line.starts_with("yolo"), "got {line:?}");
    assert!(line.contains("ready"), "and the roster it was prepended to is untouched: {line:?}");
}

/// The depth appears only while something is waiting, so a session that
/// never queues a message renders the bar it always did (**F4**).
#[test]
fn the_bar_names_the_queue_only_while_something_is_waiting() {
    let mut status = Status::new(None);
    assert!(!rendered(&status, 100).contains("queued"));

    status.set_queued(2);

    let line = rendered(&status, 100);
    assert!(line.contains("2 queued"), "got {line:?}");

    status.set_queued(0);
    assert!(!rendered(&status, 100).contains("queued"));
}

/// A running background job appears only while one is running, the same
/// posture as the queue depth beside it (**F1**).
#[test]
fn the_bar_names_running_background_jobs_only_while_there_are_any() {
    let mut status = Status::new(None);
    assert!(!rendered(&status, 100).contains("bash running"));

    status.set_running_jobs(1);
    let line = rendered(&status, 100);
    assert!(line.contains("1 bash running"), "got {line:?}");

    status.set_running_jobs(0);
    assert!(!rendered(&status, 100).contains("bash running"));
}

/// The same posture on the segment the default backend needs most: a
/// teammate running in this process has no window to look at (**D503**).
#[test]
fn the_bar_names_the_teammates_this_session_leads_only_while_there_are_any() {
    let mut status = Status::new(None);
    assert!(!rendered(&status, 100).contains("teammate"));

    status.set_teammates(1);
    let line = rendered(&status, 100);
    assert!(
        line.contains("1 teammate") && !line.contains("1 teammates"),
        "from one, unlike the task segment, and singular at one: got {line:?}"
    );

    status.set_teammates(3);
    assert!(rendered(&status, 100).contains("3 teammates"));

    status.set_teammates(0);
    assert!(!rendered(&status, 100).contains("teammate"));
}

/// The two segments concurrent children brought with them, under the same
/// appear-only-while-nonzero posture as everything beside them (**D462**).
#[test]
fn the_bar_names_running_children_and_queued_dialogs_only_while_there_are_any() {
    let mut status = Status::new(None);
    let quiet = rendered(&status, 120);
    assert!(!quiet.contains("tasks running"));
    assert!(!quiet.contains("dialogs queued"));

    status.set_running_tasks(1);
    assert!(
        !rendered(&status, 120).contains("tasks running"),
        "one delegation is not a fan-out, and the activity segment names it"
    );

    status.set_running_tasks(3);
    status.set_queued_dialogs(1);
    let line = rendered(&status, 120);
    assert!(line.contains("3 tasks running"), "got {line:?}");
    assert!(line.contains("1 dialog queued"), "got {line:?}");

    status.set_queued_dialogs(2);
    assert!(rendered(&status, 120).contains("2 dialogs queued"));

    status.set_running_tasks(0);
    status.set_queued_dialogs(0);
    let quiet = rendered(&status, 120);
    assert!(!quiet.contains("tasks running"));
    assert!(!quiet.contains("dialogs queued"));
}

/// The admission gate's count (**D524**), the same posture: a session
/// nothing is held against renders the bar it always did, and the count
/// appears the moment something is — dialog count or no dialog count.
#[test]
fn the_bar_names_held_messages_only_while_there_are_any() {
    let mut status = Status::new(None);
    assert!(!rendered(&status, 120).contains("held"));

    status.set_held(1);
    let line = rendered(&status, 120);
    assert!(line.contains("1 held"), "got {line:?}");

    status.set_held(3);
    assert!(rendered(&status, 120).contains("3 held"));

    status.set_held(0);
    assert!(!rendered(&status, 120).contains("held"));
}

/// D524's count is ordinary roster vocabulary now: a roster that names
/// `held` draws the count where it wrote the name, and only while
/// something is held — the appear-only-while-nonzero posture every
/// element keeps.
#[test]
fn a_roster_naming_held_shows_the_count_only_while_something_is_held() {
    let mut status = roster(&[StatuslineElement::Activity, StatuslineElement::Held]);
    assert!(!rendered(&status, 120).contains("held"));

    status.set_held(2);
    let line = rendered(&status, 120);
    assert!(line.contains("2 held"), "got {line:?}");

    status.set_held(0);
    assert!(!rendered(&status, 120).contains("held"));
}

/// The element name retired the dialogs piggyback: a roster that leaves
/// `held` out never draws the count — not even beside `dialogs`, where
/// the then-nameless segment used to ride.
#[test]
fn a_roster_omitting_held_never_shows_the_count() {
    let mut status = roster(&[StatuslineElement::Activity, StatuslineElement::Dialogs]);
    status.set_held(2);
    let line = rendered(&status, 120);
    assert!(!line.contains("held"), "got {line:?}");
}

/// The segment appears only while an effort is selected, so every bar
/// drawn before efforts existed — and every session on Default — renders
/// byte for byte as it always did.
#[test]
fn the_bar_names_the_model_and_effort_only_while_one_is_selected() {
    let mut status = Status::new(None);
    assert!(!rendered(&status, 100).contains('('));

    status.set_agent(Some("build".to_owned()));
    status.set_effort(Some(("claude-opus-5".to_owned(), "max".to_owned())));

    let line = rendered(&status, 100);
    assert!(line.starts_with("build"), "got {line:?}");
    assert!(line.contains("claude-opus-5 (max)"), "got {line:?}");

    status.set_effort(None);
    assert!(!rendered(&status, 100).contains("claude-opus-5"));
}

#[test]
fn a_notice_sits_next_to_the_state() {
    let status = Status::new(Some("provider defaulted".to_owned()));

    assert!(rendered(&status, 100).contains("provider defaulted"), "the notice should be visible");
}

/// The state is what a bar too narrow for everything keeps — with the
/// idle footer that is all there was, and shell mode still gives its
/// reminder up rather than the state.
#[test]
fn a_narrow_bar_drops_the_hints_rather_than_the_state() {
    let mut status = Status::new(None);
    assert_eq!(rendered(&status, 12), "ready");

    status.set_shell(true);
    assert_eq!(rendered(&status, 12), "ready");
}

#[test]
fn a_zero_width_bar_draws_nothing() {
    assert_eq!(rendered(&Status::new(None), 0), "");
}

#[test]
fn a_cancelled_turn_reads_as_stopped() {
    let mut status = Status::new(None);
    status.set_activity(Activity::Streaming);
    status.set_activity(Activity::Stopped);

    assert!(!status.is_streaming());
    assert!(rendered(&status, 100).starts_with("stopped"));
}

#[test]
fn spend_is_shown_compactly_next_to_the_state() {
    let mut status = Status::new(None);
    status.set_totals(Totals {
        input_tokens: 12_345,
        output_tokens: 1_200,
        cost_usd: Some(0.084_2),
    });

    let line = rendered(&status, 100);

    assert!(line.starts_with("ready"), "got {line:?}");
    assert!(line.contains("12.3k in"), "got {line:?}");
    assert!(line.contains("1.2k out"), "got {line:?}");
    assert!(line.contains("$0.0842"), "got {line:?}");
}

/// A turn against a model the catalog cannot price still reports its
/// tokens; inventing a dollar figure for it would be worse than omitting
/// one.
#[test]
fn an_unpriced_model_shows_tokens_without_a_price() {
    let mut status = Status::new(None);
    status.set_totals(Totals { input_tokens: 40, output_tokens: 7, cost_usd: None });

    let line = rendered(&status, 100);

    assert!(line.contains("40 in"), "got {line:?}");
    assert!(line.contains("7 out"), "got {line:?}");
    assert!(!line.contains('$'), "got {line:?}");
}

/// Sub-cent sessions are the common case early on, so the dollar figure
/// keeps enough decimals to be something other than zero.
#[test]
fn a_sub_cent_session_still_shows_a_number() {
    let mut status = Status::new(None);
    status.set_totals(Totals { input_tokens: 0, output_tokens: 0, cost_usd: Some(0.000_7) });

    let line = rendered(&status, 100);

    assert!(line.contains("$0.0007"), "got {line:?}");
}

/// Spend must not crowd out the reason a turn failed.
#[test]
fn a_notice_survives_beside_the_spend() {
    let mut status = Status::new(Some("no usable credentials".to_owned()));
    status.set_activity(Activity::Failed);
    status.set_totals(Totals { input_tokens: 1_000, output_tokens: 0, cost_usd: Some(0.5) });

    let line = rendered(&status, 120);

    assert!(line.starts_with("failed"), "got {line:?}");
    assert!(line.contains("1.0k in"), "got {line:?}");
    assert!(line.contains("no usable credentials"), "got {line:?}");
}

#[test]
fn a_failed_turn_reads_as_failed_and_explains_itself() {
    let mut status = Status::new(None);
    status.set_activity(Activity::Streaming);
    status.set_activity(Activity::Failed);
    status.set_notice(Some("no usable credentials".to_owned()));

    let line = rendered(&status, 100);

    assert!(!status.is_streaming());
    assert!(line.starts_with("failed"), "got {line:?}");
    assert!(line.contains("no usable credentials"), "got {line:?}");
}

#[test]
fn a_running_tool_names_itself_in_the_activity_label() {
    let mut status = Status::new(None);
    status.set_activity(Activity::Tool("shell".to_owned()));

    assert!(rendered(&status, 100).contains("tool: shell"));
}

#[test]
fn waiting_on_a_permission_has_its_own_label() {
    let mut status = Status::new(None);
    status.set_activity(Activity::Permission);

    assert!(rendered(&status, 100).contains("waiting on permission"));
}

/// Shell mode is the one mode whose way out is not guessable, so it is the
/// one mode that still spends bar width saying it — and it stops the
/// moment the buffer stops being a shell command.
#[test]
fn shell_mode_reminds_the_user_of_the_way_out() {
    let mut status = Status::new(None);
    assert_eq!(rendered(&status, 120), "ready");

    status.set_shell(true);

    let line = rendered(&status, 120);
    assert!(line.contains(SHELL_HINTS), "got {line:?}");
    assert!(line.ends_with(SHELL_HINTS), "got {line:?}");

    status.set_shell(false);
    assert_eq!(rendered(&status, 120), "ready");
}

/// The acceptance shape for the roster (**D469**): what the config names,
/// in the order it names it, and nothing the default bar would have added
/// on its own.
#[test]
fn a_configured_roster_renders_exactly_what_it_names_in_its_order() {
    let mut status =
        roster(&[StatuslineElement::Model, StatuslineElement::Context, StatuslineElement::Tokens]);
    status.set_model(Some("claude-opus-5".to_owned()));
    status.set_context(Some((12_000, 100_000)));
    status.set_totals(Totals { input_tokens: 40, output_tokens: 7, cost_usd: None });

    let line = rendered(&status, 120);

    assert_eq!(
        line, "Model: claude-opus-5 | ctx:[#-------]12% | 40 in \u{b7} 7 out",
        "the three named elements, in the named order, and nothing else"
    );
    assert!(!line.contains("ready"), "the activity was not named");
}

/// The `rate` element draws the vendor's tightest live window in the
/// context meter's own shape (**D484**).
#[test]
fn the_rate_element_meters_the_tightest_window_the_vendor_reported() {
    let mut status = roster(&[StatuslineElement::Rate]);
    status.set_rates(vec![
        // 10% spent — the roomy one, which must not be the one shown.
        window("requests", 1_000, 900, Some(60)),
        // 75% spent — the budget that will stop a turn first.
        window("input-tokens", 80_000, 20_000, Some(60)),
    ]);

    assert_eq!(
        rendered(&status, 60),
        "rate:[######--]75%",
        "the tightest of the two windows is the one metered"
    );
}

/// The D470 rule at the bar: a wire that heard no rate headers yields no
/// cell, not a zero.
#[test]
fn the_rate_element_renders_nothing_when_the_vendor_said_nothing() {
    let status = roster(&[StatuslineElement::Rate, StatuslineElement::Activity]);

    assert_eq!(
        rendered(&status, 60),
        "ready",
        "with no windows the element contributes no cell and no separator"
    );
}

/// P16 pre-mortem 4, resettled 2026-08-15: the meter reads the newest
/// figure the vendor gave and consults no reset clock — request buckets
/// legally reset in milliseconds, so honoring the clock either blinked
/// the cell with every response or pinned it at zero, and the gauge's
/// question is how hard the budget was pressed when the vendor last
/// spoke. The next response replaces the set; that is the whole of the
/// staleness story, exactly as it always was for a clockless bucket.
#[test]
fn a_rate_window_past_its_reset_keeps_metering_the_last_heard_figure() {
    let mut status = roster(&[StatuslineElement::Rate, StatuslineElement::Activity]);
    status.set_rates(vec![window("requests", 1_000, 4, Some(-1))]);

    let line = rendered(&status, 60);
    assert_eq!(
        line, "rate:[########]100% | ready",
        "the newest reading stands until a newer response replaces it"
    );
}

/// The other side of that decay, since P22 (`53v`): a window nothing dated
/// cannot go stale, so it stays on the bar until a later response replaces
/// the set. The element's *shape* is untouched — a used percentage needs
/// no clock — which is the whole of what the amendment costs this surface.
#[test]
fn a_rate_window_its_vendor_never_dated_keeps_metering() {
    let mut status = roster(&[StatuslineElement::Rate]);
    status.set_rates(vec![window("requests", 1_000, 900, None)]);

    assert_eq!(
        rendered(&status, 60),
        "rate:[#-------]10%",
        "grok's clockless bucket meters exactly as a dated one does"
    );
}

/// The plan bucket rides beside the throttling one, so the two questions —
/// what stops this request, what runs out this week — are both answerable
/// off one element (**D485**).
#[test]
fn the_rate_element_meters_the_plan_bucket_beside_the_throttling_one() {
    let mut status = roster(&[StatuslineElement::Rate]);
    status.set_rates(vec![window("requests", 1_000, 900, Some(60))]);
    status.set_plans(vec![plan("primary", 62.0, Some(3_600))]);

    assert_eq!(
        rendered(&status, 60),
        "plan:[#####---]62% | rate:[#-------]10%",
        "the plan meter leads, the rate meter follows"
    );
}

/// Either half alone is a whole cell: a credential that serves plan
/// headers and no rate headers still meters.
#[test]
fn the_rate_element_draws_a_plan_bucket_alone_when_no_rate_window_was_heard() {
    let mut status = roster(&[StatuslineElement::Rate]);
    status.set_plans(vec![plan("primary", 40.0, Some(3_600))]);

    assert_eq!(rendered(&status, 60), "plan:[###-----]40%");
}

/// The tightest of several, exactly as the rate half picks its own.
#[test]
fn the_plan_bucket_shown_is_the_one_that_runs_out_first() {
    let mut status = roster(&[StatuslineElement::Rate]);
    status
        .set_plans(vec![plan("primary", 12.0, Some(3_600)), plan("secondary", 75.0, Some(86_400))]);

    assert_eq!(rendered(&status, 60), "plan:[######--]75%");
}

/// A plan window whose vendor sent no reset has no clock, so nothing may
/// decay it — it stays until a later response replaces it.
#[test]
fn a_plan_window_the_vendor_never_dated_stays_on_the_bar() {
    let mut status = roster(&[StatuslineElement::Rate]);
    status.set_plans(vec![plan("premium_interactions", 88.0, None)]);

    assert_eq!(rendered(&status, 60), "plan:[#######-]88%");
}

/// The sibling shape reads the same way: a plan window's newest figure
/// stands whatever its reset clock says, beside the rate meter.
#[test]
fn a_plan_window_past_its_reset_keeps_metering_beside_the_rate_meter() {
    let mut status = roster(&[StatuslineElement::Rate, StatuslineElement::Activity]);
    status.set_rates(vec![window("requests", 100, 50, Some(60))]);
    status.set_plans(vec![plan("primary", 99.0, Some(-1))]);

    let line = rendered(&status, 60);
    assert_eq!(
        line, "plan:[########]99% | rate:[####----]50% | ready",
        "the newest plan reading stands until a newer response replaces it"
    );
}

/// The tightest window of the set is the one metered, whatever either
/// window's clock says — the meter answers for the whole set as heard.
#[test]
fn the_tightest_window_of_the_set_is_metered_whatever_its_clock_says() {
    let mut status = roster(&[StatuslineElement::Rate]);
    status.set_rates(vec![
        window("input-tokens", 100, 0, Some(-1)),
        window("requests", 100, 50, Some(60)),
    ]);

    assert_eq!(rendered(&status, 60), "rate:[########]100%");
}

/// The meter's percent is the estimate the app handed in, and 70% is
/// where the fill stops being calm (**D469**).
#[test]
fn the_context_meter_shows_the_handed_in_percent_and_warns_at_seventy() {
    let theme = Theme::default();
    let calm = {
        let mut status = roster(&[StatuslineElement::Context]);
        status.set_context(Some((69, 100)));
        status
    };
    let line = rendered(&calm, 60);
    assert!(line.contains("ctx:["), "got {line:?}");
    assert!(line.contains("]69%"), "got {line:?}");

    let area = Rect::new(0, 0, 60, 1);
    let mut buffer = Buffer::empty(area);
    calm.render(area, &mut buffer, &theme);
    // Cell 5 is the first bar slot, right after "ctx:[".
    assert_eq!(buffer[(5, 0)].symbol(), "#");
    assert_eq!(buffer[(5, 0)].style().fg, theme.success.fg);

    let warned = {
        let mut status = roster(&[StatuslineElement::Context]);
        status.set_context(Some((70, 100)));
        status
    };
    let mut buffer = Buffer::empty(area);
    warned.render(area, &mut buffer, &theme);
    assert_eq!(buffer[(5, 0)].symbol(), "#");
    assert_eq!(
        buffer[(5, 0)].style().fg,
        theme.warning.fg,
        "seventy percent is where the meter starts warning"
    );
}

/// OMC's ladder, at its boundaries: fill is `round(percent/100 * 8)` and
/// the color steps at 70, 80 and 85 — the last one the red the
/// screenshot's exhausted meters wear.
#[test]
fn the_meter_math_holds_at_the_ladder_boundaries() {
    assert_eq!(meter_fill(0), 0);
    assert_eq!(meter_fill(69), 6);
    assert_eq!(meter_fill(70), 6);
    assert_eq!(meter_fill(84), 7);
    assert_eq!(meter_fill(85), 7);
    assert_eq!(meter_fill(100), 8);

    assert_eq!(meter_severity(0), Severity::Ok);
    assert_eq!(meter_severity(69), Severity::Ok);
    assert_eq!(meter_severity(70), Severity::Warning);
    assert_eq!(meter_severity(79), Severity::Warning);
    assert_eq!(meter_severity(80), Severity::Compact);
    assert_eq!(meter_severity(84), Severity::Compact);
    assert_eq!(meter_severity(85), Severity::Critical);
    assert_eq!(meter_severity(100), Severity::Critical);
}

/// An estimate past the window still reads as full, never as more.
#[test]
fn the_context_meter_never_claims_more_than_full() {
    let mut status = roster(&[StatuslineElement::Context]);
    status.set_context(Some((250_000, 100_000)));

    let line = rendered(&status, 60);

    assert!(line.contains("ctx:[########]100%"), "got {line:?}");
}

/// A roster wider than the bar ends with the OMC ellipsis instead of a
/// silent cut.
#[test]
fn a_narrow_roster_truncates_with_an_ellipsis() {
    let mut status = roster(&[StatuslineElement::Model]);
    status.set_model(Some("a-model-with-a-very-long-name".to_owned()));

    let line = rendered(&status, 20);

    assert_eq!(line.width(), 20, "got {line:?}");
    assert!(line.ends_with("..."), "got {line:?}");
}

/// The effort segment wears the model value's own style — the same
/// color and weight the name has after `Model:` — not the accent the
/// other plain segments wear.
#[test]
fn the_effort_segment_wears_the_model_values_own_style() {
    let mut status = roster(&[StatuslineElement::Model, StatuslineElement::Effort]);
    status.set_model(Some("claude-opus-5".to_owned()));
    status.set_effort(Some(("claude-opus-5".to_owned(), "max".to_owned())));
    let area = Rect::new(0, 0, 80, 1);
    let mut buffer = Buffer::empty(area);
    status.render(area, &mut buffer, &Theme::default());
    let line: String = (0..80).map(|column| buffer[(column, 0)].symbol()).collect();
    let column_of = |needle: &str| {
        u16::try_from(line.find(needle).expect("the segment is on the bar")).expect("fits")
    };

    let model_value = buffer[(column_of("Model: ") + 7, 0)].style();
    let effort = buffer[(column_of("claude-opus-5 (max)"), 0)].style();
    assert_eq!(effort, model_value, "on {line:?}");
    assert_ne!(effort, buffer[(column_of("Model: "), 0)].style(), "and not the label's dim");
}

/// `max_width` caps the bar below the terminal's width, OMC's `maxWidth`.
#[test]
fn a_width_cap_truncates_before_the_terminal_edge_does() {
    let mut status = roster(&[StatuslineElement::Model]);
    status.max_width = Some(20);
    status.set_model(Some("a-model-with-a-very-long-name".to_owned()));

    let line = rendered(&status, 120);

    assert!(line.ends_with("..."), "got {line:?}");
    assert!(line.len() <= 20, "got {line:?}");
}

/// A status cut keeps a ZWJ family together even when its constituent
/// code points would cross the ellipsis boundary one by one.
#[test]
fn truncation_never_splits_a_zwj_family() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    let spans = truncate_spans(vec![Span::raw(format!("{family}xxxx"))], 5, &Theme::default());
    let text = spans.iter().map(|span| span.content.as_ref()).collect::<String>();

    assert_eq!(text, format!("{family}..."));
}

/// The git line reads `.git/HEAD` off the disk — both spellings — and
/// sits above the main line, the screenshot's pinned placement.
#[test]
fn the_git_line_reads_a_symbolic_head_as_its_branch() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let repo = directory.path().join("my-repo");
    fs::create_dir_all(repo.join(".git")).expect("the fixture repository is creatable");
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD writes");

    let mut status = roster(&[StatuslineElement::Git, StatuslineElement::Activity]);
    status.git = RefCell::new(discover_git(&repo));

    let rows = rendered_rows(&status, 60, 2);
    assert_eq!(rows[0], "repo:my-repo | branch:main");
    assert_eq!(rows[1], "ready");
    assert_eq!(status.height(), 2);
}

#[test]
fn a_detached_head_reads_as_a_short_hash() {
    assert_eq!(
        head_name("f0e1d2c3b4a5968778695a4b3c2d1e0f11223344\n").as_deref(),
        Some("f0e1d2c3")
    );
    assert_eq!(head_name("ref: refs/heads/feature/x\n").as_deref(), Some("feature/x"));
    assert_eq!(head_name(""), None);
}

/// The cache re-reads HEAD only when its mtime moves — the whole point of
/// the file read over a subprocess (P14 review changelog MINOR 4).
#[test]
fn the_git_line_follows_a_branch_switch_through_the_head_mtime() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let repo = directory.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("the fixture repository is creatable");
    let head = repo.join(".git/HEAD");
    fs::write(&head, "ref: refs/heads/main\n").expect("HEAD writes");

    let mut status = roster(&[StatuslineElement::Git]);
    status.git = RefCell::new(discover_git(&repo));
    assert!(rendered_rows(&status, 60, 2)[0].contains("branch:main"));

    // A fresh mtime, far enough from the first write that coarse
    // filesystem timestamps cannot collapse the two.
    fs::write(&head, "ref: refs/heads/next\n").expect("HEAD rewrites");
    let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    let stamp = fs::FileTimes::new().set_modified(bumped);
    fs::File::options()
        .append(true)
        .open(&head)
        .and_then(|file| file.set_times(stamp))
        .expect("the mtime moves");

    assert!(
        rendered_rows(&status, 60, 2)[0].contains("branch:next"),
        "a moved mtime invalidates the cache"
    );
}

/// A linked worktree's `.git` is a file pointing at the real gitdir; the
/// line still names the worktree's own directory and branch.
#[test]
fn a_linked_worktree_resolves_head_through_its_gitdir_pointer() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let gitdir = directory.path().join("main/.git/worktrees/wt");
    fs::create_dir_all(&gitdir).expect("the fixture gitdir is creatable");
    fs::write(gitdir.join("HEAD"), "ref: refs/heads/hotfix\n").expect("HEAD writes");
    let worktree = directory.path().join("wt");
    fs::create_dir_all(&worktree).expect("the fixture worktree is creatable");
    fs::write(worktree.join(".git"), format!("gitdir: {}\n", gitdir.display()))
        .expect("the pointer writes");

    let mut status = roster(&[StatuslineElement::Git]);
    status.git = RefCell::new(discover_git(&worktree));

    assert_eq!(rendered_rows(&status, 60, 2)[0], "repo:wt | branch:hotfix");
}

/// A too-short area drops the detail line first and the git line second —
/// the main line is the one row that never yields.
#[test]
fn a_short_area_keeps_the_main_line_over_the_extra_ones() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let repo = directory.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("the fixture repository is creatable");
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD writes");

    let mut status = roster(&[StatuslineElement::Git, StatuslineElement::Activity]);
    status.git = RefCell::new(discover_git(&repo));

    let rows = rendered_rows(&status, 60, 1);
    assert_eq!(rows[0], "ready", "one row leaves only the main line");
}

/// The `todos` element carries its progress inline and moves the title to
/// the detail line when one is allowed.
#[test]
fn todos_move_their_title_to_the_detail_line_when_detail_is_on() {
    let mut status = roster(&[StatuslineElement::Todos]);
    status.set_todos(Some(Todos { done: 2, total: 5, current: Some("wire the meter".to_owned()) }));

    assert_eq!(rendered(&status, 80), "todos:2/5 (working: wire the meter)");

    status.detail = true;
    let rows = rendered_rows(&status, 80, 2);
    assert_eq!(rows[0], "todos:2/5");
    assert_eq!(rows[1], "working: wire the meter");
    assert_eq!(status.height(), 2);
}

/// The session element counts from the bar's own birth, in OMC's
/// whole-minute form.
#[test]
fn the_session_element_counts_whole_minutes_from_birth() {
    let status = roster(&[StatuslineElement::Session]);

    assert_eq!(rendered(&status, 40), "session:0m");
}

/// The `hints` element keeps the right edge the default bar gave it — and
/// yields no cell at all in the modes that now have nothing to remind
/// about, which is every mode but shell.
#[test]
fn a_roster_with_hints_keeps_them_right_aligned() {
    let mut status = roster(&[StatuslineElement::Activity, StatuslineElement::Hints]);
    assert_eq!(rendered(&status, 100), "ready");

    status.set_shell(true);

    let line = rendered(&status, 100);
    assert!(line.starts_with("ready"), "got {line:?}");
    assert!(line.ends_with(SHELL_HINTS), "got {line:?}");
}
