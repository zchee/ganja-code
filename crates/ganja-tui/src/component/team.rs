//! The `/teammate` dialog: one row per member of this session's team — its name,
//! the surface it runs on, whether it is the lead, and the ring of what it
//! most recently did — with the actions a row offers behind Enter, and a
//! Spawn row that belongs to the team rather than to any member. Under all of
//! it, the team's **shared task list**: what has been filed, where each task
//! is, who holds it and what it waits on, drawn from the same listing
//! `task_list` answers a model with and naming, when there is somebody to
//! name, the members that cannot see it.
//!
//! Upstream opencode has no team, no teammates and no surface for either, so
//! nothing here cites an upstream file. What it ports is Claude Code's
//! **§4** — the spawn sequence its `--backend`/`--agent` grammar comes from
//! (§4.1), the surfaces a member may run on (§4.2) and the colour a team
//! assigns each of them (§4.3) — with §6.1's shutdown as the other action a row
//! offers. The two-step shape is [`crate::component::mcp::Mcp`]'s and the
//! row-independent action plus free-text step are
//! [`crate::component::plugin::Plugin`]'s — the same grammar every dialog in
//! this frontend already taught, applied to a new subject.
//!
//! Two decisions of the landing show up as code here:
//!
//! - **The ring under each row is D503's second half.** The in-process
//!   backend is the default and has no window of its own, so without a
//!   surface the most-used teammate would be the least observable one. The
//!   ring is live registry state, reaching this component through
//!   `MemberView::recent_calls` — ganja's own protocol projection — and never
//!   through Claude's member record, which is somebody else's document.
//! - **Nothing stands in front of a spawn.** Resolution 4 of the landing:
//!   `/teammate spawn` raises no confirmation dialog, because a person typing a
//!   spawn is the consent. What a person cannot see is where the prompt they
//!   just typed came to rest, so the one thing said afterwards is that —
//!   [`Team::spawned`]'s notice, naming the cleartext path (D-7).
//!
//! Submitting an [`Effect`] to the engine and re-polling the team are
//! [`crate::app::App`]'s, not this component's — the same split every other
//! dialog here keeps. What this component owns is its own state and its own
//! notice line.

use ganja_protocol::{MemberBackend, MemberView, TeamView};
use ganja_tool::task::TeammateSpawn;
// `UNOWNED` is the word `task_list` answers a model with for a task nobody
// holds, imported rather than restated here: a second spelling is a second
// place for the dialog and the tool to drift into naming one state two ways,
// and the tool's own constant says it is the one.
use ganja_tool::tasklist::{Summary, UNOWNED};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Widget as _};
use unicode_width::UnicodeWidthStr as _;

use crate::command::TeamSpawn;
use crate::component::chat::{RESULT, clip, pad};
use crate::component::{
    ACTION_HINTS, CHROME, INPUT_HINTS, LIST_HINTS, MARKER, MAX_HEIGHT, MAX_WIDTH, TwoStep,
    action_row, body_rows, clamped, first_visible,
};
use crate::theme::Theme;

/// How many of a member's recent calls a row shows. The registry's own ring
/// holds `ganja_core::teammate::RECENT_CALLS` of them, which is more than a
/// dialog with several members can spend on one; the newest are the ones that
/// answer "what is it doing right now".
const RING_LINES: usize = 4;

/// What is shown when the team holds nobody at all.
const EMPTY: &str = "no team members";

/// The label of the action that belongs to the team rather than to a row.
const SPAWN_LABEL: &str = "Spawn teammate\u{2026}";

/// What a spawn is refused with while another one is still starting.
///
/// Shown by this dialog when the refusal is one it can make itself — Spawn
/// does not even open its input step while a spawn is in flight — and by
/// [`crate::app::App`] for the ones only it can catch, so both doors say the
/// same thing. The `/plugin` dialog's own [`crate::component::plugin::BUSY`]
/// posture, for the same reason: letting a person type a whole spawn line
/// that is going to be refused anyway is worse than saying so at the keypress.
pub const BUSY: &str =
    "a teammate is already starting \u{b7} wait for it to finish, or Esc to close";

/// The half of a spawn's notice that has to be said out loud (D-7,
/// Resolution 4): the prompt a person just typed is persisted verbatim, so a
/// credential in it is on disk in cleartext, and the only honest moment to
/// say so is right after the spawn nothing stood in front of.
const CLEARTEXT: &str = "prompt persisted in cleartext at";

