//! The status bar: what the engine is doing, what it has spent, plus the keys
//! that matter.
//!
//! Two renderings share one set of named elements (**D469**,
//! `hud-statusline`, no upstream counterpart — opencode's footer is fixed).
//! With no `tui.statusline` in the config the bar draws exactly what it
//! always drew, segment for segment, which is what keeps every full-frame
//! snapshot taken over that bar unchanged. A config that names elements gets
//! the oh-my-claudecode HUD's behavior instead: the named elements only, in
//! the named order, joined with dim ` | ` separators, truncated with an
//! ellipsis at the width limit, a `repo:… | branch:…` line *above* the bar
//! when `git` is named, and an optional detail line under it — the rendering
//! the P14 screenshot pinned. The rate-bucket meters that screenshot also
//! shows (5h/week/spend) are deliberately absent: they need a vendor usage
//! API ganja does not speak, so the meter renderer is built once ([`meter`])
//! and the elements wait for a data source rather than inventing one.

use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use ganja_core::{
    catalog::compact_tokens,
    config::{StatuslineConfig, StatuslineElement},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::theme::Theme;

/// Separates the things on the left of the bar.
const SEPARATOR: &str = " \u{b7} ";

/// Separates a configured roster's elements — the OMC HUD's separator, dim so
/// the elements read and the plumbing recedes.
const HUD_SEPARATOR: &str = " | ";

/// The standing marker a bypassed session carries (**D479**).
///
/// The flag's own spelling, not a sentence about it: the word somebody typed
/// is the word that should be looking back at them, and a bar has room for
/// four characters where it has none for an explanation.
const YOLO: &str = "yolo";

/// What a truncated line ends with. Three ASCII dots rather than U+2026
/// because that is what OMC's `truncateLineToMaxWidth` appends, and the HUD
/// rendering is a port of that behavior.
const ELLIPSIS: &str = "...";

/// Bar cells between a meter's brackets — the screenshot's pinned shape,
/// `ctx:[####----]NN%`.
const METER_SLOTS: u64 = 8;

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
    ///
    /// `pub(crate)` so the Ctrl+T inspector's per-turn token tab (**F2**) can
    /// print the exact same string as its totals footer, rather than a second
    /// formatter that could drift from what the status bar actually shows.
    pub(crate) fn segment(&self) -> String {
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

/// Todo progress, handed in from the todowrite state the chat renders — the
/// bar cannot see tool results, so the app feeds it what the `todos` element
/// shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Todos {
    /// How many entries are completed.
    pub done: usize,
    /// How many entries there are.
    pub total: usize,
    /// The in-progress entry's title, when one is in progress.
    pub current: Option<String>,
}

/// The `.git/HEAD` identity the git line renders: a **file read**, cached
/// until HEAD's mtime moves, never a `git` subprocess — nothing in the pinned
/// rendering needs porcelain, and a subprocess per render tick is the cost
/// the plan's script-statusline option was rejected over (P14 review
/// changelog MINOR 4).
#[derive(Debug, Default)]
struct GitCache {
    /// The repository's HEAD file — through a linked worktree's `gitdir:`
    /// pointer when there is one — or absent when no ancestor of the working
    /// directory is a repository.
    head: Option<PathBuf>,
    /// The repository's name: the directory holding `.git`.
    repo: Option<String>,
    /// HEAD's mtime when `branch` was parsed.
    read_at: Option<SystemTime>,
    /// What HEAD named at `read_at`.
    branch: Option<String>,
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
    /// How many messages are waiting for the running turn (**F4**). Shown only
    /// while there are any, so a session that never queues one renders the bar
    /// it always did.
    queued: usize,
    /// How many background jobs — `bash` calls run with `run_in_background:
    /// true` — are currently running. Shown only while there are any, the
    /// same posture as `queued`, since a session that never backgrounds one
    /// renders the bar it always did.
    running_jobs: usize,
    /// How many `task` calls of the running turn are in flight (**D462**).
    /// Beside the background-job count and for the same reason: a step that
    /// fanned three children out is otherwise one inline row that says nothing
    /// about how many loops are behind it.
    running_tasks: usize,
    /// How many permission dialogs are waiting behind the one on screen. Only
    /// concurrent children can produce more than none, and a person answering
    /// one dialog with a second already queued has to be told there is a
    /// second.
    queued_dialogs: usize,
    /// Whether this session answers its own permission dialogs (**D479**).
    ///
    /// Standing, not transient: every other segment here says what is
    /// happening now, and this one says what will keep happening for the rest
    /// of the run. A bypassed session that looked like a gated one is the
    /// failure this marker exists to prevent, so it draws in the warning style
    /// and it draws first — before the agent, which is otherwise the leftmost
    /// thing on the bar.
    yolo: bool,
    /// The roster a config asked for; absent renders the default bar, which
    /// is exactly the bar this build always drew (**D469**).
    elements: Option<Vec<StatuslineElement>>,
    /// Widest a configured roster may draw; absent is the area's own width.
    max_width: Option<u16>,
    /// Whether elements with more than a segment's worth may draw a detail
    /// line under the bar.
    detail: bool,
    /// The model the next turn asks for, for the `model` element's
    /// `Model: <name>` form — the default bar names a model only through the
    /// effort segment, so this arrives by its own setter.
    model: Option<String>,
    /// `(estimated tokens, window)` for the `context` meter, absent until the
    /// app polls `Engine::context_estimate` — and kept absent for an
    /// uncataloged model, whose window nobody can size.
    context: Option<(u64, u64)>,
    /// Todo progress for the `todos` element.
    todos: Option<Todos>,
    /// When this bar was built — the `session` element's zero. Not `since`,
    /// which every activity change resets.
    started: Instant,
    /// The working directory's name, captured once for the `cwd` element.
    cwd: Option<String>,
    /// The git identity, discovered once and re-read only when HEAD moves.
    /// Interior mutability because `render` takes `&self` and the cache is a
    /// rendering detail, not state anybody else observes.
    git: RefCell<GitCache>,
}

impl Status {
    /// Builds a status bar that starts idle, optionally carrying a notice.
    #[must_use]
    pub fn new(notice: Option<String>) -> Self {
        let workdir = std::env::current_dir().ok();

        Self {
            activity: Activity::Ready,
            since: Instant::now(),
            notice,
            totals: None,
            agent: None,
            effort: None,
            shell: false,
            queued: 0,
            running_jobs: 0,
            running_tasks: 0,
            queued_dialogs: 0,
            yolo: false,
            elements: None,
            max_width: None,
            detail: false,
            model: None,
            context: None,
            todos: None,
            started: Instant::now(),
            cwd: workdir
                .as_deref()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned()),
            git: RefCell::new(workdir.as_deref().map(discover_git).unwrap_or_default()),
        }
    }

    /// Applies the config's `tui.statusline` table: the roster, the width
    /// cap, and whether a detail line may draw. [`None`] — no table written —
    /// keeps the default bar.
    pub fn set_statusline(&mut self, statusline: Option<&StatuslineConfig>) {
        self.elements = statusline.and_then(|table| table.elements.clone());
        self.max_width = statusline.and_then(|table| table.max_width);
        self.detail = statusline.and_then(|table| table.detail).unwrap_or(false);
    }

    /// Names the model the next turn asks for, for the `model` element.
    pub fn set_model(&mut self, model: Option<String>) {
        self.model = model;
    }

    /// Records `(estimated tokens, window)` for the `context` meter, or
    /// clears it — which an uncataloged model, whose window nobody can size,
    /// does.
    pub fn set_context(&mut self, context: Option<(u64, u64)>) {
        self.context = context;
    }

    /// Records todo progress for the `todos` element.
    pub fn set_todos(&mut self, todos: Option<Todos>) {
        self.todos = todos;
    }

    /// How many rows this bar wants: one, plus the git line above it and the
    /// detail line under it when the configured roster earns them. The
    /// default bar is always exactly the one row it always was.
    #[must_use]
    pub fn height(&self) -> u16 {
        let Some(elements) = &self.elements else {
            return 1;
        };

        let mut height = 1;
        if elements.contains(&StatuslineElement::Git) && self.git.borrow().head.is_some() {
            height += 1;
        }
        if self.detail
            && elements.contains(&StatuslineElement::Todos)
            && self.detail_text().is_some()
        {
            height += 1;
        }

        height
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

    /// Records how many messages are waiting for the running turn.
    pub fn set_queued(&mut self, queued: usize) {
        self.queued = queued;
    }

    /// Records how many background jobs are currently running.
    pub fn set_running_jobs(&mut self, running_jobs: usize) {
        self.running_jobs = running_jobs;
    }

    /// Records how many delegated children the running turn has in flight.
    pub fn set_running_tasks(&mut self, running_tasks: usize) {
        self.running_tasks = running_tasks;
    }

    /// Records whether this session answers its own permission dialogs.
    pub fn set_yolo(&mut self, yolo: bool) {
        self.yolo = yolo;
    }

    /// Records how many permission dialogs are waiting behind the open one.
    pub fn set_queued_dialogs(&mut self, queued_dialogs: usize) {
        self.queued_dialogs = queued_dialogs;
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

        match &self.elements {
            None => self.render_default(area, buffer, theme),
            Some(elements) => self.render_roster(elements, area, buffer, theme),
        }
    }

    /// The bar this build always drew, byte for byte — the default roster.
    fn render_default(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
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
        // What is waiting sits beside what is happening, because the two
        // together are the answer to "where is my message": a queue with a
        // depth and no visible strip row would otherwise be the one state
        // nothing on screen accounts for.
        if self.queued > 0 {
            left.push_str(SEPARATOR);
            left.push_str(&format!("{} queued", self.queued));
        }
        // Same reasoning, same place: a running background job is otherwise
        // invisible until its own `bash_output` poll names it.
        if self.running_jobs > 0 {
            left.push_str(SEPARATOR);
            left.push_str(&format!("{} bash running", self.running_jobs));
        }
        // Beside the background jobs, because both answer the same question —
        // what else is happening besides the sentence being streamed.
        //
        // **More than one, not more than none.** A lone delegation is already
        // named by the activity segment (`tool: task`) and by its own inline
        // row; a segment counting it to one would add width to every bar a
        // single `task` call has ever drawn, and say nothing the bar was not
        // already saying. What is new — and what nothing else on screen
        // accounts for — is a *fan-out*.
        if self.running_tasks > 1 {
            left.push_str(SEPARATOR);
            left.push_str(&format!("{} tasks running", self.running_tasks));
        }
        // Last of the three, and the only one that is about the person rather
        // than the machine: it says how many more questions there are once this
        // dialog is answered. From none, because one waiting question is
        // already something the dialog on screen does not mention.
        if self.queued_dialogs > 0 {
            left.push_str(SEPARATOR);
            left.push_str(&format!(
                "{} {} queued",
                self.queued_dialogs,
                if self.queued_dialogs == 1 {
                    "dialog"
                } else {
                    "dialogs"
                }
            ));
        }
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

        // The one segment with a style of its own, and the one drawn ahead of
        // everything else (**D479**). Its own span rather than text prepended
        // to `left`, because what makes it readable at a glance is that it is
        // not the colour the rest of the bar is; and a session that did not
        // ask for the bypass adds no span at all, so the default bar stays the
        // bar this build always drew, cell for cell.
        let marker = self.yolo.then(|| format!("{YOLO}{SEPARATOR}"));
        let hints = self.hints();
        let used = marker.as_ref().map_or(0, |marker| marker.width()) + left.width();
        let gap = usize::from(area.width).saturating_sub(used + hints.width());
        let mut spans = Vec::new();
        if let Some(marker) = marker {
            spans.push(Span::styled(marker, theme.warning));
        }
        spans.push(Span::styled(left, theme.accent));
        if gap > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(hints, theme.dim));
        }

        buffer.set_line(area.x, area.y, &Line::from(spans), area.width);
    }

    /// The configured roster: the named elements only, in the named order,
    /// with the git line above and the detail line under when they earned
    /// their rows.
    fn render_roster(
        &self,
        elements: &[StatuslineElement],
        area: Rect,
        buffer: &mut Buffer,
        theme: &Theme,
    ) {
        let limit = usize::from(self.max_width.map_or(area.width, |cap| cap.min(area.width)));

        let mut git = elements
            .contains(&StatuslineElement::Git)
            .then(|| self.git_spans(theme))
            .flatten();
        let mut detail = (self.detail && elements.contains(&StatuslineElement::Todos))
            .then(|| self.detail_text())
            .flatten()
            .map(|title| {
                vec![
                    Span::styled("working: ".to_owned(), theme.dim),
                    Span::styled(title, theme.fg),
                ]
            });

        // The main line is the one row that never yields: a shorter area
        // drops the detail line first, then the git line.
        let available = usize::from(area.height);
        if 1 + usize::from(git.is_some()) + usize::from(detail.is_some()) > available {
            detail = None;
        }
        if 1 + usize::from(git.is_some()) > available {
            git = None;
        }

        // The elements, in the config's order. Git renders above and hints
        // render right-aligned, so neither takes a slot in the flow.
        let mut main: Vec<Span<'static>> = Vec::new();
        // Ahead of the roster rather than inside it (**D479**): a marker that
        // was an element name would be a marker a config could leave out, and
        // whether this session asks before it runs things is not a layout
        // preference. It costs no new config vocabulary and no new element —
        // the roster below is untouched, and only its leading separator moves.
        if self.yolo {
            main.push(Span::styled(YOLO.to_owned(), theme.warning));
        }
        for element in elements {
            if matches!(element, StatuslineElement::Git | StatuslineElement::Hints) {
                continue;
            }
            let Some(mut segment) = self.hud_segment(*element, theme) else {
                continue;
            };
            if !main.is_empty() {
                main.push(Span::styled(HUD_SEPARATOR.to_owned(), theme.dim));
            }
            main.append(&mut segment);
        }
        let mut main = truncate_spans(main, limit, theme);

        // The hints keep their right edge, exactly as the default bar draws
        // them — inside the width cap, so a capped bar stays one block.
        if elements.contains(&StatuslineElement::Hints) {
            let hints = self.hints();
            let used: usize = main.iter().map(|span| span.content.width()).sum();
            let gap = limit.saturating_sub(used + hints.width());
            if gap > 0 {
                main.push(Span::raw(" ".repeat(gap)));
                main.push(Span::styled(hints, theme.dim));
            }
        }

        let mut y = area.y;
        if let Some(git) = git {
            buffer.set_line(
                area.x,
                y,
                &Line::from(truncate_spans(git, limit, theme)),
                area.width,
            );
            y += 1;
        }
        buffer.set_line(area.x, y, &Line::from(main), area.width);
        y += 1;
        if let Some(detail) = detail {
            buffer.set_line(
                area.x,
                y,
                &Line::from(truncate_spans(detail, limit, theme)),
                area.width,
            );
        }
    }

    /// One named element's spans, or [`None`] while it has nothing to say —
    /// the same appear-only-while-nonzero posture the default bar holds.
    fn hud_segment(&self, element: StatuslineElement, theme: &Theme) -> Option<Vec<Span<'static>>> {
        let plain = |text: String| Some(vec![Span::styled(text, theme.accent)]);

        match element {
            StatuslineElement::Activity => {
                let mut text = String::new();
                if self.is_streaming() {
                    text.push_str(self.spinner());
                    text.push(' ');
                }
                text.push_str(&self.activity.label());
                plain(text)
            }
            StatuslineElement::Agent => self.agent.clone().and_then(plain),
            StatuslineElement::Effort => self
                .effort
                .as_ref()
                .and_then(|(model, effort)| plain(format!("{model} ({effort})"))),
            StatuslineElement::Queued => (self.queued > 0)
                .then(|| plain(format!("{} queued", self.queued)))
                .flatten(),
            StatuslineElement::Jobs => (self.running_jobs > 0)
                .then(|| plain(format!("{} bash running", self.running_jobs)))
                .flatten(),
            StatuslineElement::Tasks => (self.running_tasks > 1)
                .then(|| plain(format!("{} tasks running", self.running_tasks)))
                .flatten(),
            StatuslineElement::Dialogs => (self.queued_dialogs > 0)
                .then(|| {
                    plain(format!(
                        "{} {} queued",
                        self.queued_dialogs,
                        if self.queued_dialogs == 1 {
                            "dialog"
                        } else {
                            "dialogs"
                        }
                    ))
                })
                .flatten(),
            StatuslineElement::Tokens => self
                .totals
                .as_ref()
                .and_then(|totals| plain(totals.segment())),
            StatuslineElement::Notice => self.notice.clone().and_then(plain),
            StatuslineElement::Model => self.model.clone().map(|model| {
                vec![
                    // The screenshot's label form, `Model: Fable 5`.
                    Span::styled("Model: ".to_owned(), theme.dim),
                    Span::styled(model, theme.fg.add_modifier(Modifier::BOLD)),
                ]
            }),
            StatuslineElement::Context => self
                .context
                .map(|(tokens, window)| meter("ctx", percent_of(tokens, window), theme)),
            StatuslineElement::Session => {
                // Whole minutes with an `m`, OMC's own `renderSession` shape.
                plain(format!(
                    "session:{}m",
                    self.started.elapsed().as_secs() / 60
                ))
            }
            StatuslineElement::Cwd => self.cwd.clone().map(|name| {
                vec![
                    Span::styled("cwd:".to_owned(), theme.dim),
                    Span::styled(name, theme.fg),
                ]
            }),
            StatuslineElement::Todos => {
                self.todos
                    .as_ref()
                    .filter(|todos| todos.total > 0)
                    .map(|todos| {
                        let mut text = format!("todos:{}/{}", todos.done, todos.total);
                        // The in-progress title rides inline — OMC's
                        // `renderTodosWithCurrent` — unless the detail line is on,
                        // where it gets a whole row instead of a squeezed suffix.
                        if !self.detail
                            && let Some(current) = &todos.current
                        {
                            text.push_str(&format!(" (working: {current})"));
                        }
                        vec![Span::styled(text, theme.accent)]
                    })
            }
            // Rendered above the bar and at its right edge respectively;
            // `render_roster` owns both placements.
            StatuslineElement::Git | StatuslineElement::Hints => None,
        }
    }

    /// The `repo:<name> | branch:<branch>` line, values bold — the
    /// screenshot's pinned shape — re-reading HEAD only when its mtime moved.
    fn git_spans(&self, theme: &Theme) -> Option<Vec<Span<'static>>> {
        let mut cache = self.git.borrow_mut();
        let head = cache.head.clone()?;
        let repo = cache.repo.clone()?;

        match fs::metadata(&head).and_then(|meta| meta.modified()) {
            Ok(stamp) => {
                if cache.read_at != Some(stamp) {
                    cache.branch = fs::read_to_string(&head)
                        .ok()
                        .as_deref()
                        .and_then(head_name);
                    cache.read_at = Some(stamp);
                }
            }
            Err(_) => {
                // HEAD went away under us — a deleted worktree, most likely.
                // Say nothing rather than a stale branch.
                cache.branch = None;
                cache.read_at = None;
            }
        }

        let branch = cache.branch.clone()?;
        let bold = theme.fg.add_modifier(Modifier::BOLD);
        Some(vec![
            Span::styled("repo:".to_owned(), theme.dim),
            Span::styled(repo, bold),
            Span::styled(HUD_SEPARATOR.to_owned(), theme.dim),
            Span::styled("branch:".to_owned(), theme.dim),
            Span::styled(branch, bold),
        ])
    }

    /// What the detail line would say: the in-progress todo's title.
    fn detail_text(&self) -> Option<String> {
        self.todos
            .as_ref()
            .filter(|todos| todos.total > 0)
            .and_then(|todos| todos.current.clone())
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

/// Cuts `spans` at `limit` cells, ending a cut line with [`ELLIPSIS`] — OMC's
/// `truncateLineToMaxWidth`, over spans instead of ANSI strings.
fn truncate_spans(spans: Vec<Span<'static>>, limit: usize, theme: &Theme) -> Vec<Span<'static>> {
    let width: usize = spans.iter().map(|span| span.content.width()).sum();
    if width <= limit {
        return spans;
    }

    let target = limit.saturating_sub(ELLIPSIS.width());
    let mut kept: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in spans {
        let span_width = span.content.width();
        if used + span_width <= target {
            used += span_width;
            kept.push(span);
            continue;
        }
        // The span that crosses the cut is kept up to the last whole
        // character that fits, exactly as OMC walks code points.
        let mut partial = String::new();
        for character in span.content.chars() {
            let character_width = character.width().unwrap_or(0);
            if used + character_width > target {
                break;
            }
            used += character_width;
            partial.push(character);
        }
        if !partial.is_empty() {
            kept.push(Span::styled(partial, span.style));
        }
        break;
    }
    kept.push(Span::styled(ELLIPSIS.to_owned(), theme.dim));

    kept
}

/// One `label:[####----]NN%` meter, the screenshot's pinned shape. Built once
/// and shared so the rate-bucket elements (5h/week/spend) can ride it the day
/// ganja can ask a vendor usage API what they hold.
fn meter(label: &str, percent: u64, theme: &Theme) -> Vec<Span<'static>> {
    let percent = percent.min(100);
    let filled = meter_fill(percent);
    let style = meter_style(percent, theme);

    vec![
        Span::styled(format!("{label}:["), theme.dim),
        Span::styled("#".repeat(usize::try_from(filled).unwrap_or(0)), style),
        Span::styled(
            "-".repeat(usize::try_from(METER_SLOTS - filled).unwrap_or(0)),
            theme.dim,
        ),
        Span::styled("]".to_owned(), theme.dim),
        Span::styled(format!("{percent}%"), style),
    ]
}

/// How many of a meter's slots are filled at `percent`: OMC's
/// `Math.round((percent / 100) * barWidth)`, in integers.
fn meter_fill(percent: u64) -> u64 {
    (percent.min(100) * METER_SLOTS + 50) / 100
}

/// The OMC severity ladder (`types.ts` thresholds 70/80/85): ok, warning at
/// 70, compact-suggestion at 80, critical at 85 — where the fill paints the
/// error red the screenshot's exhausted meters show. OMC colors warning and
/// compact the same yellow and tells them apart with a ` COMPRESS?` text
/// suffix; the pinned `NN%` shape carries no suffix, so both wear the warning
/// style here and the 80 boundary lives in [`meter_severity`] and its test
/// rather than in a color nobody could see.
fn meter_style(percent: u64, theme: &Theme) -> Style {
    match meter_severity(percent) {
        Severity::Ok => theme.success,
        Severity::Warning | Severity::Compact => theme.warning,
        Severity::Critical => theme.error,
    }
}

/// Where `percent` sits on the OMC ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Severity {
    /// Below 70.
    Ok,
    /// 70 to 79: worth a glance.
    Warning,
    /// 80 to 84: OMC starts suggesting compaction here.
    Compact,
    /// 85 and up: about to hit the wall.
    Critical,
}

