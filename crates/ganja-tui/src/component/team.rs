//! The `/team` dialog: one row per member of this session's team — its name,
//! the surface it runs on, whether it is the lead, and the ring of what it
//! most recently did — with the actions a row offers behind Enter, and a
//! Spawn row that belongs to the team rather than to any member.
//!
//! Upstream opencode has no team, no teammates and no surface for either, so
//! nothing here cites an upstream file. The two-step shape is
//! [`crate::component::mcp::Mcp`]'s and the row-independent action plus
//! free-text step are [`crate::component::plugin::Plugin`]'s — the same
//! grammar every dialog in this frontend already taught, applied to a new
//! subject.
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
//!   `/team spawn` raises no confirmation dialog, because a person typing a
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
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _},
};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    command::TeamSpawn,
    component::{body_rows, chat::clip, clamped, first_visible},
    theme::Theme,
};

/// What marks the row the cursor is on, and what pads every other row.
const MARKER: &str = "> ";

/// What a member's recent calls hang under — the transcript's own result
/// marker (`chat.rs`'s `RESULT`), because a call log under a row is the same
/// thing there and here and should read the same way.
const RING: &str = "  \u{23bf} ";

/// How many of a member's recent calls a row shows. The registry's own ring
/// holds `ganja_core::teammate::RECENT_CALLS` of them, which is more than a
/// dialog with several members can spend on one; the newest are the ones that
/// answer "what is it doing right now".
const RING_LINES: usize = 4;

/// Rows the dialog spends on chrome: a blank line and the key hints.
const CHROME: usize = 2;

/// Widest the modal grows.
const MAX_WIDTH: u16 = 76;

/// Tallest the modal grows.
const MAX_HEIGHT: u16 = 20;

/// The keys the member step answers to.
const MEMBER_HINTS: &str = "[j/k] [up/down] move   [Enter] choose   [Esc] close";

/// The keys the per-member action step answers to.
const ACTION_HINTS: &str = "[j/k] [up/down] move   [Enter] run   [Esc] close";

/// The keys the free-text step answers to.
const INPUT_HINTS: &str = "[type/backspace] edit   [Enter] submit   [Esc] cancel";

/// What is shown when the team holds nobody at all.
const EMPTY: &str = "no team members";

/// The label of the action that belongs to the team rather than to a row.
const SPAWN_LABEL: &str = "Spawn teammate\u{2026}";

/// What the free-text step asks for when a spawn is being typed. The same
/// grammar `/team spawn` takes, because [`crate::command::team_spawn`] is the
/// one that reads both.
const SPAWN_PROMPT: &str =
    "Spawn: <name> [--backend <surface>] [--agent <kind>] [--bypass] [what it should do]";

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
    pub fn of(member: &MemberView) -> Self {
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

/// A spawn as this dialog asks for it.
///
/// Two fields rather than one, and the split is the point. [`TeammateSpawn`]
/// is the **`task` tool's own request value** — the very type its teammate
/// door hands to the engine — so a spawn typed here and a spawn a model asked
/// for are the same value and cannot drift apart (AC-14, **D504**). `bypass`
/// sits beside it rather than inside it because it is not a thing a model may
/// ask for: `Teammates::start` spawns with `bypass: false` unconditionally,
/// and its own comment says why — a teammate that wants its dialogs skipped is
/// asked for by a person at `/team spawn`, never by a tool call (D-5).
///
/// `backend` inside it is **as typed**, unparsed: which surfaces exist is the
/// engine's to answer, and a second list here would be a second place for them
/// to drift.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnRequest {
    /// Exactly what the `task` door builds for the same name, surface, agent
    /// kind and prompt.
    pub spawn: TeammateSpawn,
    /// Whether `--bypass` was given.
    pub bypass: bool,
}