/// What the lead's row is marked with.
const LEAD: &str = "lead";

/// What the tasks section is headed with, when there is one.
const TASKS: &str = "tasks";

/// What the dim line under the heading says about the members that cannot see
/// this list, ahead of their names.
///
/// The one sentence this section exists to be honest about (the plan's third
/// risk): a `claude` member runs Claude Code's own task store and a `codex`,
/// `grok` or `agy` member holds no ganja tools at all, so a section drawn
/// under a roster holding any of them would otherwise read as work the whole
/// roster can see. It says *that they cannot*, and nothing about what they
/// keep instead: what a foreign surface does with its own work is not
/// something this dialog is in a position to state.
const UNSHARED: &str = "not visible to";

/// One member of the team, as the dialog shows it.
///
/// A projection rather than the protocol's own `MemberView`, which is the
/// shape every other dialog here takes: what a row needs to draw is decided by
/// the dialog, and a component that held the engine's value would re-decide it
/// every time that value grew a field. [`rows`] is the one place the
/// projection is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The bare member name, which is also its mailbox address and what a
    /// chosen action names to the engine.
    pub name: String,
    /// The surface it runs on.
    pub backend: MemberBackend,
    /// Whether it is the team's lead — which is to say, this session.
    pub is_lead: bool,
    /// §4.3's assigned colour, where one was assigned.
    pub color: Option<String>,
    /// The **D503** ring: one-line summaries of what it most recently did,
    /// newest last.
    pub recent: Vec<String>,
}

impl Row {
    /// One member, as this dialog shows it.
    #[must_use]
    fn of(member: &MemberView) -> Self {
        Self {
            name: member.name.clone(),
            backend: member.backend,
            is_lead: member.is_lead,
            color: member.color.clone(),
            recent: member.recent_calls.clone(),
        }
    }
}

/// A whole team, as this dialog shows it — what a caller polling
/// `TeammateRegistry::view()` hands to [`Team::new`] and [`Team::refresh`].
///
/// The lead comes first, because it is the row a person looks for to know
/// which session they are in; the registry already orders it that way, and
/// stating it here means a registry that stopped would not silently move it.
#[must_use]
pub fn rows(view: &TeamView) -> Vec<Row> {
    let mut rows: Vec<Row> = view.members.iter().map(Row::of).collect();
    rows.sort_by_key(|row| !row.is_lead);

    rows
}

/// Whether a member running on `backend` reads the same list this section
/// draws.
///
/// Exhaustive rather than a catch-all, so a seventh surface is a decision
/// somebody makes here rather than a member quietly listed as seeing work it
/// cannot: the two arms below are ganja's own task tools reaching a shared
/// directory, and every other surface is somebody else's agent.
const fn shares_the_list(backend: MemberBackend) -> bool {
    match backend {
        // This process's own teammate, and a `ganja` pane: both are offered
        // the four `task_*` tools over this same team directory.
        MemberBackend::InProcess | MemberBackend::Ganja => true,
        // Claude Code keeps its task list inside its own process, and the
        // three foreign CLIs hold no ganja tools at all.
        MemberBackend::Claude | MemberBackend::Codex | MemberBackend::Agy | MemberBackend::Grok => {
            false
        }
    }
}

/// A spawn as this dialog asks for it.
///
/// What comes back is [`TeammateSpawn`] itself — the **`task` tool's own
/// request value**, the very type its teammate door hands to the engine — so a
/// spawn typed here and a spawn a model asked for are the same value and cannot
/// drift apart (AC-14, **D504**). Until 2026-08-22 this door wrapped that value
/// beside a `bypass` the `task` door had no argument for (D-5); **D513**
/// retired the flag and the axis beneath it, and with it the wrapper — there is
/// one request on both doors and nothing left for them to differ on.
///
/// The agent kind is the one place this door fills in what the other one is
/// always told: `subagent_type` is a required `task` argument, and a person who
/// did not name a kind means the roster's general-purpose one. Named from
/// `ganja_core` rather than spelled here, so the default is the same string the
/// engine's own roster registers.
///
/// `backend` is **as typed**, unparsed: which surfaces exist is the engine's to
/// answer, and a second list here would be a second place for them to drift.
#[must_use]
pub fn spawn_request(line: &TeamSpawn) -> TeammateSpawn {
    TeammateSpawn {
        name: line.name.clone(),
        backend: line.backend.clone(),
        agent_type: line
            .agent_type
            .clone()
            .unwrap_or_else(|| ganja_core::agent::GENERAL.to_owned()),
        prompt: line.prompt.clone(),
    }
}