fn meter_severity(percent: u64) -> Severity {
    match percent {
        0..70 => Severity::Ok,
        70..80 => Severity::Warning,
        80..85 => Severity::Compact,
        _ => Severity::Critical,
    }
}

/// `tokens` as a whole percentage of `window`, clamped to 100 — the display
/// never claims more than full, whatever the estimate says.
fn percent_of(tokens: u64, window: u64) -> u64 {
    if window == 0 {
        return 100;
    }

    (tokens.saturating_mul(100) / window).min(100)
}

/// Finds the repository `dir` sits in: the nearest ancestor holding a `.git`,
/// through a linked worktree's one-line `gitdir: <path>` file.
fn discover_git(dir: &Path) -> GitCache {
    for ancestor in dir.ancestors() {
        let dotgit = ancestor.join(".git");
        let head = if dotgit.is_dir() {
            dotgit.join("HEAD")
        } else if dotgit.is_file() {
            let Some(pointed) = fs::read_to_string(&dotgit).ok().and_then(|text| {
                text.trim()
                    .strip_prefix("gitdir:")
                    .map(|path| path.trim().to_owned())
            }) else {
                continue;
            };
            let gitdir = PathBuf::from(pointed);
            let gitdir = if gitdir.is_absolute() {
                gitdir
            } else {
                ancestor.join(gitdir)
            };
            gitdir.join("HEAD")
        } else {
            continue;
        };

        return GitCache {
            head: Some(head),
            repo: ancestor
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            read_at: None,
            branch: None,
        };
    }

    GitCache::default()
}

