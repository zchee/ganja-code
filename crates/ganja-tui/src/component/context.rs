//! The `/context` dialog: what fills the model's context window, category by
//! category, as a colored cell grid over the window with a legend of token
//! counts and percentages — Claude Code's own visualization over
//! [`ContextBreakdown`], the engine's on-demand estimate.
//!
//! **D470** (`slash-context`): upstream opencode has no such surface at all —
//! no slash command, no panel — so nothing here cites an upstream file. The
//! rendering is pinned by real Claude Code screenshots (2026-08-12,
//! transcribed in the P14 W4f charter): a `Context Usage` title, a 10×20 grid
//! of single-character cells — stacked discs (U+26C1) per used category, a
//! hollow square (U+26F6) for free space — with the model, the window line
//! and the category legend in a column beside it. The *chrome* — the
//! bordered, centered modal — stays the house shape every dialog here uses
//! ([`crate::component::mcp`]). Two honest divergences from the screenshot:
//! the autocompact reserve renders as its own legend row because ganja really
//! holds one back (its grid cells draw as free — the screenshot distinguishes
//! only used from free), and the per-category detail sections ("193 tools ·
//! 518 tokens") are absent because the breakdown carries no item counts to
//! print. The screenshot's `/context all to expand` footer is not copied:
//! ganja has no expand mode, and a hint must never name an affordance that
//! does not exist.
//!
//! The panel says **estimated** where Claude Code says it (P14 pre-mortem 2):
//! in the `Estimated usage by category` legend header. These are the
//! compaction estimator's chars-per-token figures, not a provider's bill, and
//! a panel that looked authoritative would earn the bug report the plan
//! warned about. The one exception rides in from the engine — the
//! conversation share prefers usage-event actuals where a turn reported
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
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{component::chat::clip, theme::Theme};

/// Widest the modal grows. Wider than the house 76: the pinned side-by-side
/// layout — a [`GRID_WIDTH`]-column grid beside legend rows that run to ~46
/// columns — needs ~90 columns of body, and capping at the house width would
/// leave the pinned layout unreachable on any terminal.
const MAX_WIDTH: u16 = 96;

/// Cells in the grid — the screenshot's two hundred, half a percent each.
const CELLS: usize = 200;

/// Cells per grid row — ten rows of twenty.
const CELLS_PER_ROW: usize = 20;

/// Columns one grid row occupies: single-character cells, one space apart.
const GRID_WIDTH: usize = CELLS_PER_ROW * 2 - 1;

/// Columns between the grid and the column seated beside it.
const GRID_GAP: usize = 2;

/// The glyph a used category's cells are drawn with: the screenshot's
/// stacked discs.
const USED_CELL: &str = "\u{26c1}";

/// The glyph free space is drawn with: the screenshot's hollow
/// four-corners square — visibly empty, still a cell.
const FREE_CELL: &str = "\u{26f6}";

/// The key hint at the dialog's foot.
const HINTS: &str = "[Esc] close";

/// One legend row: a category, what it holds, and how it is painted.
struct Row {
    /// The cell glyph this category's grid cells and legend bullet use.
    cell: &'static str,
    /// The legend label.
    label: &'static str,
    /// Estimated tokens.
    tokens: u64,
    /// Which theme style paints it, resolved at render time so the dialog
    /// carries no `Style` of its own.
    paint: fn(&Theme) -> Style,
    /// Whether the legend row says "tokens" after its count — the
    /// screenshot's free-space row alone drops the word.
    counted_in_tokens: bool,
}

impl Row {
    /// The legend row's text after its bullet, in the screenshot's shape:
    /// `Name: <count> tokens (N.N%)`.
    fn legend_text(&self, window: u64) -> String {
        let share = (self.tokens as f64) * 100.0 / (window.max(1) as f64);
        let unit = if self.counted_in_tokens {
            " tokens"
        } else {
            ""
        };

        format!(
            "{}: {}{unit} ({share:.1}%)",
            self.label,
            compact_tokens(self.tokens)
        )
    }
}

/// One row of the column beside the grid: an optional colored bullet, then
/// text in one style.
struct Beside {
    bullet: Option<(&'static str, Style)>,
    text: String,
    style: Style,
}

impl Beside {
    /// Display columns the row wants, bullet and its trailing space included.
    fn width(&self) -> usize {
        self.text.width() + if self.bullet.is_some() { 2 } else { 0 }
    }

