//! The `/usage` dialog: what this session has spent, in Claude Code's panel
//! language — section headers, label-value rows, block bars — over what
//! ganja actually holds: the status bar's running totals, the per-turn rows
//! the Ctrl+T inspector already keeps, the context estimate the bar's meter
//! polls, and the cache splits the session record accumulates.
//!
//! **D471** (`slash-usage`): upstream opencode has no such surface at all, so
//! nothing here cites an upstream file. The rendering is pinned by real
//! Claude Code screenshots (2026-08-12, transcribed in the P14 W4f charter):
//! a `Session` section with a `Total cost:` row and per-model `Usage by
//! model:` lines — cost in parentheses at the tail — and solid block bars
//! (filled cells, dim remainder, ` NN%` after) where W4 drew the HUD's ASCII
//! `[####----]` meter. The HUD status bar deliberately keeps that ASCII
//! shape ([`crate::component::status`]): the two surfaces differ per pin
//! now, this one by the screenshot, that one by its own frozen test.
//!
//! Claude's panel leads with subscription plan-limit meters — the 5h and
//! weekly buckets — which ride a vendor usage API ganja does not speak;
//! those rows are **explicitly absent**, with one honest line naming why,
//! rather than drawn empty or faked (the plan's honest-degradation rule,
//! ruled sufficient in Open question 2).
//!
//! P16 narrowed that absence rather than filling it (**D484**): the vendor's
//! *rate-limit* windows — which every response's headers already carry, and
//! which need no usage API — get a `Current window` section of their own, in
//! the same block-bar shape, tightest budget first. What the honest line now
//! names is only the plan meters, which still have no source. A backend that
//! sends no such headers renders no section at all, and a window past its own
//! reset renders as expired rather than as a live figure.
//!
//! Its `Total duration` row renders
//! when the opener hands one in (W7: the app's own wall clock since it was
//! built — never an invented API duration); its code-change row stays absent
//! the same way as the plan meters: no data source exists anywhere in the
//! TUI. Two honest additions ride the pinned shapes: the per-model lines
//! carry a reasoning item Claude's line lacks, because ganja really tracks
//! that counter, and the per-turn table stays — inspector data with no
//! Claude equivalent, kept from W4.
//!
//! AC5 still holds by construction: the `Total cost:` value is
//! [`Totals::cost_usd`] — the status bar's own accumulator, not a second
//! sum — and the per-model and per-turn rows reuse the inspector's
//! [`TurnUsage`] records verbatim.
//!
//! A snapshot, not a view: everything is read when the dialog opens. Esc
//! closes it; [`crate::app::App`] owns that key, the same split every other
//! dialog keeps.

use ganja_core::{
    catalog::{self, compact_tokens},
    provider::RateWindow,
};
use ganja_protocol::Usage as TokenUsage;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::Modifier,
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};

use crate::{
    component::{
        chat::clip,
        inspector::{TurnUsage, short_id, turn_cost},
        status::Totals,
    },
    theme::Theme,
};

/// Widest the modal grows. Wider than the house 76: the pinned per-model
/// line — model id, five comma-separated counters, the cost tail — runs to
/// ~90 columns, and capping at the house width would clip it on every
/// terminal; narrow terminals still clip it honestly.
const MAX_WIDTH: u16 = 96;

/// Tallest the modal grows: enough for the sections and a handful of turn
/// rows; a longer session keeps its newest rows, the ones a person opening
/// the panel is asking about.
///
/// Two taller than P14 left it, which is exactly what the `Current window`
/// section costs at [`RATE_ROWS`] (**D484**). Grown rather than left to push
/// the honest-absence line and the key hints off the bottom: a section added
/// at the cost of the footer that explains the panel would be a poor trade.
const MAX_HEIGHT: u16 = 26;

/// How many rate windows the `Current window` section draws before it starts
/// counting the rest.
///
/// Two, because the question the section answers is "what stops me first" and
/// the tightest two are that answer — and because a vendor sending four
/// buckets would otherwise cost this modal eight rows it does not have. What
/// is left out is named on its own line rather than dropped silently.
const RATE_ROWS: usize = 2;

/// Cells in a block bar, the same span of columns the old ASCII meter held.
const METER_WIDTH: usize = 20;

/// The key hint at the dialog's foot.
const HINTS: &str = "[Esc] close";

