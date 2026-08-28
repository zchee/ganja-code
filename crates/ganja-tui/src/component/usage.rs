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
//! weekly buckets — which P14 could reach only through a vendor usage API
//! ganja does not speak; those rows were **explicitly absent**, with one
//! honest line naming why, rather than drawn empty or faked (the plan's
//! honest-degradation rule, ruled sufficient in Open question 2). Two
//! narrowings since have left that rule intact and moved what it applies to:
//!
//! P16 narrowed that absence rather than filling it (**D484**): the vendor's
//! *rate-limit* windows — which every response's headers already carry, and
//! which need no usage API — get a `Current window` section of their own, in
//! the same block-bar shape, tightest budget first. A backend that
//! sends no such headers renders no section at all, and a window past its own
//! reset renders as expired rather than as a live figure. P22 widened that
//! last clause rather than changing it: a window whose vendor sent no reset at
//! all renders `resets: —` — nothing dated it, so it can neither be shown
//! expired nor given an instant it never had.
//!
//! P17 narrowed it again, and this time on the plan meters themselves
//! (**D485**): the W-A1 probe found two backends spelling the 5h/weekly
//! budgets in *headers* rather than behind the usage API D471 ruled out, so a
//! `Plan limits` section renders them — from windows this session's own
//! credential really served, never from a shape. The honest line consequently
//! became a *conditional* one: it is drawn only when no plan window was
//! captured, and it now names which credentials are still silent and why,
//! citing the probe rather than claiming the meters are unbuildable anywhere.
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

use ganja_core::catalog::{self, compact_tokens};
use ganja_core::provider::{PlanWindow, RateWindow};
use ganja_protocol::Usage as TokenUsage;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Widget as _};

use crate::component::chat::clip;
use crate::component::inspector::{TurnUsage, short_id, turn_cost};
use crate::component::status::Totals;
use crate::theme::Theme;

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
///
/// Four taller again for P17's `Plan limits` section (**D485**), by the same
/// trade. The two additions never both cost their maximum: the section and the
/// honest-absence tail are mutually exclusive — a session that captured plan
/// windows draws the section and no tail, and one that captured none draws the
/// tail and no section.
const MAX_HEIGHT: u16 = 30;

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

/// How many plan windows the `Plan limits` section draws before it starts
/// counting the rest — [`RATE_ROWS`]'s number, for [`RATE_ROWS`]'s reason.
///
/// Two is also exactly what the codex family sends per limit (its 5h and
/// weekly windows), so the common case is never truncated at all.
const PLAN_ROWS: usize = 2;

/// The honest lines where Claude Code draws its plan-limit meters and this
/// session's credential served none.
///
/// **Narrowed twice.** P16 (**D484**) took the rate-limit windows out of its
/// scope; P17 (**D485**) took the plan meters themselves out of it for the
/// credentials that *do* serve them, leaving this to say what the W-A1 probe
/// of 2026-08-14 found still silent, and why — one sentence per reason, with
/// the vendor's own documentation named where the reason is a credential tier
/// rather than an absent feature. Rendered only when no plan window was
/// captured: a panel that drew a real meter and then denied having one would
/// be less honest than either half alone.
///
/// Kept under seventy columns a line, because the panel is as narrow as the
/// terminal is and a clipped URL is not a shorter URL — it is a wrong one.
const NO_PLAN_LIMITS: [&str; 3] = [
    "plan limits unavailable on this credential (probed 2026-08-14):",
    "  an openai API key's backend sends none; anthropic's needs an",
    "  Admin key \u{2014} platform.claude.com/docs/en/manage-claude/usage-cost-api",
];

/// What the `Current window` section says when a bucket's own reset has
/// passed. The vendor said a true thing at a moment that is over; renaming it
/// is the whole staleness guard (P16 pre-mortem 4).
const EXPIRED: &str = "expired \u{2014} refreshes on the next request";

