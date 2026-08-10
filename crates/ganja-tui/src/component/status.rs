//! The status bar: what the engine is doing, what it has spent, plus the keys
//! that matter.

use std::time::{Duration, Instant};

use ganja_core::catalog::compact_tokens;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr as _;

use crate::theme::Theme;

/// Separates the things on the left of the bar.
const SEPARATOR: &str = " \u{b7} ";

/// Spinner phases, one braille cell each.
const SPINNER: [&str; 8] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
];

/// How long each spinner phase is shown.
const SPINNER_PERIOD: Duration = Duration::from_millis(80);

/// Key reminders, dropped whole when the terminal is too narrow for them.
const HINTS: &str = "Enter send \u{b7} Alt+Enter newline \u{b7} Esc cancel \u{b7} Ctrl-C quit";

/// The same reminders while the composer is running shell commands. Upstream
/// replaces its whole footer too (`component/prompt/index.tsx:1680-1682`):
/// half of what the normal one offers does not apply, and the one key a user
/// needs here is the way out.
const SHELL_HINTS: &str = "Enter run \u{b7} Esc exit shell mode \u{b7} Ctrl-C quit";

/// What the engine is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Idle, waiting for a prompt.
    Ready,
    /// A reply is streaming in.
    Streaming,
    /// A tool call is executing, named by its registry id.
    Tool(String),
    /// A tool call is waiting on the user's permission decision.
    Permission,
    /// The last turn was cancelled.
    Stopped,
    /// The last turn could not be answered; the notice says why.
    Failed,
}

impl Activity {
    fn label(&self) -> String {
        match self {
            Self::Ready => "ready".to_owned(),
            Self::Streaming => "streaming".to_owned(),
            Self::Tool(tool) => format!("tool: {tool}"),
            Self::Permission => "waiting on permission".to_owned(),
            Self::Stopped => "stopped".to_owned(),
            Self::Failed => "failed".to_owned(),
        }
    }
}

/// What a session has spent so far.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    /// Tokens sent, cache traffic included.
    pub input_tokens: u64,
    /// Tokens generated, thinking included.
    pub output_tokens: u64,
    /// Dollars spent, absent while the model is one the catalog cannot price.
    pub cost_usd: Option<f64>,
}

impl Totals {
    /// The compact rendering the bar has room for beside everything else.
    fn segment(&self) -> String {
        let tokens = format!(
            "{} in{SEPARATOR}{} out",
            compact_tokens(self.input_tokens),
            compact_tokens(self.output_tokens)
        );

        match self.cost_usd {
            // Four decimals because a short exchange with a cheap model costs
            // less than a cent, and two would round the whole session to
            // nothing until it had run for a while.
            Some(cost) => format!("{tokens}{SEPARATOR}${cost:.4}"),
            None => tokens,
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
    /// Absent until a provider reports what a turn spent.
    totals: Option<Totals>,
    /// The agent the next turn runs as, absent on a session that has no agent
    /// registry — which is every scripted and golden run, and is why the bar
    /// says nothing rather than saying "none".
    agent: Option<String>,
    /// The model and the effort it runs under, shown only while an effort is
    /// selected: the bar never named the model before efforts existed, and a
    /// session running Default keeps exactly the bar it always had. The model
    /// rides along because an effort's name alone ("max") says nothing
    /// without the model it belongs to.
    effort: Option<(String, String)>,
    /// Whether the composer is running shell commands, which changes which
    /// keys are worth reminding the user about.
    shell: bool,
}

impl Status {
    /// Builds a status bar that starts idle, optionally carrying a notice.
    #[must_use]
    pub fn new(notice: Option<String>) -> Self {
        Self {
            activity: Activity::Ready,
            since: Instant::now(),
            notice,
            totals: None,
            agent: None,
            effort: None,
            shell: false,
        }
    }

    /// Names the agent the next turn runs as.
    pub fn set_agent(&mut self, agent: Option<String>) {
        self.agent = agent;
    }

    /// Names the `(model, effort)` the next turn runs under, or clears the
    /// segment — which a session back on Default does.
    pub fn set_effort(&mut self, effort: Option<(String, String)>) {
        self.effort = effort;
    }

    /// Records whether the composer is running shell commands.
    pub fn set_shell(&mut self, shell: bool) {
        self.shell = shell;
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

    /// Shows what the session has spent so far.
    pub fn set_totals(&mut self, totals: Totals) {
        self.totals = Some(totals);
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
        // The agent comes first because it is the one part of the bar that
        // says what the *next* turn will be, rather than what the last one
        // did — and because a switch is otherwise invisible until a reply
        // arrives written in a different voice.
        if let Some(agent) = &self.agent {
            left.push_str(agent);
            left.push_str(SEPARATOR);
        }
        // Beside the agent, and for the same reason: both say what the *next*
        // turn will be.
        if let Some((model, effort)) = &self.effort {
            left.push_str(model);
            left.push_str(" (");
            left.push_str(effort);
            left.push(')');
            left.push_str(SEPARATOR);
        }
        left.push_str(&self.activity.label());
        // Spend sits beside the state, where its width is predictable; the
        // notice is last because it is the one part with no length limit.
        if let Some(totals) = &self.totals {
            left.push_str(SEPARATOR);
            left.push_str(&totals.segment());
        }
        if let Some(notice) = &self.notice {
            left.push_str(SEPARATOR);
            left.push_str(notice);
        }

        let hints = self.hints();
        let gap = usize::from(area.width).saturating_sub(left.width() + hints.width());
        let mut spans = vec![Span::styled(left, theme.accent)];
        if gap > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(hints, theme.dim));
        }

        buffer.set_line(area.x, area.y, &Line::from(spans), area.width);
    }

    /// The reminders this mode is worth showing.
    fn hints(&self) -> &'static str {
        if self.shell { SHELL_HINTS } else { HINTS }
    }