/// The one honest line where Claude Code draws its plan-limit meters: ganja
/// speaks no vendor usage API, so there is nothing true to draw (D471).
///
/// **Narrowed in P16** (**D484**): it used to stand for every window this panel
/// could not show. The vendor's own rate-limit windows *are* now shown, in
/// [`Usage::render`]'s `Current window` section, so this line names only what
/// is still genuinely absent — the subscription plan's meters, which need the
/// usage API D471 ruled out.
const NO_PLAN_LIMITS: &str = "plan limits (5h/weekly) unavailable: no vendor usage API";

/// What the `Current window` section says when a bucket's own reset has
/// passed. The vendor said a true thing at a moment that is over; renaming it
/// is the whole staleness guard (P16 pre-mortem 4).
const EXPIRED: &str = "expired \u{2014} refreshes on the next request";

/// Column widths of the per-turn table, matching the inspector's tab so the
/// same rows read the same in both places.
const ID_WIDTH: usize = 10;
const COUNT_WIDTH: usize = 8;

/// Everything the dialog shows, read once when `/usage` opens.
#[derive(Clone, Debug, Default)]
pub struct Data {
    /// The status bar's running totals — the `Total cost:` row renders their
    /// own accumulated cost, so the two surfaces cannot disagree (AC5).
    pub totals: Totals,
    /// The session's accumulated splits: cache traffic and reasoning, which
    /// the running totals collapse — the cache bar's numerator and
    /// denominator.
    pub splits: TokenUsage,
    /// `(estimated tokens, window)`, absent for an uncataloged model — the
    /// same pair the status bar's context meter polls.
    pub context: Option<(u64, u64)>,
    /// One row per finished turn, the inspector's own rows.
    pub turns: Vec<TurnUsage>,
    /// Wall-clock time this session has been open — the app's own clock,
    /// measured from its construction, because that is the only duration
    /// this side truly holds (W7). Absent, the row is absent: an invented
    /// duration would be worse than none.
    pub duration: Option<std::time::Duration>,
    /// The vendor's own rate-limit windows, as `Engine::rate_windows` last
    /// answered (**D484**). Empty renders no `Current window` section at all —
    /// the D470 rule, which is why this panel can carry the section and the
    /// honest-absence line side by side without either becoming a lie.
    pub rates: Vec<RateWindow>,
    /// What "now" the section's expiry is judged against. Its own field rather
    /// than a [`SystemTime::now`] inside the renderer so a test can manufacture
    /// an already-expired bucket without waiting for a clock.
    ///
    /// [`SystemTime::now`]: std::time::SystemTime::now
    pub now: Option<std::time::SystemTime>,
}

/// The dialog itself.
#[derive(Clone, Debug)]
pub struct Usage {
    data: Data,
}

impl Usage {
    /// Opens the dialog over what the app read at `/usage` time.
    #[must_use]
    pub fn new(data: Data) -> Self {
        Self { data }
    }

    /// Input tokens served from cache, over every token that went in — the
    /// fraction the cache saved. [`None`] when nothing has gone in at all.
    fn cache_hit_rate(&self) -> Option<f64> {
        let splits = &self.data.splits;
        let input = splits
            .input_tokens
            .saturating_add(splits.cache_read_tokens)
            .saturating_add(splits.cache_write_tokens);
        if input == 0 {
            return None;
        }

        Some(splits.cache_read_tokens as f64 / input as f64)
    }

    /// Per-model sums of the finished turns — the screenshot's `Usage by
    /// model:` rows — in first-use order. Each turn is priced on its own,
    /// so a model the catalog cannot price sums its tokens and simply
    /// carries no cost tail.
    fn by_model(&self) -> Vec<(String, TokenUsage, Option<f64>)> {
        let mut rows: Vec<(String, TokenUsage, Option<f64>)> = Vec::new();
        for turn in &self.data.turns {
            let cost = catalog::model(&turn.model)
                .map(|model| catalog::cost(&turn.usage, &model).total_usd);
            match rows.iter_mut().find(|(model, ..)| *model == turn.model) {
                Some((_, usage, total)) => {
                    usage.input_tokens = usage.input_tokens.saturating_add(turn.usage.input_tokens);
                    usage.output_tokens =
                        usage.output_tokens.saturating_add(turn.usage.output_tokens);
                    usage.reasoning_tokens = usage
                        .reasoning_tokens
                        .saturating_add(turn.usage.reasoning_tokens);
                    usage.cache_read_tokens = usage
                        .cache_read_tokens
                        .saturating_add(turn.usage.cache_read_tokens);
                    usage.cache_write_tokens = usage
                        .cache_write_tokens
                        .saturating_add(turn.usage.cache_write_tokens);
                    *total = match (*total, cost) {
                        (Some(sum), Some(add)) => Some(sum + add),
                        (kept, added) => kept.or(added),
                    };
                }
                None => rows.push((turn.model.clone(), turn.usage, cost)),
            }
        }

        rows
    }