impl SpawnRequest {
    /// The request a parsed `/team spawn` line asks for.
    ///
    /// The agent kind is the one place this door fills in what the other one
    /// is always told: `subagent_type` is a required `task` argument, and a
    /// person who did not name a kind means the roster's general-purpose one.
    /// Named from `ganja_core` rather than spelled here, so the default is the
    /// same string the engine's own roster registers.
    #[must_use]
    pub fn new(line: &TeamSpawn) -> Self {
        Self {
            spawn: TeammateSpawn {
                name: line.name.clone(),
                backend: line.backend.clone(),
                agent_type: line
                    .agent_type
                    .clone()
                    .unwrap_or_else(|| ganja_core::agent::GENERAL.to_owned()),
                prompt: line.prompt.clone(),
            },
            bypass: line.bypass,
        }
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

/// What Enter resolved to — everything the app has to act on. Movement and
/// step changes stay inside the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Start a teammate, through the same door a `task` call reaches.
    Spawn(SpawnRequest),
    /// Send what was typed to one member.
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
    /// A spawn line, in `/team spawn`'s own grammar.
    Spawn,
    /// A message for the named member.
    Message(String),
}

/// Which of the dialog's steps is on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// Choosing a member row or the Spawn row under them.
    Members,
    /// Choosing one of the selected member's actions, by index.
    Actions(usize),
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
    /// Opens the dialog over `rows`, cursor on the first member — or on the
    /// Spawn row when the team holds nobody.
    #[must_use]
    pub fn new(rows: Vec<Row>) -> Self {
        Self {
            rows,
            selected: 0,
            step: Step::Members,
            notice: None,
            busy: false,
        }
    }

    /// Replaces the rows with a fresh poll, keeping the cursor and the step
    /// where they were — reclamped, because a shutdown shrinks the roster
    /// under it. A ring growing under a person mid-decision must not move what
    /// their next keypress lands on, which is the whole reason this is not a
    /// fresh [`Team::new`] every tick.
    pub fn refresh(&mut self, rows: Vec<Row>) {
        self.rows = rows;
        self.selected = self.selected.min(self.total_rows().saturating_sub(1));
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
        self.notice = Some(format!(
            "{name} started \u{b7} {CLEARTEXT} {path}",
            name = outcome.name,
            path = outcome.prompt_path,
        ));
    }

    /// Says whether a spawn the app started is still running, which is what
    /// dims the Spawn row and refuses a second one.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    /// Whether such a spawn is running.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Whether the free-text step currently owns the keyboard.
    #[must_use]
    pub fn is_typing(&self) -> bool {
        matches!(self.step, Step::Input { .. })
    }

    /// Whether the per-member action step is the one on screen.
    #[must_use]
    pub fn is_choosing_action(&self) -> bool {
        matches!(self.step, Step::Actions(_))
    }

    /// What has been typed into the free-text step.
    #[must_use]
    pub fn input(&self) -> Option<&str> {
        match &self.step {
            Step::Input { buffer, .. } => Some(buffer.as_str()),
            Step::Members | Step::Actions(_) => None,
        }
    }

    /// The member under the cursor, or [`None`] when the cursor is on the
    /// Spawn row.
    #[must_use]
    pub fn selected_member(&self) -> Option<&Row> {
        self.rows.get(self.selected)
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
        if row.is_lead {
            &[]
        } else {
            &[RowAction::Message, RowAction::Shutdown]
        }
    }

