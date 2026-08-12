//! The `/usage` dialog: what this session has spent, in Claude Code's panel
//! language — section headers and horizontal meters — over what ganja
//! actually holds: the status bar's own running totals, the per-turn rows the
//! Ctrl+T inspector already keeps, the context estimate the bar's meter
//! polls, and the cache splits the session record accumulates.
//!
//! **D471** (`slash-usage`): upstream opencode has no such surface at all, so
//! nothing here cites an upstream file. Claude Code's panel leads with
//! subscription plan-limit meters — the 5h and weekly buckets — which ride a
//! vendor usage API ganja does not speak; those rows are **explicitly
//! absent**, with one honest line naming why, rather than drawn empty or
//! faked (the plan's honest-degradation rule, ruled sufficient for this
//! phase in Open question 2). The meter shape is the HUD statusline's own
//! pinned `[####----]` ASCII bar, not Claude's shaded blocks, so one build
//! draws its meters one way.
//!
//! The totals line **is** [`Totals::segment`] — the status bar's exact
//! string, not a second formatting of the same numbers (AC5) — and the
//! per-turn table reuses the inspector's [`TurnUsage`] rows; its few
//! formatting columns are deliberately duplicated here rather than shared,
//! because the inspector is another lane's frozen surface and a helper
//! carved out of it for four format strings would couple two dialogs that
//! merely look alike.
//!
//! A snapshot, not a view: everything is read when the dialog opens. Esc
//! closes it; [`crate::app::App`] owns that key, the same split every other
//! dialog keeps.

use ganja_core::catalog::{self, compact_tokens};
use ganja_protocol::Usage as TokenUsage;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};

use crate::{
    component::{chat::clip, inspector::TurnUsage, status::Totals},
    theme::Theme,
};

/// Widest the modal grows, the house width every dialog here uses.
const MAX_WIDTH: u16 = 76;

/// Tallest the modal grows: enough for the sections and a handful of turn
/// rows; a longer session keeps its newest rows, the ones a person opening
/// the panel is asking about.
const MAX_HEIGHT: u16 = 24;

/// Columns inside a meter's brackets, the HUD statusline's own bar width.
const METER_WIDTH: usize = 20;

/// The key hint at the dialog's foot.
const HINTS: &str = "[Esc] close";

/// The one honest line where Claude Code draws its plan-limit meters: ganja
/// speaks no vendor usage API, so there is nothing true to draw (D471).
const NO_PLAN_LIMITS: &str = "plan limits (5h/weekly) unavailable: no vendor usage API";

/// Column widths of the per-turn table, matching the inspector's tab so the
/// same rows read the same in both places.
const ID_WIDTH: usize = 10;
const COUNT_WIDTH: usize = 8;

/// Everything the dialog shows, read once when `/usage` opens.
#[derive(Clone, Debug, Default)]
pub struct Data {
    /// The status bar's running totals — rendered through their own
    /// formatter, so the two surfaces cannot disagree (AC5).
    pub totals: Totals,
    /// The session's accumulated splits: cache traffic and reasoning, which
    /// the running totals collapse.
    pub splits: TokenUsage,
    /// `(estimated tokens, window)`, absent for an uncataloged model — the
    /// same pair the status bar's context meter polls.
    pub context: Option<(u64, u64)>,
    /// One row per finished turn, the inspector's own rows.
    pub turns: Vec<TurnUsage>,
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

