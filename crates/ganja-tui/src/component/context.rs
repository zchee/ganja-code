//! The `/context` dialog: what fills the model's context window, category by
//! category, as a colored cell grid over the window with a legend of token
//! counts and percentages — Claude Code's own visualization over
//! [`ContextBreakdown`], the engine's on-demand estimate.
//!
//! **D470** (`slash-context`): upstream opencode has no such surface at all —
//! no slash command, no panel — so nothing here cites an upstream file. The
//! rendering target is Claude Code's `/context` grid (cells per category over
//! the window, a legend with counts and percentages, an autocompact-reserve
//! row); the *chrome* — the bordered, centered modal — is the house shape
//! every dialog here already uses ([`crate::component::mcp`]), because Claude
//! Code's exact glyphs and colors are not pinned by anything this port holds.
//!
//! The panel says **estimated** where Claude Code says it (P14 pre-mortem 2):
//! these are the compaction estimator's chars-per-token figures, not a
//! provider's bill, and a panel that looked authoritative would earn the bug
//! report the plan warned about. The one exception rides in from the engine —
//! the conversation share prefers usage-event actuals where a turn reported
//! them — and changes nothing about the label: the whole is still an
//! estimate.
//!
//! A snapshot, not a view: the breakdown is read when the dialog opens and
//! never re-polled — the measure only moves when a turn lands, and a person
//! reading a legend mid-turn is better served by numbers that hold still.
//! Esc closes it; [`crate::app::App`] owns that key, the same split every
//! other dialog keeps.

use ganja_core::{catalog::compact_tokens, engine::ContextBreakdown};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};

use crate::{component::chat::clip, theme::Theme};

/// Widest the modal grows, the house width every dialog here uses.
const MAX_WIDTH: u16 = 76;

/// Cells in the grid: one per percent of the window.
const CELLS: usize = 100;

/// Cells per grid row — ten rows of ten, each cell one percent.
const CELLS_PER_ROW: usize = 10;

/// The glyph a used category's cells are drawn with.
const USED_CELL: &str = "█";

/// The glyph free space is drawn with — visibly empty, still a cell.
const FREE_CELL: &str = "░";

/// The glyph the autocompact reserve is drawn with: held back rather than
/// free, so neither the used block nor the empty one.
const RESERVE_CELL: &str = "▒";

/// The key hint at the dialog's foot.
const HINTS: &str = "[Esc] close";

/// One legend row: a category, what it holds, and how it is painted.
struct Row {
    /// The cell glyph this category's grid cells and legend marker use.
    cell: &'static str,
    /// The legend label.
    label: &'static str,
    /// Estimated tokens.
    tokens: u64,
    /// Which theme style paints it, resolved at render time so the dialog
    /// carries no `Style` of its own.
    paint: fn(&Theme) -> Style,
}

/// The dialog itself: the breakdown as it stood when `/context` was typed,
/// and the model it describes.
#[derive(Clone, Debug)]
pub struct Context {
    model: String,
    breakdown: ContextBreakdown,
}

impl Context {
    /// Opens the dialog over `breakdown`, naming `model` in the header.
    #[must_use]
    pub fn new(model: String, breakdown: ContextBreakdown) -> Self {
        Self { model, breakdown }
    }

    /// The used categories, in the order the grid fills and the legend lists
    /// them. Categories the accessor split apart stay split — builtin and MCP
    /// tools are different costs with different remedies — while the two
    /// conversation roles fold into Claude Code's one "Messages" row, whose
    /// panel this mirrors.
    fn used(&self) -> Vec<Row> {
        vec![
            Row {
                cell: USED_CELL,
                label: "System prompt",
                tokens: self.breakdown.system_prompt,
                paint: |theme| theme.primary,
            },
            Row {
                cell: USED_CELL,
                label: "Instruction files",
                tokens: self.breakdown.instructions,
                paint: |theme| theme.secondary,
            },
            Row {
                cell: USED_CELL,
                label: "Builtin tools",
                tokens: self.breakdown.tools_builtin,
                paint: |theme| theme.info,
            },
            Row {
                cell: USED_CELL,
                label: "MCP tools",
                tokens: self.breakdown.tools_mcp,
                paint: |theme| theme.warning,
            },
            Row {
                cell: USED_CELL,
                label: "Skills",
                tokens: self.breakdown.skills,
                paint: |theme| theme.success,
            },
            Row {
                cell: USED_CELL,
                label: "Messages",
                tokens: self
                    .breakdown
                    .conversation_user
                    .saturating_add(self.breakdown.conversation_assistant),
                paint: |theme| theme.accent,
            },
        ]
    }