    fn spinner(&self) -> &'static str {
        let phase = self.since.elapsed().as_millis() / SPINNER_PERIOD.as_millis();

        SPINNER[usize::try_from(phase).unwrap_or(0) % SPINNER.len()]
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::{Activity, HINTS, SHELL_HINTS, Status, Totals};
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

    /// A session with no agent registry — every scripted and golden run — says
    /// nothing rather than saying "none", which is what keeps the layout
    /// snapshots taken over those runs unchanged.
    #[test]
    fn the_bar_names_the_agent_only_once_there_is_one() {
        let mut status = Status::new(None);
        assert!(rendered(&status, 100).starts_with("ready"));

        status.set_agent(Some("plan".to_owned()));

        assert!(
            rendered(&status, 100).starts_with("plan"),
            "got {:?}",
            rendered(&status, 100)
        );
    }

    /// The segment appears only while an effort is selected, so every bar
    /// drawn before efforts existed — and every session on Default — renders
    /// byte for byte as it always did.
    #[test]
    fn the_bar_names_the_model_and_effort_only_while_one_is_selected() {
        let mut status = Status::new(None);
        assert!(!rendered(&status, 100).contains('('));

        status.set_agent(Some("build".to_owned()));
        status.set_effort(Some(("claude-opus-5".to_owned(), "max".to_owned())));

        let line = rendered(&status, 100);
        assert!(line.starts_with("build"), "got {line:?}");
        assert!(line.contains("claude-opus-5 (max)"), "got {line:?}");

        status.set_effort(None);
        assert!(!rendered(&status, 100).contains("claude-opus-5"));
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

    #[test]
    fn spend_is_shown_compactly_next_to_the_state() {
        let mut status = Status::new(None);
        status.set_totals(Totals {
            input_tokens: 12_345,
            output_tokens: 1_200,
            cost_usd: Some(0.084_2),
        });

        let line = rendered(&status, 100);

        assert!(line.starts_with("ready"), "got {line:?}");
        assert!(line.contains("12.3k in"), "got {line:?}");
        assert!(line.contains("1.2k out"), "got {line:?}");
        assert!(line.contains("$0.0842"), "got {line:?}");
    }

    /// A turn against a model the catalog cannot price still reports its
    /// tokens; inventing a dollar figure for it would be worse than omitting
    /// one.
    #[test]
    fn an_unpriced_model_shows_tokens_without_a_price() {
        let mut status = Status::new(None);
        status.set_totals(Totals {
            input_tokens: 40,
            output_tokens: 7,
            cost_usd: None,
        });

        let line = rendered(&status, 100);

        assert!(line.contains("40 in"), "got {line:?}");
        assert!(line.contains("7 out"), "got {line:?}");
        assert!(!line.contains('$'), "got {line:?}");
    }

    /// Sub-cent sessions are the common case early on, so the dollar figure
    /// keeps enough decimals to be something other than zero.
    #[test]
    fn a_sub_cent_session_still_shows_a_number() {
        let mut status = Status::new(None);
        status.set_totals(Totals {
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: Some(0.000_7),
        });

        let line = rendered(&status, 100);

        assert!(line.contains("$0.0007"), "got {line:?}");
    }

    /// Spend must not crowd out the reason a turn failed.
    #[test]
    fn a_notice_survives_beside_the_spend() {
        let mut status = Status::new(Some("no usable credentials".to_owned()));
        status.set_activity(Activity::Failed);
        status.set_totals(Totals {
            input_tokens: 1_000,
            output_tokens: 0,
            cost_usd: Some(0.5),
        });

        let line = rendered(&status, 120);

        assert!(line.starts_with("failed"), "got {line:?}");
        assert!(line.contains("1.0k in"), "got {line:?}");
        assert!(line.contains("no usable credentials"), "got {line:?}");
    }

    #[test]
    fn a_failed_turn_reads_as_failed_and_explains_itself() {
        let mut status = Status::new(None);
        status.set_activity(Activity::Streaming);
        status.set_activity(Activity::Failed);
        status.set_notice(Some("no usable credentials".to_owned()));

        let line = rendered(&status, 100);

        assert!(!status.is_streaming());
        assert!(line.starts_with("failed"), "got {line:?}");
        assert!(line.contains("no usable credentials"), "got {line:?}");
    }

    #[test]
    fn a_running_tool_names_itself_in_the_activity_label() {
        let mut status = Status::new(None);
        status.set_activity(Activity::Tool("shell".to_owned()));

        assert!(rendered(&status, 100).contains("tool: shell"));
    }

    #[test]
    fn waiting_on_a_permission_has_its_own_label() {
        let mut status = Status::new(None);
        status.set_activity(Activity::Permission);

        assert!(rendered(&status, 100).contains("waiting on permission"));
    }

    /// Half of what the normal footer offers does not apply while the buffer
    /// is a shell command, and the one key that does — the way out — is not on
    /// it at all.
    #[test]
    fn shell_mode_reminds_the_user_of_the_way_out_instead() {
        let mut status = Status::new(None);
        assert!(rendered(&status, 120).contains(HINTS));

        status.set_shell(true);

        let line = rendered(&status, 120);
        assert!(line.contains(SHELL_HINTS), "got {line:?}");
        assert!(!line.contains("Enter send"), "got {line:?}");

        status.set_shell(false);
        assert!(rendered(&status, 120).contains(HINTS));
    }
}