    /// Moves whichever list is showing by `delta` rows. The free-text step
    /// has no rows to move.
    pub fn move_selection(&mut self, delta: isize) {
        match &self.step {
            Step::Members => self.selected = clamped(self.selected, delta, self.total_rows()),
            Step::Actions(option) => {
                let count = self
                    .selected_member()
                    .map_or(0, |row| Self::actions(row).len());
                self.step = Step::Actions(clamped(*option, delta, count));
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
                if self.selected < self.rows.len() {
                    // A row with nothing to offer leaves the dialog exactly as
                    // it was — the `/mcp` dialog's own answer for a row it
                    // cannot act on.
                    if self
                        .selected_member()
                        .is_some_and(|row| Self::actions(row).is_empty())
                    {
                        return None;
                    }
                    self.step = Step::Actions(0);
                    return None;
                }
                if self.busy {
                    self.notice = Some(BUSY.to_owned());
                    return None;
                }
                self.step = Step::Input {
                    asking: Asking::Spawn,
                    buffer: String::new(),
                };

                None
            }
            Step::Actions(option) => {
                let option = *option;
                let row = self.selected_member()?;
                let effect = match *Self::actions(row).get(option)? {
                    RowAction::Shutdown => Effect::Shutdown(row.name.clone()),
                    RowAction::Message => {
                        let asking = Asking::Message(row.name.clone());
                        self.step = Step::Input {
                            asking,
                            buffer: String::new(),
                        };

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
                        let effect = Effect::Message {
                            to: member.clone(),
                            text: typed,
                        };
                        self.step = Step::Members;

                        Some(effect)
                    }
                    Asking::Spawn => match crate::command::team_spawn(&typed) {
                        Ok(line) => {
                            let effect = Effect::Spawn(SpawnRequest::new(&line));
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
            Step::Actions(option) => self.action_rows(inner_width, *option, theme),
            Step::Input { asking, buffer } => Self::input_rows(inner_width, asking, buffer, theme),
        };
        if let Some(notice) = &self.notice {
            let first = notice.lines().next().unwrap_or(notice).trim();
            lines.push(Line::styled(clip(first, inner_width), theme.dim));
        }
        let hints = match &self.step {
            Step::Members => MEMBER_HINTS,
            Step::Actions(_) => ACTION_HINTS,
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
            Step::Actions(_) | Step::Input { .. } => u16::try_from(lines.len().saturating_add(2))
                .unwrap_or(available)
                .min(available),
        };
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" team "))
            .style(theme.fg.patch(theme.background_panel))
            .render(popup, buffer);
    }

    /// The member step: one line per member with its ring hanging under it,
    /// then the Spawn row.
    fn member_rows(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let name_width = self
            .rows
            .iter()
            .map(|row| row.name.width())
            .max()
            .unwrap_or(0);
        let backend_width = self
            .rows
            .iter()
            .map(|row| backend_label(row.backend).width())
            .max()
            .unwrap_or(0);

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
                "{marker}{name:<name_width$}  {backend:<backend_width$}  {lead}",
                marker = if index == self.selected { MARKER } else { "  " },
                name = row.name,
                backend = backend_label(row.backend),
                lead = if row.is_lead { LEAD } else { "" },
            );
            let line = clip(head.trim_end(), width);
            lines.push(Line::styled(
                format!("{line:<width$}"),
                if index == self.selected {
                    theme.selection
                } else {
                    theme.fg
                },
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
                &format!(
                    "{marker}{SPAWN_LABEL}",
                    marker = if on_spawn { MARKER } else { "  " },
                ),
                width,
            ),
            style,
        ));

        let first = first_visible(selected_line, rows);

        lines.into_iter().skip(first).take(rows).collect()
    }

    /// The per-member action step: which member it is about, then what can be
    /// done to it.
    fn action_rows(&self, width: usize, option: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(row) = self.selected_member() else {
            return vec![Line::styled(clip(EMPTY, width), theme.dim)];
        };

        let mut lines = vec![Line::styled(clip(&row.name, width), theme.fg)];
        for (index, action) in Self::actions(row).iter().enumerate() {
            let line = format!(
                "{marker}{label}",
                marker = if index == option { MARKER } else { "  " },
                label = action.label(),
            );
            lines.push(Line::styled(
                clip(&line, width),
                if index == option {
                    theme.accent
                } else {
                    theme.fg
                },
            ));
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
            Asking::Spawn => SPAWN_PROMPT.to_owned(),
            Asking::Message(member) => format!("Message {member}:"),
        };

        vec![
            Line::styled(clip(&prompt, width), theme.fg),
            Line::styled(
                clip(
                    &format!("{MARKER}{buffer}\u{2588}"),
                    width.saturating_sub(1).max(1),
                ),
                theme.selection,
            ),
        ]
    }
}