    /// Every legend row, the free-space and reserve rows included when the
    /// window is sized. The free row is `ContextBreakdown::free` — window −
    /// used − reserve — and the reserve is the accessor's own, never
    /// re-derived here (AC4).
    fn legend(&self) -> Vec<Row> {
        let mut rows = self.used();
        if let (Some(free), Some(reserve)) = (self.breakdown.free(), self.breakdown.reserve) {
            rows.push(Row {
                cell: FREE_CELL,
                label: "Free space",
                tokens: free,
                paint: |theme| theme.dim,
            });
            rows.push(Row {
                cell: RESERVE_CELL,
                label: "Autocompact reserve",
                tokens: reserve,
                paint: |theme| theme.dim,
            });
        }

        rows
    }

    /// How many of the grid's hundred cells each legend row paints, by
    /// largest remainder so they always sum to exactly [`CELLS`]: a grid with
    /// a hole in it would read as a bug, and one cell too many would claim
    /// context that does not exist.
    fn cells(&self, window: u64) -> Vec<usize> {
        let rows = self.legend();
        let total: u64 = window.max(1);

        let mut counts: Vec<(usize, usize, u64)> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let scaled = row.tokens.saturating_mul(CELLS as u64);
                let floor = usize::try_from(scaled / total).unwrap_or(CELLS);
                (index, floor, scaled % total)
            })
            .collect();

        let assigned: usize = counts.iter().map(|(_, floor, _)| floor).sum();
        let mut missing = CELLS.saturating_sub(assigned);
        // Largest remainder first; ties break on category order so the same
        // breakdown always draws the same grid.
        counts.sort_by(|left, right| right.2.cmp(&left.2).then(left.0.cmp(&right.0)));
        for row in &mut counts {
            if missing == 0 {
                break;
            }
            row.1 += 1;
            missing -= 1;
        }
        counts.sort_by_key(|(index, ..)| *index);

        counts.into_iter().map(|(_, cells, _)| cells).collect()
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let inner_width = usize::from(width).saturating_sub(2);

        let mut lines = match self.breakdown.window {
            Some(window) => self.sized_lines(window, inner_width, theme),
            None => self.degraded_lines(inner_width, theme),
        };
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(HINTS, inner_width), theme.dim));

        let height = u16::try_from(lines.len().saturating_add(2))
            .unwrap_or(u16::MAX)
            .min(area.height.saturating_sub(2).max(1));
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" context "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// The cataloged rendering: header, grid, legend with percentages.
    fn sized_lines(&self, window: u64, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let total = self.breakdown.total();
        let percent = (total as f64) * 100.0 / (window.max(1) as f64);
        let mut lines = vec![
            Line::styled(
                clip(
                    &format!(
                        "{} \u{b7} {} of {} tokens ({percent:.0}%) \u{2014} estimated",
                        self.model,
                        compact_tokens(total),
                        compact_tokens(window),
                    ),
                    width,
                ),
                theme.fg,
            ),
            Line::raw(""),
        ];

        // The grid: a hundred cells, ten to a row, painted in legend order.
        let rows = self.legend();
        let cells = self.cells(window);
        let mut flat: Vec<(&'static str, Style)> = Vec::with_capacity(CELLS);
        for (row, count) in rows.iter().zip(&cells) {
            for _ in 0..*count {
                flat.push((row.cell, (row.paint)(theme)));
            }
        }
        for chunk in flat.chunks(CELLS_PER_ROW) {
            let mut spans = vec![Span::raw("  ")];
            for (cell, style) in chunk {
                spans.push(Span::styled((*cell).repeat(2), *style));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));

        for row in rows {
            let share = (row.tokens as f64) * 100.0 / (window.max(1) as f64);
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", row.cell), (row.paint)(theme)),
                Span::styled(
                    clip(
                        &format!(
                            "{:<20} {:>8} tokens ({share:.1}%)",
                            row.label,
                            compact_tokens(row.tokens),
                        ),
                        width.saturating_sub(4),
                    ),
                    theme.fg,
                ),
            ]));
        }

        lines
    }

    /// The uncataloged rendering: totals alone, and the honest sentence —
    /// only the catalog can size a window, so no denominator is invented.
    fn degraded_lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled(
                clip(
                    &format!(
                        "{} \u{b7} {} tokens \u{2014} estimated",
                        self.model,
                        compact_tokens(self.breakdown.total()),
                    ),
                    width,
                ),
                theme.fg,
            ),
            Line::styled(
                clip("unsized model \u{2014} percentages unavailable", width),
                theme.dim,
            ),
            Line::raw(""),
        ];

        for row in self.used() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", row.cell), (row.paint)(theme)),
                Span::styled(
                    clip(
                        &format!("{:<20} {:>8} tokens", row.label, compact_tokens(row.tokens)),
                        width.saturating_sub(4),
                    ),
                    theme.fg,
                ),
            ]));
        }
        lines.push(Line::styled(
            clip(
                &format!(
                    "  {:<22} {:>8} tokens",
                    "total",
                    compact_tokens(self.breakdown.total())
                ),
                width,
            ),
            theme.accent,
        ));

        lines
    }
}

