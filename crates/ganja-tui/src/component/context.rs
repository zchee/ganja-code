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
//! hollow square (U+26F6) for free space — with the model pair (catalog
//! display name bold over the dim model id; an uncataloged model renders its
//! id once), the window line and the category legend in a column beside it,
//! and per-category detail sections below the grid for the categories whose
//! item counts the breakdown carries (W7: the two tool categories — the
//! engine walks tools item by item, while the instruction and skill shares
//! are measured off one composed string, so no honest count exists for
//! them). The *chrome* — the bordered, centered modal — stays the house
//! shape every dialog here uses ([`crate::component::mcp`]). One honest
//! divergence from the screenshot: the autocompact reserve renders as its
//! own legend row because ganja really holds one back (its grid cells draw
//! as free — the screenshot distinguishes only used from free). The
//! screenshot's `/context all to expand` footer is not copied: ganja has no
//! expand mode, and a hint must never name an affordance that does not
//! exist.
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

/// The dialog itself: the breakdown as it stood when `/context` was typed —
/// which carries the model id it describes — and the catalog display name
/// the opener resolved for it, absent for an uncataloged model.
#[derive(Clone, Debug)]
pub struct Context {
    display: Option<String>,
    breakdown: ContextBreakdown,
}

impl Context {
    /// Opens the dialog over `breakdown`, with `display` the catalog's
    /// human name for [`ContextBreakdown::model`] — resolved by the caller
    /// rather than looked up here, so a test's rendering never depends on
    /// what the compiled-in catalog happens to hold.
    #[must_use]
    pub fn new(display: Option<String>, breakdown: ContextBreakdown) -> Self {
        Self { display, breakdown }
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
        // Body rows the popup can actually show: the popup is clamped to
        // `area.height - 2` below and its border eats two more. Passed into
        // the sized rendering so what does not fit is *dropped* — a panel
        // that instead clipped its own close hint would pin garbage (the
        // judgment W4f recorded when one extra line pushed the hint off the
        // house chat area).
        let room = usize::from(area.height.saturating_sub(4)).max(1);

        let mut lines = match self.breakdown.window {
            Some(window) => self.sized_lines(window, inner_width, room, theme),
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
    /// beside it — the model pair, window line, and the category legend —
    /// then the per-category detail sections, full width. The column sits to
    /// the grid's right when the panel is wide enough for its widest row and
    /// drops below the grid when it is not (the house width at an 80-column
    /// terminal cannot seat a 39-column grid beside a ~45-column legend, and
    /// clipping every legend row would pin garbage).
    ///
    /// `room` is the body rows the popup can show. What overflows it yields
    /// in fixed order — the detail sections first, then the pair's dim id
    /// line — so the close hint always survives; the numbers a person opened
    /// the panel for outrank the metadata around them.
    fn sized_lines(
        &self,
        window: u64,
        width: usize,
        room: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let bold = theme.fg.add_modifier(Modifier::BOLD);
        let total = self.breakdown.total();
        let percent = (total as f64) * 100.0 / (window.max(1) as f64);

        let rows = self.legend();
        let cells = self.cells(window);

        // The pinned pair: catalog display name bold over the dim model id.
        // No display name — an uncataloged model — renders the id once, and
        // a catalog name that *is* the id collapses the same way: two
        // identical lines would read as a stutter, not a pair.
        let id = self.breakdown.model.clone();
        let pair = self.display.as_ref().is_some_and(|name| *name != id);
        let mut beside = vec![Beside {
            bullet: None,
            text: self.display.clone().unwrap_or_else(|| id.clone()),
            style: bold,
        }];
        if pair {
            beside.push(Beside {
                bullet: None,
                text: id,
                style: theme.dim,
            });
        }
        beside.extend([
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
        ]);
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
        let layout = |beside: &[Beside]| -> Vec<Line<'static>> {
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
                lines.extend(grid.iter().cloned().map(Line::from));
                lines.push(Line::raw(""));
                lines.extend(beside.iter().map(|row| Line::from(row.spans(width))));
            }

            lines
        };

        // The blank-plus-hint tail `render` appends still has to fit.
        const TAIL: usize = 2;
        let mut lines = layout(&beside);
        if pair && lines.len() + TAIL > room {
            beside.remove(1);
            lines = layout(&beside);
        }
        let details = self.detail_lines(width, theme);
        if !details.is_empty() && lines.len() + details.len() + TAIL <= room {
            lines.extend(details);
        }

        lines
    }

    /// The pinned per-category detail sections, full width below the grid:
    /// a bold name with the dim ` · <hint>` naming ganja's own door or
    /// source — `/mcp` really exists here; Claude's `/memory` and agent-file
    /// fictions do not — then `└ <N> tools · <count> tokens`. Exactly the
    /// categories whose item counts the breakdown carries earn a section
    /// (W7: the two tool categories), and a zero count earns none — "0
    /// tools" is noise, not information.
    fn detail_lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let bold = theme.fg.add_modifier(Modifier::BOLD);
        let sections = [
            (
                "System tools",
                "builtin registry",
                self.breakdown.tools_builtin_count,
                self.breakdown.tools_builtin,
            ),
            (
                "MCP tools",
                "/mcp",
                self.breakdown.tools_mcp_count,
                self.breakdown.tools_mcp,
            ),
        ];

        let mut lines = Vec::new();
        for (name, hint, count, tokens) in sections {
            if count == 0 {
                continue;
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(clip(name, width), bold),
                Span::styled(
                    clip(
                        &format!(" \u{b7} {hint}"),
                        width.saturating_sub(name.width()),
                    ),
                    theme.dim,
                ),
            ]));
            let unit = if count == 1 { "tool" } else { "tools" };
            lines.push(Line::styled(
                clip(
                    &format!(
                        "\u{2514} {count} {unit} \u{b7} {} tokens",
                        compact_tokens(tokens)
                    ),
                    width,
                ),
                theme.dim,
            ));
        }

        lines
    }

    /// The uncataloged rendering: totals alone, and the honest sentence —
    /// only the catalog can size a window, so no denominator is invented.
    /// The model renders as its id once: an uncataloged model has no display
    /// name to pair it with.
    fn degraded_lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled(
                clip(
                    &format!(
                        "{} \u{b7} {} tokens \u{2014} estimated",
                        self.breakdown.model,
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
#[path = "context_tests.rs"]
mod tests;
