//! What the event loop reacts to.

use ganja_core::Event as CoreEvent;
use ratatui::crossterm::event::Event as TermEvent;

/// One thing that woke the loop up.
///
/// Folding the sources into a single enum keeps [`App::handle`] the only place
/// that mutates state, which is what lets the components be tested without a
/// terminal or a running turn.
///
/// [`App::handle`]: crate::app::App::handle
#[derive(Clone, Debug)]
pub enum AppEvent {
    /// The user pressed a key, moved the wheel, or resized the window.
    Term(TermEvent),
    /// The engine reported progress on a turn.
    Core(CoreEvent),
    /// The frame budget elapsed; nothing changed but the clock.
    Tick,
}
