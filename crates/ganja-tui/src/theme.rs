//! The P1 palette: three roles, no configuration. Themes become loadable data
//! in P5, which is when a revision counter starts mattering to the render
//! cache.

use ratatui::style::{Color, Modifier, Style};

/// Styles the components share.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Body text.
    pub fg: Style,
    /// Chrome that should recede: headers, hints, separators.
    pub dim: Style,
    /// Whatever the eye should land on first.
    pub accent: Style,
    /// A diff line that added text.
    pub add: Style,
    /// A diff line that removed text.
    pub remove: Style,
    /// A tool call that failed, or was refused.
    pub error: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: Style::new().fg(Color::Reset),
            dim: Style::new().fg(Color::DarkGray),
            accent: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            add: Style::new().fg(Color::Green),
            remove: Style::new().fg(Color::Red),
            error: Style::new().fg(Color::Red),
        }
    }
}