/// What a HEAD file names: the branch of a symbolic ref, or the first eight
/// hex digits of a detached head.
fn head_name(text: &str) -> Option<String> {
    let text = text.trim();
    if let Some(reference) = text.strip_prefix("ref:") {
        let reference = reference.trim();
        let branch = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        return (!branch.is_empty()).then(|| branch.to_owned());
    }

    (!text.is_empty()).then(|| text.chars().take(8).collect())
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fs};

    use ganja_core::config::StatuslineElement;
    use ratatui::{buffer::Buffer, layout::Rect};
    use unicode_width::UnicodeWidthStr as _;

    use super::{
        Activity, HINTS, SHELL_HINTS, Severity, Status, Todos, Totals, discover_git, head_name,
        meter_fill, meter_severity,
    };
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

    /// Every row of a taller render, for the rosters that earn extra lines.
    fn rendered_rows(status: &Status, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        status.render(area, &mut buffer, &Theme::default());

        (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// A status bar rendering `elements`, without going through a config
    /// file.
    fn roster(elements: &[StatuslineElement]) -> Status {
        let mut status = Status::new(None);
        status.elements = Some(elements.to_vec());
        status
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

    /// The bypass is standing, so its marker is too (**D479**) — and a
    /// session that did not ask for it renders the bar this build always drew,
    /// which is what the rest of this file's expectations are written against.
    #[test]
    fn the_bar_carries_the_yolo_marker_only_in_a_bypassed_session() {
        let mut status = Status::new(None);
        let gated = rendered(&status, 100);
        assert!(!gated.contains("yolo"), "got {gated:?}");

        status.set_yolo(true);

        let bypassed = rendered(&status, 100);
        assert!(
            bypassed.starts_with("yolo"),
            "the marker is the first thing on the bar: {bypassed:?}"
        );
        assert!(
            bypassed.ends_with(HINTS),
            "and it takes its width out of the gap rather than off the hints: \
             {bypassed:?}"
        );

        status.set_yolo(false);
        assert_eq!(
            rendered(&status, 100),
            gated,
            "turning it off restores the bar cell for cell"
        );
    }

    /// The marker is not a roster element, so it does not depend on a config
    /// naming it: a `tui.statusline` table that leaves everything else out
    /// still says this session is bypassed (**D479**).
    #[test]
    fn a_configured_roster_carries_the_marker_too() {
        let mut status = roster(&[StatuslineElement::Activity]);
        assert!(!rendered(&status, 100).contains("yolo"));

        status.set_yolo(true);

        let line = rendered(&status, 100);
        assert!(line.starts_with("yolo"), "got {line:?}");
        assert!(
            line.contains("ready"),
            "and the roster it was prepended to is untouched: {line:?}"
        );
    }

    /// The depth appears only while something is waiting, so a session that
    /// never queues a message renders the bar it always did (**F4**).
    #[test]
    fn the_bar_names_the_queue_only_while_something_is_waiting() {
        let mut status = Status::new(None);
        assert!(!rendered(&status, 100).contains("queued"));

        status.set_queued(2);

        let line = rendered(&status, 100);
        assert!(line.contains("2 queued"), "got {line:?}");

        status.set_queued(0);
        assert!(!rendered(&status, 100).contains("queued"));
    }

    /// A running background job appears only while one is running, the same
    /// posture as the queue depth beside it (**F1**).
    #[test]
    fn the_bar_names_running_background_jobs_only_while_there_are_any() {
        let mut status = Status::new(None);
        assert!(!rendered(&status, 100).contains("bash running"));

        status.set_running_jobs(1);
        let line = rendered(&status, 100);
        assert!(line.contains("1 bash running"), "got {line:?}");

        status.set_running_jobs(0);
        assert!(!rendered(&status, 100).contains("bash running"));
    }

    /// The two segments concurrent children brought with them, under the same
    /// appear-only-while-nonzero posture as everything beside them (**D462**).
    #[test]
    fn the_bar_names_running_children_and_queued_dialogs_only_while_there_are_any() {
        let mut status = Status::new(None);
        let quiet = rendered(&status, 120);
        assert!(!quiet.contains("tasks running"));
        assert!(!quiet.contains("dialogs queued"));

        status.set_running_tasks(1);
        assert!(
            !rendered(&status, 120).contains("tasks running"),
            "one delegation is not a fan-out, and the activity segment names it"
        );

        status.set_running_tasks(3);
        status.set_queued_dialogs(1);
        let line = rendered(&status, 120);
        assert!(line.contains("3 tasks running"), "got {line:?}");
        assert!(line.contains("1 dialog queued"), "got {line:?}");

        status.set_queued_dialogs(2);
        assert!(rendered(&status, 120).contains("2 dialogs queued"));

        status.set_running_tasks(0);
        status.set_queued_dialogs(0);
        let quiet = rendered(&status, 120);
        assert!(!quiet.contains("tasks running"));
        assert!(!quiet.contains("dialogs queued"));
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

    /// The acceptance shape for the roster (**D469**): what the config names,
    /// in the order it names it, and nothing the default bar would have added
    /// on its own.
    #[test]
    fn a_configured_roster_renders_exactly_what_it_names_in_its_order() {
        let mut status = roster(&[
            StatuslineElement::Model,
            StatuslineElement::Context,
            StatuslineElement::Tokens,
        ]);
        status.set_model(Some("claude-opus-5".to_owned()));
        status.set_context(Some((12_000, 100_000)));
        status.set_totals(Totals {
            input_tokens: 40,
            output_tokens: 7,
            cost_usd: None,
        });

        let line = rendered(&status, 120);

        assert_eq!(
            line, "Model: claude-opus-5 | ctx:[#-------]12% | 40 in \u{b7} 7 out",
            "the three named elements, in the named order, and nothing else"
        );
        assert!(!line.contains("ready"), "the activity was not named");
    }

    /// The meter's percent is the estimate the app handed in, and 70% is
    /// where the fill stops being calm (**D469**).
    #[test]
    fn the_context_meter_shows_the_handed_in_percent_and_warns_at_seventy() {
        let theme = Theme::default();
        let calm = {
            let mut status = roster(&[StatuslineElement::Context]);
            status.set_context(Some((69, 100)));
            status
        };
        let line = rendered(&calm, 60);
        assert!(line.contains("ctx:["), "got {line:?}");
        assert!(line.contains("]69%"), "got {line:?}");

        let area = Rect::new(0, 0, 60, 1);
        let mut buffer = Buffer::empty(area);
        calm.render(area, &mut buffer, &theme);
        // Cell 5 is the first bar slot, right after "ctx:[".
        assert_eq!(buffer[(5, 0)].symbol(), "#");
        assert_eq!(buffer[(5, 0)].style().fg, theme.success.fg);

        let warned = {
            let mut status = roster(&[StatuslineElement::Context]);
            status.set_context(Some((70, 100)));
            status
        };
        let mut buffer = Buffer::empty(area);
        warned.render(area, &mut buffer, &theme);
        assert_eq!(buffer[(5, 0)].symbol(), "#");
        assert_eq!(
            buffer[(5, 0)].style().fg,
            theme.warning.fg,
            "seventy percent is where the meter starts warning"
        );
    }

    /// OMC's ladder, at its boundaries: fill is `round(percent/100 * 8)` and
    /// the color steps at 70, 80 and 85 — the last one the red the
    /// screenshot's exhausted meters wear.
    #[test]
    fn the_meter_math_holds_at_the_ladder_boundaries() {
        assert_eq!(meter_fill(0), 0);
        assert_eq!(meter_fill(69), 6);
        assert_eq!(meter_fill(70), 6);
        assert_eq!(meter_fill(84), 7);
        assert_eq!(meter_fill(85), 7);
        assert_eq!(meter_fill(100), 8);

        assert_eq!(meter_severity(0), Severity::Ok);
        assert_eq!(meter_severity(69), Severity::Ok);
        assert_eq!(meter_severity(70), Severity::Warning);
        assert_eq!(meter_severity(79), Severity::Warning);
        assert_eq!(meter_severity(80), Severity::Compact);
        assert_eq!(meter_severity(84), Severity::Compact);
        assert_eq!(meter_severity(85), Severity::Critical);
        assert_eq!(meter_severity(100), Severity::Critical);
    }

    /// An estimate past the window still reads as full, never as more.
    #[test]
    fn the_context_meter_never_claims_more_than_full() {
        let mut status = roster(&[StatuslineElement::Context]);
        status.set_context(Some((250_000, 100_000)));

        let line = rendered(&status, 60);

        assert!(line.contains("ctx:[########]100%"), "got {line:?}");
    }

    /// A roster wider than the bar ends with the OMC ellipsis instead of a
    /// silent cut.
    #[test]
    fn a_narrow_roster_truncates_with_an_ellipsis() {
        let mut status = roster(&[StatuslineElement::Model]);
        status.set_model(Some("a-model-with-a-very-long-name".to_owned()));

        let line = rendered(&status, 20);

        assert_eq!(line.width(), 20, "got {line:?}");
        assert!(line.ends_with("..."), "got {line:?}");
    }

    /// `max_width` caps the bar below the terminal's width, OMC's `maxWidth`.
    #[test]
    fn a_width_cap_truncates_before_the_terminal_edge_does() {
        let mut status = roster(&[StatuslineElement::Model]);
        status.max_width = Some(20);
        status.set_model(Some("a-model-with-a-very-long-name".to_owned()));

        let line = rendered(&status, 120);

        assert!(line.ends_with("..."), "got {line:?}");
        assert!(line.len() <= 20, "got {line:?}");
    }

    /// The git line reads `.git/HEAD` off the disk — both spellings — and
    /// sits above the main line, the screenshot's pinned placement.
    #[test]
    fn the_git_line_reads_a_symbolic_head_as_its_branch() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let repo = directory.path().join("my-repo");
        fs::create_dir_all(repo.join(".git")).expect("the fixture repository is creatable");
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD writes");

        let mut status = roster(&[StatuslineElement::Git, StatuslineElement::Activity]);
        status.git = RefCell::new(discover_git(&repo));

        let rows = rendered_rows(&status, 60, 2);
        assert_eq!(rows[0], "repo:my-repo | branch:main");
        assert_eq!(rows[1], "ready");
        assert_eq!(status.height(), 2);
    }

    #[test]
    fn a_detached_head_reads_as_a_short_hash() {
        assert_eq!(
            head_name("f0e1d2c3b4a5968778695a4b3c2d1e0f11223344\n").as_deref(),
            Some("f0e1d2c3")
        );
        assert_eq!(
            head_name("ref: refs/heads/feature/x\n").as_deref(),
            Some("feature/x")
        );
        assert_eq!(head_name(""), None);
    }

    /// The cache re-reads HEAD only when its mtime moves — the whole point of
    /// the file read over a subprocess (P14 review changelog MINOR 4).
    #[test]
    fn the_git_line_follows_a_branch_switch_through_the_head_mtime() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let repo = directory.path().join("repo");
        fs::create_dir_all(repo.join(".git")).expect("the fixture repository is creatable");
        let head = repo.join(".git/HEAD");
        fs::write(&head, "ref: refs/heads/main\n").expect("HEAD writes");

        let mut status = roster(&[StatuslineElement::Git]);
        status.git = RefCell::new(discover_git(&repo));
        assert!(rendered_rows(&status, 60, 2)[0].contains("branch:main"));

        // A fresh mtime, far enough from the first write that coarse
        // filesystem timestamps cannot collapse the two.
        fs::write(&head, "ref: refs/heads/next\n").expect("HEAD rewrites");
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let stamp = fs::FileTimes::new().set_modified(bumped);
        fs::File::options()
            .append(true)
            .open(&head)
            .and_then(|file| file.set_times(stamp))
            .expect("the mtime moves");

        assert!(
            rendered_rows(&status, 60, 2)[0].contains("branch:next"),
            "a moved mtime invalidates the cache"
        );
    }

    /// A linked worktree's `.git` is a file pointing at the real gitdir; the
    /// line still names the worktree's own directory and branch.
    #[test]
    fn a_linked_worktree_resolves_head_through_its_gitdir_pointer() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let gitdir = directory.path().join("main/.git/worktrees/wt");
        fs::create_dir_all(&gitdir).expect("the fixture gitdir is creatable");
        fs::write(gitdir.join("HEAD"), "ref: refs/heads/hotfix\n").expect("HEAD writes");
        let worktree = directory.path().join("wt");
        fs::create_dir_all(&worktree).expect("the fixture worktree is creatable");
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect("the pointer writes");

        let mut status = roster(&[StatuslineElement::Git]);
        status.git = RefCell::new(discover_git(&worktree));

        assert_eq!(rendered_rows(&status, 60, 2)[0], "repo:wt | branch:hotfix");
    }

    /// A too-short area drops the detail line first and the git line second —
    /// the main line is the one row that never yields.
    #[test]
    fn a_short_area_keeps_the_main_line_over_the_extra_ones() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let repo = directory.path().join("repo");
        fs::create_dir_all(repo.join(".git")).expect("the fixture repository is creatable");
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD writes");

        let mut status = roster(&[StatuslineElement::Git, StatuslineElement::Activity]);
        status.git = RefCell::new(discover_git(&repo));

        let rows = rendered_rows(&status, 60, 1);
        assert_eq!(rows[0], "ready", "one row leaves only the main line");
    }

    /// The `todos` element carries its progress inline and moves the title to
    /// the detail line when one is allowed.
    #[test]
    fn todos_move_their_title_to_the_detail_line_when_detail_is_on() {
        let mut status = roster(&[StatuslineElement::Todos]);
        status.set_todos(Some(Todos {
            done: 2,
            total: 5,
            current: Some("wire the meter".to_owned()),
        }));

        assert_eq!(rendered(&status, 80), "todos:2/5 (working: wire the meter)");

        status.detail = true;
        let rows = rendered_rows(&status, 80, 2);
        assert_eq!(rows[0], "todos:2/5");
        assert_eq!(rows[1], "working: wire the meter");
        assert_eq!(status.height(), 2);
    }

    /// The session element counts from the bar's own birth, in OMC's
    /// whole-minute form.
    #[test]
    fn the_session_element_counts_whole_minutes_from_birth() {
        let status = roster(&[StatuslineElement::Session]);

        assert_eq!(rendered(&status, 40), "session:0m");
    }

    /// The `hints` element keeps the right edge the default bar gave it.
    #[test]
    fn a_roster_with_hints_keeps_them_right_aligned() {
        let status = roster(&[StatuslineElement::Activity, StatuslineElement::Hints]);

        let line = rendered(&status, 100);
        assert!(line.starts_with("ready"), "got {line:?}");
        assert!(line.ends_with(HINTS), "got {line:?}");
    }
}
