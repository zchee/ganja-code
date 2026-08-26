//! The `/mcp` dialog: one row per configured server — status, tool count, and
//! the first line of an error — with a second step for the actions a row
//! offers: Reconnect for a server the dialog shows as failed (**D463**), and
//! Login for a remote server configured with `oauth`, whatever its status
//! (**D466**) — a server with nothing stored yet dials to a "needs a login"
//! failure the same way a login that later expires would.
//!
//! **D465** (`mcp-dialog-is-a-claude-port`): upstream opencode has no TUI
//! surface for its MCP servers at all — only the `opencode mcp` CLI listing
//! this port's own `ganja mcp` mirrors. The row shape and the two-step
//! server-then-action flow are this build's own reading of Claude Code's
//! `/mcp` panel, not a port of anything upstream ships; nothing here cites an
//! upstream file for that reason.
//!
//! Two steps, the same shape [`crate::component::rewind::Rewind`] uses for a
//! checkpoint and its scope: choosing a server, then choosing what to do
//! about it. Sending the chosen action to the engine and closing the dialog
//! are [`crate::app::App`]'s, not this component's — the same split every
//! other dialog here keeps. Status stays poll-driven: [`Mcp::refresh`] is
//! what a tick hands a fresh set of rows to while the dialog is open, no new
//! protocol event involved.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    component::{action_row, body_rows, chat::clip, clamped, first_visible},
    theme::Theme,
};

/// What marks the row the cursor is on, and what pads every other row.
const MARKER: &str = "> ";

/// Rows the dialog spends on chrome: a blank line and the key hints.
const CHROME: usize = 2;

/// Widest the modal grows.
const MAX_WIDTH: u16 = 76;

/// Tallest the modal grows.
const MAX_HEIGHT: u16 = 20;

/// The keys the server step answers to.
const SERVER_HINTS: &str = "[j/k] [up/down] move   [Enter] actions   [Esc] close";

/// The keys the action step answers to.
const ACTION_HINTS: &str = "[j/k] [up/down] move   [Enter] run   [Esc] close";

/// Columns between a row's head and its error detail.
const GAP: usize = 2;

/// What is shown when nothing is configured.
const EMPTY: &str = "no MCP servers configured";

/// Something a row's Enter offers, beyond just looking at the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Re-dial a server the dialog shows as failed.
    Reconnect,
    /// Start (or restart) an OAuth login for a remote server configured with
    /// `oauth`.
    Login,
}

impl Action {
    /// The label the action step shows for it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Reconnect => "Reconnect",
            Self::Login => "Login",
        }
    }
}

/// One configured server, as the dialog shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// What a chosen action names to the engine — this build's server key,
    /// not a display label.
    pub name: String,
    /// "Connected" / "Disabled" / "Failed" / "dialling", matching
    /// [`ganja_core::McpStatus`]'s three variants plus the fourth state its
    /// own doc describes: absent from the map.
    pub status: String,
    /// How many tools it lends. [`None`] for a server that cannot be lending
    /// any right now — disabled, failed, still dialling.
    pub tools: Option<usize>,
    /// The first line of a failure, where there is one.
    pub detail: Option<String>,
    /// What Enter on this row offers. Empty means Enter does nothing.
    pub actions: Vec<Action>,
}

/// Which of the dialog's two steps is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Choosing a server.
    Servers,
    /// Choosing one of the selected server's actions, by index.
    Actions(usize),
}

/// The servers, which one is under the cursor, and which step is showing.
#[derive(Clone, Debug)]
pub struct Mcp {
    rows: Vec<Row>,
    /// Index into [`Mcp::rows`]; always in range while it is non-empty.
    selected: usize,
    step: Step,
}

impl Mcp {
    /// Opens the dialog over `rows`, cursor on the first one.
    #[must_use]
    pub fn new(rows: Vec<Row>) -> Self {
        Self {
            rows,
            selected: 0,
            step: Step::Servers,
        }
    }