/// What a submitted spawn came back with, as the notice line needs it.
///
/// The path is the caller's to supply because it is the engine's fact — where
/// this team keeps its documents — and a dialog that computed it would be a
/// frontend deciding where a store lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawned {
    /// The name the team really gave it, which is not always the one asked
    /// for: a collision is resolved with a counter rather than refused.
    pub name: String,
    /// Where the spawn prompt was persisted verbatim.
    pub prompt_path: String,
}

impl Spawned {
    /// The one sentence a finished spawn is reported with, wherever it is
    /// reported.
    ///
    /// A method rather than a `format!` at each caller because the dialog is
    /// not always open to hold it: a `/teammate spawn` line typed at the composer
    /// raises no dialog at all and reports into the status bar instead. The
    /// half of this sentence that must survive that is the second one —
    /// Resolution 4's disclosure that the prompt is on disk in cleartext — and
    /// the way to keep a shorter spelling from dropping it is for there to be
    /// no shorter spelling.
    #[must_use]
    pub fn notice(&self) -> String {
        format!(
            "{name} started \u{b7} {CLEARTEXT} {path}",
            name = self.name,
            path = self.prompt_path,
        )
    }
}

/// What Enter resolved to — everything the app has to act on. Movement and
/// step changes stay inside the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Start a teammate, through the same door a `task` call reaches.
    Spawn {
        /// The spawn, parsed — the `task` door's own value ([`spawn_request`]).
        request: TeammateSpawn,
        /// The words as typed into the step — [`crate::command::SPAWN_GRAMMAR`]'s
        /// shape, without the `/teammate spawn` a composer line carries — so the
        /// app can remember the spawn in the prompt history as the line it
        /// is equivalent to, and an Up-arrow can bring it back to edit.
        typed: String,
    },
    /// Send what was typed to one member.
    ///
    /// Not remembered in the prompt history, on purpose: recalled into the
    /// composer it would be sent to the lead's own model, which is not where
    /// it went.
    Message {
        /// Who it goes to.
        to: String,
        /// What was typed.
        text: String,
    },
    /// Ask the named member to shut down.
    Shutdown(String),
}

/// What Enter on a member row offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowAction {
    /// Write a message into that member's inbox.
    Message,
    /// Ask it to shut down.
    Shutdown,
}

impl RowAction {
    /// The label the action step shows for it.
    fn label(self) -> &'static str {
        match self {
            Self::Message => "Message\u{2026}",
            Self::Shutdown => "Shutdown",
        }
    }
}

/// What a free-text step is collecting.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Asking {
    /// A spawn line, in `/teammate spawn`'s own grammar.
    Spawn,
    /// A message for the named member.
    Message(String),
}

/// Which of the dialog's steps is on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// Choosing a member row or the Spawn row under them.
    Members,
    /// Choosing one of a named member's actions.
    ///
    /// The **name** rather than the cursor's index, because this dialog
    /// re-polls the roster on every tick: a teammate that retires or spawns
    /// while somebody is deciding moves the rows under an index, and Enter
    /// would then act on whoever had slid into that slot. A name cannot slide,
    /// and one that leaves the roster drops the step
    /// ([`Team::refresh`]) rather than resolving to a stranger.
    Actions {
        /// Whose actions these are.
        member: String,
        /// Which of them the cursor is on.
        option: usize,
    },
    /// Typing the text a step needs.
    Input {
        /// What the text is for.
        asking: Asking,
        /// What has been typed so far.
        buffer: String,
    },
}

/// The team, which row the cursor is on, which step is showing, and what the
/// last action had to say.
#[derive(Clone, Debug)]
pub struct Team {
    rows: Vec<Row>,
    /// The team's shared task list, exactly as `Engine::task_list` answered
    /// it — drawn under the roster, selected by nothing. Empty draws no
    /// section at all.
    ///
    /// **In the order it arrived**, which is the store's own lowest-id-first,
    /// and deliberately not re-sorted here: the dialog and the `task_list` a
    /// model reads are two renderings of one listing, and a second ordering
    /// would be a second answer to "which is the next task".
    tasks: Vec<Summary>,
    /// Index over the member rows *and* the Spawn row after them; always in
    /// range, because the Spawn row makes the list non-empty.
    selected: usize,
    step: Step,
    /// A refused spawn line, a spawn's cleartext notice, or whatever the app
    /// had to say about the action it just ran.
    notice: Option<String>,
    /// Whether a spawn the app started is still running off the loop.
    busy: bool,
}