    /// The pinned block bar: filled cells, dim remainder, the percentage
    /// after — the caller appends its own phrase (` used`, ` of input read
    /// from cache`).
    fn bar(fraction: f64, theme: &Theme) -> Vec<Span<'static>> {
        let filled = ((fraction * METER_WIDTH as f64).round() as usize).min(METER_WIDTH);

        vec![
            Span::styled("\u{2588}".repeat(filled), theme.info),
            Span::styled("\u{2591}".repeat(METER_WIDTH - filled), theme.dim),
            Span::styled(format!(" {:.0}%", fraction * 100.0), theme.fg),
        ]
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let available = area.height.saturating_sub(2).clamp(1, MAX_HEIGHT);
        let inner_width = usize::from(width).saturating_sub(2);
        let room = usize::from(available).saturating_sub(2);
        let header = theme.accent.add_modifier(Modifier::BOLD);

        let mut lines = Vec::new();

        // Session: the pinned label-value rows — the bar's own cost, then
        // one line per model summed off the inspector's turn rows.
        lines.push(Line::styled(clip("Session", inner_width), header));
        let cost = self.data.totals.cost_usd.map_or_else(
            || "unpriced \u{2014} uncataloged model".to_owned(),
            |dollars| format!("${dollars:.2}"),
        );
        lines.push(Line::styled(
            clip(&format!("  {:<16} {cost}", "Total cost:"), inner_width),
            theme.fg,
        ));
        // The pinned row between cost and the per-model lines — rendered
        // only over a duration the opener really measured.
        if let Some(duration) = self.data.duration {
            lines.push(Line::styled(
                clip(
                    &format!("  {:<16} {}", "Total duration:", compact_duration(duration)),
                    inner_width,
                ),
                theme.fg,
            ));
        }
        lines.push(Line::styled(
            clip("  Usage by model:", inner_width),
            theme.fg,
        ));
        let models = self.by_model();
        if models.is_empty() {
            lines.push(Line::styled(clip("    none yet", inner_width), theme.dim));
        } else {
            for (model, usage, cost) in models {
                // Reasoning rides as one more comma item: Claude's line has
                // no equivalent, but ganja really tracks the counter.
                let tail = cost.map_or_else(String::new, |dollars| format!(" (${dollars:.2})"));
                lines.push(Line::styled(
                    clip(
                        &format!(
                            "    {model}:  {} input, {} output, {} reasoning, {} cache read, {} cache write{tail}",
                            compact_tokens(usage.input_tokens),
                            compact_tokens(usage.output_tokens),
                            compact_tokens(usage.reasoning_tokens),
                            compact_tokens(usage.cache_read_tokens),
                            compact_tokens(usage.cache_write_tokens),
                        ),
                        inner_width,
                    ),
                    theme.fg,
                ));
            }
        }
        lines.push(Line::raw(""));

        // Context: the same estimate the status bar's meter polls, as the
        // pinned block bar with its dim sub-line — or the same honest
        // absence.
        lines.push(Line::styled(clip("Context", inner_width), header));
        match self.data.context {
            Some((tokens, window)) => {
                let fraction = (tokens as f64 / window.max(1) as f64).min(1.0);
                let mut spans = vec![Span::raw("  ")];
                spans.extend(Self::bar(fraction, theme));
                spans.push(Span::styled(" used", theme.fg));
                lines.push(Line::from(spans));
                lines.push(Line::styled(
                    clip(
                        &format!(
                            "  {} of {} tokens \u{2014} estimated",
                            compact_tokens(tokens),
                            compact_tokens(window),
                        ),
                        inner_width,
                    ),
                    theme.dim,
                ));
            }
            None => lines.push(Line::styled(
                clip("  unsized model \u{2014} no window to meter", inner_width),
                theme.dim,
            )),
        }
        lines.push(Line::raw(""));