/// What the same section says when a bucket's vendor sent no reset at all
/// (**D484** as P22 amended it: grok's `x-ratelimit-*` carry the counts and no
/// clock).
///
/// An em-dash where the instant would be, rather than a manufactured one or a
/// borrowed [`EXPIRED`]: nothing dated this window, so nothing may say it has
/// gone stale and nothing may say when it comes back.
const NO_RESET: &str = "resets: \u{2014}";

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
    /// The vendor's own plan buckets, as `Engine::plan_windows` last answered
    /// (**D485**). Empty renders no `Plan limits` section and the honest
    /// `NO_PLAN_LIMITS` tail instead; non-empty renders the section and no
    /// tail. Never both, and never a section over nothing.
    pub plans: Vec<PlanWindow>,
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
                    usage.reasoning_tokens =
                        usage.reasoning_tokens.saturating_add(turn.usage.reasoning_tokens);
                    usage.cache_read_tokens =
                        usage.cache_read_tokens.saturating_add(turn.usage.cache_read_tokens);
                    usage.cache_write_tokens =
                        usage.cache_write_tokens.saturating_add(turn.usage.cache_write_tokens);
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
        lines.push(Line::styled(clip("  Usage by model:", inner_width), theme.fg));
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

        // Plan limits: the meters Claude's panel leads with, over the header
        // families the W-A1 probe confirmed (**D485**). Absent entirely when
        // this credential served none, for the `Current window` section's own
        // reason — and the honest tail at the foot is what speaks instead.
        if !self.data.plans.is_empty() {
            let now = self.data.now.unwrap_or_else(std::time::SystemTime::now);
            // Tightest first, as below: the budget that runs out first is the
            // one somebody opening this panel is asking about.
            let mut plans: Vec<_> = self.data.plans.iter().collect();
            plans.sort_by(|left, right| right.used().total_cmp(&left.used()));

            lines.push(Line::styled(clip("Plan limits", inner_width), header));
            for plan in plans.iter().take(PLAN_ROWS) {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(Self::bar(plan.used(), theme));
                spans.push(Span::styled(format!(" {}", plan_label(plan, now)), theme.fg));
                lines.push(Line::from(spans));
            }
            if let Some(rest) = plans.len().checked_sub(PLAN_ROWS).filter(|rest| *rest > 0) {
                lines.push(Line::styled(
                    clip(&format!("  ({rest} roomier plan windows not shown)"), inner_width),
                    theme.dim,
                ));
            }
            lines.push(Line::raw(""));
        }

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
                        match window.reset {
                            Some(_) if window.expired(now) => EXPIRED.to_owned(),
                            Some(reset) => format!(
                                "resets in {}",
                                compact_duration(reset.duration_since(now).unwrap_or_default())
                            ),
                            // A vendor that sent no reset at all (grok's
                            // shape, D484 as P22 amended it): an empty slot
                            // says the clock is missing without inventing one,
                            // and [`EXPIRED`] cannot fire for a window nobody
                            // dated. `plan_label`'s three arms, on the sibling
                            // shape — with its own wording, because this row's
                            // other clause is already `resets in`.
                            None => NO_RESET.to_owned(),
                        },
                    ),
                    theme.fg,
                ));
                lines.push(Line::from(spans));
            }
            if let Some(rest) = windows.len().checked_sub(RATE_ROWS).filter(|rest| *rest > 0) {
                lines.push(Line::styled(
                    clip(&format!("  ({rest} roomier windows not shown)"), inner_width),
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
            None => lines.push(Line::styled(clip("  nothing sent yet", inner_width), theme.dim)),
        }
        lines.push(Line::raw(""));

        // Turns: the inspector's rows, newest kept when the room runs out —
        // a ganja addition Claude's panel has no equivalent of.
        lines.push(Line::styled(clip("Turns", inner_width), header));
        if self.data.turns.is_empty() {
            lines.push(Line::styled(clip("  no finished turns yet", inner_width), theme.dim));
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
            // Header lines above plus the honest-absence tail and hints
            // below — measured rather than assumed, since P17 made the tail's
            // own height depend on whether a plan meter was drawn (**D485**).
            let fixed = lines.len() + 2 + tail_height(&self.data.plans);
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
                    clip(&format!("  ({skipped} earlier turns not shown)"), inner_width),
                    theme.dim,
                ));
            }
        }
        lines.push(Line::raw(""));
        // No blank between the tail and the hints, where P16 had one: three
        // lines where there was one would otherwise cost this modal two rows,
        // and on a terminal that caps it the row it loses is the footer
        // explaining the panel — the trade [`MAX_HEIGHT`]'s doc refuses.
        if self.data.plans.is_empty() {
            for line in NO_PLAN_LIMITS {
                lines.push(Line::styled(clip(line, inner_width), theme.dim));
            }
        }
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

        let height =
            u16::try_from(lines.len().saturating_add(2)).unwrap_or(available).min(available);
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" usage "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }
}

/// How many rows the panel's foot costs: the blank before it, the hints, and
/// the honest-absence tail when it is drawn at all (**D485**).
fn tail_height(plans: &[PlanWindow]) -> usize {
    if plans.is_empty() { NO_PLAN_LIMITS.len() } else { 0 }
}

/// What one plan window says after its bar: what it is, how long its window
/// runs, and when it comes back.
///
/// Every clause is conditional because every one of them is optional on the
/// wire. A window whose vendor sent no reset says exactly that rather than
/// borrowing the expired phrasing — nothing dated it, so nothing has gone
/// stale (**D485**).
fn plan_label(plan: &PlanWindow, now: std::time::SystemTime) -> String {
    let mut label = plan.name.clone();
    if let Some(limit) = &plan.limit_name {
        label.push_str(&format!(" ({limit})"));
    }

    let mut clauses = Vec::new();
    if let Some(minutes) = plan.window_minutes {
        clauses.push(format!("{} window", compact_minutes(minutes)));
    }
    clauses.push(match plan.resets_at {
        Some(_) if plan.expired(now) => EXPIRED.to_owned(),
        Some(reset) => {
            format!("resets in {}", compact_duration(reset.duration_since(now).unwrap_or_default()))
        }
        None => "no reset reported".to_owned(),
    });

    format!("{label} \u{b7} {}", clauses.join(", "))
}

/// A rolling window's length in the unit a person thinks about it in: `5h` for
/// the short bucket, `7d` for the weekly one, plain minutes under an hour.
///
/// Its own spelling rather than [`compact_duration`]'s, which is pinned to the
/// session row's `3h 9m` shape and would render a weekly window as `168h 0m`.
fn compact_minutes(minutes: u64) -> String {
    match minutes {
        0..60 => format!("{minutes}m"),
        60..1_440 => match (minutes / 60, minutes % 60) {
            (hours, 0) => format!("{hours}h"),
            (hours, rest) => format!("{hours}h {rest}m"),
        },
        _ => match (minutes / 1_440, (minutes % 1_440) / 60) {
            (days, 0) => format!("{days}d"),
            (days, hours) => format!("{days}d {hours}h"),
        },
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
#[path = "usage_tests.rs"]
mod tests;
