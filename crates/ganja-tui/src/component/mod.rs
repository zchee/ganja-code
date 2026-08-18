//! The three panes: transcript, prompt editor, status bar, plus the
//! modals that overlay them — one for a tool call waiting on a decision, one
//! for choosing a stored session to resume, one for choosing a theme, one for
//! choosing a model or an agent, the command palette, the reference card, one
//! for fuzzy-searching remembered prompts, one for rewinding to a checkpoint,
//! one for the configured MCP servers and what to do about them, one for the
//! installed plugins and the store actions beside them, one for this session's
//! team and what a member's row offers, the strip of messages waiting for the
//! running turn, and the two inline menus the editor raises — one on a leading
//! slash, one on an `@` — plus the two read-only panels `/context` and
//! `/usage` raise over the same chrome.

use ratatui::layout::{Constraint, Rect};

/// What marks the row the cursor is on, and what pads every other row.
pub(crate) const MARKER: &str = "> ";

/// Rows a two-step dialog spends on chrome: a blank line and the key hints.
pub(crate) const CHROME: usize = 2;

/// Widest the two-step modals grow.
pub(crate) const MAX_WIDTH: u16 = 76;

/// Tallest the two-step modals grow.
pub(crate) const MAX_HEIGHT: u16 = 20;

/// The keys a two-step dialog's list step answers to.
pub(crate) const LIST_HINTS: &str = "[j/k] [up/down] move   [Enter] choose   [Esc] close";

/// The keys its per-row action step answers to.
pub(crate) const ACTION_HINTS: &str = "[j/k] [up/down] move   [Enter] run   [Esc] close";

/// The keys its free-text step answers to.
pub(crate) const INPUT_HINTS: &str = "[type/backspace] edit   [Enter] submit   [Esc] cancel";

/// The key surface the `/plugin` and `/team` dialogs share: a list, a per-row
/// action step, and a free-text step that takes the printable keys. One
/// driver in `app.rs` reads it, so the two dialogs cannot answer the same
/// keypress two ways. What stays each dialog's own — the step enums, and what
/// `submit` decides — is the tested divergence between them.
pub(crate) trait TwoStep {
    /// What Enter hands the app to run.
    type Effect;

    /// Whether the free-text step owns the keyboard.
    fn is_typing(&self) -> bool;

    /// Esc; `false` means the dialog itself should close.
    fn cancel(&mut self) -> bool;

    /// Backspace, while the free-text step owns the keyboard.
    fn backspace(&mut self);

    /// A printable key, while the free-text step owns the keyboard.
    fn push(&mut self, character: char);

    /// Up/Down (and j/k) on whichever list is showing.
    fn move_selection(&mut self, delta: isize);

    /// Enter, wherever the dialog is.
    fn submit(&mut self) -> Option<Self::Effect>;
}

pub mod chat;
pub mod context;
pub mod dropdown;
pub mod editor;
pub mod effort;
pub mod files;
pub mod help;
pub mod inspector;
pub mod list;
pub mod mcp;
pub mod palette;
pub mod permission;
pub mod plugin;
pub mod question;
pub mod queue;
pub mod rewind;
pub mod search;
pub mod sessions;
pub mod skill_menu;
pub mod status;
pub mod team;
pub mod themes;
pub mod usage;

/// The box a dialog draws itself in, and the two sizes it lays text out
/// against: the popup rectangle, the columns inside its border, and the body
/// rows left once `chrome` — whatever fixed lines that dialog always draws
/// under its list — is taken out.
///
/// Two margins are baked in because every dialog here has always used them:
/// four columns and two rows of the screen stay outside the box, and the
/// border takes one of each on both sides. What differs per dialog is only
/// how wide and tall it is willing to grow and how much chrome it carries, so
/// those are the arguments.
///
/// The dialogs whose height depends on what they are about — `rewind`, `mcp`,
/// `plugin`, `help`, `context`, `usage` — size their own box and take
/// [`body_rows`] alone; `search` splits its rows between a list and a preview
/// and keeps its own floor.
pub(crate) fn modal(
    area: Rect,
    max_width: u16,
    max_height: u16,
    chrome: usize,
) -> (Rect, usize, usize) {
    let width = area.width.saturating_sub(4).clamp(1, max_width);
    let height = area.height.saturating_sub(2).clamp(1, max_height);
    let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

    (
        popup,
        usize::from(width).saturating_sub(2),
        body_rows(height, chrome),
    )
}

/// The body rows a box `height` rows tall has room for: its border takes two,
/// its own `chrome` takes the rest, and at least one row survives whatever is
/// left — a list with nowhere to draw is a dialog that shows nothing at all.
pub(crate) fn body_rows(height: u16, chrome: usize) -> usize {
    usize::from(height)
        .saturating_sub(2)
        .saturating_sub(chrome)
        .max(1)
}

/// The first row on screen: far enough down to keep the selected one visible,
/// and no further. Every scrolling list here answers it the same way.
pub(crate) fn first_visible(selected: usize, rows: usize) -> usize {
    selected.saturating_sub(rows.saturating_sub(1))
}

/// `selected` moved by `delta` rows and held at the ends of a `len`-row list.
///
/// Clamped rather than wrapped: the lists are ordered, so running off one end
/// and landing on the other is never what the keypress meant.
pub(crate) fn clamped(selected: usize, delta: isize, len: usize) -> usize {
    let last = len.saturating_sub(1);
    let moved = if delta < 0 {
        selected.saturating_sub(delta.unsigned_abs())
    } else {
        selected.saturating_add(delta.unsigned_abs())
    };

    moved.min(last)
}