impl Team {
    /// Opens the dialog over `rows` and the team's `tasks`, cursor on the
    /// first member — or on the Spawn row when the team holds nobody.
    ///
    /// The tasks travel beside the roster rather than through a setter of
    /// their own because they arrive together: one tick polls both, and a
    /// dialog that could hold a roster from now and a list from a minute ago
    /// would be a dialog able to show a member owning a task that no longer
    /// exists.
    #[must_use]
    pub fn new(rows: Vec<Row>, tasks: Vec<Summary>) -> Self {
        Self { rows, tasks, selected: 0, step: Step::Members, notice: None, busy: false }
    }

    /// Replaces the rows and the task list with a fresh poll, keeping the
    /// cursor and the step
    /// where they were — reclamped, because a shutdown shrinks the roster
    /// under it. A ring growing under a person mid-decision must not move what
    /// their next keypress lands on, which is the whole reason this is not a
    /// fresh [`Team::new`] every tick.
    ///
    /// The one thing a refresh **does** move is an action step whose member is
    /// gone: there is no honest way to keep offering Shutdown for a teammate
    /// that has shut down, so the step drops back to the roster and the person
    /// chooses again.
    ///
    /// Answers whether anything really changed, so a caller polling every tick
    /// repaints only when it did. A `/teammate` dialog left open would otherwise
    /// mark every one of those ticks dirty and redraw the screen at frame rate
    /// for a roster nobody touched.
    pub fn refresh(&mut self, rows: Vec<Row>, tasks: Vec<Summary>) -> bool {
        let mut moved = rows != self.rows || tasks != self.tasks;
        self.rows = rows;
        self.tasks = tasks;
        self.selected = self.selected.min(self.total_rows().saturating_sub(1));
        let orphaned = match &self.step {
            Step::Actions { member, .. } => self.row_named(member).is_none(),
            Step::Members | Step::Input { .. } => false,
        };
        if orphaned {
            self.step = Step::Members;
            moved = true;
        }

        moved
    }

    /// Sets the notice line the next frame shows.
    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// Says a spawn succeeded, and says the one thing about it that nothing
    /// asked first (Resolution 4): where the prompt now sits in cleartext.
    ///
    /// The door to call after a spawn, rather than [`Team::set_notice`] with a
    /// sentence of the caller's own: this is where that sentence is written,
    /// so it is written once.
    pub fn spawned(&mut self, outcome: &Spawned) {
        self.notice = Some(outcome.notice());
    }