    /// The HUD statusline's pinned meter shape: `[####----] NN%`.
    fn meter(fraction: f64) -> String {
        let filled = ((fraction * METER_WIDTH as f64).round() as usize).min(METER_WIDTH);

        format!(
            "[{}{}] {:.0}%",
            "#".repeat(filled),
            "-".repeat(METER_WIDTH - filled),
            fraction * 100.0
        )
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

        let mut lines = Vec::new();

        // Session: the status bar's own segment string, then the splits it
        // collapses.
        lines.push(Line::styled(clip("Session", inner_width), theme.accent));
        lines.push(Line::styled(
            clip(&format!("  {}", self.data.totals.segment()), inner_width),
            theme.fg,
        ));
        let splits = &self.data.splits;
        lines.push(Line::styled(
            clip(
                &format!(
                    "  input {} \u{b7} output {} \u{b7} reasoning {} \u{b7} cache read {} \u{b7} cache write {}",
                    compact_tokens(splits.input_tokens),
                    compact_tokens(splits.output_tokens),
                    compact_tokens(splits.reasoning_tokens),
                    compact_tokens(splits.cache_read_tokens),
                    compact_tokens(splits.cache_write_tokens),
                ),
                inner_width,
            ),
            theme.dim,
        ));
        lines.push(Line::raw(""));

        // Context: the same estimate the status bar's meter polls, or the
        // same honest absence.
        lines.push(Line::styled(clip("Context", inner_width), theme.accent));
        match self.data.context {
            Some((tokens, window)) => {
                let fraction = (tokens as f64 / window.max(1) as f64).min(1.0);
                lines.push(Line::styled(
                    clip(
                        &format!(
                            "  {} of {} tokens \u{2014} estimated",
                            Self::meter(fraction),
                            compact_tokens(window),
                        ),
                        inner_width,
                    ),
                    theme.fg,
                ));
            }
            None => lines.push(Line::styled(
                clip("  unsized model \u{2014} no window to meter", inner_width),
                theme.dim,
            )),
        }
        lines.push(Line::raw(""));

        // Cache: what the splits above bought.
        lines.push(Line::styled(clip("Cache", inner_width), theme.accent));
        match self.cache_hit_rate() {
            Some(rate) => lines.push(Line::styled(
                clip(
                    &format!("  {} of input read from cache", Self::meter(rate)),
                    inner_width,
                ),
                theme.fg,
            )),
            None => lines.push(Line::styled(
                clip("  nothing sent yet", inner_width),
                theme.dim,
            )),
        }
        lines.push(Line::raw(""));

        // Turns: the inspector's rows, newest kept when the room runs out.
        lines.push(Line::styled(clip("Turns", inner_width), theme.accent));
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
                let cost = catalog::model(&row.model)
                    .map(|model| catalog::cost(&row.usage, &model).total_usd);
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
                            cost.map_or_else(|| "-".to_owned(), |dollars| format!("${dollars:.4}")),
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

/// The last few characters of a message id — the counter half is the half
/// that tells two ids minted moments apart apart, the inspector's own rule.
fn short_id(id: &ganja_protocol::MessageId) -> String {
    let raw = id.as_str();

    raw.get(raw.len().saturating_sub(8)..)
        .unwrap_or(raw)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use ganja_protocol::Message;
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Data, TokenUsage, TurnUsage, Usage};
    use crate::{component::status::Totals, theme::Theme};

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 26,
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
        }
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

    /// AC5's core: the session line is the status bar's own segment string,
    /// byte for byte, not a second formatting of the same numbers.
    #[test]
    fn the_session_line_is_the_status_bars_own_segment_string() {
        let data = data();
        let segment = data.totals.segment();

        let screen = rendered(&Usage::new(data), AREA);
        assert!(screen.contains(&segment), "want {segment:?} in:\n{screen}");
    }

    #[test]
    fn every_split_and_the_turn_table_are_shown() {
        let screen = rendered(&Usage::new(data()), AREA);

        for section in ["Session", "Context", "Cache", "Turns"] {
            assert!(screen.contains(section), "{section} missing:\n{screen}");
        }
        for label in ["reasoning 5", "cache read 6", "cache write 7"] {
            assert!(screen.contains(label), "{label} missing:\n{screen}");
        }
        assert!(
            screen.contains("estimated"),
            "the context meter says estimated:\n{screen}"
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
    }

    #[test]
    fn an_empty_session_names_its_own_empty_states() {
        let screen = rendered(&Usage::new(Data::default()), AREA);

        assert!(screen.contains("nothing sent yet"), "got:\n{screen}");
        assert!(screen.contains("no finished turns yet"), "got:\n{screen}");
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