        // Current window: the vendor's own rate-limit buckets, in the same
        // block-bar shape as everything above (**D484**). Absent entirely
        // when the wire has heard none — a section header over "none" would
        // be this panel claiming to know something about a backend that
        // never spoke.
        if !self.data.rates.is_empty() {
            let now = self.data.now.unwrap_or_else(std::time::SystemTime::now);
            // Tightest first: the budget that will stop a turn soonest is the
            // one somebody opening this panel is asking about, and it is what
            // the cap below keeps.
            let mut windows: Vec<_> = self.data.rates.iter().collect();
            windows.sort_by(|left, right| right.used().total_cmp(&left.used()));

            lines.push(Line::styled(clip("Current window", inner_width), header));
            for window in windows.iter().take(RATE_ROWS) {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(Self::bar(window.used(), theme));
                spans.push(Span::styled(
                    format!(
                        " {} \u{b7} {} of {} left, {}",
                        window.kind,
                        compact_tokens(window.remaining),
                        compact_tokens(window.limit),
                        if window.expired(now) {
                            EXPIRED.to_owned()
                        } else {
                            format!(
                                "resets in {}",
                                compact_duration(
                                    window.reset.duration_since(now).unwrap_or_default()
                                )
                            )
                        },
                    ),
                    theme.fg,
                ));
                lines.push(Line::from(spans));
            }
            if let Some(rest) = windows
                .len()
                .checked_sub(RATE_ROWS)
                .filter(|rest| *rest > 0)
            {
                lines.push(Line::styled(
                    clip(
                        &format!("  ({rest} roomier windows not shown)"),
                        inner_width,
                    ),
                    theme.dim,
                ));
            }
            lines.push(Line::raw(""));
        }

        // Cache: what the splits bought — a real, derived rate Claude shows
        // only as raw counters (which the Session lines now carry too).
        lines.push(Line::styled(clip("Cache", inner_width), header));
        match self.cache_hit_rate() {
            Some(rate) => {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(Self::bar(rate, theme));
                spans.push(Span::styled(" of input read from cache", theme.fg));
                lines.push(Line::from(spans));
            }
            None => lines.push(Line::styled(
                clip("  nothing sent yet", inner_width),
                theme.dim,
            )),
        }
        lines.push(Line::raw(""));

        // Turns: the inspector's rows, newest kept when the room runs out —
        // a ganja addition Claude's panel has no equivalent of.
        lines.push(Line::styled(clip("Turns", inner_width), header));
        if self.data.turns.is_empty() {
            lines.push(Line::styled(
                clip("  no finished turns yet", inner_width),
                theme.dim,
            ));
        } else {
            lines.push(Line::styled(
                clip(
                    &format!(
                        "  {:<ID_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$}",
                        "turn", "in", "out", "reason", "cache-r", "cache-w", "cost",
                    ),
                    inner_width,
                ),
                theme.dim,
            ));
            // Header lines above plus the honest-absence tail and hints below.
            let fixed = lines.len() + 4;
            let visible = room.saturating_sub(fixed).max(1);
            let skipped = self.data.turns.len().saturating_sub(visible);
            for row in self.data.turns.iter().skip(skipped) {
                lines.push(Line::styled(
                    clip(
                        &format!(
                            "  {:<ID_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$} {:>COUNT_WIDTH$}",
                            short_id(&row.message_id),
                            row.usage.input_tokens,
                            row.usage.output_tokens,
                            row.usage.reasoning_tokens,
                            row.usage.cache_read_tokens,
                            row.usage.cache_write_tokens,
                            turn_cost(row),
                        ),
                        inner_width,
                    ),
                    theme.fg,
                ));
            }
            if skipped > 0 {
                lines.push(Line::styled(
                    clip(
                        &format!("  ({skipped} earlier turns not shown)"),
                        inner_width,
                    ),
                    theme.dim,
                ));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(NO_PLAN_LIMITS, inner_width), theme.dim));
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

        let height = u16::try_from(lines.len().saturating_add(2))
            .unwrap_or(available)
            .min(available);
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" usage "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }
}