/// A member's recent calls, newest last, hung under its row (**D503**).
///
/// What was cut is admitted above what is shown rather than below it — the
/// transcript's own posture for a clamped call log, and the one that keeps the
/// newest line closest to the eye.
fn ring_rows(calls: &[String], width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let hidden = calls.len().saturating_sub(RING_LINES);
    let mut lines = Vec::new();
    if hidden > 0 {
        lines.push(Line::styled(
            clip(
                &format!(
                    "{RING}\u{2026} +{hidden} earlier call{plural}",
                    plural = if hidden == 1 { "" } else { "s" },
                ),
                width,
            ),
            theme.dim,
        ));
    }
    lines.extend(
        calls
            .iter()
            .skip(hidden)
            .map(|call| Line::styled(clip(&format!("{RING}{call}"), width), theme.dim)),
    );

    lines
}

/// How a member's surface is spelled on its row.
///
/// The `--backend` argument's own three spellings, so the word on the row is
/// the word a person would type to ask for another one like it.
fn backend_label(backend: MemberBackend) -> &'static str {
    match backend {
        MemberBackend::InProcess => "in-process",
        MemberBackend::Pane => "pane",
        MemberBackend::Claude => "claude",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{Arc, Mutex},
    };

    use ganja_protocol::{MemberBackend, MemberView, TeamView};
    use ganja_tool::{
        Credentials, FileTimes, Tool as _, ToolCtx,
        task::{
            Delegated, Delegation, NotSpawned, Offered, Subagents, TaskTool, TeammateSpawn,
            Teammated, Unanswered,
        },
    };
    use ratatui::{buffer::Buffer, layout::Rect};
    use tokio_util::sync::CancellationToken;

    use super::{BUSY, Effect, Row, SpawnRequest, Spawned, Team, rows};
    use crate::{command, theme::Theme};

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 76,
        height: 20,
    };

    fn row(name: &str, backend: MemberBackend, recent: &[&str]) -> Row {
        Row {
            name: name.to_owned(),
            backend,
            is_lead: false,
            color: None,
            recent: recent.iter().map(|call| (*call).to_owned()).collect(),
        }
    }

    fn lead() -> Row {
        Row {
            name: "team-lead".to_owned(),
            backend: MemberBackend::InProcess,
            is_lead: true,
            color: None,
            recent: Vec::new(),
        }
    }

    fn dialog() -> Team {
        Team::new(vec![
            lead(),
            row(
                "w1",
                MemberBackend::InProcess,
                &["read(src/lib.rs)", "grep(fn spawn)"],
            ),
            row("w2", MemberBackend::Claude, &[]),
        ])
    }

    fn rendered(dialog: &Team, area: Rect) -> String {
        let mut buffer = Buffer::empty(area);
        dialog.render(area, &mut buffer, &Theme::default());

        (0..area.height)
            .map(|line| {
                (0..area.width)
                    .map(|column| buffer[(column, line)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Types `text` into whichever free-text step is open.
    fn type_in(dialog: &mut Team, text: &str) {
        for character in text.chars() {
            dialog.push(character);
        }
    }

    /// Drives the dialog's spawn flow for `line` and answers with the request
    /// it built.
    fn spawn_through_the_dialog(line: &str) -> SpawnRequest {
        let mut dialog = dialog();
        // Past the three members, onto the Spawn row.
        dialog.move_selection(9);
        assert!(dialog.selected_member().is_none(), "the Spawn row is last");
        assert_eq!(dialog.submit(), None, "Spawn opens its free-text step");
        assert!(dialog.is_typing());
        type_in(&mut dialog, line);

        match dialog.submit() {
            Some(Effect::Spawn(request)) => request,
            other => panic!("expected a spawn, got {other:?}"),
        }
    }

    /// A `Subagents` seam that records the request the `task` tool's teammate
    /// door hands it.
    ///
    /// Hand-desugared rather than `#[async_trait]`: this crate does not depend
    /// on `async-trait` and taking a dependency on it for one test double is
    /// not worth a manifest edit — the trait's own attribute expands to
    /// exactly this signature.
    #[derive(Debug)]
    struct Recorder {
        started: Mutex<Vec<TeammateSpawn>>,
    }

    impl Subagents for Recorder {
        fn delegate<'life0, 'async_trait>(
            &'life0 self,
            _request: Delegation,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<Delegated, Unanswered>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { Err(Unanswered::Unknown) })
        }

        fn spawn_teammate<'life0, 'async_trait>(
            &'life0 self,
            request: TeammateSpawn,
        ) -> Pin<Box<dyn Future<Output = Result<Teammated, NotSpawned>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                self.started
                    .lock()
                    .expect("the spawn log is never poisoned")
                    .push(request);

                Ok(Teammated {
                    name: "w3".to_owned(),
                    agent_id: "w3@session-abcd1234".to_owned(),
                    backend: "in-process".to_owned(),
                    note: "it reads this through its mailbox".to_owned(),
                })
            })
        }
    }

    /// The request the **real** `task` door builds for the same arguments —
    /// run through `TaskTool` itself rather than reconstructed here, because a
    /// hand-written expectation would assert this test's reading of the door
    /// instead of the door.
    async fn spawn_through_the_task_door(args: serde_json::Value) -> TeammateSpawn {
        let recorder = Arc::new(Recorder {
            started: Mutex::new(Vec::new()),
        });
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            call_id: "call_1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: Credentials::Unguarded,
            spawn: Some(Arc::clone(&recorder) as Arc<dyn Subagents>),
            postbox: None,
            ask: None,
            switch: None,
            jobs: None,
        };
        TaskTool::new(&[Offered {
            name: "general".to_owned(),
            description: None,
        }])
        .run(args, &ctx)
        .await
        .expect("a teammate starts");

        let started = recorder.started.lock().expect("no panic").clone();

        started.into_iter().next().expect("one spawn was recorded")
    }

    /// **AC-14**, the `/team spawn` half: the two doors are one sequence
    /// because they build one request. The `task` door's value is taken from
    /// the door itself, so this cannot pass by both sides sharing a mistake.
    ///
    /// What is compared is [`SpawnRequest::spawn`] — the `ganja-tool` type
    /// both doors really hand the engine — and not the whole
    /// [`SpawnRequest`]: its other field is `bypass`, which the `task` door
    /// has no argument for by design (D-5), so comparing it would be asserting
    /// that a model can ask for something it must not.
    #[tokio::test]
    async fn the_dialog_builds_the_same_spawn_request_the_task_door_does() {
        let cases = [
            (
                "w3 --backend in-process hold the fort",
                serde_json::json!({
                    "description": "spin up a worker",
                    "prompt": "hold the fort",
                    "subagent_type": "general",
                    "name": "w3",
                    "backend": "in-process",
                }),
            ),
            // No `--backend`: absence is the far side's default on both doors,
            // never a value either of them writes in.
            (
                "w3 hold the fort",
                serde_json::json!({
                    "description": "spin up a worker",
                    "prompt": "hold the fort",
                    "subagent_type": "general",
                    "name": "w3",
                }),
            ),
            // AC-11's own spelling, which carries no prompt at all.
            (
                "w3 --backend pane",
                serde_json::json!({
                    "description": "spin up a worker",
                    "prompt": "",
                    "subagent_type": "general",
                    "name": "w3",
                    "backend": "pane",
                }),
            ),
            // A named agent kind reaches the same field `subagent_type` does.
            (
                "w3 --agent explore --backend claude look around",
                serde_json::json!({
                    "description": "spin up a worker",
                    "prompt": "look around",
                    "subagent_type": "explore",
                    "name": "w3",
                    "backend": "claude",
                }),
            ),
        ];

        for (line, args) in cases {
            assert_eq!(
                spawn_through_the_dialog(line).spawn,
                spawn_through_the_task_door(args).await,
                "the dialog and the task door disagree about {line:?}"
            );
        }
    }

    /// The one field the human door has that the model's does not, and the
    /// reason it is not a `TeammateSpawn` field: a model may not ask for its
    /// teammate's dialogs to be skipped.
    #[test]
    fn only_the_typed_door_can_ask_for_a_bypass() {
        assert!(!spawn_through_the_dialog("w3 do the thing").bypass);

        let bypassed = spawn_through_the_dialog("w3 --bypass do the thing");
        assert!(bypassed.bypass);
        assert_eq!(
            bypassed.spawn.prompt, "do the thing",
            "the flag is consumed rather than left in the prompt"
        );
    }

    /// Resolution 4: nothing stands in front of a spawn, so the one thing a
    /// person is told is told afterwards — and it is where their prompt now
    /// sits in cleartext.
    #[test]
    fn a_spawn_says_where_the_prompt_came_to_rest() {
        let mut dialog = dialog();
        dialog.spawned(&Spawned {
            name: "w3".to_owned(),
            prompt_path: "/t/teams/t1.json".to_owned(),
        });

        let screen = rendered(&dialog, AREA);
        assert!(
            screen.contains("prompt persisted in cleartext at"),
            "got:\n{screen}"
        );
        assert!(screen.contains("/t/teams/t1.json"), "got:\n{screen}");
        assert!(
            screen.contains("w3 started"),
            "and which spawn it is about:\n{screen}"
        );
    }

    /// A spawn already starting refuses a second one at the keypress rather
    /// than after a whole line has been typed — the `/plugin` dialog's posture
    /// for the actions that would race each other.
    #[test]
    fn a_second_spawn_during_the_first_is_refused_before_anything_is_typed() {
        let mut dialog = dialog();
        dialog.set_busy(true);
        assert!(dialog.is_busy());
        dialog.move_selection(9);

        assert_eq!(dialog.submit(), None);
        assert!(!dialog.is_typing(), "the input step must not even open");
        let screen = rendered(&dialog, AREA);
        assert!(
            BUSY.split(" \u{b7} ")
                .next()
                .is_some_and(|head| screen.contains(head)),
            "the refusal is the dialog's own sentence:\n{screen}"
        );

        dialog.set_busy(false);
        assert_eq!(dialog.submit(), None, "and once it is done, it opens");
        assert!(dialog.is_typing());
    }

    /// **D503**: the backend with no window of its own is not the least
    /// observable one — its recent calls hang under its row.
    #[test]
    fn every_member_lists_with_its_backend_and_its_recent_calls() {
        let screen = rendered(&dialog(), AREA);

        assert!(screen.contains("team-lead"), "got:\n{screen}");
        assert!(screen.contains("lead"), "got:\n{screen}");
        assert!(screen.contains("in-process"), "got:\n{screen}");
        assert!(screen.contains("claude"), "got:\n{screen}");
        assert!(screen.contains("read(src/lib.rs)"), "got:\n{screen}");
        assert!(screen.contains("grep(fn spawn)"), "got:\n{screen}");
    }

    /// A ring longer than the row shows admits what it cut rather than
    /// quietly showing the oldest four.
    #[test]
    fn a_ring_longer_than_the_row_shows_admits_the_cut() {
        let calls: Vec<&str> = vec!["a", "b", "c", "d", "e", "f"];
        let screen = rendered(
            &Team::new(vec![row("w1", MemberBackend::InProcess, &calls)]),
            AREA,
        );

        assert!(screen.contains("+2 earlier calls"), "got:\n{screen}");
        assert!(
            screen.contains("\u{23bf} f"),
            "the newest is shown:\n{screen}"
        );
        assert!(
            !screen.contains("\u{23bf} a"),
            "the oldest is cut:\n{screen}"
        );
    }

    /// Enter on a teammate opens Message and Shutdown; the lead's row offers
    /// neither, because this session is the lead and `/exit` is its door.
    #[test]
    fn a_teammate_row_offers_message_and_shutdown_and_the_leads_offers_nothing() {
        let mut dialog = dialog();
        assert_eq!(
            dialog.selected_member().map(|row| row.name.as_str()),
            Some("team-lead")
        );
        assert_eq!(dialog.submit(), None, "the lead's row has nothing to open");
        assert!(!dialog.is_choosing_action());

        dialog.move_selection(1);
        assert_eq!(dialog.submit(), None, "Enter opens the action step");
        assert!(dialog.is_choosing_action());

        let screen = rendered(&dialog, AREA);
        assert!(screen.contains("Message"), "got:\n{screen}");
        assert!(screen.contains("Shutdown"), "got:\n{screen}");

        dialog.move_selection(1);
        assert_eq!(dialog.submit(), Some(Effect::Shutdown("w1".to_owned())));
        assert!(!dialog.is_choosing_action(), "and back to the roster");
    }

    /// Message takes its text in the dialog's own free-text step, the way the
    /// `/plugin` dialog takes a marketplace: nothing about what one teammate
    /// says to another rides an engine `question` round trip.
    #[test]
    fn messaging_a_member_takes_the_text_in_the_dialog_itself() {
        let mut dialog = dialog();
        dialog.move_selection(1);
        dialog.submit();
        assert_eq!(dialog.submit(), None, "Message opens the free-text step");
        assert!(dialog.is_typing());

        type_in(&mut dialog, "status?");
        assert_eq!(dialog.input(), Some("status?"));

        assert_eq!(
            dialog.submit(),
            Some(Effect::Message {
                to: "w1".to_owned(),
                text: "status?".to_owned(),
            })
        );
        assert!(!dialog.is_typing());
    }

    /// A refused spawn line keeps the step and the text: the answer to a
    /// mistyped flag is to fix that word.
    #[test]
    fn a_refused_spawn_line_says_why_and_keeps_what_was_typed() {
        let mut dialog = dialog();
        dialog.move_selection(9);
        dialog.submit();
        type_in(&mut dialog, "w3 --nonesuch");

        assert_eq!(dialog.submit(), None, "nothing is sent");
        assert!(dialog.is_typing(), "and the line is still there to fix");
        assert_eq!(dialog.input(), Some("w3 --nonesuch"));

        let screen = rendered(&dialog, AREA);
        assert!(screen.contains("--nonesuch"), "got:\n{screen}");
    }

    /// Esc on the free-text step abandons the text without sending it, and
    /// leaves the dialog open — `false` from `cancel` is what closes it.
    #[test]
    fn escaping_the_free_text_step_abandons_it_without_closing_the_dialog() {
        let mut dialog = dialog();
        dialog.move_selection(9);
        dialog.submit();
        type_in(&mut dialog, "w3");

        assert!(dialog.cancel(), "the free-text step consumes Esc");
        assert!(!dialog.is_typing());
        assert_eq!(dialog.input(), None);
        assert!(!dialog.cancel(), "and the next Esc closes the dialog");
    }

    /// Backspace edits the free-text step and nothing else.
    #[test]
    fn backspace_takes_a_character_off_the_typed_line() {
        let mut dialog = dialog();
        dialog.backspace();
        dialog.push('x');
        assert_eq!(dialog.input(), None, "the list step has no line to edit");

        dialog.move_selection(9);
        dialog.submit();
        type_in(&mut dialog, "w3x");
        dialog.backspace();

        assert_eq!(dialog.input(), Some("w3"));
    }

    /// A poll refresh keeps the cursor on the same position rather than
    /// resetting it, and reclamps when a shutdown shrank the roster.
    #[test]
    fn refreshing_keeps_the_cursor_where_it_was_and_reclamps_a_shrink() {
        let mut dialog = dialog();
        dialog.move_selection(1);
        dialog.refresh(vec![
            lead(),
            row("w1", MemberBackend::InProcess, &["write(src/main.rs)"]),
            row("w2", MemberBackend::Claude, &[]),
        ]);
        assert_eq!(
            dialog.selected_member().map(|row| row.name.as_str()),
            Some("w1")
        );
        assert_eq!(
            dialog.selected_member().map(|row| row.recent.len()),
            Some(1),
            "and the ring is the fresh one"
        );

        dialog.move_selection(2);
        dialog.refresh(vec![lead()]);
        assert!(
            dialog.selected_member().is_none(),
            "the cursor reclamps onto the Spawn row"
        );
    }

    /// The command grammar is the dialog's grammar, so a `/team` line and the
    /// dialog's own step cannot mean two different things.
    #[test]
    fn a_typed_team_line_and_the_dialogs_step_build_the_same_request() {
        let Some(command::Team::Spawn(line)) = command::team("/team spawn w3 --bypass go") else {
            panic!("`/team spawn` should parse");
        };

        assert_eq!(
            SpawnRequest::new(&line),
            spawn_through_the_dialog("w3 --bypass go")
        );
    }

    /// The projection a caller polling the registry hands in, and the one
    /// ordering the dialog promises: the lead first, because it is the row a
    /// person looks for to know which session they are in.
    #[test]
    fn the_lead_is_the_first_row_however_the_registry_ordered_it() {
        let view = TeamView {
            team: "session-abcd1234".to_owned(),
            lead: "team-lead".to_owned(),
            members: vec![
                MemberView {
                    name: "w1".to_owned(),
                    agent_id: "w1@session-abcd1234".to_owned(),
                    backend: MemberBackend::Claude,
                    color: Some("blue".to_owned()),
                    is_lead: false,
                    recent_calls: vec!["read(src/lib.rs)".to_owned()],
                },
                MemberView {
                    name: "team-lead".to_owned(),
                    agent_id: "team-lead@session-abcd1234".to_owned(),
                    backend: MemberBackend::InProcess,
                    color: None,
                    is_lead: true,
                    recent_calls: Vec::new(),
                },
            ],
        };

        let projected = rows(&view);

        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0].name, "team-lead");
        assert!(projected[0].is_lead);
        assert_eq!(projected[1].name, "w1");
        assert_eq!(projected[1].color.as_deref(), Some("blue"));
        assert_eq!(projected[1].recent, vec!["read(src/lib.rs)".to_owned()]);
    }

    #[test]
    fn a_team_with_nobody_in_it_says_so_and_still_offers_a_spawn() {
        let dialog = Team::new(Vec::new());
        let screen = rendered(&dialog, AREA);

        assert!(dialog.selected_member().is_none());
        assert!(screen.contains("no team members"), "got:\n{screen}");
        assert!(screen.contains("Spawn teammate"), "got:\n{screen}");
    }

    #[test]
    fn a_row_too_wide_for_the_column_is_cut_rather_than_wrapped() {
        let long = "very long call ".repeat(20);
        let dialog = Team::new(vec![row(
            &"w".repeat(90),
            MemberBackend::InProcess,
            &[long.as_str()],
        )]);

        for line in rendered(&dialog, Rect::new(0, 0, 60, 20)).lines() {
            assert!(
                line.chars().count() <= 60,
                "a row must not overflow the dialog: {line:?}"
            );
        }
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        for (width, height) in [(1, 1), (3, 2), (8, 4)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);

            dialog().render(area, &mut buffer, &Theme::default());
        }
    }

    #[test]
    fn a_zero_area_draws_nothing_and_does_not_panic() {
        let screen = rendered(&dialog(), Rect::new(0, 0, 0, 0));

        assert!(
            screen.is_empty(),
            "a zero area has no cell to hold: {screen}"
        );
    }
}