    /// Says whether a spawn the app started is still running, which is what
    /// dims the Spawn row and refuses a second one.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    /// Whether such a spawn is running.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_busy(&self) -> bool {
        self.busy
    }

    /// Whether the free-text step currently owns the keyboard.
    #[must_use]
    pub fn is_typing(&self) -> bool {
        matches!(self.step, Step::Input { .. })
    }

    /// Whether the per-member action step is the one on screen.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_choosing_action(&self) -> bool {
        matches!(self.step, Step::Actions { .. })
    }

    /// What has been typed into the free-text step.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn input(&self) -> Option<&str> {
        match &self.step {
            Step::Input { buffer, .. } => Some(buffer.as_str()),
            Step::Members | Step::Actions { .. } => None,
        }
    }

    /// The member under the cursor, or [`None`] when the cursor is on the
    /// Spawn row.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn selected_member(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// The row a name belongs to, which is how an action step finds the member
    /// it was opened for however the roster has moved since.
    fn row_named(&self, name: &str) -> Option<&Row> {
        self.rows.iter().find(|row| row.name == name)
    }

    /// Every position the member-step cursor can land on: the members, then
    /// the Spawn row.
    fn total_rows(&self) -> usize {
        self.rows.len() + 1
    }

    /// The actions a member row offers.
    ///
    /// The lead is not a member anybody may shut down from here: it is this
    /// session, and the door out of this session is `/exit`. Messaging it
    /// would be talking to oneself, so its row offers nothing and Enter on it
    /// leaves the dialog where it was.
    fn actions(row: &Row) -> &'static [RowAction] {
        if row.is_lead { &[] } else { &[RowAction::Message, RowAction::Shutdown] }
    }

    /// Moves whichever list is showing by `delta` rows. The free-text step
    /// has no rows to move.
    pub fn move_selection(&mut self, delta: isize) {
        match &self.step {
            Step::Members => self.selected = clamped(self.selected, delta, self.total_rows()),
            Step::Actions { member, option } => {
                let count = self.row_named(member).map_or(0, |row| Self::actions(row).len());
                let moved = clamped(*option, delta, count);
                self.step = Step::Actions { member: member.clone(), option: moved };
            }
            Step::Input { .. } => {}
        }
    }

    /// Adds `character` while the free-text step owns the keyboard.
    pub fn push(&mut self, character: char) {
        if let Step::Input { buffer, .. } = &mut self.step {
            buffer.push(character);
        }
    }

    /// Takes the last character back off while the free-text step owns the
    /// keyboard.
    pub fn backspace(&mut self) {
        if let Step::Input { buffer, .. } = &mut self.step {
            buffer.pop();
        }
    }

    /// Esc: leaves the free-text step for the member list, keeping the dialog
    /// open and sending nothing anywhere — the typed text is abandoned, never
    /// submitted. Answers whether the key was consumed; `false` means the
    /// dialog itself should close, which is what Esc means on the other two
    /// steps, exactly as it does in the `/mcp` and `/plugin` dialogs.
    pub fn cancel(&mut self) -> bool {
        if self.is_typing() {
            self.step = Step::Members;
            self.notice = None;
            return true;
        }

        false
    }

    /// Enter, wherever the dialog is: steps forward where a step is what Enter
    /// means, and answers with the [`Effect`] the app has to run where a
    /// decision was made.
    ///
    /// A spawn line the grammar refuses does not close the step: the refusal
    /// goes on the notice line and the text stays, because the answer to a
    /// mistyped flag is to fix that word rather than to type the line again.
    pub fn submit(&mut self) -> Option<Effect> {
        match &mut self.step {
            Step::Members => {
                if let Some(row) = self.rows.get(self.selected) {
                    // A row with nothing to offer leaves the dialog exactly as
                    // it was — the `/mcp` dialog's own answer for a row it
                    // cannot act on.
                    if Self::actions(row).is_empty() {
                        return None;
                    }
                    // The name is taken here, at the keypress a person made
                    // while looking at this row, and is what every later step
                    // resolves against.
                    self.step = Step::Actions { member: row.name.clone(), option: 0 };
                    return None;
                }
                if self.busy {
                    self.notice = Some(BUSY.to_owned());
                    return None;
                }
                self.step = Step::Input { asking: Asking::Spawn, buffer: String::new() };

                None
            }
            Step::Actions { member, option } => {
                let option = *option;
                let member = member.clone();
                // Resolved by the name this step was opened for, never by the
                // cursor: a poll that arrived mid-decision may have moved every
                // row, and a member that left the roster is answered by
                // `refresh` dropping this step before Enter is ever read.
                let row = self.row_named(&member)?;
                let effect = match *Self::actions(row).get(option)? {
                    RowAction::Shutdown => Effect::Shutdown(member),
                    RowAction::Message => {
                        self.step =
                            Step::Input { asking: Asking::Message(member), buffer: String::new() };

                        return None;
                    }
                };
                // Back to the list so the outcome shows up on the row the
                // app's next poll repaints.
                self.step = Step::Members;

                Some(effect)
            }
            Step::Input { asking, buffer } => {
                let typed = buffer.trim().to_owned();
                if typed.is_empty() {
                    // Nothing to submit is not a decision; the step stays.
                    return None;
                }
                match asking {
                    Asking::Message(member) => {
                        let effect = Effect::Message { to: member.clone(), text: typed };
                        self.step = Step::Members;

                        Some(effect)
                    }
                    Asking::Spawn => match crate::command::team_spawn(&typed) {
                        Ok(line) => {
                            let effect = Effect::Spawn { request: spawn_request(&line), typed };
                            self.step = Step::Members;

                            Some(effect)
                        }
                        Err(refusal) => {
                            self.notice = Some(refusal);

                            None
                        }
                    },
                }
            }
        }
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
        let available = area.height.saturating_sub(2).clamp(1, MAX_HEIGHT);
        let inner_width = usize::from(width).saturating_sub(2);
        // The notice takes a row of the list's budget when it has something to
        // say.
        let rows = body_rows(available, CHROME + usize::from(self.notice.is_some()));

        let mut lines = match &self.step {
            Step::Members => self.member_rows(inner_width, rows, theme),
            Step::Actions { member, option } => {
                self.action_rows(inner_width, member, *option, theme)
            }
            Step::Input { asking, buffer } => Self::input_rows(inner_width, asking, buffer, theme),
        };
        if let Some(notice) = &self.notice {
            let first = notice.lines().next().unwrap_or(notice).trim();
            lines.push(Line::styled(clip(first, inner_width), theme.dim));
        }
        let hints = match &self.step {
            Step::Members => LIST_HINTS,
            Step::Actions { .. } => ACTION_HINTS,
            Step::Input { .. } => INPUT_HINTS,
        };
        lines.push(Line::raw(""));
        lines.push(Line::styled(clip(hints, inner_width), theme.dim));

        // The member step takes the screenful it was given, since the roster
        // is as long as the team is. The other two are a handful of rows and
        // never grow, so they take exactly what they need — the `/mcp`
        // dialog's own two-height scheme.
        let height = match &self.step {
            Step::Members => available,
            Step::Actions { .. } | Step::Input { .. } => {
                u16::try_from(lines.len().saturating_add(2)).unwrap_or(available).min(available)
            }
        };
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" teammate "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// The member step: one line per member with its ring hanging under it,
    /// then the Spawn row.
    fn member_rows(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let name_width = self.rows.iter().map(|row| row.name.width()).max().unwrap_or(0);
        let backend_width =
            self.rows.iter().map(|row| backend_label(row.backend).width()).max().unwrap_or(0);

        let mut lines: Vec<Line<'static>> = Vec::new();
        // Where the cursor's own line ends up once the rings have pushed the
        // rows apart — what the scroll window has to keep on screen.
        let mut selected_line = 0;
        if self.rows.is_empty() {
            lines.push(Line::styled(clip(EMPTY, width), theme.dim));
        }
        for (index, row) in self.rows.iter().enumerate() {
            if index == self.selected {
                selected_line = lines.len();
            }
            let head = format!(
                "{marker}{name}  {backend}  {lead}",
                marker = if index == self.selected { MARKER } else { "  " },
                name = pad(&row.name, name_width),
                backend = pad(backend_label(row.backend), backend_width),
                lead = if row.is_lead { LEAD } else { "" },
            );
            let line = clip(head.trim_end(), width);
            lines.push(Line::styled(
                pad(&line, width),
                if index == self.selected { theme.selection } else { theme.fg },
            ));
            lines.extend(ring_rows(&row.recent, width, theme));
        }
        lines.push(Line::raw(""));
        let on_spawn = self.selected == self.rows.len();
        if on_spawn {
            selected_line = lines.len();
        }
        // A spawn in flight dims the row that would race it, so the refusal on
        // the notice line is not the first a person hears of it.
        let style = if self.busy {
            theme.dim
        } else if on_spawn {
            theme.accent
        } else {
            theme.fg
        };
        lines.push(Line::styled(
            clip(
                &format!("{marker}{SPAWN_LABEL}", marker = if on_spawn { MARKER } else { "  " },),
                width,
            ),
            style,
        ));

        lines.extend(self.task_lines(width, theme));

        let first = first_visible(selected_line, rows);

        lines.into_iter().skip(first).take(rows).collect()
    }

    /// The Tasks section: the team's shared list under the roster, one line
    /// per task — its id, where it is, who holds it, what it waits on and
    /// what it is.
    ///
    /// **Under the Spawn row rather than between it and the members**, so the
    /// rows a cursor can land on stay one unbroken run: a section nothing
    /// selects sitting inside that run would put lines between a person's eye
    /// and the row their next keypress moves to. It is drawn inside the same
    /// scroll window as everything else, which makes it a window onto the
    /// **head** of the list rather than a scrolling one: nothing below the
    /// Spawn row is selectable, so the window never travels down to these
    /// lines, and a list longer than the rows left under the roster is cut at
    /// the bottom with no marker. The roster stays on screen however long the
    /// list grows, which is what the placement is for.
    ///
    /// An empty list draws **nothing at all** — no heading, no placeholder.
    /// A team that has filed no task is the ordinary state of every session
    /// that never uses the list, and a heading over nothing would cost those
    /// two rows forever to say what their absence already says. (`no team
    /// members` is the other way round for the other reason: a roster is the
    /// thing this dialog is *for*, so an empty one is news.)
    fn task_lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.tasks.is_empty() {
            return Vec::new();
        }

        let id_width = self.tasks.iter().map(|task| printable(&task.id).width()).max().unwrap_or(0);
        let status_width =
            self.tasks.iter().map(|task| task.status.as_str().width()).max().unwrap_or(0);
        let owner_width = self
            .tasks
            .iter()
            .map(|task| printable(owner_label(&task.owner)).width())
            .max()
            .unwrap_or(0);

        let mut lines = vec![Line::raw(""), Line::styled(clip(TASKS, width), theme.fg)];
        if let Some(unshared) = self.unshared_line() {
            lines.push(Line::styled(clip(&unshared, width), theme.dim));
        }
        for task in &self.tasks {
            let head = format!(
                "  {id}  {status}  {owner}  ",
                id = pad(&printable(&task.id), id_width),
                status = pad(task.status.as_str(), status_width),
                owner = pad(&printable(owner_label(&task.owner)), owner_width),
            );
            let blocked = if task.blocked_by.is_empty() {
                String::new()
            } else {
                format!("  (blocked by {})", printable(&task.blocked_by.join(", ")))
            };
            let subject = printable(&task.subject);
            // **The suffix is never cut part-way.** What a task waits on is
            // the one thing on this line a reader acts on, and half of it —
            // `(blocked by 1` — reads as a fact about a different task. So the
            // subject is what gives way first, and the suffix survives whole
            // for as long as one column of subject is left beside it; past
            // that it is dropped whole rather than shown as a fragment, and
            // the row says what the task is instead. The list is one
            // `task_get` away either way.
            //
            // The rule is decided on what the cut actually **kept**, never on
            // the room it was offered: [`clip`] consumes at least one grapheme
            // cluster whatever the budget, so a subject opening on a
            // two-column glyph comes back two columns wide out of one column
            // of room, and composing the suffix beside it would overrun the
            // row — and what a `Paragraph` then cuts off the end is the suffix
            // this rule exists to keep whole. Measuring `kept` is what holds
            // both halves: the composed line never exceeds `width`, and the
            // suffix is whole or absent.
            let room = width.saturating_sub(head.width() + blocked.width());
            let kept = clip(&subject, room);
            let line = if blocked.is_empty() || room == 0 || kept.width() > room {
                clip(format!("{head}{subject}").trim_end(), width)
            } else {
                format!("{head}{kept}{blocked}")
            };
            lines.push(Line::styled(line, theme.fg));
        }

        lines
    }

    /// The dim line naming the members that cannot see this list, or [`None`]
    /// when every member can.
    ///
    /// Named rather than counted, and drawn only when there is somebody to
    /// name: a standing disclaimer under every team would be read past, where
    /// two names beside the list are the fact somebody needs at the moment
    /// they are wondering why a member has not picked anything up.
    fn unshared_line(&self) -> Option<String> {
        let unshared: Vec<&str> = self
            .rows
            .iter()
            // The lead is this session, which is the session drawing the
            // list; whatever its own row says it runs on, it is looking at
            // the list right now.
            .filter(|row| !row.is_lead && !shares_the_list(row.backend))
            .map(|row| row.name.as_str())
            .collect();

        (!unshared.is_empty()).then(|| format!("  {UNSHARED} {}", unshared.join(", ")))
    }

    /// The per-member action step: which member it is about, then what can be
    /// done to it.
    ///
    /// Resolved by name for [`Step::Actions`]'s reason. A name with no row is
    /// unreachable — [`Team::refresh`] drops the step the moment one leaves —
    /// and is drawn as the empty roster rather than unwrapped, because the cost
    /// of being wrong here is a panic in somebody's render.
    fn action_rows(
        &self,
        width: usize,
        member: &str,
        option: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let Some(row) = self.row_named(member) else {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        };

        let mut lines = vec![Line::styled(clip(&row.name, width), theme.fg)];
        for (index, action) in Self::actions(row).iter().enumerate() {
            lines.push(action_row(index, option, action.label(), width, theme));
        }

        lines
    }

    /// The free-text step: what is being asked for, and the line being typed —
    /// the question dialog's own editing rendering, block cursor included,
    /// with no engine on the other end of it.
    fn input_rows(
        width: usize,
        asking: &Asking,
        buffer: &str,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let prompt = match asking {
            // The same grammar `/teammate spawn` takes, spelled by the constant
            // its refusal names, because [`crate::command::team_spawn`] is the
            // one parser both doors feed.
            Asking::Spawn => format!("Spawn: {}", crate::command::SPAWN_GRAMMAR),
            Asking::Message(member) => format!("Message {member}:"),
        };

        vec![
            Line::styled(clip(&prompt, width), theme.fg),
            Line::styled(
                clip(&format!("{MARKER}{buffer}\u{2588}"), width.saturating_sub(1).max(1)),
                theme.selection,
            ),
        ]
    }
}