#[cfg(test)]
mod tests {
    use ganja_core::engine::ContextBreakdown;
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{CELLS, Context};
    use crate::theme::Theme;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 30,
    };

    /// A breakdown with something in every category over a small round
    /// window, so shares are easy to reason about by hand.
    fn sized() -> ContextBreakdown {
        ContextBreakdown {
            system_prompt: 3_000,
            instructions: 2_000,
            tools_builtin: 11_000,
            tools_mcp: 1_000,
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
        let dialog = Context::new("claude-sonnet-5".to_owned(), breakdown);

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
    /// the shares, the cells sum to exactly one hundred.
    #[test]
    fn the_grid_always_paints_exactly_one_hundred_cells() {
        for breakdown in [
            sized(),
            ContextBreakdown {
                system_prompt: 1,
                window: Some(1_000_000),
                reserve: Some(100_000),
                ..ContextBreakdown::default()
            },
        ] {
            let dialog = Context::new("m".to_owned(), breakdown);
            let cells: usize = dialog
                .cells(breakdown.window.expect("both fixtures are sized"))
                .iter()
                .sum();
            assert_eq!(cells, CELLS, "{breakdown:?}");
        }
    }

    /// The panel carries the word Claude Code carries: these are estimates,
    /// and the header says so in both renderings (P14 pre-mortem 2).
    #[test]
    fn both_renderings_say_estimated() {
        let sized = rendered(&Context::new("m".to_owned(), sized()), AREA);
        assert!(sized.contains("estimated"), "got:\n{sized}");

        let degraded = rendered(&Context::new("m".to_owned(), unsized_model()), AREA);
        assert!(degraded.contains("estimated"), "got:\n{degraded}");
    }

    #[test]
    fn a_sized_window_renders_the_grid_and_every_legend_row() {
        let screen = rendered(&Context::new("claude-sonnet-5".to_owned(), sized()), AREA);

        for label in [
            "System prompt",
            "Instruction files",
            "Builtin tools",
            "MCP tools",
            "Skills",
            "Messages",
            "Free space",
            "Autocompact reserve",
        ] {
            assert!(screen.contains(label), "{label} missing:\n{screen}");
        }
        assert!(screen.contains('█'), "the grid draws used cells:\n{screen}");
        assert!(screen.contains('░'), "and free ones:\n{screen}");
        assert!(screen.contains('▒'), "and the reserve:\n{screen}");
        assert!(
            screen.contains("100.0k tokens"),
            "the header names the window:\n{screen}"
        );
    }

    /// The degraded panel: totals, the honest sentence, and no invented
    /// percentages anywhere.
    #[test]
    fn an_unsized_model_renders_totals_and_the_honest_sentence() {
        let screen = rendered(&Context::new("fake-1".to_owned(), unsized_model()), AREA);

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

            Context::new("m".to_owned(), sized()).render(area, &mut buffer, &Theme::default());
        }
    }
}