    /// The row as spans, its text clipped to `width`.
    fn spans(&self, width: usize) -> Vec<Span<'static>> {
        let mut spans = Vec::with_capacity(2);
        let mut room = width;
        if let Some((glyph, style)) = self.bullet {
            spans.push(Span::styled(format!("{glyph} "), style));
            room = room.saturating_sub(2);
        }
        spans.push(Span::styled(clip(&self.text, room), self.style));

        spans
    }
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
    /// them — the screenshot's order and, where the mapping is honest, its
    /// vocabulary: "System tools" is ganja's builtin tool schemas, "Memory
    /// files" its `AGENTS.md` instruction family. Categories the accessor
    /// split apart stay split, while the two conversation roles fold into
    /// Claude Code's one "Messages" row.
    fn used(&self) -> Vec<Row> {
        vec![
            Row {
                cell: USED_CELL,
                label: "System prompt",
                tokens: self.breakdown.system_prompt,
                paint: |theme| theme.primary,
                counted_in_tokens: true,
            },
            Row {
                cell: USED_CELL,
                label: "System tools",
                tokens: self.breakdown.tools_builtin,
                paint: |theme| theme.info,
                counted_in_tokens: true,
            },
            Row {
                cell: USED_CELL,
                label: "MCP tools",
                tokens: self.breakdown.tools_mcp,
                paint: |theme| theme.warning,
                counted_in_tokens: true,
            },
            Row {
                cell: USED_CELL,
                label: "Memory files",
                tokens: self.breakdown.instructions,
                paint: |theme| theme.secondary,
                counted_in_tokens: true,
            },
            Row {
                cell: USED_CELL,
                label: "Skills",
                tokens: self.breakdown.skills,
                paint: |theme| theme.success,
                counted_in_tokens: true,
            },
            Row {
                cell: USED_CELL,
                label: "Messages",
                tokens: self
                    .breakdown
                    .conversation_user
                    .saturating_add(self.breakdown.conversation_assistant),
                paint: |theme| theme.accent,
                counted_in_tokens: true,
            },
        ]
    }

    /// Every legend row, the reserve and free-space rows included when the
    /// window is sized — free last, the screenshot's own order. The free row
    /// is `ContextBreakdown::free` — window − used − reserve — and the
    /// reserve is the accessor's own, never re-derived here (AC4). Both draw
    /// their grid cells with the free glyph — the screenshot distinguishes
    /// only used from free — but the reserve keeps a legend row of its own
    /// because ganja really holds those tokens back.
    fn legend(&self) -> Vec<Row> {
        let mut rows = self.used();
        if let (Some(free), Some(reserve)) = (self.breakdown.free(), self.breakdown.reserve) {
            rows.push(Row {
                cell: FREE_CELL,
                label: "Autocompact reserve",
                tokens: reserve,
                paint: |theme| theme.dim,
                counted_in_tokens: true,
            });
            rows.push(Row {
                cell: FREE_CELL,
                label: "Free space",
                tokens: free,
                paint: |theme| theme.dim,
                // The screenshot's free row alone drops the word "tokens".
                counted_in_tokens: false,
            });
        }

        rows
    }

    /// How many of the grid's cells each legend row paints, by largest
    /// remainder so they always sum to exactly [`CELLS`]: a grid with a hole
    /// in it would read as a bug, and one cell too many would claim context
    /// that does not exist.
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