/// A wall duration in the screenshot's compact spelling: `3h 9m`, `2m`, or
/// `45s` under a minute — never more than two units, because a person asking
/// how long a session ran does not want its seconds once it has hours.
fn compact_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    let (hours, minutes) = (seconds / 3_600, (seconds % 3_600) / 60);

    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use ganja_protocol::Message;
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Data, RateWindow, TokenUsage, TurnUsage, Usage};
    use crate::{component::status::Totals, theme::Theme};

    /// Wide enough for the pinned per-model line to render whole.
    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 28,
    };

    fn data() -> Data {
        Data {
            totals: Totals {
                input_tokens: 16,
                output_tokens: 4,
                cost_usd: Some(0.5),
            },
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
            now: None,
        }
    }

    /// One rate-limit window, `remaining` of `limit` left, refilling `in_secs`
    /// from [`NOW`] — negative for one whose reset has already gone by.
    fn window(kind: &str, limit: u64, remaining: u64, in_secs: i64) -> RateWindow {
        let offset = Duration::from_secs(in_secs.unsigned_abs());

        RateWindow {
            kind: kind.to_owned(),
            limit,
            remaining,
            reset: if in_secs < 0 {
                NOW - offset
            } else {
                NOW + offset
            },
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
        assert!(
            screen.contains("estimated"),
            "the context sub-line says estimated:\n{screen}"
        );
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
        let rate = Usage::new(data())
            .cache_hit_rate()
            .expect("the fixture sent input");

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
            let screen = rendered(
                &Usage::new(Data {
                    duration: Some(duration),
                    ..data()
                }),
                AREA,
            );
            assert!(
                screen.contains(&format!("Total duration:  {spelled}")),
                "want {spelled} in:\n{screen}"
            );
        }

        let unmeasured = rendered(&Usage::new(data()), AREA);
        assert!(
            !unmeasured.contains("Total duration"),
            "no measured duration, no row:\n{unmeasured}"
        );
    }

    /// The plan-limit meters Claude Code leads with are explicitly absent,
    /// with the one honest line naming why — never drawn empty, never faked.
    #[test]
    fn plan_limits_are_absent_with_the_honest_line() {
        let screen = rendered(&Usage::new(data()), AREA);

        assert!(
            screen.contains("plan limits (5h/weekly) unavailable: no vendor usage API"),
            "got:\n{screen}"
        );
        assert!(
            !screen.contains("5h:["),
            "no plan-limit meter may be drawn:\n{screen}"
        );
        assert!(
            !screen.contains("wk:["),
            "no weekly meter may be drawn:\n{screen}"
        );
        // The P16 narrowing (**D484**): with no windows heard, the section is
        // absent too — the honest-absence line is not standing in for it.
        assert!(
            !screen.contains("Current window"),
            "a vendor that said nothing gets no section:\n{screen}"
        );
    }

    /// The `Current window` section renders what the vendor really said, in
    /// the panel's own block-bar shape (**D484**).
    #[test]
    fn the_current_window_section_meters_each_bucket_the_vendor_reported() {
        let screen = rendered(
            &Usage::new(Data {
                rates: vec![
                    window("requests", 1_000, 900, 90),
                    window("input-tokens", 80_000, 20_000, 30),
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
        // The narrowed absence line still stands beside it: what is missing is
        // the plan meters, and only those.
        assert!(
            screen.contains("plan limits (5h/weekly) unavailable: no vendor usage API"),
            "got:\n{screen}"
        );
    }

    /// P16 pre-mortem 4, on the panel: a window past its reset says so instead
    /// of presenting a number as current.
    #[test]
    fn a_window_past_its_reset_renders_as_expired_rather_than_as_a_live_number() {
        let screen = rendered(
            &Usage::new(Data {
                rates: vec![window("requests", 1_000, 4, -1)],
                now: Some(NOW),
                ..data()
            }),
            AREA,
        );

        assert!(
            screen.contains("expired \u{2014} refreshes on the next request"),
            "got:\n{screen}"
        );
        assert!(
            !screen.contains("resets in"),
            "nothing may claim the window is still counting down:\n{screen}"
        );
        // The counts stay: they were true when the vendor said them, and the
        // panel's job is to date them, not to erase them.
        assert!(screen.contains("4 of 1.0k left"), "got:\n{screen}");
    }

    /// More windows than the section draws are counted, never dropped in
    /// silence — and the ones kept are the tightest (**D484**).
    #[test]
    fn windows_past_the_sections_cap_are_counted_and_the_tightest_are_kept() {
        let screen = rendered(
            &Usage::new(Data {
                rates: vec![
                    window("requests", 100, 99, 60),
                    window("input-tokens", 100, 5, 60),
                    window("output-tokens", 100, 50, 60),
                    window("tokens", 100, 90, 60),
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
            screen.contains("plan limits (5h/weekly) unavailable"),
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
        let screen = rendered(
            &Usage::new(Data {
                context: None,
                ..data()
            }),
            AREA,
        );

        assert!(
            screen.contains("unsized model \u{2014} no window to meter"),
            "got:\n{screen}"
        );
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        for (width, height) in [(1, 1), (4, 3), (20, 5)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);

            Usage::new(data()).render(area, &mut buffer, &Theme::default());
        }
    }
}