/// A task's text as this dialog draws it: every control character replaced,
/// none dropped.
///
/// The member names on the rows above were vetted by `registry::vet_name`
/// before they reached this file. A task's id, owner, subject and the ids it
/// names as blockers were not: they were written by another process, and
/// nothing between the task tools and the store refuses a `\n`, `\r` or `\t`
/// in a task's text. What that costs was
/// measured rather than assumed — the frame survives, because ratatui gives a
/// control character zero width and skips the cell, so the character is
/// **silently swallowed** and `fix a\nb` is drawn `fix ab`, two words joined
/// with nothing on screen to say one ever separated them. Replacing rather
/// than dropping is what puts the fact back where somebody reading the row
/// can see it. (`ganja-cli`'s own `report::printable` guards the `mcp` tables
/// against the same class of foreign text; mirrored rather than shared,
/// because a frontend does not depend on that crate.)
fn printable(text: &str) -> String {
    text.chars()
        .map(
            |character| {
                if character.is_control() { char::REPLACEMENT_CHARACTER } else { character }
            },
        )
        .collect()
}

/// How a task's owner is listed: the member's name, or the word the
/// `task_list` tool answers with for a task nobody holds.
fn owner_label(owner: &str) -> &str {
    if owner.is_empty() { UNOWNED } else { owner }
}

