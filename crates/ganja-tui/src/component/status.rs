//! The status bar: what the engine is doing, plus the keys that matter.

use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr as _;

use crate::theme::Theme;

/// Spinner phases, one braille cell each.
const SPINNER: [&str; 8] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
];

/// How long each spinner phase is shown.
const SPINNER_PERIOD: Duration = Duration::from_millis(80);

/// Key reminders, dropped whole when the terminal is too narrow for them.
const HINTS: &str = "Enter send \u{b7} Alt+Enter newline \u{b7} Esc cancel \u{b7} Ctrl-C quit";

/// What the engine is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Idle, waiting for a prompt.
    Ready,
    /// A reply is streaming in.
    Streaming,
    /// The last turn was cancelled.
    Stopped,
}

impl Activity {
    fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Streaming => "streaming",
            Self::Stopped => "stopped",
        }
    }
}

/// The bottom line of the screen.
#[derive(Debug)]
pub struct Status {
    activity: Activity,
    /// When the current activity began; the spinner phase is derived from it
    /// rather than from a counter the render loop has to advance.
    since: Instant,
    notice: Option<String>,
}

impl Status {
    /// Builds a status bar that starts idle, optionally carrying a notice.
    #[must_use]
    pub fn new(notice: Option<String>) -> Self {
        Self {
            activity: Activity::Ready,
            since: Instant::now(),
            notice,
        }
    }

    /// Records what the engine is doing now.
    pub fn set_activity(&mut self, activity: Activity) {
        if self.activity != activity {
            self.since = Instant::now();
        }
        self.activity = activity;
    }

    /// Replaces the message shown next to the activity.
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// Whether a turn is streaming, which is what keeps the spinner animating.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.activity == Activity::Streaming
    }

    /// Draws the status bar into `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let mut left = String::new();
        if self.is_streaming() {
            left.push_str(self.spinner());
            left.push(' ');
        }
        left.push_str(self.activity.label());
        if let Some(notice) = &self.notice {
            left.push_str(" \u{b7} ");
            left.push_str(notice);
        }

        let gap = usize::from(area.width).saturating_sub(left.width() + HINTS.width());
        let mut spans = vec![Span::styled(left, theme.accent)];
        if gap > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(HINTS, theme.dim));
        }

        buffer.set_line(area.x, area.y, &Line::from(spans), area.width);
    }

    fn spinner(&self) -> &'static str {
        let phase = self.since.elapsed().as_millis() / SPINNER_PERIOD.as_millis();

        SPINNER[usize::try_from(phase).unwrap_or(0) % SPINNER.len()]
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Activity, HINTS, Status};
    use crate::theme::Theme;

    fn rendered(status: &Status, width: u16) -> String {
        let area = Rect::new(0, 0, width, 1);
        let mut buffer = Buffer::empty(area);
        status.render(area, &mut buffer, &Theme::default());

        (0..width)
            .map(|column| buffer[(column, 0)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn an_idle_bar_shows_the_state_and_the_hints() {
        let line = rendered(&Status::new(None), 100);

        assert!(line.starts_with("ready"), "got {line:?}");
        assert!(line.ends_with(HINTS), "got {line:?}");
    }

    #[test]
    fn a_streaming_bar_leads_with_a_spinner() {
        let mut status = Status::new(None);
        status.set_activity(Activity::Streaming);

        let line = rendered(&status, 100);

        assert!(status.is_streaming());
        assert!(line.contains("streaming"), "got {line:?}");
        assert!(!line.starts_with("streaming"), "got {line:?}");
    }

    #[test]
    fn a_notice_sits_next_to_the_state() {
        let status = Status::new(Some("provider defaulted".to_owned()));

        assert!(
            rendered(&status, 100).contains("provider defaulted"),
            "the notice should be visible"
        );
    }

    #[test]
    fn a_narrow_bar_drops_the_hints_rather_than_the_state() {
        let line = rendered(&Status::new(None), 12);

        assert_eq!(line, "ready");
    }

    #[test]
    fn a_zero_width_bar_draws_nothing() {
        assert_eq!(rendered(&Status::new(None), 0), "");
    }

    #[test]
    fn a_cancelled_turn_reads_as_stopped() {
        let mut status = Status::new(None);
        status.set_activity(Activity::Streaming);
        status.set_activity(Activity::Stopped);

        assert!(!status.is_streaming());
        assert!(rendered(&status, 100).starts_with("stopped"));
    }
}