    /// The cataloged rendering: the pinned title, the grid, and the column
    /// beside it — model, window line, and the category legend. The column
    /// sits to the grid's right when the panel is wide enough for its widest
    /// row and drops below the grid when it is not (the house width at an
    /// 80-column terminal cannot seat a 39-column grid beside a ~45-column
    /// legend, and clipping every legend row would pin garbage).
    fn sized_lines(&self, window: u64, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let bold = theme.fg.add_modifier(Modifier::BOLD);
        let total = self.breakdown.total();
        let percent = (total as f64) * 100.0 / (window.max(1) as f64);

        let rows = self.legend();
        let cells = self.cells(window);

        // The column beside the grid. The screenshot stacks a display name
        // over a dim model id; ganja holds one string, so it renders once.
        let mut beside = vec![
            Beside {
                bullet: None,
                text: self.model.clone(),
                style: bold,
            },
            Beside {
                bullet: None,
                text: format!(
                    "{}/{} tokens ({percent:.0}%)",
                    compact_tokens(total),
                    compact_tokens(window),
                ),
                style: theme.fg,
            },
            Beside {
                bullet: None,
                text: String::new(),
                style: theme.fg,
            },
            Beside {
                bullet: None,
                text: "Estimated usage by category".to_owned(),
                style: theme.dim.add_modifier(Modifier::BOLD),
            },
        ];
        for row in &rows {
            beside.push(Beside {
                bullet: Some((row.cell, (row.paint)(theme))),
                text: row.legend_text(window),
                style: theme.fg,
            });
        }

        // The grid: two hundred cells, twenty to a row, painted in legend
        // order — used categories with their own glyph and color, reserve
        // and free with the hollow one.
        let mut flat: Vec<(&'static str, Style)> = Vec::with_capacity(CELLS);
        for (row, count) in rows.iter().zip(&cells) {
            for _ in 0..*count {
                flat.push((row.cell, (row.paint)(theme)));
            }
        }
        let grid: Vec<Vec<Span<'static>>> = flat
            .chunks(CELLS_PER_ROW)
            .map(|chunk| {
                let mut spans = Vec::with_capacity(chunk.len() * 2);
                for (index, (cell, style)) in chunk.iter().enumerate() {
                    if index > 0 {
                        spans.push(Span::raw(" "));
                    }
                    spans.push(Span::styled(*cell, *style));
                }
                spans
            })
            .collect();

        // No blank under the title: the stacked layout already runs to 26
        // body lines, and one more would push the close hint off the house
        // chat area at 80×36.
        let mut lines = vec![Line::styled("Context Usage", bold)];

        let widest = beside.iter().map(Beside::width).max().unwrap_or(0);
        if width >= GRID_WIDTH + GRID_GAP + widest {
            for index in 0..grid.len().max(beside.len()) {
                let mut spans = match grid.get(index) {
                    Some(row) => row.clone(),
                    None => vec![Span::raw(" ".repeat(GRID_WIDTH))],
                };
                if let Some(row) = beside.get(index) {
                    spans.push(Span::raw(" ".repeat(GRID_GAP)));
                    spans.extend(row.spans(width.saturating_sub(GRID_WIDTH + GRID_GAP)));
                }
                lines.push(Line::from(spans));
            }
        } else {
            lines.extend(grid.into_iter().map(Line::from));
            lines.push(Line::raw(""));
            lines.extend(beside.iter().map(|row| Line::from(row.spans(width))));
        }

        // The screenshot's per-category detail sections ("193 tools · 518
        // tokens") would follow here, full width — absent because the
        // breakdown carries no item counts to print.
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
            let dialog = Context::new("m".to_owned(), breakdown);
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
        let sized = rendered(&Context::new("m".to_owned(), sized()), WIDE);
        assert!(
            sized.contains("Estimated usage by category"),
            "got:\n{sized}"
        );

        let degraded = rendered(&Context::new("m".to_owned(), unsized_model()), NARROW);
        assert!(degraded.contains("estimated"), "got:\n{degraded}");
    }

    #[test]
    fn a_sized_window_renders_the_title_the_grid_and_every_legend_row() {
        let screen = rendered(&Context::new("claude-sonnet-5".to_owned(), sized()), WIDE);

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
        let screen = rendered(&Context::new("claude-sonnet-5".to_owned(), sized()), WIDE);

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
        let beside = rendered(&Context::new("claude-sonnet-5".to_owned(), sized()), WIDE);
        assert!(
            beside
                .lines()
                .any(|line| line.contains("\u{26f6} \u{26f6}") && line.contains("System prompt")),
            "wide panels seat the legend beside the grid:\n{beside}"
        );

        let stacked = rendered(&Context::new("claude-sonnet-5".to_owned(), sized()), NARROW);
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

    /// The degraded panel: totals, the honest sentence, and no invented
    /// percentages anywhere.
    #[test]
    fn an_unsized_model_renders_totals_and_the_honest_sentence() {
        let screen = rendered(&Context::new("fake-1".to_owned(), unsized_model()), NARROW);

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
