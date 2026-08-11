//! The three panes: transcript, prompt editor, status bar, plus the
//! modals that overlay them — one for a tool call waiting on a decision, one
//! for choosing a stored session to resume, one for choosing a theme, one for
//! choosing a model or an agent, the command palette, the reference card, one
//! for fuzzy-searching remembered prompts, and the two inline menus the
//! editor raises — one on a leading slash, one on an `@`.

pub mod chat;
pub mod dropdown;
pub mod editor;
pub mod effort;
pub mod files;
pub mod help;
pub mod list;
pub mod palette;
pub mod permission;
pub mod question;
pub mod search;
pub mod sessions;
pub mod status;
pub mod themes;

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