/// A member's recent calls, newest last, hung under its row (**D503**) behind
/// the transcript's own result marker — a call log under a row is the same
/// thing there and here and should read the same way.
///
/// What was cut is admitted above what is shown rather than below it — the
/// transcript's own posture for a clamped call log, and the one that keeps the
/// newest line closest to the eye.
///
/// A call's text goes through [`printable`] for the reason a task's does: it
/// is composed from arguments the model chose, so a control character in a
/// path or a pattern arrives here and would be drawn as nothing at all. The
/// line admitting what was cut is this file's own arithmetic and carries none
/// of that text, which is why only the calls go through it.
fn ring_rows(calls: &[String], width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let hidden = calls.len().saturating_sub(RING_LINES);
    let mut lines = Vec::new();
    if hidden > 0 {
        lines.push(Line::styled(
            clip(
                &format!(
                    "{RESULT}\u{2026} +{hidden} earlier call{plural}",
                    plural = if hidden == 1 { "" } else { "s" },
                ),
                width,
            ),
            theme.dim,
        ));
    }
    lines.extend(
        calls.iter().skip(hidden).map(|call| {
            Line::styled(clip(&printable(&format!("{RESULT}{call}")), width), theme.dim)
        }),
    );

    lines
}

/// The shared key surface, every method the inherent one: what the trait
/// exists for is the one driver in `app.rs`, and what stays this dialog's own
/// is everything `submit` decides.
impl TwoStep for Team {
    type Effect = Effect;

    fn is_typing(&self) -> bool {
        Self::is_typing(self)
    }

    fn cancel(&mut self) -> bool {
        Self::cancel(self)
    }

    fn backspace(&mut self) {
        Self::backspace(self);
    }

    fn push(&mut self, character: char) {
        Self::push(self, character);
    }

    fn move_selection(&mut self, delta: isize) {
        Self::move_selection(self, delta);
    }

    fn submit(&mut self) -> Option<Effect> {
        Self::submit(self)
    }
}

/// How a member's surface is spelled on its row.
///
/// The `--backend` argument's own six spellings, so the word on the row is
/// the word a person would type to ask for another one like it.
fn backend_label(backend: MemberBackend) -> &'static str {
    match backend {
        MemberBackend::InProcess => "in-process",
        MemberBackend::Ganja => "ganja",
        MemberBackend::Claude => "claude",
        MemberBackend::Codex => "codex",
        MemberBackend::Agy => "agy",
        MemberBackend::Grok => "grok",
    }
}

#[cfg(test)]
#[path = "team_tests.rs"]
mod tests;