    /// Replaces the rows with a fresh poll, keeping the cursor and step where
    /// they were: a status changing under a person mid-decision must not move
    /// what their next keypress lands on. The server list is config-driven and
    /// does not change size mid-session, so the cursor only ever needs
    /// reclamping against a shrink this dialog itself never causes.
    pub fn refresh(&mut self, rows: Vec<Row>) {
        self.rows = rows;
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    /// Whether the action step is the one on screen.
    #[must_use]
    pub fn is_choosing_action(&self) -> bool {
        matches!(self.step, Step::Actions(_))
    }

    /// The server under the cursor, or [`None`] over an empty list.
    #[must_use]
    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Moves whichever list is showing by `delta` rows.
    pub fn move_selection(&mut self, delta: isize) {
        match self.step {
            Step::Servers => self.selected = clamped(self.selected, delta, self.rows.len()),
            Step::Actions(option) => {
                let count = self.selected().map_or(0, |row| row.actions.len());
                self.step = Step::Actions(clamped(option, delta, count));
            }
        }
    }

    /// Enter on the server step: opens the action choice for the row under
    /// the cursor.
    ///
    /// Answers `false` for a row with nothing to choose — unlike
    /// [`Rewind::advance`](crate::component::rewind::Rewind::advance)'s
    /// `(Current)` row, there is no "explicit no-op" decision to record here,
    /// only a row this dialog cannot act on yet, so the caller leaves the
    /// dialog exactly as it was rather than closing it.
    pub fn advance(&mut self) -> bool {
        if self.selected().is_none_or(|row| row.actions.is_empty()) {
            return false;
        }
        self.step = Step::Actions(0);

        true
    }

    /// Enter on the action step: the server name and the chosen action.
    ///
    /// [`None`] while the server step is showing — [`Mcp::is_choosing_action`]
    /// is what a caller checks first.
    #[must_use]
    pub fn chosen(&self) -> Option<(&str, Action)> {
        let Step::Actions(option) = self.step else {
            return None;
        };
        let row = self.selected()?;
        let action = *row.actions.get(option)?;

        Some((row.name.as_str(), action))
    }

    /// Leaves the action step for the server list, without closing the
    /// dialog — what running a chosen action does, so its outcome shows up on
    /// the row the next poll refreshes.
    pub fn back_to_servers(&mut self) {
        self.step = Step::Servers;
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let available = area.height.saturating_sub(2).clamp(1, MAX_HEIGHT);
        let inner_width = usize::from(width).saturating_sub(2);
        let rows = body_rows(available, CHROME);

        let mut lines = match self.step {
            Step::Servers => self.server_rows(inner_width, rows, theme),
            Step::Actions(option) => self.action_rows(inner_width, option, theme),
        };
        let hints = match self.step {
            Step::Servers => SERVER_HINTS,
            Step::Actions(_) => ACTION_HINTS,
        };
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(hints, inner_width), theme.dim));

        // The server step takes the screenful it was given, since the list is
        // as long as there are configured servers. The action step is a
        // handful of answers and never grows, so it takes exactly the rows it
        // needs, mirroring `Rewind`'s own two-height scheme.
        let height = match self.step {
            Step::Servers => available,
            Step::Actions(_) => u16::try_from(lines.len().saturating_add(2))
                .unwrap_or(available)
                .min(available),
        };
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" mcp "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// One line per visible server.
    fn server_rows(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.rows.is_empty() {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        }

        let first = first_visible(self.selected, rows);
        let name_width = self
            .rows
            .iter()
            .map(|row| row.name.width())
            .max()
            .unwrap_or(0);
        let status_width = self
            .rows
            .iter()
            .map(|row| row.status.width())
            .max()
            .unwrap_or(0);

        self.rows
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(index, row)| {
                let tools = row
                    .tools
                    .map_or_else(|| "-".to_owned(), |count| count.to_string());
                let head = format!(
                    "{marker}{name:<name_width$}  {status:<status_width$}  {tools:>3} tools",
                    marker = if index == self.selected { MARKER } else { "  " },
                    name = row.name,
                    status = row.status,
                );
                let detail = row.detail.as_deref().unwrap_or_default();
                let detail_width = width.saturating_sub(head.width() + GAP).max(1);
                let line = if detail.is_empty() {
                    head
                } else {
                    format!(
                        "{head}{gap}{detail}",
                        gap = " ".repeat(GAP),
                        detail = clip(detail, detail_width),
                    )
                };
                let line = clip(&line, width);

                Line::styled(
                    format!("{line:<width$}"),
                    if index == self.selected {
                        theme.selection
                    } else {
                        theme.fg
                    },
                )
            })
            .collect()
    }

    /// The action step: which server it is about, and its offered actions.
    fn action_rows(&self, width: usize, option: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(row) = self.selected() else {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        };

        let mut lines = vec![Line::styled(clip(&row.name, width), theme.fg)];
        for (index, action) in row.actions.iter().enumerate() {
            lines.push(action_row(index, option, action.label(), width, theme));
        }

        lines
    }
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
