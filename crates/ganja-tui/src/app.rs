//! The event loop and the state it owns.
//!
//! One [`tokio::select!`] owns every mutable piece of UI state; the engine is
//! reached through `&self` seams on a shared [`Arc<Engine>`] (**D505**) and
//! answers through its event stream. No arm awaits work of unbounded
//! duration: a prompt is handed to the engine, which answers through the event
//! stream, and the loop goes straight back to drawing.
//!
//! Frames are coalesced. A burst of fragments redraws at most once per
//! [`FRAME`], while a keystroke always redraws immediately — the two rules that
//! keep streaming cheap without making typing feel laggy.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use etcetera::{BaseStrategy as _, base_strategy::Xdg};
use futures::StreamExt as _;
use ganja_core::{
    Engine, EngineError, SessionId, attachment, catalog,
    config::{NotificationEvent, StatuslineConfig},
    provider,
    teammate::{
        Delivery,
        // The `uds:` address scheme (**D528**): the resolver's own pub
        // spelling, so every candidate this side names carries an address
        // `to` accepts verbatim without a second copy that could drift.
        identity::ADDRESS_SCHEME,
        lead_inbox::{Delivered, LeadInbox},
        posture::{Forwarded, Posture},
    },
};
use ganja_protocol::{
    Command, Event as CoreEvent, FinishReason, HeldDecision, HeldId, HoldCause, Mention, Message,
    MessageId, PartBody, PermissionId, PermissionReply, RevertScope, Role, ToolState, Usage,
};
use ganja_tool::{Credentials, FileTimes, ToolCtx, job::Jobs as _, registry};
use ratatui::{
    DefaultTerminal, Terminal,
    backend::Backend,
    crossterm::event::{
        Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    layout::{Constraint, Layout},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    NOTICE_SEPARATOR, binder, clipboard, command,
    component::{
        chat::{self, Chat, WHEEL_LINES, Working},
        context,
        dropdown::{self, Dropdown},
        editor::{self, Editor, Mode},
        effort,
        files::{Files, Row as MenuRow},
        held,
        help::Help,
        inspector::{Feed, Inspector, TurnUsage},
        list::{self, ListDialog},
        mcp,
        palette::Palette,
        permission::Permission,
        plugin,
        question::Question,
        queue::Queue,
        rewind::Rewind,
        search::HistorySearch,
        sessions::{self, Sessions},
        skill_menu::SkillMenu,
        status::{Activity, Status, Todos, Totals},
        team,
        themes::ThemeList,
        usage,
    },
    escrepair::EscRepair,
    event::AppEvent,
    external, graphics,
    history::{self, History},
    keybind::{self, Keybinds},
    lister, member, mention, notify,
    theme::{Theme, Themes},
    transcript,
};

/// Shortest gap between frames: roughly 60 FPS.
pub const FRAME: Duration = Duration::from_millis(16);

/// Milliseconds since the epoch, for [`registry::Record::started_at`] —
/// display and sort only, never consulted for liveness (the flock is).
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The incumbent's own collision re-scan runs at most this often (**S1**,
/// **R4**): the bound that keeps the probe's documented side costs — an
/// absent `.lock` created, a concurrently walking binder's one-digit stem
/// extension — rare by design rather than a per-tick cost.
const COLLISION_RESCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Modifiers that turn Enter into a line break. Terminals disagree about which
/// of these they can report, so all of them mean the same thing.
const NEWLINE_MODIFIERS: KeyModifiers = KeyModifiers::SHIFT
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::CONTROL);

/// Modifiers that stop a printable key from reaching a filter line: they mean
/// the key is a shortcut rather than a character.
const SHORTCUT_MODIFIERS: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);

/// Rows a Page key moves the reference card. A fixed step rather than the
/// card's own height: the card sizes itself inside the render and nothing out
/// here has seen the result.
const HELP_PAGE: isize = 8;

/// Cap on the Ctrl+T inspector's raw-log ring buffer (**F2**), named so the
/// bound is visible rather than implicit. Every core event this session
/// emits is teed in before [`App::handle_core`] consumes the original.
const MAX_EVENT_LOG: usize = 2000;

/// Cap on the Ctrl+T inspector's per-turn usage table (**F2**). Far more than
/// a real session reaches — one row per *finished* turn, not per event — but
/// still bounded rather than open-ended.
const MAX_TURN_USAGE: usize = 1000;

/// Most peer messages one [`App::deliver_peers`] drain hands the model
/// (**D526**). Ganja's own number: a pass's batch becomes rows on a single
/// user message, and eight is more than a working team says in a second while
/// keeping what a flooded mailbox can make of one turn readable. The
/// remainder stays in the inbox — the durable queue — and rides the next
/// pass, oldest first; see the function's own note for why that is free.
const PEER_BATCH_CAP: usize = 8;

/// How long a first Esc stays armed for the backtrack gesture (**D467**).
///
/// Short enough that two deliberate presses are the only thing that reaches
/// it, and that an Esc a person meant as "never mind" is over long before
/// their next one.
const ESC_CHORD: Duration = Duration::from_millis(500);

/// What the status bar says while the backtrack walk is up, cleared with it.
const BACKTRACK_HINT: &str = "backtrack: Esc older, Enter revert, any other key exits";

/// The Esc Esc backtrack walk's state, while it is up (**D467**).
///
/// Ids only, off the same roster the rewind picker lists — never the roster's
/// titles: a [`crate::component::rewind::Checkpoint`]'s `title` is the
/// prompt's first line clipped for a row, and the composer prefill must be
/// the whole prompt, which only the engine's `RevertChanged` carries.
struct Backtrack {
    /// The user messages the highlight can land on, newest first.
    candidates: Vec<MessageId>,
    /// Which of them it is on.
    index: usize,
}

/// Most files the `@` menu offers at once. Upstream's server default
/// (`server/routes/instance/httpapi/handlers/file.ts:43-60`).
const MAX_FILES: usize = 10;

/// The call id the `@` menu's walk runs under. Nothing correlates it with a
/// provider call — no model asked for it — but the field is not optional and a
/// name is more use in a trace than a blank.
const MENTION_CALL: &str = "mention";

/// What the copy commands say when they worked, in upstream's own words
/// (`routes/session/index.tsx:906`, `:935`).
const MESSAGE_COPIED: &str = "Message copied to clipboard!";
/// See [`MESSAGE_COPIED`].
const TRANSCRIPT_COPIED: &str = "Session transcript copied to clipboard!";
/// Upstream's failure toasts. The reason the clipboard gave is appended
/// (deviation: copy-failure-notice-names-the-reason) — upstream swallows it,
/// and "Failed to copy to clipboard" on a machine with no display is a
/// sentence that leaves the user with nothing to do about it.
const MESSAGE_COPY_FAILED: &str = "Failed to copy to clipboard";
/// See [`MESSAGE_COPY_FAILED`].
const TRANSCRIPT_COPY_FAILED: &str = "Failed to copy session transcript";
/// What `/copy` says before there is a conversation to copy. Upstream returns
/// silently here (deviation: copy-with-no-session-says-so).
const NOTHING_TO_COPY: &str = "there is no session to copy yet";

/// What a paste says when the clipboard holds neither text nor an image
/// (F3): an empty selection, or a format `arboard` cannot decode either way.
const CLIPBOARD_EMPTY: &str = "the clipboard holds neither text nor an image";

/// What `/effort` says when the active model's catalog row offers none —
/// upstream's toast message, reworded for ganja (`app.tsx:717`).
const NO_EFFORTS: &str = "The current model does not support any efforts.";

/// What the `/plugin` dialog's Reload answers when it worked (**D474**): the
/// honest split, verbatim. Hooks and the skill roots really are rebuilt
/// in-session; the agents roster, the MCP dials and the LSP servers are
/// assembled at startup and are *named as such* rather than half-reloaded.
const RELOAD_SPLIT: &str = "reloaded now: hooks, skills \u{b7} restart required: agents, mcp, lsp";

/// The one-line notice a failed MCP server earns in the status bar, or [`None`]
/// while every configured server is either connected, disabled, or still being
/// dialled.
///
/// The fake-provider-notice pattern (**R3**): a server that could not be
/// reached costs its tools and a line of the status bar, never the session. A
/// server still dialling has no entry in the map at all, so it says nothing
/// until it has something to say.
///
/// Only the first line of the error travels. The status bar is one row, and a
/// transport that failed with a stack of context would otherwise take the row
/// away from everything else on it.
fn mcp_notice(
    status: &std::collections::BTreeMap<String, ganja_core::McpStatus>,
) -> Option<String> {
    let failures: Vec<String> = status
        .iter()
        .filter_map(|(name, status)| match status {
            ganja_core::McpStatus::Failed { error } => Some(format!(
                "mcp {name}: {}",
                error.lines().next().unwrap_or(error).trim()
            )),
            ganja_core::McpStatus::Connected | ganja_core::McpStatus::Disabled => None,
        })
        .collect();

    (!failures.is_empty()).then(|| failures.join(NOTICE_SEPARATOR))
}

/// Runs one store-backed `/plugin` action and answers with the notice line
/// the dialog shows — the confirmation the CLI would print, or the refusal,
/// git's captured stderr included, since [`ganja_core::plugin::PluginError`]
/// carries a failed clone's stderr as its message.
///
/// A free function because [`App::run_plugin_effect`] runs it on a blocking
/// task that outlives the call: what it needs is the store and the decision,
/// and handing it less than `self` is what makes that spawn possible.
fn run_store_effect(store: &ganja_core::plugin::Store, effect: plugin::Effect) -> String {
    match effect {
        plugin::Effect::Enable(name) => match store.set_enabled(&name, true) {
            Ok(()) => format!("enabled {name}"),
            Err(error) => error.to_string(),
        },
        plugin::Effect::Disable(name) => match store.set_enabled(&name, false) {
            Ok(()) => format!("disabled {name}"),
            Err(error) => error.to_string(),
        },
        plugin::Effect::Remove(name) => match store.remove(&name) {
            Ok(()) => format!("removed {name}"),
            Err(error) => error.to_string(),
        },
        plugin::Effect::AddMarketplace(origin) => match store.add_marketplace(&origin) {
            Ok(name) => format!("added marketplace {name} from {origin}"),
            Err(error) => error.to_string(),
        },
        plugin::Effect::Install(spec) => {
            // The CLI's own spelling rule, kept verbatim so the two doors
            // refuse the same way.
            let Some((name, marketplace)) = spec.split_once('@') else {
                return format!(
                    "spell it <plugin>@<marketplace>, the way `ganja plugin list` and the \
                     marketplace file spell it; got \"{spec}\""
                );
            };
            match store.install(name, marketplace) {
                Ok(()) => format!("installed {name} from {marketplace}, enabled"),
                Err(error) => error.to_string(),
            }
        }
        plugin::Effect::Reload => unreachable!("the reload never reaches the store helper"),
    }
}

/// What the dialog says while `effect` runs off the loop: the present tense
/// of the confirmation [`run_store_effect`] will answer with, so the notice
/// line reads as one sentence finishing rather than as two unrelated states.
fn pending_notice(effect: &plugin::Effect) -> String {
    match effect {
        plugin::Effect::Enable(name) => format!("enabling {name}\u{2026}"),
        plugin::Effect::Disable(name) => format!("disabling {name}\u{2026}"),
        plugin::Effect::Remove(name) => format!("removing {name}\u{2026}"),
        plugin::Effect::AddMarketplace(origin) => {
            format!("adding marketplace from {origin}\u{2026}")
        }
        plugin::Effect::Install(spec) => format!("installing {spec}\u{2026}"),
        plugin::Effect::Reload => unreachable!("the reload never reaches the store task"),
    }
}

/// What a `RevertChanged` carrying no revert means for the messages this
/// frontend has hidden.
///
/// The engine sends the same event for two different things and says so: a
/// redo that stepped past the newest undone prompt, where the messages come
/// back, and the prompt or shell command that followed an undo, where they
/// have just been deleted from history and from storage. It draws no
/// distinction because the frontend's own last command already did — this is
/// where that command is remembered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cleared {
    /// Show the hidden entries again.
    Unhide,
    /// Drop them: nothing is left to bring them back.
    Drop,
}

/// Which list a dialog is showing, and therefore what choosing a row sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chooser {
    /// The provider's catalog models.
    Models,
    /// The active model's catalog efforts, "Default" first.
    Effort,
    /// The agents this session may run as.
    Agents,
    /// The skills a `$` invocation can load; Enter inserts the token rather
    /// than switching anything (**D491**).
    Skills,
}

/// What the background model-listing fetch resolves to: the seam's whole
/// answer, with [`None`] still meaning the catalog owns this provider.
type WireListing = Option<Result<provider::WireModels, provider::ProviderError>>;

/// The chooser rows a wire listing becomes: the id is what a switch sends,
/// the display name rides beside it, and the active mark follows the model
/// the session is on — absent when the listing does not carry that model,
/// which refuses nothing.
/// Reads the bar's `todos` element off a finished `todowrite` call's
/// metadata — the whole-list copy the tool publishes for frontends
/// (`ganja_tool::todo`). A list the metadata does not carry, or carries
/// malformed, clears the element rather than showing a count nobody wrote.
fn todo_progress(metadata: &serde_json::Value) -> Option<Todos> {
    let todos = metadata.get("todos")?.as_array()?;
    let done = todos
        .iter()
        .filter(|todo| todo.get("status").and_then(serde_json::Value::as_str) == Some("completed"))
        .count();
    let current = todos.iter().find_map(|todo| {
        if todo.get("status")?.as_str()? != "in_progress" {
            return None;
        }
        Some(todo.get("content")?.as_str()?.to_owned())
    });

    Some(Todos {
        done,
        total: todos.len(),
        current,
    })
}

fn wire_rows(models: &[provider::ListedModel], current: &str) -> Vec<list::Row> {
    models
        .iter()
        .map(|model| list::Row {
            value: model.id.clone(),
            label: model.id.clone(),
            detail: Some(model.name.clone()),
            active: model.id == current,
        })
        .collect()
}

/// What the status bar says after a code-only rewind: the files that really
/// came back, which is not always the ones the checkpoint's patches named (see
/// `snapshot::Snapshots::revert`).
fn restored(files: &[String]) -> String {
    if files.is_empty() {
        return "the rewind restored no files".to_owned();
    }

    format!(
        "restored {count} file{plural}: {named}",
        count = files.len(),
        plural = if files.len() == 1 { "" } else { "s" },
        named = files.join(", "),
    )
}

/// Whether the editor itself would do something with `key`.
///
/// The one exit binding that is also an editing key. Upstream gates its whole
/// exit binding on an empty-or-unfocused prompt; ganja gates the half that
/// would otherwise take a keystroke away from typing, and leaves Ctrl-C and
/// Ctrl-Q the unconditional interrupts they have always been here
/// (deviation: exit-gate-only-for-editing-keys).
fn edits(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL
}

/// Which of vim's half-page pair `key` is — `-1` for Ctrl+U, `1` for Ctrl+D
/// — or [`None`]: the inspector's own scroll step, and the one exit chord the
/// overlay takes for itself while it is open ([`App::exits`]).
fn half_page(key: KeyEvent) -> Option<isize> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('u') if ctrl => Some(-1),
        KeyCode::Char('d') if ctrl => Some(1),
        _ => None,
    }
}

/// The reply a key sends while the permission dialog is open, or [`None`]
/// for a key the dialog swallows without acting on it. Pulled out of
/// [`App::handle_key`] so the mapping can be asserted on its own.
fn permission_reply(code: KeyCode) -> Option<PermissionReply> {
    match code {
        KeyCode::Char('y') => Some(PermissionReply::Once),
        KeyCode::Char('a') => Some(PermissionReply::Always),
        KeyCode::Char('n') | KeyCode::Esc => Some(PermissionReply::Reject),
        _ => None,
    }
}

/// The decision a key sends while the held-message approval modal is open
/// (**D524**), or [`None`] for a key the modal swallows.
///
/// Esc is a decision, not an escape: dismissing a held message drops it,
/// exactly as denying it does. And no key maps to anything "always" — a
/// standing accept for inbound has exactly one spelling, the config's
/// `cross_session_inbound: "accept"`, never a dialog answer.
fn held_decision(code: KeyCode) -> Option<HeldDecision> {
    match code {
        KeyCode::Char('y') => Some(HeldDecision::Release),
        KeyCode::Char('n') | KeyCode::Esc => Some(HeldDecision::Deny),
        _ => None,
    }
}

/// One dialog waiting on the person — what [`App::permission`] shows and
/// [`App::queued_permissions`] holds behind it (**D462**, widened by
/// **D524**).
///
/// The hold variant is **structurally unanswerable by the yolo drain
/// (B1)**: it carries a [`HeldId`] and no [`PermissionId`], the
/// `PermissionRequested` arm that feeds [`App::auto_permissions`] can only
/// ever see permission requests, and what that drain answers with —
/// `Command::ReplyPermission` — the engine routes through a wait registry no
/// hold ever enters (a hold settles by `Command::SettleHeld`). A bypassed
/// session therefore still shows every parity-hold dialog to a person;
/// unattended inbound is spelled `cross_session_inbound: "accept"` in a
/// trusted config tier, not a flag.
enum PendingDialog {
    /// A tool call waiting on the person's decision.
    Permission(Permission),
    /// An inbound peer message held for the person's review (**D524**).
    Held(held::HeldApproval),
}

impl PendingDialog {
    /// The permission request this item shows — [`None`] for a hold, which
    /// is the whole of B1: no path that answers [`PermissionId`]s can name
    /// one.
    fn permission_id(&self) -> Option<&PermissionId> {
        match self {
            Self::Permission(permission) => Some(permission.id()),
            Self::Held(_) => None,
        }
    }

    /// The hold this item reviews — [`None`] for a permission request.
    fn held_id(&self) -> Option<&HeldId> {
        match self {
            Self::Permission(_) => None,
            Self::Held(held) => Some(held.id()),
        }
    }
}

/// The whole terminal application.
pub struct App {
    /// Shared rather than owned since P25: a lead's engine is also the one
    /// its session socket serves (**D505**), and the server holds a clone of
    /// this for as long as it accepts. Every seam this app reaches through it
    /// takes `&self`.
    engine: Arc<Engine>,
    /// Provider this session runs on, which is what the model list is narrowed
    /// to: switching model is same-provider only, so offering the rest of the
    /// catalog would be offering refusals.
    provider: String,
    /// Model the engine asks for, kept here because pricing a turn needs it and
    /// the engine's copy is not the frontend's business.
    model: String,
    /// Catalog effort the next turn runs under, [`None`] for Default. Kept
    /// beside [`App::model`] because the picker's active mark and the status
    /// segment both read the pair together.
    effort: Option<String>,
    /// Agent the next turn runs as, [`None`] on a session built without a
    /// registry. Tracked here rather than read back per frame because this is
    /// the side that issues the switches.
    agent: Option<String>,
    chat: Chat,
    editor: Editor,
    status: Status,
    /// Which keys reach which actions this run.
    keys: Keybinds,
    /// The dialog currently waiting on the user's decision, if any: a tool
    /// call's permission request, or a held inbound peer message's review
    /// (**D524** — the widening is [`PendingDialog`]'s doc).
    permission: Option<PendingDialog>,
    /// Dialogs that arrived while another was already on screen, in arrival
    /// order (**D462**).
    ///
    /// One at a time is still what a person is shown — two modals over each
    /// other is not a design — so a second request queues rather than
    /// replacing the first, and the bar counts what is behind it. Only
    /// concurrent children can produce a second permission request: a single
    /// call is a turn blocked inside it, which is what made the engine's own
    /// registry a single cell until this wave. A parity hold can arrive
    /// behind anything, since no turn waits on one.
    queued_permissions: VecDeque<PendingDialog>,
    /// Whether this session answers its own permission dialogs (**D479**).
    ///
    /// `ganja --yolo`, and its two other spellings. What it changes is exactly
    /// one thing: a request that would have opened a dialog is answered
    /// [`PermissionReply::Once`] instead, immediately, for this request and
    /// every one behind it — so the queue above never fills and no turn waits
    /// on a dialog that will never be drawn.
    ///
    /// **Never [`PermissionReply::Always`]**: an "always" is a rule written
    /// into the project's store, and a flag on one invocation may not leave
    /// standing permissions behind on the machine.
    ///
    /// What this does *not* touch, and deliberately:
    /// - A rule that resolves to **deny** raises no dialog at all, so there is
    ///   nothing here to answer and the call stays refused. The engine remains
    ///   the single gate; this only stands in for the person at it.
    /// - `question`, and the two plan doors that ride it, arrive as
    ///   [`CoreEvent::QuestionAsked`] rather than as a permission request and
    ///   keep asking. A person is sitting here — the whole difference from
    ///   `ganja run --auto`, which has nobody — so what is bypassed is
    ///   permission, not conversation.
    yolo: bool,
    /// Requests a yolo session has to answer, filled by the synchronous
    /// [`App::handle_core`] and drained by the one caller that may await.
    auto_permissions: VecDeque<PermissionId>,
    /// The question currently waiting on the user's answer.
    question: Option<Question>,
    /// The stored sessions the user is choosing between, while the picker is
    /// open.
    sessions: Option<Sessions>,
    /// The themes the user is choosing between, while that picker is open.
    theme_list: Option<ThemeList>,
    /// The Ctrl+R fuzzy search over remembered prompts, while it is open.
    history_search: Option<HistorySearch>,
    /// The rewind picker, while it is open (**F7**).
    rewind: Option<Rewind>,
    /// The `/mcp` dialog, while it is open (**F5**).
    mcp_dialog: Option<mcp::Mcp>,
    /// The `/held` listing, while it is open (**D524**): every held inbound
    /// peer message, with Release and Deny on each row. Rows are re-polled
    /// off `Engine::held_messages` on the tick, like every status surface.
    held_dialog: Option<held::HeldList>,
    /// The `/plugin` dialog, while it is open (**D474**).
    plugin_dialog: Option<plugin::Plugin>,
    /// The `/team` dialog, while it is open (**D504**).
    team_dialog: Option<team::Team>,
    /// A `/team spawn` while it is in flight, carrying the name it started or
    /// the sentence that refused it.
    ///
    /// Reaped on the tick beside [`App::plugin_task`] and for its reason, with
    /// one of its own that is sharper: a spawn's own permission dialog goes to
    /// [`App::spawn_asks`], and a loop that awaited the spawn would be waiting
    /// on a person it had stopped drawing for. Also the guard that keeps a
    /// second spawn off the team file while one is being written.
    ///
    /// A pane-mode shim spawn (`--backend codex|agy|grok`, **D512**) holds
    /// this handle for up to [`ganja_core::teammate::shim_tui::READY_WAIT`]
    /// plus [`ganja_core::teammate::shim_tui::READY_SETTLE`] — about sixteen
    /// seconds — while its readiness poll waits for the CLI's own composer
    /// and then lets it settle, or times out into a paste nobody submits, so
    /// a second `/team spawn` typed meanwhile is answered [`team::BUSY`] for
    /// that long. That is the design and not a hang: the wait is off this loop,
    /// which keeps drawing, and the guard is exactly what stops two spawns
    /// writing the team file at once.
    team_spawn: Option<JoinHandle<Result<String, String>>>,
    /// Where a spawn in flight puts its own permission dialog (**D-5**).
    spawn_asks: tokio::sync::mpsc::Receiver<SpawnQuestion>,
    /// The sender each spawn is handed a clone of.
    spawn_asker: tokio::sync::mpsc::Sender<SpawnQuestion>,
    /// Where a spawn dialog's answer goes back, by the id it was raised under.
    ///
    /// Beside [`App::forwarded_dialogs`] rather than inside it, because the two
    /// answer different questions — may this teammate *run*, versus may this
    /// teammate's *call* run — and one map answering both would let a reply
    /// routed by id alone reach the wrong waiter.
    spawn_dialogs: HashMap<PermissionId, tokio::sync::oneshot::Sender<PermissionReply>>,
    /// Where the plugin store lives, when a test moved it; [`None`] resolves
    /// the real store under the config home when the dialog opens, the same
    /// discovery `ganja plugin` runs.
    plugin_store: Option<ganja_core::plugin::Store>,
    /// The `/context` panel, while it is open (**D470**). A snapshot of the
    /// engine's breakdown taken when the command ran, never re-polled.
    context_dialog: Option<context::Context>,
    /// The `/usage` panel, while it is open (**D471**). The same
    /// read-on-open posture.
    usage_dialog: Option<usage::Usage>,
    /// The models or agents the user is choosing between, and which of the two
    /// the list is.
    chooser: Option<(Chooser, ListDialog)>,
    /// The command palette, while it is open.
    palette: Option<Palette>,
    /// What was typed into the palette's filter, kept across a close so that
    /// reopening does not mean typing it again.
    palette_filter: String,
    /// The reference card, while it is open.
    help: Option<Help>,
    /// The Ctrl+T inspector overlay, while it is open (**F2**).
    inspector: Option<Inspector>,
    /// Every core event this session has emitted, capped (**F2**): what the
    /// inspector's raw-log tab replays. Teed from `AppEvent::Core`'s own
    /// `Clone` (`event.rs:14`) before `App::handle_core` consumes the
    /// original by value.
    event_log: VecDeque<CoreEvent>,
    /// One row per turn that reported a `Usage`, capped the same way: what
    /// the inspector's per-turn token tab replays. The reasoning and cache
    /// splits ride along untouched, where `App::record`'s own running totals
    /// collapse them.
    turn_usages: VecDeque<TurnUsage>,
    /// The inline command menu, while the buffer is a command being typed —
    /// or, in values mode, a `/team` slot being filled (**D519**).
    dropdown: Option<Dropdown>,
    /// The `/team` slot the values menu is over, for the span a chosen value
    /// replaces; [`None`] whenever the menu is the command menu or closed.
    completion: Option<command::Slot>,
    /// The agent kinds `--agent` may name, read off the engine's registry
    /// once: what the `task` door may spawn is what this menu offers.
    agent_kinds: Vec<command::Completion>,
    /// The inline file menu, while the buffer is mentioning a file.
    files: Option<Files>,
    /// The inline skill menu, while the buffer is invoking one with `$`
    /// (**D491**, the Codex CLI's selector).
    skill_menu: Option<SkillMenu>,
    /// The commands the **engine** offers, resolved when the app was built.
    /// Choosing one types its name rather than running it, because every one
    /// of them expects arguments.
    engine_commands: Vec<command::EngineCommand>,
    /// What the next `RevertChanged { revert: None }` means, decided by the
    /// last command this frontend sent that could produce one. See [`Cleared`].
    cleared: Cleared,
    /// Messages typed while a turn already held the engine (**F4**): handed to
    /// that turn as a `Command::Steer` where it could take them, held for
    /// replay where it could not.
    queue: Queue,
    /// Whether a turn holds the engine's slot. Read off the event stream — the
    /// assistant envelope opens it, the finish closes it — rather than asked
    /// of the engine, because the slot is the engine's and a frontend that
    /// polled it would still be one event behind. Both races that leaves are
    /// answered by the engine's own typed refusals: `Busy` on the prompt that
    /// thought it was idle, `NotStreaming` on the steer that thought it was
    /// not.
    turn_running: bool,
    /// How many turns this session has started, which is what rotates the
    /// working line's verb (**D487**). Counted here rather than in the pane
    /// because the pane is handed a turn's facts, not asked to notice one
    /// beginning.
    turns: u64,
    /// How many messages this session has queued, so each gets a correlation
    /// id of its own for `SteerConsumed` to name.
    steers: u32,
    /// Whether the engine is holding a revert. The fallback lane pauses while
    /// one is: a prompt after an undo is what makes that undo permanent, and
    /// a message queued before the user undid anything must not be what
    /// decides it for them.
    revert_pending: bool,
    /// Whether the next `RevertChanged` answers a code-only rewind (**F7**).
    ///
    /// The engine records no revert for one — the files move and the
    /// transcript does not — so the event it sends looks like every other
    /// revert and means something else. Which one it is was decided by the
    /// command this side sent, exactly as [`Cleared`] decides what a cleared
    /// revert means: the engine draws no distinction because the frontend's
    /// own command already did (**R10**).
    code_only_rewind: bool,
    /// When Esc was last pressed with nothing streaming and no modal open, for
    /// the Esc Esc gesture. See the Esc arm in [`App::handle_key`].
    last_esc: Option<Instant>,
    /// Repairs escape sequences a read boundary split (**D516**): the drive
    /// loop feeds every terminal event through it before anything reaches
    /// [`App::handle`], so a phantom Esc and its `[D` tail become the arrow
    /// key they were.
    escrepair: EscRepair,
    /// Whether the kitty keyboard flags are pushed (**D517**) — what the
    /// external editor door pops on the way out and re-pushes on the way
    /// back, keeping the alternate screen's stack balanced at one entry.
    kitty_keys: bool,
    /// The backtrack walk, while the Esc Esc gesture is stepping the
    /// transcript's user messages (**D467**).
    backtrack: Option<Backtrack>,
    /// Where a mention resolves from, and where the walk that offers files
    /// starts.
    cwd: PathBuf,
    /// Where the project starts. What a submitted `@path` is checked against,
    /// because that is what the engine resolves an attachment against — see
    /// [`mention::attachable`].
    root: PathBuf,
    /// Where a copy goes. Behind a trait so a test asserts what ganja decided
    /// to copy rather than what the machine running it has for a desktop.
    clipboard: Box<dyn clipboard::Clipboard>,
    /// How many clipboard images this session has already saved, so each new
    /// one earns a fresh `clipboard-<n>.png` name (**F3**) instead of
    /// colliding with the last.
    clipboard_pastes: u32,
    /// The kitty-graphics emitter this terminal earned at startup, or
    /// [`None`] where previews would be escape noise — every test's value.
    graphics: Option<graphics::Emitter>,
    /// Per attached-image path: the id its pixels were transmitted under and
    /// the thumbnail's pixel box — or the zero id, cached for a file that
    /// would not decode so a broken file is never re-read every frame.
    transmitted: HashMap<String, (u32, u32, u32)>,
    /// The last id handed out; ids start at one so zero can mean "failed".
    image_id: u32,
    /// When the clipboard-image hint last showed, for its 30-second rate
    /// limit — matching Claude Code's own observed behaviour.
    image_hint_last: Option<std::time::Instant>,
    /// The clipboard images pasted this session, by the number their
    /// composer token carries: `[Image #N]` in the text is Claude Code's own
    /// spelling (2026-08-15), and this map is what turns the token back into
    /// the saved file at send time — the token stays in the text, compact
    /// where a scratch path is noise, and the path rides `mentions` exactly
    /// as an `@` attachment does.
    pasted_images: Vec<(u32, String)>,
    /// Where a pasted clipboard image is saved, or [`None`] to resolve the
    /// real `<XDG data>/ganja/clipboard` at paste time. Overridden in tests
    /// (`App::with_clipboard_scratch_dir`) for the same reason
    /// [`App::cwd`]/[`App::root`] are builder-set: a paste in a test must
    /// never reach a real person's data directory.
    clipboard_scratch: Option<PathBuf>,
    /// What the composer remembers across submissions; an Up-arrow on an empty
    /// prompt walks back through it. Inert until [`App::with_history`] hands it
    /// a real store — the default touches no disk, so a test never reads or
    /// writes the machine's own prompt history.
    history: History,
    /// OSC 52 escapes waiting to reach the terminal. A copy queues one here
    /// rather than writing it straight out, so the sequence is flushed at draw
    /// time and serializes after a frame instead of landing in the middle of
    /// one. See [`App::draw`].
    pending_osc: Vec<String>,
    /// Whether the terminal reports itself looked at, off the crossterm
    /// focus-change events. **Assumed focused at startup**: crossterm only
    /// learns the state from the first focus event, and until one arrives the
    /// quiet reading is the one a wrong guess costs least under — a session
    /// that starts watched and never loses focus should never hear a bell
    /// (**D468**).
    focused: bool,
    /// The focus-gated notification writer (**D468**). Holds the loaded `tui`
    /// table; [`App::announce`] is the one door to it, which is where the
    /// focus gate lives.
    notifier: notify::Notifier,
    /// How many MCP servers this run configured, and therefore how many
    /// statuses there are still to wait for. Zero means nothing to watch, and
    /// the poll below never runs.
    mcp_servers: usize,
    /// The MCP notice the status bar is already carrying, so a poll that finds
    /// nothing new touches nothing.
    mcp_notice: Option<String>,
    /// How many of them have answered.
    mcp_resolved: usize,
    /// How many background jobs the status bar last reported running, so a
    /// tick that finds the same count touches nothing (**F1**).
    running_jobs: usize,
    /// The lead's own mailbox, on a session that leads a team (**D503**).
    ///
    /// [`None`] is a session with no team at all — no directory on disk and
    /// nobody to hear from — which is every session until something installs
    /// one, and every test that does not.
    lead_inbox: Option<LeadInbox>,
    /// When the §6.2 pass last ran. The loop ticks far faster than the lead's
    /// own cadence, and the pass reads a file: a gate here is what keeps the
    /// reference's 1000 ms from becoming the loop's 16.
    team_polled: Option<Instant>,
    /// How many teammates the bar last reported, for [`App::running_jobs`]'s
    /// reason on the sibling count.
    teammates: usize,
    /// The queue a teammate's permission dialogs arrive on (**D-5**), claimed
    /// once from the engine when the app was built.
    ///
    /// Take-once on the engine's side, so this is the one reader. A lead that
    /// never claimed it would leave its teammates' asks refused rather than
    /// hanging — which is why claiming it is not optional for a frontend that
    /// installed a team.
    teammate_dialogs: Option<tokio::sync::mpsc::Receiver<Forwarded>>,
    /// Where a forwarded dialog's answer goes back, by the id of the request
    /// that raised it.
    ///
    /// The reply travels on a channel rather than as a `ReplyPermission`,
    /// because the turn waiting on it is a **different engine's**: a command
    /// sent to this one would name an id it holds nothing for. Dropping the
    /// sender is an answer too — the refusal a dialog nobody could show means
    /// — so a request that leaves this map unanswered is refused rather than
    /// left hanging.
    forwarded_dialogs: HashMap<PermissionId, tokio::sync::oneshot::Sender<PermissionReply>>,
    /// Peer messages handed to the running turn and not yet consumed, keyed by
    /// the steer id the strip renders them under (**D503**).
    ///
    /// A **batch** per id, because a whole §6.2 pass crosses as one
    /// `Command::Steer`: the event that names the id retires everything that
    /// command carried, so the id owns a list rather than a message.
    ///
    /// Only [`Delivery::Acknowledged`] senders are ever in here; see
    /// [`App::deliver_peers`] for what the other arm does instead. The
    /// [`Delivered`] is kept rather than the text, because pruning the lead's
    /// inbox needs the identity it carries — which is what makes the entry
    /// **durable**: until the engine says it consumed the message, the message
    /// is still in the mailbox and the next pass would offer it again. That
    /// durability is also why this map is what a pass is filtered against:
    /// re-offering is by design, and delivering twice is not.
    peer_steers: HashMap<String, Vec<Delivered>>,
    /// Peer messages a `SteerConsumed` retired, waiting to leave the mailbox.
    ///
    /// [`App::auto_permissions`]'s shape and its reason: `handle_core` is
    /// synchronous and cannot reach the disk, so what it decides is carried to
    /// the first point after the event where an `await` is allowed.
    settled: Vec<Delivered>,
    /// This process's own mailbox, on a session that **is** a pane teammate
    /// (§10.3) — the other posture, and the mirror of [`App::lead_inbox`].
    ///
    /// [`None`] is every session that was not launched with §4.1's flags,
    /// which is every session a person starts and every test that does not
    /// say otherwise. A session is one or the other and never both: a pane
    /// teammate leads no team of its own, so the two fields cannot be
    /// populated together by construction of `lib.rs`'s startup.
    member: Option<member::Inbox>,
    /// When the member's §6.1 pass last ran, for [`App::team_polled`]'s
    /// reason at the member's own cadence.
    member_polled: Option<Instant>,
    /// The socket a lead session serves under its current id (**D505**),
    /// kept in step with the engine's session slot after every event and
    /// closed at the tail of the run. [`None`] for every session that is not
    /// a lead handed a binder — a pane member, a build with no config home,
    /// every test — which binds nothing and costs nothing.
    socket: Option<binder::SessionSocket>,
    /// The live-session listing the `@` menu and the incumbent collision
    /// scan read (**D527**–**D530**), handed in for every interactive
    /// non-member session — wider than [`App::socket`]'s lead-only gate.
    /// [`None`] degrades to files and roster only (**AC-27**).
    lister: Option<Box<dyn lister::Lister>>,
    /// The engine's session id and the bound socket path this session's own
    /// registration record was last written beside, if this session is
    /// registered right now. Both are re-derived from it: [`App::socket`]
    /// itself exposes neither outside tests, and the app tracking its own
    /// copy is what lets it know a rebind is happening (P3) without touching
    /// [`binder::SessionSocket::sync`].
    registered: Option<(SessionId, PathBuf)>,
    /// Where [`App::registered`]'s name came from — `--name`'s or the
    /// project root's derived default (**D527**) — flipped to
    /// [`registry::NameSource::User`] by every `/rename` (**ADJ-2**): a
    /// typed rename is always the person's own.
    self_name_source: registry::NameSource,
    /// The registry directory this session's own registration and collision
    /// scans read and write, standing in for [`ganja_tool::socket::directory`]
    /// — the hidden `--socket-dir` override in production (a lead's own
    /// bound path already reflects it; a **teamless** session binds no
    /// socket, so this is the only way its own scan reads the same
    /// directory), and [`App::clipboard_scratch`]'s own reason in a test: a
    /// test's registration and collision scans must never reach a real
    /// person's `/tmp`.
    registry_directory: Option<PathBuf>,
    /// The live sessions the `@` menu last saw (**D529** Axis 5), also what
    /// submit-time classification checks a token's name against —
    /// persists past the menu's own close, since submit runs after Enter
    /// already closed it.
    session_listing: Vec<lister::LiveSession>,
    /// Colliders already surfaced by the incumbent's re-scan (**S1**), so a
    /// name already warned about is not warned about again every thirty
    /// seconds — "once per newly seen collider".
    known_colliders: std::collections::HashSet<String>,
    /// When the incumbent collision re-scan last ran, throttled to at most
    /// once every [`COLLISION_RESCAN_INTERVAL`] (**R4**) — [`App::team_polled`]'s
    /// pattern, on the same `Tick`.
    collision_scanned: Option<Instant>,
    /// A `shutdown_request` this member has taken and not yet answered,
    /// because a turn was still running when it arrived.
    ///
    /// The answer waits for the turn's end rather than cutting it short,
    /// which is `Teammate::shutdown`'s courtesy on the in-process side: the
    /// turn is a transcript somebody may open tomorrow. It waits
    /// [`ganja_core::teammate::SETTLE`] and no longer, and then cancels.
    member_shutdown: Option<MemberShutdown>,
    /// A turn's end this member has yet to tell the lead about (§10.3-3).
    ///
    /// [`App::settled`]'s shape and reason: `handle_core` is synchronous and
    /// the write is a file, so what the event decided is carried to the first
    /// point after it where an `await` is allowed.
    member_finished: Option<(FinishReason, Option<String>)>,
    /// Asks this member's rules raised and its posture forwards to the lead
    /// (D-5), waiting for the first `await` after the event to be written.
    ///
    /// [`App::auto_permissions`]'s shape, pointed at a file instead of at
    /// this engine: `handle_core` decides, the write happens after. The event
    /// itself is carried, because the value that writes it — the member's
    /// [`ganja_core::teammate::member::Asks`] — reads the ask off the event
    /// and holds the wait by its id.
    member_asks: Vec<CoreEvent>,
    /// The `(tokens, window)` pair the context meter last showed, so a tick
    /// that finds the estimate unmoved touches nothing (**D469**).
    context: Option<(u64, u64)>,
    /// The rate-limit windows the bar last showed, so a tick that finds the
    /// set unmoved touches nothing (**D484**; since 2026-08-15 the bar meters
    /// the newest set as heard, no clock consulted, so the raw set is the
    /// whole of what the redraw depends on).
    rates: Vec<ganja_core::provider::RateWindow>,
    /// The plan buckets the bar last showed, kept for [`App::poll_rates`]'s
    /// reason on the sibling set (**D485**).
    plans: Vec<ganja_core::provider::PlanWindow>,
    /// The wire-served model rows for this session's provider, once a fetch
    /// has landed them. Held for the App's lifetime on purpose: a login
    /// stored mid-session is picked up by a restart, not by a later fetch.
    wire_models: Option<Vec<provider::ListedModel>>,
    /// The listing fetch while one is in flight, reaped on Tick the way the
    /// MCP dial is polled. Also the guard that keeps a second `/model` from
    /// spawning a second fetch.
    wire_fetch: Option<JoinHandle<WireListing>>,
    /// The `@` menu's project walk while one is in flight, reaped on Tick
    /// exactly as `wire_fetch` is (2026-08-15: awaiting it inline stalled
    /// every keystroke by the whole walk's duration on a big or cold tree).
    /// The newest fragment wins — a keystroke cancels and replaces an older
    /// walk — and the reap installs the menu only while the composer still
    /// shows the fragment it walked for.
    file_walk: Option<FileWalk>,
    /// The `/plugin` store action while one is in flight, carrying the notice
    /// it will answer with. Reaped on Tick beside [`App::wire_fetch`] and for
    /// the same reason: a marketplace add is a `git clone`, and the loop
    /// draws rather than clones. Also the guard that keeps a second action
    /// from racing the first over the same `plugins.json`.
    plugin_task: Option<JoinHandle<String>>,
    /// The tools the `@` menu drives. The registry rather than the glob tool
    /// alone, so the menu asks for its walker by the name the engine knows it
    /// by instead of holding a second copy of the decision.
    tools: ganja_tool::Registry,
    /// Every theme this run can switch to, and which one is active.
    themes: Themes,
    theme: Theme,
    /// What the session has spent, accumulated across turns.
    totals: Totals,
    /// State changed since the last frame.
    dirty: bool,
    /// The change came from the keyboard, which skips the coalescing gate.
    urgent: bool,
    /// Something else had the terminal, so the next frame cannot trust the
    /// diff against what was last drawn.
    stale: bool,
    last_draw: Instant,
    /// When this app was built — the `/usage` dialog's `Total duration`
    /// (W7). The app's own wall clock, not the stored session's: a resumed
    /// session's earlier processes left no clock behind, and inventing one
    /// would put a wrong figure where an honest short one belongs.
    session_start: Instant,
    quit: bool,
}

impl App {
    /// Builds an app driven by `engine`, showing `notice` in the status bar,
    /// drawn in whichever of `themes` is active.
    ///
    /// The model is **asked of the engine** rather than passed in beside it.
    /// The two can already differ by the time a session starts — the default
    /// agent may name a model of its own, and a resumed session restores the
    /// one it was left on — and a frontend that priced against the model the
    /// process was launched with would be pricing tokens nobody spent. Every
    /// later change re-reads it from the same place, so this is the one that
    /// used to be able to disagree.
    ///
    /// The registry is handed in rather than loaded here so that the disk —
    /// the user's theme directory and their stored pick — is read on the one
    /// startup path that should read it, and so that the lane wiring
    /// configuration in has somewhere to put a configured theme.
    #[must_use]
    pub fn new(engine: Engine, notice: Option<String>, mut themes: Themes) -> Self {
        // Shared from the first line rather than at the struct literal,
        // because the gated inbox below closes over the engine it will ask
        // for the receiver class — a clone of the same handle every other
        // seam holds (**D505**).
        let engine = Arc::new(engine);
        let theme = themes.theme();
        let agent = engine.agent();
        let model = engine.model();
        let engine_commands = command::EngineCommand::roster(engine.commands());
        let agent_kinds: Vec<command::Completion> = engine
            .agents()
            .map(|registry| {
                registry
                    .agents()
                    .iter()
                    .filter(|agent| agent.spawnable())
                    .map(|agent| command::Completion {
                        text: agent.name.clone(),
                        detail: agent.description.clone().unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut status = Status::new(notice);
        status.set_agent(agent.clone());
        status.set_model(Some(model.clone()));
        // Both halves of the lead side, claimed here because this is the one
        // frontend and both are take-once: the mailbox pass, and the queue a
        // teammate's dialogs cross on. A session leading no team gets neither,
        // which is what makes every test that builds a bare engine cost
        // nothing (**D503**). The pass is **gated** (**D523**): the engine's
        // own admission state, and its receiver-class read carried as a
        // closure, so a non-roster scribble in the inbox is policy's to
        // admit, hold or refuse — and a fabricated frame from one raises no
        // dialog (AC-21) — instead of the pre-gate deliver-everything bridge.
        let lead_inbox = engine.teammates().map(|team| {
            LeadInbox::new(Arc::clone(team.registry())).gated(Arc::clone(engine.inbound()), {
                let engine = Arc::clone(&engine);
                move || engine.receiver_class()
            })
        });
        let teammate_dialogs = engine.teammate_dialogs();
        // Built unconditionally, because a channel nobody sends on costs one
        // allocation and a branch here would be a second thing to keep in step
        // with whether a team was installed.
        let (spawn_asker, spawn_asks) = tokio::sync::mpsc::channel(SPAWN_ASKS);

        Self {
            engine,
            provider: String::new(),
            model,
            // A fresh engine runs no effort, and a resumed one announces its
            // restoration through the same accessor the resume path re-reads.
            effort: None,
            agent,
            chat: Chat::default(),
            editor: Editor::new(&theme),
            status,
            keys: Keybinds::defaults(),
            permission: None,
            queued_permissions: VecDeque::new(),
            yolo: false,
            auto_permissions: VecDeque::new(),
            question: None,
            sessions: None,
            theme_list: None,
            history_search: None,
            rewind: None,
            mcp_dialog: None,
            held_dialog: None,
            plugin_dialog: None,
            team_dialog: None,
            team_spawn: None,
            spawn_asks,
            spawn_asker,
            spawn_dialogs: HashMap::new(),
            plugin_store: None,
            context_dialog: None,
            usage_dialog: None,
            chooser: None,
            palette: None,
            palette_filter: String::new(),
            help: None,
            inspector: None,
            event_log: VecDeque::new(),
            turn_usages: VecDeque::new(),
            dropdown: None,
            completion: None,
            agent_kinds,
            files: None,
            skill_menu: None,
            engine_commands,
            // Nothing has been sent yet, and the one `RevertChanged` that can
            // arrive before anything is — a resumed session's — carries a
            // revert rather than clearing one. Unhide is the reading that
            // keeps a transcript replayable if that ever stops being true:
            // showing entries the engine still holds is recoverable, and
            // dropping entries it holds is not.
            cleared: Cleared::Unhide,
            queue: Queue::default(),
            turn_running: false,
            turns: 0,
            steers: 0,
            revert_pending: false,
            code_only_rewind: false,
            last_esc: None,
            escrepair: EscRepair::active(),
            kitty_keys: false,
            backtrack: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            clipboard: Box::new(clipboard::System::default()),
            clipboard_pastes: 0,
            pasted_images: Vec::new(),
            clipboard_scratch: None,
            // Inert until the startup lane hands over the loaded store: reading
            // the disk here would mean every test touched the real history.
            history: History::default(),
            pending_osc: Vec::new(),
            focused: true,
            notifier: notify::Notifier::default(),
            mcp_servers: 0,
            mcp_notice: None,
            mcp_resolved: 0,
            running_jobs: 0,
            lead_inbox,
            team_polled: None,
            teammates: 0,
            teammate_dialogs,
            forwarded_dialogs: HashMap::new(),
            peer_steers: HashMap::new(),
            settled: Vec::new(),
            member: None,
            member_polled: None,
            socket: None,
            lister: None,
            registered: None,
            // No one has typed anything yet; the first registration derives
            // its name from the project root, which is what makes the
            // default `Derived` rather than `User` (**D527**).
            self_name_source: registry::NameSource::Derived,
            registry_directory: None,
            session_listing: Vec::new(),
            known_colliders: std::collections::HashSet::new(),
            collision_scanned: None,
            member_shutdown: None,
            member_finished: None,
            member_asks: Vec::new(),
            context: None,
            rates: Vec::new(),
            plans: Vec::new(),
            wire_models: None,
            wire_fetch: None,
            file_walk: None,
            graphics: None,
            transmitted: HashMap::new(),
            image_id: 0,
            image_hint_last: None,
            plugin_task: None,
            tools: ganja_tool::Registry::with_builtins(),
            themes,
            theme,
            totals: Totals::default(),
            dirty: true,
            urgent: true,
            stale: false,
            last_draw: Instant::now(),
            session_start: Instant::now(),
            quit: false,
        }
    }

    /// Resolves `@` mentions against `cwd` instead of the process's own.
    ///
    /// A builder because only the file menu reads it, and because every test
    /// that does not raise one should not have to answer for where the machine
    /// running it happened to be standing.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();

        self
    }

    /// Checks submitted `@path` mentions against `root` instead of the
    /// process's own directory.
    ///
    /// Separate from [`App::with_cwd`] because the two are different questions
    /// and can legitimately differ: the menu offers files from where the user
    /// is standing, and an attachment is resolved from where the project
    /// starts.
    #[must_use]
    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = root.into();

        self
    }

    /// Enables kitty-graphics previews, handed in by the real frontend after
    /// environment detection — never defaulted, so a test's `TestBackend`
    /// frame can never leak escape sequences onto a real stdout. The chat
    /// learns at the same moment: with pixels available, an attached image's
    /// transcript row is a reserved box rather than a path.
    #[must_use]
    pub fn with_graphics(mut self, graphics: Option<graphics::Emitter>) -> Self {
        self.graphics = graphics;
        self.chat.set_graphics(graphics.is_some());
        self
    }

    /// Copies through `clipboard` instead of the system's.
    #[cfg(test)]
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: Box<dyn clipboard::Clipboard>) -> Self {
        self.clipboard = clipboard;

        self
    }

    /// Saves a pasted clipboard image under `dir` instead of the real
    /// `<XDG data>/ganja/clipboard` — a test seam, so a paste never reaches a
    /// real person's data directory.
    #[cfg(test)]
    #[must_use]
    pub fn with_clipboard_scratch_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.clipboard_scratch = Some(dir.into());

        self
    }

    /// Announces through `notifier` instead of the inert default.
    ///
    /// A builder because only the startup lane holds a loaded config to build
    /// one from — the default announces nothing, so a test that does not opt
    /// in writes no escape anywhere (**D468**).
    #[must_use]
    pub fn with_notifier(mut self, notifier: notify::Notifier) -> Self {
        self.notifier = notifier;

        self
    }

    /// Runs this session **as** the pane teammate `inbox` belongs to (§10.3).
    ///
    /// A builder because only the startup lane holds §4.1's flags: the default
    /// is a session that is nobody's teammate, so a test that does not opt in
    /// reads no inbox and writes no frame. What it changes is the tick — the
    /// member's own §6.1 pass runs from it — and the turn's end, which now
    /// tells the lead. Everything else about the session is what it always
    /// was, which is the whole of §10.3's claim.
    #[must_use]
    pub fn with_member(mut self, inbox: member::Inbox) -> Self {
        self.member = Some(inbox);

        self
    }

    /// Serves this session on a socket bound through `binder` (**D505**).
    ///
    /// A builder because only the startup lane knows whether this session is
    /// a lead and holds the binder the binary handed in: the default binds
    /// nothing, so a test that does not opt in serves nothing. Nothing is
    /// bound here — the first [`App::run`] pass binds under the id the
    /// engine is on by then, which is after any startup resume.
    #[must_use]
    pub fn with_socket(mut self, binder: Box<dyn binder::Binder>, served: binder::Served) -> Self {
        self.socket = Some(binder::SessionSocket::new(binder, served));

        self
    }

    /// Offers the `@` menu and the send resolver the live-session listing
    /// `lister` answers (**D529** Axis 5, **D530**'s re-derived gate).
    ///
    /// A builder for the reason [`App::with_socket`] is one: only the
    /// startup lane knows whether this session is interactive and not a
    /// pane member, and holds whatever the binary built over the registry
    /// and a health probe. The default offers nothing, so a test that does
    /// not opt in sees files and roster only (**AC-27**).
    #[must_use]
    pub fn with_lister(mut self, lister: Box<dyn lister::Lister>) -> Self {
        self.lister = Some(lister);

        self
    }

    /// Reads and writes this session's own registration record, and scans
    /// for collisions, under `directory` instead of
    /// [`ganja_tool::socket::directory`].
    ///
    /// Two callers, one seam: the hidden `--socket-dir` override, so a
    /// **teamless** session's own collision scan — which binds no socket of
    /// its own to read a directory off — reads the same directory the
    /// binder and the resolver do (a lead's own scan already gets this for
    /// free, off its bound path); and a test, for the same reason
    /// `clipboard_scratch` exists — it must never reach a real person's
    /// `/tmp/ganja-<uid>/`, and a fake-bound socket's own path
    /// (`/nowhere/...`, `binder.rs::fake`) names no directory a record
    /// could really be written into.
    #[must_use]
    pub fn with_registry_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.registry_directory = Some(directory.into());

        self
    }

    /// Where this session's registration record will name its own name
    /// from once it registers — `--name`'s (or a fresh `/rename`'s) is
    /// [`registry::NameSource::User`],
    /// the project root's derived basename is
    /// [`registry::NameSource::Derived`] (**D527**).
    ///
    /// A builder because only the startup lane knows which of the two
    /// [`ganja_core::Engine::set_self_name`] was seeded with (REVISION-3
    /// P5's resolution); the default is [`registry::NameSource::Derived`],
    /// matching a session nothing renamed yet.
    #[must_use]
    pub fn with_self_name_source(mut self, source: registry::NameSource) -> Self {
        self.self_name_source = source;

        self
    }

    /// Renders the configured statusline roster instead of the default bar.
    ///
    /// A builder because only the startup lane holds a loaded config to read
    /// the `tui.statusline` table from — absent, the bar is the fixed layout
    /// it always was, so a test that does not opt in renders what it always
    /// rendered (**D469**).
    #[must_use]
    pub fn with_statusline(mut self, statusline: Option<&StatuslineConfig>) -> Self {
        self.status.set_statusline(statusline);

        self
    }

    /// Answers this session's own permission dialogs (**D479**).
    ///
    /// A builder because only the startup lane holds the command line that
    /// asked for it: the default is a session that asks, so a test that does
    /// not opt in raises every dialog it always raised. See `App::yolo` for
    /// what the bypass covers and what it deliberately leaves alone.
    #[must_use]
    pub fn with_yolo(mut self, yolo: bool) -> Self {
        self.yolo = yolo;
        // The marker is set here rather than read by the bar, so that the one
        // decision has one owner: a bar that could disagree with the app about
        // whether this session is bypassed is the one bug this whole feature
        // cannot afford.
        self.status.set_yolo(yolo);

        self
    }

    /// Records the kitty keyboard verdict (**D517**): with the protocol
    /// active the split-Esc ambiguity cannot occur, so the repair machine
    /// runs in passthrough rather than paying the hold-off (**D516**).
    /// Where the focus starts — what the notifier's gate (**D468**) reads
    /// until the first focus event. Focus is otherwise learned from changes,
    /// and a change presumes a starting state: outside tmux "looked at" is
    /// the only honest one, and inside tmux the pane's own standing is what
    /// tmux answers (`Server::focused`) — a pane split beside the lead starts
    /// unfocused, and is told nothing of it.
    #[must_use]
    pub fn with_focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    #[must_use]
    pub fn with_kitty_keys(mut self, kitty: bool) -> Self {
        self.kitty_keys = kitty;
        if kitty {
            self.escrepair = EscRepair::passthrough();
        }
        self
    }

    /// Reads and writes plugins in `store` instead of discovering the real
    /// one under the config home.
    ///
    /// A builder for the reason [`App::with_history`] is one: the default
    /// discovers the machine's own store only when the `/plugin` dialog
    /// opens, so a test that hands one in never touches a real person's
    /// plugins.
    #[cfg(test)]
    #[must_use]
    pub fn with_plugin_store(mut self, store: ganja_core::plugin::Store) -> Self {
        self.plugin_store = Some(store);

        self
    }

    /// Remembers submitted prompts in `history` instead of the inert default.
    ///
    /// A builder because only the startup lane should read the disk: the
    /// default store touches nothing, so a test that does not opt in never
    /// reaches the machine's own prompt history.
    #[must_use]
    pub fn with_history(mut self, history: History) -> Self {
        self.history = history;

        self
    }

    /// Watches `servers` MCP connections come up, and says so when one fails.
    ///
    /// The count is handed over rather than asked of the engine because the
    /// engine's status map is deliberately silent about a server that is still
    /// being dialled: knowing how many there are is what tells "all still
    /// connecting" from "none configured", and therefore when the loop can
    /// stop waking up to look.
    #[must_use]
    pub fn watching_mcp(mut self, servers: usize) -> Self {
        self.mcp_servers = servers;

        self
    }

    /// Names the provider the engine was built on.
    ///
    /// A builder rather than a constructor argument because it is exactly one
    /// dialog's business — the model list — and every test that does not open
    /// that dialog should not have to answer for it.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();

        self
    }

    /// Uses `keys` instead of the compiled-in bindings.
    #[must_use]
    pub fn with_keybinds(mut self, keys: Keybinds) -> Self {
        self.keys = keys;

        self
    }

    /// Fills the transcript from a resumed session's stored `transcript`.
    ///
    /// The messages take the same route into the viewport that a live
    /// `MessageStarted` takes, so a resumed session and a streamed one are the
    /// same entries built the same way. Nothing is invented on the way in: the
    /// engine hands over what it read back, including replies it never saw
    /// finish, which render as interrupted rather than as complete.
    ///
    /// The spend counters are deliberately left alone. What a stored session
    /// cost cannot be recomputed here — the store records the tokens but not
    /// which model spent them — so the status bar keeps meaning what it has
    /// always meant: what this run has spent. The picker is where a session's
    /// accumulated size is shown.
    pub fn seed(&mut self, transcript: Vec<Message>) {
        for message in transcript {
            self.chat.restore_message(message);
        }
    }

    /// Runs until the user quits or the terminal goes away.
    ///
    /// # Errors
    ///
    /// Returns an error if the engine refuses a subscription, or if the
    /// terminal cannot be read from or drawn to.
    pub async fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        // Bound before the first frame and after every startup resume, which
        // is what `lib.rs` has done by the time it hands the app over: the
        // first socket is named by the session the screen opens on.
        self.sync_socket().await;
        let outcome = self.drive(terminal).await;
        // The record first, and the socket after it (**D527**): stop
        // advertising before stopping answering. A record without a live
        // lock is filtered by every reader; a socket without a record is
        // simply unlisted — so either order is safe, and this one means
        // nobody ever reads a record naming a socket that has already gone
        // quiet. Then the socket, whichever way the loop ended: nobody here
        // will read a peer's message once the loop is over, and the file is
        // what `ganja sessions --live` would otherwise list as dead. Held
        // here for the reason `session_end` is — `run` consumes the app.
        self.unregister_self();
        if let Some(socket) = &mut self.socket {
            socket.shutdown().await;
        }
        // Whichever way the loop ended, the error paths included: this session
        // is over, and a `SessionEnd` hook that only fired on the clean exits
        // would miss exactly the endings somebody would want to hear about.
        // Held here rather than in `lib.rs` because `run` consumes the app, and
        // the id the envelope names is the session the engine is on *now* —
        // which a resume may have moved since startup.
        self.engine.session_end(ganja_core::hook::EXIT_REASON).await;

        outcome
    }

    /// Keeps the session socket bound under the engine's current id, when
    /// this session serves one: after every event, because the slot can move
    /// through this app's own doors (`/new`, the picker) — never through the
    /// socket, which serves three routes and no session route — and one
    /// comparison of an id is cheaper than knowing every door. A refusal
    /// reaches the status bar once per id (**D505**, best-effort by design).
    async fn sync_socket(&mut self) {
        if self.socket.is_none() {
            return;
        }
        let wanted = self.engine.session_id();
        // The registration record's own rebind rule (**D527**, **P3**): the
        // app removes its own old record the moment it observes the slot
        // is about to move — ahead of the new bind's outcome, and without
        // touching `SessionSocket::sync` itself (`binder.rs` stays
        // byte-untouched) — so a refused rebind still leaves no stale
        // advertisement behind (N9a). A first bind (nothing registered yet)
        // removes nothing.
        if self
            .registered
            .as_ref()
            .is_some_and(|(previous, _)| *previous != wanted)
        {
            self.unregister_self();
        }
        let synced = {
            let socket = self.socket.as_mut().expect("checked above");
            socket.sync(&self.engine).await
        };
        match synced {
            binder::Synced::Unchanged => {}
            binder::Synced::Bound(path) => self.register_self(wanted, path),
            binder::Synced::Refused(sentence) => {
                self.status.set_notice(Some(sentence));
                self.dirty = true;
            }
        }
    }

    /// Where this session's own registration lives, and where the
    /// collision scan reads: [`App::registry_directory`] in a test, else
    /// wherever this session is registered right now, else the well-known
    /// default (**D527**).
    ///
    /// The default is what a **teamless** session's collision scan reads —
    /// it binds no socket, so it has no bound path of its own to derive a
    /// directory from, and the shared `/tmp/ganja-<uid>/` is a well-known
    /// location rather than something only a bound socket's path can name.
    fn registry_dir(&self) -> PathBuf {
        self.registry_directory.clone().unwrap_or_else(|| {
            self.registered
                .as_ref()
                .and_then(|(_, path)| path.parent().map(Path::to_path_buf))
                .unwrap_or_else(ganja_tool::socket::directory)
        })
    }

    /// Registers this session under a fresh bind, replacing whatever it held
    /// before (**D527**): writes a [`registry::Record`] beside the socket at
    /// `path`, the stem read off the path itself. Registration **never
    /// refuses** — a same-uid collision surfaces a notice naming the
    /// holder's stem and cwd and registers anyway (**user-ratified
    /// 2026-08-26**).
    fn register_self(&mut self, session_id: SessionId, path: PathBuf) {
        let Some(stem) = path
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_owned)
        else {
            return;
        };
        let directory = self
            .registry_directory
            .clone()
            .unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default());
        let name = self.engine.self_name();

        self.warn_of_collision(&directory, &name, session_id.as_str());

        let record = registry::Record {
            format: registry::FORMAT,
            session_id: session_id.as_str().to_owned(),
            name,
            name_source: self.self_name_source,
            cwd: self.cwd.clone(),
            root: self.root.clone(),
            pid: std::process::id(),
            started_at: now_millis(),
        };
        if let Err(error) = registry::write(&directory, &stem, &record) {
            tracing::warn!(stem, %error, "failed to write this session's registration record");
            return;
        }
        self.registered = Some((session_id, path));
    }

    /// Removes this session's own registration record, when it has one
    /// (**D527**). The app's own act, never conditioned on the socket that
    /// named it still being bound — see the two call sites: a rebind that
    /// has only just been observed, and the tail of [`App::run`].
    fn unregister_self(&mut self) {
        let Some((_, path)) = self.registered.take() else {
            return;
        };
        let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
            return;
        };
        let directory = self
            .registry_directory
            .clone()
            .unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default());

        if let Err(error) = std::fs::remove_file(registry::record_path(&directory, stem))
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(stem, %error, "failed to remove this session's registration record");
        }
    }

    /// Surfaces a status-bar notice when a live record already holds `name`
    /// (**user-ratified 2026-08-26**): registration and `/rename` alike call
    /// this, and it never refuses — the registry is advisory data any
    /// same-uid process can write.
    ///
    /// Every holder found is also seeded into [`App::known_colliders`] — not
    /// only the one the notice names — so [`App::poll_collision_scan`]'s own
    /// first pass, due immediately after registration because nothing else
    /// has set [`App::collision_scanned`] yet, does not re-report a holder
    /// that predates this session under the *other* notice's wording ("another
    /// session registered your name"): that framing is for a collider
    /// arriving after registration, and a holder this call already told the
    /// person about is not one.
    ///
    /// Answers whether a notice actually fired, so a caller that has a
    /// second notice of its own to show does not clobber this one.
    fn warn_of_collision(&mut self, directory: &Path, name: &str, own_session: &str) -> bool {
        let Ok(holders) = registry::holders(directory, name, own_session) else {
            return false;
        };
        for holder in &holders {
            self.known_colliders.insert(holder.stem.clone());
        }
        let Some(holder) = holders.first() else {
            return false;
        };
        self.status.set_notice(Some(format!(
            "another session is already registered as {name:?} ({} at {})",
            holder.stem,
            holder.record.cwd.display()
        )));
        self.dirty = true;

        true
    }

    /// The incumbent's own re-scan (**S1**, **R4**): on the app's existing
    /// `Tick`, throttled to at most one pass per [`COLLISION_RESCAN_INTERVAL`],
    /// tells this session when another one has registered under its own
    /// name — the notice's other direction, since [`App::warn_of_collision`]
    /// only ever tells the *registering* side. Surfaced once per newly seen
    /// collider (tracked in [`App::known_colliders`]); the never-refuse rule
    /// stays intact — nothing here undoes a registration.
    fn poll_collision_scan(&mut self) {
        // A registered lead is one whose own scan matters; a teamless
        // session's own collision notice already fires at `/rename` and at
        // whatever assembly seam seeds its name — there is no record here
        // for another session to have taken over from.
        let Some((session_id, _)) = &self.registered else {
            return;
        };
        let due = self
            .collision_scanned
            .is_none_or(|last| last.elapsed() >= COLLISION_RESCAN_INTERVAL);
        if !due {
            return;
        }
        self.collision_scanned = Some(Instant::now());

        let name = self.engine.self_name();
        let directory = self.registry_dir();
        let Ok(holders) = registry::holders(&directory, &name, session_id.as_str()) else {
            return;
        };
        let fresh: Vec<_> = holders
            .into_iter()
            .filter(|holder| !self.known_colliders.contains(&holder.stem))
            .collect();
        let Some(holder) = fresh.into_iter().next() else {
            return;
        };
        self.known_colliders.insert(holder.stem.clone());
        self.status.set_notice(Some(format!(
            "another session registered your name {name:?} ({} at {})",
            holder.stem,
            holder.record.cwd.display()
        )));
        self.dirty = true;
    }

    /// The loop itself; see [`App::run`], which owns what happens after it.
    async fn drive(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut core_events = self
            .engine
            .subscribe()
            .await
            .context("failed to subscribe to engine events")?;
        let mut term_events = EventStream::new();

        loop {
            if self.needs_draw() {
                self.draw(terminal)?;
            }

            let repair_wake = self
                .escrepair
                .deadline()
                .map(tokio::time::Instant::from_std);

            let events: Vec<AppEvent> = tokio::select! {
                incoming = term_events.next() => match incoming {
                    Some(incoming) => {
                        let event = incoming.context("failed to read a terminal event")?;
                        self.escrepair
                            .accept(event, Instant::now())
                            .into_iter()
                            .map(AppEvent::Term)
                            .collect()
                    }
                    // The event source closed; there is nothing left to react to.
                    None => break,
                },
                incoming = core_events.next() => match incoming {
                    Some(incoming) => vec![AppEvent::core(incoming)],
                    None => break,
                },
                () = tokio::time::sleep(self.until_next_wakeup()), if self.wants_wakeup() => {
                    vec![AppEvent::Tick]
                }
                // A held Esc's deadline passed with nothing behind it: what
                // was held was a real key press, released here (**D516**).
                () = tokio::time::sleep_until(repair_wake.unwrap_or_else(tokio::time::Instant::now)), if repair_wake.is_some() => {
                    self.escrepair
                        .expire(Instant::now())
                        .into_iter()
                        .map(AppEvent::Term)
                        .collect()
                }
                // Raw mode swallows Ctrl-C, so this arm only fires for a signal
                // raised from outside the terminal, such as `kill -INT`.
                _ = tokio::signal::ctrl_c() => break,
            };

            for event in events {
                self.handle(event).await?;
            }

            if self.quit {
                // A spawn still in flight has nobody left to report to and may
                // be blocked on a dialog this loop will never draw again.
                // Aborted here rather than at each of the three doors that set
                // the flag, so the rule is written once — and at an `await`
                // rather than mid-write: everything the registry does to a file
                // is staged and renamed on a blocking thread the abort does not
                // reach.
                if let Some(spawn) = self.team_spawn.take() {
                    spawn.abort();
                }
                break;
            }
        }

        Ok(())
    }

    /// Applies one event to the UI state.
    ///
    /// # Errors
    ///
    /// Returns an error only if the engine fails for a reason the status bar
    /// cannot explain; refused commands are shown, not propagated.
    pub async fn handle(&mut self, event: AppEvent) -> Result<()> {
        match event {
            AppEvent::Term(event) => {
                self.handle_terminal(event).await?;
                self.dirty = true;
                self.urgent = true;
            }
            AppEvent::Core(event) => {
                self.tee_event(&event);
                self.handle_core(*event);
                // Here rather than inside `handle_core`, which is synchronous
                // and cannot reach the engine: a yolo session's answer is a
                // command like any other, and this is the first point after
                // the event where one can be sent (**D479**).
                self.answer_for_the_absent().await?;
                // Here for `answer_for_the_absent`'s reason, on the other
                // decision `handle_core` can take without being able to act on
                // it: a message the turn has provably consumed is a message
                // the mailbox may stop holding (**D503**).
                self.settle_consumed_peers().await;
                // And the member's three writes, for the same reason: a turn's
                // end is a frame the lead reads, a permission ask it forwards
                // is one too (D-5), and a shutdown waiting on that end can now
                // be answered (§10.3-3, §10.3-4).
                self.report_member_idle().await;
                self.write_asks_to_lead().await;
                self.finish_member_shutdown().await;
                self.dirty = true;
                // Run after every engine event, because the event that just
                // landed may have been the one that ended the turn — and the
                // end of a turn is the moment the fallback lane can act.
                self.replay_queued().await;
            }
            AppEvent::Tick => {
                self.poll_mcp();
                self.poll_jobs();
                self.poll_context();
                self.poll_rates();
                self.poll_plans();
                self.poll_mcp_dialog();
                self.poll_held();
                self.poll_collision_scan();
                self.poll_team().await;
                self.poll_member().await;
                self.poll_wire_models().await;
                self.poll_file_walk().await;
                self.poll_plugin_task().await;
                // The other door into the same lane: a replay that lost a race
                // to a turn starting under it keeps its place and is retried
                // here, where nothing else would wake the loop to try again.
                self.replay_queued().await;
            }
        }
        self.sync_socket().await;

        Ok(())
    }

    /// Tees `event` into the Ctrl+T inspector's raw-log ring buffer before
    /// [`App::handle_core`] consumes the original by value (**F2**).
    /// [`CoreEvent`] is `Clone` (`event.rs:14`) precisely so this can happen
    /// without disturbing the one path that already owns the event.
    fn tee_event(&mut self, event: &CoreEvent) {
        self.event_log.push_back(event.clone());
        if self.event_log.len() > MAX_EVENT_LOG {
            self.event_log.pop_front();
        }
    }

    /// Looks at where the MCP servers stand, and says so if one failed.
    ///
    /// Polled rather than pushed: the engine dials in the background and has
    /// no event for it, and a status map that has not changed costs a lock and
    /// a small clone.
    ///
    /// This runs on every tick the loop takes, which is what catches a server
    /// whose transport goes away mid-session — `mcp_status` reaps those. What
    /// [`App::pending_mcp`] adds is only the *extra* ticks during startup, when
    /// an otherwise idle app would not be waking up at all.
    fn poll_mcp(&mut self) {
        if self.mcp_servers == 0 {
            return;
        }

        let status = self.engine.mcp_status();
        self.mcp_resolved = status.len();

        let notice = mcp_notice(&status);
        if notice.is_none() || notice == self.mcp_notice {
            return;
        }

        self.mcp_notice.clone_from(&notice);
        self.status.set_notice(notice);
        self.dirty = true;
    }

    /// Counts the background jobs currently running, and updates the status
    /// bar's segment when that count changed (**F1**).
    ///
    /// Polled on the same tick the MCP dial is, for the same reason: the
    /// engine's job registry has no event of its own, and a count that has
    /// not changed costs a lock and a small clone.
    fn poll_jobs(&mut self) {
        let running = self
            .engine
            .jobs()
            .list()
            .iter()
            .filter(|status| status.state == ganja_tool::job::State::Running)
            .count();
        if running == self.running_jobs {
            return;
        }

        self.running_jobs = running;
        self.status.set_running_jobs(running);
        self.dirty = true;
    }

    /// Feeds the context meter the engine's current estimate (**D469**).
    ///
    /// Polled on the same tick the job registry is, for the same reason: the
    /// measure moves only when a turn lands or a session resumes, there is no
    /// event for it, and a value that has not changed touches nothing. An
    /// uncataloged model has no window to meter against, and the cleared
    /// value is the honest-degradation path — the element simply does not
    /// render.
    fn poll_context(&mut self) {
        let estimate = self.engine.context_estimate();
        let context = estimate.window.map(|window| (estimate.tokens, window));
        if context == self.context {
            return;
        }

        self.context = context;
        self.status.set_context(context);
        self.dirty = true;
    }

    /// Feeds the `rate` element the vendor's own rate-limit windows
    /// (**D484**).
    ///
    /// Beside [`App::poll_context`] and for its reasons: the numbers move only
    /// when a request finishes, there is no event for them, and a set that has
    /// not changed redraws nothing. Read through `Engine::rate_windows`, which
    /// reads through to the wire — so a session that resumes onto the same
    /// credential keeps the same windows, which is what they were about all
    /// along.
    ///
    /// What the bar is handed is the **raw** set, metered as heard: since
    /// 2026-08-15 the bar consults no reset clock — request buckets legally
    /// reset in milliseconds, so a clock-honoring meter either blinked with
    /// every response or pinned itself at zero — and the newest figures
    /// simply stand until a later response replaces the set. `/usage` reads
    /// the same raw set, with the room to say "expired" in words.
    fn poll_rates(&mut self) {
        let live = self.engine.rate_windows();
        if live == self.rates {
            return;
        }

        self.rates = live.clone();
        self.status.set_rates(live);
        self.dirty = true;
    }

    /// Feeds the same element the vendor's own plan buckets (**D485**).
    ///
    /// [`App::poll_rates`]'s shape and posture, on the sibling set: the raw
    /// buckets go to the bar, metered as heard until a later response
    /// replaces them.
    fn poll_plans(&mut self) {
        let live = self.engine.plan_windows();
        if live == self.plans {
            return;
        }

        self.plans = live.clone();
        self.status.set_plans(live);
        self.dirty = true;
    }

    /// The lead's side of the mailbox, once a tick (**D503**).
    ///
    /// Six things, and only the last is rate-limited *here*. Counting
    /// teammates, carrying their dialogs and the spawn gate's asks, and
    /// repainting the open `/team` dialog are reads of memory this process
    /// already holds; reaping a finished spawn awaits a handle that already
    /// reported finished. The §6.2 pass **is** a file read — one
    /// `read_to_string` and, when it finds anything, a locked
    /// read-modify-write — so it keeps the reference's own 1000 ms rather than
    /// the loop's 16.
    ///
    /// What decides how often this runs at all is [`App::until_next_wakeup`],
    /// and the two gates answer different questions. A session with a teammate
    /// running keeps waking at frame rate, because that teammate may hand over
    /// a permission dialog on a channel nothing else wakes for. A lead with an
    /// empty roster has only the file to wait for, so the loop sleeps until the
    /// pass below is really due instead of arriving sixty times a second to
    /// find it is not.
    async fn poll_team(&mut self) {
        self.poll_teammate_count();
        self.drain_teammate_dialogs();
        self.drain_spawn_asks();
        self.poll_team_dialog();
        self.poll_team_spawn().await;
        if self.lead_inbox.is_none() {
            return;
        }
        let due = self
            .team_polled
            .is_none_or(|last| last.elapsed() >= ganja_core::teammate::lead_inbox::POLL);
        if !due {
            return;
        }
        self.team_polled = Some(Instant::now());

        let Some(pass) = self.team_pass().await else {
            return;
        };
        // Control frames are already acted on — the pass did that, and never
        // handed one over to be queued. What is left is what a person reads,
        // minus whatever this app is already holding: a plain message stays in
        // the inbox until the turn provably took it, so every pass in between
        // offers the same message again. Re-offering is the durable half of
        // the design working; delivering it again is the same words reaching
        // the model N times over one long step.
        let fresh: Vec<Delivered> = pass
            .messages
            .into_iter()
            .filter(|message| !self.in_flight(message))
            .collect();
        // One command for the whole pass, rather than one per message. The
        // engine takes a batch of peers on a single `Steer`, and sending them
        // one at a time made the second refuse: `Busy` is what the engine says
        // between accepting the first prompt and starting the turn it becomes,
        // so message two of three waited out another cadence for nothing.
        self.deliver_peers(fresh).await;
        // A TUI teammate whose pane stopped running is said once, where a
        // finished spawn is said (D512 as amended for bead g9u): the pass
        // already retired it, and the model already has the prose. Two in one
        // pass share the one notice line rather than the later overwriting
        // the earlier.
        if !pass.exited.is_empty() {
            let notice = pass
                .exited
                .iter()
                .map(ganja_core::teammate::shim_tui::Exited::notice)
                .collect::<Vec<_>>()
                .join(" · ");
            self.tell_team(notice);
        }
        if !pass.retired.is_empty() {
            // The roster shrank under the bar, and nothing else this tick will
            // notice.
            self.poll_teammate_count();
        }
        self.dirty = true;
    }

    /// The member's side of the mailbox, once a tick (§6.1, §10.3-1).
    ///
    /// [`App::poll_team`]'s mirror at the teammate's own cadence
    /// ([`member::POLL`], half the lead's, because this is the side that has
    /// to notice a shutdown promptly). The pass reads the file and rules on
    /// every entry; what comes back is acted on here in §6.1's order — a
    /// shutdown ahead of everything, the lead's modes sent to the engine, and
    /// the plain messages handed to the very lane a peer's message takes on
    /// the lead's side, which is what makes the seeded task the first turn
    /// with no mechanism of its own (§10.3-2).
    async fn poll_member(&mut self) {
        // A shutdown that has waited out its courtesy is pressed before the
        // pass, so a wedged turn is cancelled on the tick that finds it late
        // rather than on some later message.
        self.press_member_shutdown().await;
        self.finish_member_shutdown().await;
        if self.member.is_none() || self.member_shutdown.is_some() {
            // Answering a shutdown is the last thing this member does with its
            // inbox; nothing that arrives after the request is delivered.
            return;
        }
        let due = self
            .member_polled
            .is_none_or(|last| last.elapsed() >= member::POLL);
        if !due {
            return;
        }
        self.member_polled = Some(Instant::now());

        let Some(pass) = self.member_pass().await else {
            return;
        };
        for mode in pass.modes {
            // The lead's posture for this member (D-15). A refusal is a log
            // line rather than a notice: nothing a person at this pane did
            // asked for it, and the frame has already left the inbox.
            if let Err(error) = self.engine.send(Command::SetPermissionMode { mode }).await {
                tracing::warn!(%error, "a permission mode the lead set was refused");
            }
        }
        for (id, reply) in pass.answers {
            self.apply_leads_answer(id, reply).await;
        }
        if let Some(request_id) = pass.shutdown {
            self.begin_member_shutdown(request_id).await;

            return;
        }
        let fresh: Vec<Delivered> = pass
            .messages
            .into_iter()
            .filter(|message| !self.in_flight(message))
            .collect();
        self.deliver_peers(fresh).await;
        self.dirty = true;
    }

    /// Whether an ask this engine raises goes to the lead rather than onto
    /// this screen (D-5): a pane teammate under [`Posture::ForwardToLead`].
    ///
    /// The session's own bypass (**D479**, `--auto` typed by a person) is
    /// checked first by the caller, as it is for every other dialog; a lead
    /// never composes that flag for a pane (**D513**), so for a teammate this
    /// is the ordinary road.
    fn forwards_asks_to_lead(&self) -> bool {
        self.member
            .as_ref()
            .is_some_and(|inbox| inbox.membership().posture() == Posture::ForwardToLead)
    }

    /// Writes the asks `handle_core` decided to forward, at the first point
    /// after the event where the disk is reachable (D-5, AC-8).
    ///
    /// An ask that could not be forwarded — the event was not a permission
    /// request, or the lead's inbox would not take the frame — is refused
    /// **here**, by this app, with the reply it would have sent for any dialog
    /// nobody could see: nothing was asked of anybody, and a turn left waiting
    /// on an answer that is not coming would be worse than a refused call the
    /// model reads.
    async fn write_asks_to_lead(&mut self) {
        if self.member_asks.is_empty() {
            return;
        }
        let asks = std::mem::take(&mut self.member_asks);
        for ask in asks {
            let forwarded = match &self.member {
                Some(inbox) => inbox.asks().forward(&ask).await,
                None => continue,
            };
            let Err(error) = forwarded else {
                continue;
            };
            let CoreEvent::PermissionRequested { id, .. } = ask else {
                continue;
            };
            tracing::warn!(request = id.as_str(), %error, "an ask was refused instead of forwarded");
            if let Err(error) = self
                .engine
                .send(Command::ReplyPermission {
                    id,
                    reply: PermissionReply::Reject,
                })
                .await
            {
                tracing::warn!(%error, "the refusal of an unforwarded ask was itself refused");
            }
        }
    }

    /// Carries the lead's answer to the engine, and puts the bar back to
    /// streaming once nothing is waiting on anybody.
    async fn apply_leads_answer(
        &mut self,
        id: ganja_protocol::PermissionId,
        reply: PermissionReply,
    ) {
        if let Err(error) = self
            .engine
            .send(Command::ReplyPermission {
                id: id.clone(),
                reply,
            })
            .await
        {
            tracing::warn!(
                request = id.as_str(),
                %error,
                "the lead's answer to a forwarded ask was refused by the engine"
            );
        }
        self.settle_member_activity();
    }

    /// The bar's activity once a forwarded ask is answered: streaming again
    /// only when no ask is waiting on the lead and no dialog is on screen.
    fn settle_member_activity(&mut self) {
        let waiting = self
            .member
            .as_ref()
            .is_some_and(|inbox| inbox.asks().waiting() > 0);
        if !waiting && self.permission.is_none() {
            self.status.set_activity(Activity::Streaming);
        }
    }

    /// One §6.1 pass, or [`None`] on a session that is nobody's teammate —
    /// [`App::team_pass`]'s shape, for its borrow.
    async fn member_pass(&self) -> Option<member::Pass> {
        let pass = self.member.as_ref()?.poll().await;

        (!pass.is_empty()).then_some(pass)
    }

    /// Takes a `shutdown_request` (§10.3-4).
    ///
    /// An idle member answers at once. One with a turn in flight waits for the
    /// turn's end — [`App::finish_member_shutdown`] answers on the finish
    /// event — and [`App::press_member_shutdown`] bounds the wait.
    async fn begin_member_shutdown(&mut self, request_id: String) {
        if self.member_idle().await {
            self.approve_member_shutdown(&request_id).await;

            return;
        }
        self.status.set_notice(Some(SHUTTING_DOWN.to_owned()));
        self.member_shutdown = Some(MemberShutdown {
            request_id,
            since: Instant::now(),
            cancelled: false,
        });
    }

    /// Cancels the turn a shutdown has waited [`ganja_core::teammate::SETTLE`]
    /// for, once.
    ///
    /// The in-process teammate's own bound: waiting is the courtesy a
    /// transcript is owed, and waiting forever is not one anybody asked for.
    /// A cancelled turn ends in a `MessageFinished` like any other, which is
    /// what then answers the request.
    async fn press_member_shutdown(&mut self) {
        let Some(pending) = &mut self.member_shutdown else {
            return;
        };
        if pending.cancelled || pending.since.elapsed() < ganja_core::teammate::SETTLE {
            return;
        }
        pending.cancelled = true;
        tracing::warn!("a teammate was still working when it was asked to shut down");
        if let Err(error) = self.engine.send(Command::CancelTurn).await {
            tracing::warn!(%error, "a teammate's turn could not be cancelled on the way out");
        }
    }

    /// Answers the shutdown once the turn it waited on has ended.
    async fn finish_member_shutdown(&mut self) {
        if self.member_shutdown.is_none() || !self.member_idle().await {
            return;
        }
        let Some(pending) = self.member_shutdown.take() else {
            return;
        };
        self.approve_member_shutdown(&pending.request_id).await;
    }

    /// Whether nothing of a turn is left running: neither the streaming this
    /// side has seen start, nor the tail the engine runs after the finish
    /// event — the `Stop` hook, the slot release — which `Engine::settle`
    /// observes and a frontend cannot see any other way. One look, no wait:
    /// the wait is the tick's, bounded by [`App::press_member_shutdown`].
    async fn member_idle(&self) -> bool {
        !self.turn_running && self.engine.settle(Duration::ZERO).await
    }

    /// Writes `shutdown_approved` to the lead and leaves through the exit path
    /// this app always had — the MCP servers, the jobs and the terminal are
    /// torn down after the loop in the order they always were.
    async fn approve_member_shutdown(&mut self, request_id: &str) {
        if let Some(inbox) = &self.member {
            inbox.approve_shutdown(request_id).await;
        }
        self.quit = true;
    }

    /// Tells the lead a turn ended, at the first point after the event where
    /// the disk is reachable (§10.3-3).
    async fn report_member_idle(&mut self) {
        let Some((reason, failure)) = self.member_finished.take() else {
            return;
        };
        if let Some(inbox) = &self.member {
            inbox.report_idle(reason, failure.as_deref()).await;
        }
    }

    /// One §6.2 pass, or [`None`] on a session leading no team.
    ///
    /// Its own method so the borrow of the inbox ends before the delivery that
    /// follows mutates everything else.
    async fn team_pass(&self) -> Option<ganja_core::teammate::lead_inbox::Pass> {
        let pass = self.lead_inbox.as_ref()?.poll().await;

        (!pass.is_empty()).then_some(pass)
    }

    /// Counts the teammates this session is leading, and updates the bar when
    /// that count changed (**D503**).
    ///
    /// [`App::poll_jobs`]'s shape and its reason: the registry has no event of
    /// its own — a teammate that shut itself down clears its own flag — and a
    /// count that has not changed costs a lock and nothing else.
    fn poll_teammate_count(&mut self) {
        let running = self
            .engine
            .teammates()
            .map_or(0, |team| team.registry().running());
        if running == self.teammates {
            return;
        }

        self.teammates = running;
        self.status.set_teammates(running);
        self.dirty = true;
    }

    /// Opens the `/team` dialog over the roster as it stands (**D504**).
    fn open_team(&mut self) {
        let Some(view) = self.team_roster() else {
            self.status.set_notice(Some(NO_TEAM.to_owned()));

            return;
        };
        let mut dialog = team::Team::new(team::rows(&view));
        dialog.set_busy(self.team_spawn.is_some());
        self.team_dialog = Some(dialog);
    }

    /// The team as the dialog and the arg door both read it.
    fn team_roster(&self) -> Option<ganja_protocol::team::TeamView> {
        self.engine.team_view()
    }

    /// Repaints the open `/team` dialog off a fresh roster.
    ///
    /// [`App::poll_mcp_dialog`]'s pattern, and it earns it twice over: a
    /// member's ring of recent calls (**D503**) moves on every tool call its
    /// teammate makes, and a teammate that shuts itself down leaves the roster
    /// with no event of its own.
    ///
    /// The frame is marked dirty only when the poll really found something
    /// different, which is that sibling's rule too: a roster nobody touched
    /// redrawing at frame rate is a dialog that costs more open than the app
    /// does streaming.
    fn poll_team_dialog(&mut self) {
        if self.team_dialog.is_none() {
            return;
        }
        let Some(view) = self.team_roster() else {
            return;
        };
        let rows = team::rows(&view);
        let moved = self
            .team_dialog
            .as_mut()
            .is_some_and(|dialog| dialog.refresh(rows));
        self.dirty |= moved;
    }

    /// One keypress while the `/team` dialog is open, which owns every key —
    /// [`drive_two_step`], the same driver the `/plugin` dialog reads.
    async fn handle_team_key(&mut self, key: KeyEvent) {
        let Some(dialog) = &mut self.team_dialog else {
            return;
        };
        match drive_two_step(dialog, key) {
            Driven::Close => self.team_dialog = None,
            Driven::Run(effect) => self.run_team_effect(effect).await,
            Driven::Stay => {}
        }
    }

    /// Runs a typed `/team` line (**D504**).
    ///
    /// **Only asking for the roster raises the dialog** (user directive,
    /// 2026-08-20). Every arm used to open it first so that a spawn's notice,
    /// a refusal and the roster converged on one surface; what that cost was
    /// an overlay in front of somebody who had just said, in one line and in
    /// full, exactly what they wanted done — and who is now looking at the
    /// composer rather than at a list of members.
    ///
    /// So the notice goes where that person is looking, and the one thing
    /// that made the dialog worth raising travels with it: the sentence a
    /// finished spawn is reported with is [`team::Spawned::notice`]'s, said
    /// on the status bar when no dialog is open, cleartext path included. A
    /// refusal reaches [`App::tell_team`], which has always had the same
    /// fallback. Nothing here needs a dialog to guard a missing team either —
    /// [`App::spawn_teammate`], [`App::ask_shutdown`] and
    /// [`App::ask_whole_team_to_stop`] each answer that for themselves.
    async fn run_team_line(&mut self, line: command::Team) {
        match line {
            // The roster **is** what the dialog is for, so asking for it is
            // the one line that raises one. `open_team` says so itself when
            // this session leads no team.
            command::Team::List => self.open_team(),
            command::Team::Spawn(spawn) => {
                self.spawn_teammate(team::spawn_request(&spawn));
            }
            // No name is the whole team, fanned out here rather than in the
            // grammar: which members there are is the registry's answer, and a
            // parser that had to know would be holding half a roster.
            command::Team::Shutdown { member } => match member {
                Some(member) => self.ask_shutdown(&member).await,
                None => self.ask_whole_team_to_stop().await,
            },
            // A refusal is about the **words**, and a session leading no team
            // mistyped them just the same — so it is answered without the
            // roster being consulted at all.
            command::Team::Refused(refusal) => self.tell_team(refusal),
        }
    }

    /// Runs a typed `/rename` line (**D527**).
    async fn run_rename_line(&mut self, line: command::Rename) {
        match line {
            command::Rename::Missing => {
                self.status
                    .set_notice(Some("/rename needs a name: /rename <name>".to_owned()));
                self.dirty = true;
            }
            command::Rename::To(name) => self.rename_self(name),
        }
    }

    /// `/rename <name>` (**D527**, **ADJ-2**): validates through
    /// [`registry::vet_name`], surfacing each refusal's own sentence; sets
    /// the engine's self-name cell through [`Engine::set_self_name`] — the
    /// one seam **every** `/rename` calls, whether or not this session
    /// leads, since a lead's own wire identity (`<name>@<team>`) never moves
    /// and a teamless session's self-name is what its next send stamps
    /// `from` with. A lead additionally rewrites its record in place (same
    /// stem) — the TUI stays the record's one writer. Either way the
    /// collision notice fires against live records (**F9**): the notice
    /// warns about a name this session may later lead under, teamless or
    /// not.
    fn rename_self(&mut self, name: String) {
        if let Err(refusal) = registry::vet_name(&name) {
            self.status.set_notice(Some(refusal.to_string()));
            self.dirty = true;
            return;
        }

        self.engine.set_self_name(name.clone());
        self.self_name_source = registry::NameSource::User;

        match self.registered.clone() {
            // `register_self` runs its own collision scan against the
            // fresh name, so nothing here duplicates it.
            Some((session_id, path)) => self.register_self(session_id, path),
            None => {
                // No record of this session's own to rewrite — a teamless
                // session, or a lead that has not bound yet — but the
                // notice still fires (F9): the collision is about the name
                // this session now answers to, record or not.
                let own_session = self.engine.session_id();
                let directory = self.registry_dir();
                if !self.warn_of_collision(&directory, &name, own_session.as_str()) {
                    self.status.set_notice(Some(format!("renamed to {name:?}")));
                    self.dirty = true;
                }
            }
        }
    }

    /// Asks every teammate to shut down — `/team shutdown` with nobody named.
    ///
    /// The fan-out and the frames are [`ganja_core::Teammates`]'s, for
    /// [`App::ask_shutdown`]'s reason; what is here is the one sentence a
    /// person reads, said **once** for the whole question they asked rather
    /// than written per member and overwritten by the next one.
    async fn ask_whole_team_to_stop(&mut self) {
        let Some(teammates) = self.engine.teammates().map(Arc::clone) else {
            self.tell_team(NO_TEAM.to_owned());

            return;
        };
        let outcomes = teammates.ask_whole_team_to_stop().await;
        if outcomes.is_empty() {
            self.tell_team(NOBODY_TO_STOP.to_owned());

            return;
        }
        let asked = outcomes.iter().filter(|(_, sent)| sent.is_ok()).count();
        // A count on its own would be a claim about teammates nobody reached,
        // so the first refusal comes with it: the rest are in the log, and one
        // notice line is what there is room to say.
        let refused = outcomes
            .iter()
            .find_map(|(member, sent)| sent.as_ref().err().map(|why| said(member, Err(why))));
        let total = outcomes.len();
        self.tell_team(match refused {
            None => format!("asked {total} teammates to stop"),
            Some(refusal) => {
                format!("asked {asked} of {total} teammates to stop{NOTICE_SEPARATOR}{refusal}")
            }
        });
    }

    /// Runs what the `/team` dialog decided.
    ///
    /// The two mailbox effects are awaited here, and the spawn is not. That is
    /// not an inconsistency: a message and a shutdown are one locked
    /// read-modify-write of one small file, where a spawn builds a second engine
    /// and may stop to ask a person — and a loop that awaited *that* would be
    /// waiting on somebody it had stopped drawing for.
    async fn run_team_effect(&mut self, effect: team::Effect) {
        match effect {
            team::Effect::Spawn { request, typed } => {
                // Remembered as the composer line it is equivalent to, so the
                // two doors onto a spawn leave the same thing behind for an
                // Up-arrow or Ctrl+R to bring back (user directive,
                // 2026-08-20). Before the spawn, which may stop to ask a
                // person: what is remembered is what was typed, not whether
                // the team took it — the rule every `/team` line follows.
                self.history
                    .append(history::PromptInfo::text(format!("/team spawn {typed}")));
                self.spawn_teammate(request);
            }
            team::Effect::Message { to, text } => {
                let said = self.post_to_member(
                    &to,
                    ganja_tool::team::Body::Text {
                        text,
                        summary: None,
                    },
                );
                self.tell_team(said.await);
            }
            team::Effect::Shutdown(member) => self.ask_shutdown(&member).await,
        }
    }

    /// Starts a teammate through the door a `task` call reaches (AC-14).
    ///
    /// The very same [`ganja_tool::task::TeammateSpawn`] a tool call hands over,
    /// through the very same [`ganja_core::Teammates::start`] — one entry for
    /// both doors since **D513** retired the `--bypass` that once told them
    /// apart. Everything else about the spawn is the team's to decide, which is
    /// what makes the two doors one sequence rather than two.
    fn spawn_teammate(&mut self, spawn: ganja_tool::task::TeammateSpawn) {
        if self.team_spawn.is_some() {
            // The dialog refuses its own input step while one runs; this
            // catches an arg-door line typed at the composer meanwhile.
            self.tell_team(team::BUSY.to_owned());

            return;
        }
        let Some(teammates) = self.engine.teammates().map(Arc::clone) else {
            self.tell_team(NO_TEAM.to_owned());

            return;
        };
        let caller = ganja_core::Caller {
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            // The **live** ruleset rather than a snapshot, because a stored
            // "always" answered five minutes ago is part of what decides this.
            permissions: self.engine.permissions(),
            // The lead's project root, which is what its rules were loaded
            // for — `posture::spawn_gate` calls that distinction the
            // anti-laundering rule rather than a detail.
            project_root: self.root.clone(),
        };
        let asker = DialogAsker {
            asks: self.spawn_asker.clone(),
        };
        self.team_spawn = Some(tokio::spawn(async move {
            teammates
                .start(spawn, &caller, &asker)
                .await
                .map(|started| started.name)
                .map_err(|refusal| refusal.reason)
        }));
        if let Some(dialog) = &mut self.team_dialog {
            dialog.set_busy(true);
        }
    }

    /// Reaps a finished `/team spawn` and says on the dialog what it did.
    ///
    /// [`App::poll_plugin_task`]'s shape: polled on the tick, awaited only once
    /// the handle reports finished, so the loop never waits on the spawn it
    /// started. A dialog closed meanwhile has nowhere to put the answer, and
    /// the status bar takes it instead — a teammate that started while nobody
    /// was looking is still a fact worth one line.
    async fn poll_team_spawn(&mut self) {
        if !self
            .team_spawn
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            return;
        }
        let handle = self.team_spawn.take().expect("checked finished above");
        let outcome = handle
            .await
            .unwrap_or_else(|error| Err(format!("the spawn task failed: {error}")));

        if let Some(dialog) = &mut self.team_dialog {
            dialog.set_busy(false);
        }
        match outcome {
            Ok(name) => {
                // The component owns Resolution 4's sentence, because the fact
                // it has to say out loud — the prompt is on disk in cleartext —
                // belongs beside the row it is about rather than in a bar the
                // next notice overwrites.
                match self.team_prompt_path() {
                    Some(prompt_path) => {
                        // The component owns the sentence either way, because
                        // the fact it has to say out loud — the prompt is on
                        // disk in cleartext — is not the dialog's to keep: a
                        // line typed at the composer raises none, and this is
                        // where that person is looking instead.
                        let spawned = team::Spawned { name, prompt_path };
                        match &mut self.team_dialog {
                            Some(dialog) => dialog.spawned(&spawned),
                            None => self.status.set_notice(Some(spawned.notice())),
                        }
                    }
                    // A team that answered a spawn and then had nowhere to
                    // keep its documents is not a thing that happens; the
                    // shorter line is here so it could not be a panic if it
                    // did.
                    None => self
                        .status
                        .set_notice(Some(format!("teammate {name} started"))),
                }
            }
            Err(refusal) => self.tell_team(refusal),
        }
        self.poll_team_dialog();
        self.dirty = true;
    }

    /// Where this team's documents are, which is where a spawn prompt lands in
    /// cleartext.
    fn team_prompt_path(&self) -> Option<String> {
        self.engine.teammates().map(|team| {
            let registry = team.registry();

            registry
                .root()
                .config_path(registry.team())
                .display()
                .to_string()
        })
    }

    /// Asks one member to shut down, and says what the mailbox answered.
    ///
    /// The **frame** is [`ganja_core::Teammates`]'s to build, and that is the
    /// whole reason this is two lines: which §6.1 frames exist, which of them a
    /// lead may send and in what order the far side reads them are the engine's
    /// facts, exactly as reading them back is
    /// ([`ganja_core::teammate::lead_inbox`] argues it for that direction). A
    /// frontend that encoded one would be a second place for one wire to
    /// drift. What is left here is a sentence.
    async fn ask_shutdown(&mut self, member: &str) {
        let Some(teammates) = self.engine.teammates().map(Arc::clone) else {
            self.tell_team(NO_TEAM.to_owned());

            return;
        };
        let outcome = teammates.ask_shutdown(member).await;
        self.tell_team(said(member, outcome.as_ref()));
    }

    /// Writes `body` into one member's inbox, through the lead's own postbox.
    ///
    /// The **same** door `send_message` posts through
    /// ([`ganja_core::Postbox::lead`]), and that matters for one reason beyond
    /// tidiness: the sender's name is bound when the postbox is built and is not
    /// an argument, so nothing here — a frontend included — can stamp somebody
    /// else's name on a message.
    async fn post_to_member(&self, to: &str, body: ganja_tool::team::Body) -> String {
        use ganja_tool::team::Postbox as _;

        let Some(teammates) = self.engine.teammates() else {
            return NO_TEAM.to_owned();
        };
        // Always a known roster name from the `/team` dialog, never a
        // resolved one — the D528 identity index is `None` here for the same
        // reason `Teammates::ask_shutdown`'s internal use is.
        let postbox = ganja_core::Postbox::lead(teammates.registry(), None);
        let outcome = postbox
            .deliver(ganja_tool::team::Address::Local(to.to_owned()), body)
            .await;

        said(to, outcome.as_ref())
    }

    /// Puts one sentence where the person who asked is looking: the dialog's
    /// own notice line while it is open, the status bar once it is not.
    fn tell_team(&mut self, notice: String) {
        match &mut self.team_dialog {
            Some(dialog) => dialog.set_notice(notice),
            None => self.status.set_notice(Some(notice)),
        }
        self.dirty = true;
    }

    /// Shows `asked` now, or queues it behind the dialog already up, and says
    /// so (**D468**): a dialog raised is a person needed, and the turn is
    /// blocked on the answer either way.
    ///
    /// One dialog at a time is still what a person is shown — the frontend's
    /// half of **D462**: the engine holds every request open and routes each
    /// reply by id, so the one on screen stays answerable and a queued one is
    /// asked as soon as it is. Written once for the three raisers — the
    /// engine's own `PermissionRequested`, a teammate's forwarded dialog, and
    /// a spawn's own gate — so they cannot drift apart.
    fn raise_permission(&mut self, summary: &str, asked: Permission) {
        self.raise_dialog(summary, PendingDialog::Permission(asked));
    }

    /// [`App::raise_permission`]'s machinery, over the widened item
    /// (**D524**): a held message's review rides the same one-on-screen,
    /// rest-queued discipline, because a person answering questions should
    /// not have two modals fighting for the same three keys.
    fn raise_dialog(&mut self, summary: &str, asked: PendingDialog) {
        self.announce(NotificationEvent::ApprovalRequested, summary);
        match &self.permission {
            Some(_) => self.queued_permissions.push_back(asked),
            None => self.permission = Some(asked),
        }
        self.status.set_activity(Activity::Permission);
        self.sync_dialog_status();
        self.dirty = true;
    }

    /// Raises the permission dialogs `/team spawn` put in front of a person.
    ///
    /// [`App::drain_teammate_dialogs`]'s twin on the other question — may this
    /// teammate *run*, rather than may its call run — and it shares that one's
    /// whole machinery: an id is minted for the ask, the dialog is shown or
    /// queued behind the one already up, and the answer is routed back on the
    /// oneshot the asker is waiting on.
    fn drain_spawn_asks(&mut self) {
        let mut asked = Vec::new();
        while let Ok(question) = self.spawn_asks.try_recv() {
            asked.push(question);
        }
        for (ask, reply) in asked {
            // A yolo session answers this one too (**D479**): only an *Ask*
            // reaches here, since a rule that denies the spawn refused it
            // before anybody was asked. `Once`, for that flag's own reason —
            // the gate remembers nothing either way.
            if self.yolo {
                let _ = reply.send(PermissionReply::Once);
                continue;
            }
            let summary = format!("approval requested: {}", ask.title);
            let id = PermissionId::ascending();
            let asked = Permission::new(
                id.clone(),
                SPAWN_TOOL.to_owned(),
                ask.title,
                ask.args,
                ask.directories
                    .iter()
                    .map(|directory| directory.display().to_string())
                    .collect(),
            );
            self.spawn_dialogs.insert(id, reply);
            self.raise_permission(&summary, asked);
        }
    }

    /// Shows the permission dialogs this session's teammates raised (**D-5**).
    ///
    /// Drained without ever awaiting the channel: the loop's job is to draw,
    /// and a lead that blocked here would stop drawing until a teammate asked
    /// something. Every one of them is shown through the **same** machinery the
    /// engine's own `PermissionRequested` goes through — one dialog on screen,
    /// the rest queued behind it — because a person answering questions should
    /// not have to know which conversation raised which.
    ///
    /// What differs is only where the answer goes: back on the request's own
    /// [`tokio::sync::oneshot`] rather than as a `ReplyPermission` to this
    /// engine, which holds nothing by that id.
    fn drain_teammate_dialogs(&mut self) {
        let mut asked = Vec::new();
        if let Some(dialogs) = &mut self.teammate_dialogs {
            while let Ok(forwarded) = dialogs.try_recv() {
                asked.push(forwarded);
            }
        }
        for forwarded in asked {
            self.raise_teammate_dialog(forwarded);
        }
    }

    /// Raises one teammate's dialog, or answers it where nobody is going to be
    /// asked.
    fn raise_teammate_dialog(&mut self, forwarded: Forwarded) {
        let CoreEvent::PermissionRequested {
            id,
            tool,
            title,
            args,
            directories,
            ..
        } = forwarded.request
        else {
            // The channel carries permission requests and nothing else, and
            // the type that fills it says so. An event of another shape is a
            // contract broken on the other side, so it is named rather than
            // silently dropped — and the sender goes with it, which refuses
            // the ask rather than leaving the teammate waiting on it.
            tracing::warn!(
                teammate = forwarded.teammate,
                "a teammate forwarded something that was not a permission request"
            );

            return;
        };
        // A yolo session stands in for the person here exactly as it does for
        // its own dialogs (**D479**), and for the same reason: only an *Ask*
        // ever reaches this channel, since a teammate's denial refuses the
        // call inside the teammate's own engine. The answer is `Once` — never
        // `Always`, which would write a rule into the project's store on the
        // strength of a flag, and a teammate's rules are not the lead's to
        // write. Answered here rather than through `auto_permissions`, whose
        // whole path is a command to *this* engine.
        if self.yolo {
            let _ = forwarded.reply.send(PermissionReply::Once);

            return;
        }
        let summary = format!("{} asks: {title}", forwarded.teammate);
        // The teammate's name is put in front of the title, because the one
        // thing this dialog has that the engine's own does not is a subject:
        // the call is not this conversation's, and answering it as though it
        // were is the mistake worth spending a few columns to prevent.
        let asked = Permission::new(
            id.clone(),
            tool,
            format!("{} · {title}", forwarded.teammate),
            args,
            directories,
        );
        self.forwarded_dialogs.insert(id, forwarded.reply);
        self.raise_permission(&summary, asked);
    }

    /// Answers a dialog a teammate raised, and says whether one was waiting.
    ///
    /// The queue is advanced here rather than on a `PermissionReplied`,
    /// because none is coming: the event that would announce this answer is
    /// published by the **teammate's** engine, which this frontend does not
    /// subscribe to. What it leaves behind is exactly what the engine's own
    /// path leaves — the next queued dialog on screen, and the activity back
    /// to what the lead is really doing.
    fn answer_teammate_dialog(&mut self, id: &PermissionId, reply: PermissionReply) -> bool {
        // Two maps, one door: a spawn's dialog and a teammate's call dialog are
        // answered by the same keys and retired the same way, and which map a
        // reply belongs to is decided by which one is holding the id rather than
        // by anything the key press knows.
        let sender = self
            .forwarded_dialogs
            .remove(id)
            .or_else(|| self.spawn_dialogs.remove(id));
        let Some(sender) = sender else {
            return false;
        };
        if sender.send(reply).is_err() {
            // The teammate stopped waiting — its turn was cancelled, or it
            // shut down while the dialog was up. Nothing to do about it, and
            // nothing lost: the call it was asking about is already refused.
            tracing::debug!("a teammate stopped waiting on a dialog before it was answered");
        }
        self.permission = self.queued_permissions.pop_front();
        if self.permission.is_none() {
            self.status.set_activity(if self.turn_running {
                Activity::Streaming
            } else {
                Activity::Ready
            });
        }
        self.sync_dialog_status();

        true
    }

    /// Whether `message` is one this app has already handed to the engine and
    /// has yet to take out of the lead's mailbox.
    ///
    /// Derived from what is really held rather than tracked beside it, and
    /// that is the point: a set kept in parallel is a set that can disagree
    /// with the thing it describes, where these two collections **are** the
    /// in-flight set — [`App::peer_steers`] is what a `SteerConsumed` will
    /// retire, and [`App::settled`] is what one already did and is waiting for
    /// an `await` to prune. Both are a handful of entries at their largest.
    fn in_flight(&self, message: &Delivered) -> bool {
        let identity = message.identity();

        self.peer_steers
            .values()
            .flatten()
            .chain(&self.settled)
            .any(|held| held.identity() == identity)
    }

    /// Hands one §6.2 pass of peers' messages to this conversation (**D-3**,
    /// **D503**).
    ///
    /// [`App::enqueue`]'s two lanes — steer a running turn, prompt an idle one
    /// — and deliberately **not** [`App::enqueue`] itself, because everything
    /// that function does besides sending is about the composer and would be
    /// wrong here. A peer's message is scanned for no `@` mentions and no `$`
    /// skill tokens, is never matched against the engine's command roster, and
    /// never reaches the prompt history. §7-5 is why: a peer's words are
    /// information the model reads, never an instruction it is bound by and
    /// never consent for anything — and a mention scan is consent to read a
    /// file, a skill token consent to load one, a command name consent to run
    /// one. The person at the terminal typed none of it.
    ///
    /// # One command per pass
    ///
    /// The whole batch rides a single command, because the engine's own
    /// vocabulary takes a list of peers and because sending them one at a time
    /// serialised badly: an idle engine accepts the first as a prompt and
    /// answers `Busy` to the second until the turn it just started is really
    /// running, so the rest of the pass waited out another cadence for no
    /// reason. One command also means one outcome — the batch lands or none of
    /// it does — and nothing has to reason about half a delivered pass.
    ///
    /// # At most [`PEER_BATCH_CAP`] per pass
    ///
    /// A drain hands the model the cap and no more (**D526**); everything past
    /// it is simply not consumed this pass. That costs no machinery because
    /// the durable-queue shape below already carries it: the remainder is
    /// never delivered, so never pruned, never in the in-flight set, and the
    /// next pass offers it again in the same order. The admission gate agrees
    /// — its `disposition` is a read and its reconcile keeps every identity
    /// still present in the file, so an **admitted** message capped out of one
    /// drain is still admitted, and still undelivered, at the next.
    ///
    /// **The mailbox is the durable queue.** Nothing is pruned until the
    /// engine has taken the message, so a refusal, a crash or a turn that
    /// ended without draining its steers all end the same way: the message is
    /// still in the file, and the next pass offers it again — which is exactly
    /// why [`App::in_flight`] exists, since re-offering is by design and
    /// re-delivering is not.
    ///
    /// # It travels as a peer, not as text
    ///
    /// The bodies ride `peers` rather than `text` (**D495**): the engine turns
    /// each payload into a `PartBody::Peer` on the user message, and the request
    /// assembly renders §5.3's `<teammate-message …>` envelope around it. `text`
    /// is left **empty** on purpose — the engine drops a blank text part when
    /// peers are present, and putting the same words in both would tell the
    /// model twice, once attributed and once as though this conversation had
    /// said it.
    async fn deliver_peers(&mut self, mut messages: Vec<Delivered>) -> bool {
        if messages.is_empty() {
            return true;
        }
        // The batch cap (D526): what is cut off here was never delivered, so
        // the mailbox still holds it and the next pass offers it again.
        messages.truncate(PEER_BATCH_CAP);
        if !self.turn_running {
            // An accepted prompt *is* the turn, so there is nothing to render
            // as pending and nothing to wait for.
            if self.start_peer_turn(&messages).await {
                self.settle_peer(&messages).await;

                return true;
            }

            return false;
        }

        let id = self.mint_steer_id();
        let steered = self
            .engine
            .send(Command::Steer {
                id: id.clone(),
                text: String::new(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: messages.iter().map(payload).collect(),
            })
            .await;
        match steered {
            Ok(()) => {}
            // The turn ended between the event that said it was running and
            // this send. Left in the mailbox, and the next pass finds an idle
            // engine and prompts it.
            Err(EngineError::NotStreaming) => return false,
            Err(refusal) => {
                self.status.set_notice(Some(refusal.to_string()));

                return false;
            }
        }
        // One command, two kinds of sender: what the engine now holds is one
        // batch, and how long each message stays claimed is still its own
        // backend's answer.
        let (acknowledged, fire_and_forget): (Vec<Delivered>, Vec<Delivered>) = messages
            .into_iter()
            .partition(|message| message.delivery == Delivery::Acknowledged);
        // The sender's backend gives the lead a consumption fact it can wait
        // for, so the strip renders these entries **pending until consumed**
        // and the mailbox keeps them until then.
        for message in &acknowledged {
            // The strip shows the words, since that is what a person is
            // looking for; what the engine holds is the attributed part. A peer
            // row, so no Up arrow can lift a teammate's words into the composer
            // (§7-5).
            self.queue.push_peer(id.clone(), message.body.clone());
        }
        if !acknowledged.is_empty() {
            self.peer_steers.insert(id, acknowledged);
            self.sync_queue_status();
        }
        // The other backend gives no such fact — a real `claude` pane marks a
        // message read when it *reads* it, not when a turn takes it on — so
        // those entries are **sent** at write time and retired immediately
        // rather than waiting for an acknowledgement that will never come.
        // Without this split a claude peer's message sits pending in the lead's
        // UI forever. One prune for the whole half, because each is a locked
        // read-modify-write of one file.
        self.settle_peer(&fire_and_forget).await;

        true
    }

    /// Starts a turn answering a pass of peers' messages, with none of the
    /// composer's interpretation — see [`App::deliver_peers`].
    async fn start_peer_turn(&mut self, messages: &[Delivered]) -> bool {
        let sent = self
            .engine
            .send(Command::SendPrompt {
                text: String::new(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: messages.iter().map(payload).collect(),
            })
            .await;
        match sent {
            Ok(()) => true,
            Err(EngineError::Busy) => false,
            Err(refusal) => {
                self.status.set_notice(Some(refusal.to_string()));

                false
            }
        }
    }

    /// Takes messages the engine really took out of this session's mailbox —
    /// the lead's on a lead, this member's own on a pane teammate.
    async fn settle_peer(&self, messages: &[Delivered]) {
        if let Some(inbox) = &self.lead_inbox {
            inbox.delivered(messages).await;
        }
        if let Some(inbox) = &self.member {
            inbox.delivered(messages).await;
        }
    }

    /// Drains what a `SteerConsumed` retired, at the first point after the
    /// event where the disk is reachable.
    async fn settle_consumed_peers(&mut self) {
        if self.settled.is_empty() {
            return;
        }
        let settled = std::mem::take(&mut self.settled);
        self.settle_peer(&settled).await;
    }

    /// Gives back every peer message the finished turn never consumed.
    ///
    /// The strip entry goes and the mailbox entry stays, which is the opposite
    /// of what a typed message does: [`Queue::strand`] moves those into the
    /// fallback lane to be replayed as prompts, and that lane replays through
    /// [`App::start_turn_with`] — which scans mentions, resolves skills and
    /// matches the engine's command roster. A peer's words must cross none of
    /// those (§7-5), so they are re-offered from the file by the next §6.2
    /// pass instead.
    ///
    /// Dropping the map is also what makes those messages deliverable again:
    /// it *is* the in-flight set [`App::in_flight`] reads, so a pass that finds
    /// them still in the inbox now finds nothing holding them either.
    fn strand_peers(&mut self) {
        if self.peer_steers.is_empty() {
            return;
        }
        let stranded = std::mem::take(&mut self.peer_steers);
        for id in stranded.keys() {
            // One id per batch, and `consume` retires every row it stood for.
            self.queue.consume(id);
        }
        self.sync_queue_status();
    }

    /// Whether a configured server has yet to report where it stands.
    ///
    /// Nothing is ever retried (**R3**), so every server answers exactly once
    /// and this settles for good a moment after startup.
    fn pending_mcp(&self) -> bool {
        self.mcp_resolved < self.mcp_servers
    }

    /// Reaps a finished model-listing fetch, and opens the list it was for.
    ///
    /// Polled on the same tick the MCP dial rides, and awaited only once the
    /// handle reports finished, so the loop never blocks on the RPC. Every
    /// arm clears the slot: a retry is another `/model` away, never
    /// automatic.
    async fn poll_wire_models(&mut self) {
        if !self
            .wire_fetch
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            return;
        }
        let handle = self.wire_fetch.take().expect("checked finished above");

        // A modal already up keeps the keys it claimed, so the list does not
        // open over it: the rows are cached and the next `/model` opens
        // instantly instead. The set is the one the key router checks, the
        // inline menus included.
        let claimed = self.modal_open()
            || self.dropdown.is_some()
            || self.files.is_some()
            || self.skill_menu.is_some();

        match handle.await {
            Ok(Some(Ok(listed))) if listed.models.is_empty() => {
                self.status
                    .set_notice(Some(format!("the {} wire served no models", self.provider)));
            }
            Ok(Some(Ok(listed))) => {
                self.status.set_notice(None);
                if !claimed {
                    let rows = wire_rows(&listed.models, &self.model);
                    self.chooser = Some((Chooser::Models, ListDialog::new(" models ", rows)));
                }
                self.wire_models = Some(listed.models);
            }
            // The catalog is this provider's source of truth and it had no
            // rows: the empty list a `/model` on such a provider has always
            // opened, one tick later. Nothing is cached — the answer is
            // recomputed in microseconds, and an empty cache entry would
            // shadow the day the catalog does grow rows for it.
            Ok(None) => {
                self.status.set_notice(None);
                if !claimed {
                    self.chooser = Some((Chooser::Models, ListDialog::new(" models ", Vec::new())));
                }
            }
            Ok(Some(Err(error))) => self.status.set_notice(Some(error.to_string())),
            // A panic inside the fetch task; its message is all there is.
            Err(joining) => self.status.set_notice(Some(joining.to_string())),
        }

        self.dirty = true;
    }

    /// Renders one frame.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot be written to.
    pub fn draw<B>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        // Something else — the user's own editor — drew over the screen, so
        // the backend's idea of what is on it is wrong and a diff against it
        // would leave that program's last frame showing through.
        if self.stale {
            terminal.clear().context("failed to repaint the screen")?;
            self.stale = false;
        }

        // The token the cursor sits on lights up reversed — Claude Code's
        // own composer rendering (2026-08-15 screenshot) — before the editor
        // draws this frame.
        self.highlight_image_token();

        // The dim argument hint after a typed command name (**D518**),
        // recomputed each frame from the buffer: a shell line hints nothing,
        // and neither does a name with arguments already behind it.
        let hint = if self.editor.mode() == Mode::Prompt {
            command::inline_hint(&self.editor.text(), &self.engine_commands)
        } else {
            None
        };
        self.editor.set_hint(hint);

        terminal
            .draw(|frame| {
                let area = frame.area();
                // Sized before the split so the strip holds this frame's
                // lines, and capped so a long checklist cannot squeeze the
                // conversation itself off the screen.
                let working_height = self
                    .chat
                    .lay_out_working(area.width, &self.theme)
                    .min(area.height / 2);
                let [transcript, working, prompt, status] = Layout::vertical([
                    Constraint::Min(1),
                    // What the running turn is doing now, pinned above the
                    // composer rather than scrolled with the transcript
                    // (**D487**, amended by the 2026-08-15 screenshots).
                    Constraint::Length(working_height),
                    Constraint::Length(editor::HEIGHT),
                    // A configured roster may earn a git line above the bar
                    // and a detail line below it; the default bar is the one
                    // row it always was (**D469**).
                    Constraint::Length(self.status.height()),
                ])
                .areas(area);

                let buffer = frame.buffer_mut();
                // The theme's surface goes down first and everything is drawn
                // over it, which is how a theme reaches the cells no component
                // writes to. A theme with no background of its own patches
                // nothing, leaving the terminal's — image, transparency and
                // all — showing (upstream `context/theme.tsx:269`).
                buffer.set_style(area, self.theme.background);
                self.chat.render(transcript, buffer, &self.theme);
                self.chat.render_working(working, buffer);
                // What is waiting sits directly above the working strip,
                // under whichever inline menu is open: the strip is a
                // standing account of messages the engine still owes, and a
                // menu is a transient answer to what is being typed right
                // now. Anchored to the working block rather than the
                // composer so a queued message never papers over what the
                // turn is doing — when no turn runs, the anchor sits where
                // the composer starts and nothing has moved.
                self.queue.render(working, buffer, &self.theme);
                // Anchored to the editor and drawn over the transcript, which
                // is what makes it read as part of what is being typed rather
                // than as another dialog.
                if let Some(dropdown) = &self.dropdown {
                    dropdown.render(prompt, buffer, &self.theme);
                }
                if let Some(files) = &self.files {
                    files.render(prompt, buffer, &self.theme);
                }
                if let Some(skills) = &self.skill_menu {
                    skills.render(prompt, buffer, &self.theme);
                }
                // The two dialogs that can block a turn draw last so they are
                // on top. Permission stays above question if an impossible
                // overlapping pair ever arrives, matching which one owns keys.
                if let Some(sessions) = &self.sessions {
                    sessions.render(transcript, buffer, &self.theme);
                }
                if let Some(themes) = &self.theme_list {
                    themes.render(transcript, buffer, &self.theme);
                }
                if let Some(search) = &self.history_search {
                    search.render(transcript, buffer, &self.theme);
                }
                if let Some(rewind) = &self.rewind {
                    rewind.render(transcript, buffer, &self.theme);
                }
                if let Some(mcp_dialog) = &self.mcp_dialog {
                    mcp_dialog.render(transcript, buffer, &self.theme);
                }
                if let Some(held_dialog) = &self.held_dialog {
                    held_dialog.render(transcript, buffer, &self.theme);
                }
                if let Some(plugin_dialog) = &self.plugin_dialog {
                    plugin_dialog.render(transcript, buffer, &self.theme);
                }
                if let Some(team_dialog) = &self.team_dialog {
                    team_dialog.render(transcript, buffer, &self.theme);
                }
                if let Some(context_dialog) = &self.context_dialog {
                    context_dialog.render(transcript, buffer, &self.theme);
                }
                if let Some(usage_dialog) = &self.usage_dialog {
                    usage_dialog.render(transcript, buffer, &self.theme);
                }
                if let Some((_, chooser)) = &self.chooser {
                    chooser.render(transcript, buffer, &self.theme);
                }
                if let Some(palette) = &self.palette {
                    palette.render(transcript, buffer, &self.theme);
                }
                if let Some(help) = &mut self.help {
                    help.render(transcript, buffer, &self.theme);
                }
                // A view, not a mode (**F2**): everything it reads is handed
                // in fresh from `App`'s own state, so a turn streaming
                // beneath it is never behind what this shows. Full-terminal
                // takeover (screenshot-sourced, see the module doc): `area`,
                // not `transcript`, so it covers the composer and status bar
                // too — which is why those two are skipped below rather than
                // drawn over the bottom of it.
                if let Some(inspector) = &mut self.inspector {
                    let session = self.engine.current_session();
                    inspector.render(
                        area,
                        buffer,
                        &self.theme,
                        &Feed {
                            session: session.as_ref(),
                            messages: &self.chat.messages(),
                            events: &self.event_log,
                            usages: &self.turn_usages,
                            totals: self.totals,
                        },
                    );
                }
                if let Some(question) = &self.question {
                    question.render(transcript, buffer, &self.theme);
                }
                match &self.permission {
                    Some(PendingDialog::Permission(permission)) => {
                        permission.render(transcript, buffer, &self.theme);
                    }
                    Some(PendingDialog::Held(held)) => {
                        held.render(transcript, buffer, &self.theme);
                    }
                    None => {}
                }
                let mut cursor = None;
                if self.inspector.is_none() {
                    cursor = self.editor.render(prompt, buffer);
                    self.status.render(status, buffer, &self.theme);
                }
                // The composer's cursor is the terminal's own, placed after
                // the frame is drawn: whatever the terminal shows for a
                // cursor — a block, a bar, the hollow box of an unfocused
                // window, nothing in an inactive tmux pane — is what shows
                // here (2026-08-25). A modal has the keys, so while one is up
                // the frame places no cursor and the terminal hides it.
                if let Some(cursor) = cursor.filter(|_| !self.modal_open()) {
                    frame.set_cursor_position(cursor);
                }
            })
            .context("failed to draw a frame")?;

        // After the frame's own bytes have gone out, never during them: the
        // OSC 52 escape is the terminal's clipboard channel, and a copy queued
        // it here so it lands between frames rather than splitting one. Written
        // straight to stdout — the backend the terminal is on — the way the app
        // already writes its mouse and paste sequences. A write that fails is
        // one lost copy over that channel, not a reason to fail the frame.
        self.flush_osc();
        // Same seam, same reasoning: the image strip's kitty-graphics
        // escapes go out between frames, positioned onto the rows the split
        // above reserved (2026-08-15).
        self.flush_graphics();

        self.dirty = false;
        self.urgent = false;
        self.last_draw = Instant::now();

        Ok(())
    }

    /// Writes any queued OSC 52 escapes to the terminal and empties the queue.
    ///
    /// Serialized after the frame by its one caller ([`App::draw`]); pulled out
    /// so the queue itself can be asserted on without a terminal.
    fn flush_osc(&mut self) {
        use std::io::Write as _;

        if self.pending_osc.is_empty() {
            return;
        }

        let mut stdout = std::io::stdout();
        for sequence in self.pending_osc.drain(..) {
            if let Err(error) = stdout.write_all(sequence.as_bytes()) {
                tracing::warn!(%error, "an OSC 52 clipboard escape could not be written");
            }
        }
        if let Err(error) = stdout.flush() {
            tracing::warn!(%error, "the OSC 52 clipboard escape could not be flushed");
        }
    }

    async fn handle_terminal(&mut self, event: TermEvent) -> Result<()> {
        match event {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                self.handle_key(key).await?
            }
            TermEvent::Mouse(mouse) if !self.modal_open() => match mouse.kind {
                MouseEventKind::ScrollUp => self.chat.scroll_lines(-WHEEL_LINES),
                MouseEventKind::ScrollDown => self.chat.scroll_lines(WHEEL_LINES),
                _ => {}
            },
            // The terminal wrapped a paste in its brackets, so this arrives as
            // content rather than as the keystrokes it would otherwise be
            // mistaken for — which is the whole point of turning bracketed
            // paste on: an Enter inside pasted text is a line, not a submit.
            // A modal owns the keyboard while it is up, and it owns this too.
            // An **empty** bracketed paste is Cmd+V over an image-only
            // clipboard: the terminal has no text to put inside the envelope
            // and sends the brackets bare, and that emptiness is the signal —
            // Claude Code's own Cmd+V mechanism (observed 2026-08-15), since
            // no terminal forwards the system paste chord
            // as a key. Routed through the full clipboard chain, whose text
            // question cannot re-enter here: a clipboard the terminal found
            // no text on answers the file and image questions instead.
            TermEvent::Paste(text) if !self.modal_open() && text.is_empty() => {
                self.paste_from_clipboard().await;
            }
            TermEvent::Paste(text) if !self.modal_open() => self.paste(&text).await,
            // What the notifier's gate reads (**D468**): a terminal being
            // looked at needs no announcement, and these two events are the
            // only way this side ever learns which it is.
            TermEvent::FocusGained => {
                self.focused = true;
                self.hint_clipboard_image();
            }
            TermEvent::FocusLost => self.focused = false,
            _ => {}
        }

        Ok(())
    }

    /// Announces `event` with `summary` — unless somebody is watching.
    ///
    /// **D468** (`tui-notifications`): the Codex CLI's focus-gated terminal
    /// notification, which upstream opencode does not make. The gate lives
    /// here, on the app's own focus knowledge, and each triggering *moment*
    /// calls this exactly once — emission rides the event that is the moment,
    /// never the frames drawn after it. External-program notification is
    /// deliberately not duplicated on these seams: a config that wants a
    /// command run already has the `Notification`/`Stop` hooks (**D456**),
    /// which carry the full JSON envelope a one-line escape never could.
    fn announce(&mut self, event: NotificationEvent, summary: &str) {
        if self.focused {
            return;
        }

        self.notifier.notify(event, summary);
    }

    /// Inserts pasted `text` at the cursor.
    ///
    /// Both line endings a terminal may send become `\n` before anything sees
    /// them: a CRLF paste is what Windows terminals produce, and a lone CR is
    /// what ConPTY sends (upstream normalizes the same pair,
    /// `component/prompt/index.tsx:1395-1420`). Left alone they would reach the
    /// buffer as stray characters rather than as the line breaks they are.
    ///
    /// Before anything reaches the buffer, [`mention::classify_drop`] checks
    /// whether this is a *drop* rather than ordinary text — one or more paths
    /// a terminal handed over as a paste (**F5**). Every qualifying token
    /// becomes its own `@mention `, in order; a paste that fails the
    /// classifier — including one that is not text a file path could ever be
    /// made of — goes in raw, exactly as before.
    ///
    /// Raw text goes in **unfolded**: upstream would collapse a long paste
    /// behind a `[Pasted ~N lines]` placeholder, which stays deferred now
    /// that the image half of the same ruling has landed (**D111**).
    async fn paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");

        match mention::classify_drop(&normalized, &self.cwd) {
            Some(paths) => {
                for path in paths {
                    // A dropped image is the picture, not its path
                    // (2026-08-15): the same [Image #N] token every other
                    // door inserts, so the strip previews it and the
                    // composer never carries forty characters of filesystem.
                    // Everything else keeps the @ spelling a drop always had.
                    let mime = attachment::mime(&path);
                    if mime.starts_with("image/") && attachment::is_binary(mime) {
                        let number = self.next_image_number();
                        self.pasted_images.push((number, path));
                        self.editor.insert(&format!("[Image #{number}] "));
                    } else {
                        self.editor
                            .insert(&format!("{} ", mention::token(&path, None, None)));
                    }
                }
            }
            None => self.editor.insert(&normalized),
        }
        // The cursor moved, and both inline menus are about where it is.
        self.sync_menus().await;
    }

    /// Pastes whatever the clipboard holds, for a terminal that did not send
    /// the paste itself.
    ///
    /// Upstream binds `ctrl+v` to the same fallback; there it is the only
    /// path for the image case too, since an image has no bracketed-paste
    /// channel to arrive through on any terminal (**F3**, lifting D111's
    /// image half). Text is tried first — upstream's own order
    /// (`clipboard.ts:70-79` falls through platform image grabs before
    /// text) reaches the same place here, because [`clipboard::Clipboard`]
    /// already answers the two as independent questions.
    async fn paste_from_clipboard(&mut self) {
        // Files first (2026-08-15): a file copied in Finder rides the
        // pasteboard as its URL *and* its bare name as text, so a paste that
        // asked for text first inserted a basename that resolves nowhere —
        // the pinned screenshot. Only a clipboard that holds no files at all
        // falls through to the text and image questions.
        if let Ok(files) = self.clipboard.read_files()
            && !files.is_empty()
        {
            self.paste_files(files).await;
            return;
        }

        match self.clipboard.read() {
            Ok(text) => self.paste(&text).await,
            Err(clipboard::Error::NotText) => self.paste_clipboard_image().await,
            // A machine with no clipboard costs a notice and never the
            // keystroke: nothing here may eat what was being typed.
            Err(error) => self.status.set_notice(Some(error.to_string())),
        }
    }

    /// A paste of copied files: images become inline `[Image #N]` tokens
    /// backed by the copied file itself — no scratch copy, the file is
    /// already somewhere durable — and everything else joins the composer the
    /// way the same paths *typed or dropped* would, through
    /// [`App::paste`]'s own classifier.
    async fn paste_files(&mut self, files: Vec<std::path::PathBuf>) {
        let mut leftovers: Vec<String> = Vec::new();
        let mut tokenized = false;
        for file in files {
            let path = file.display().to_string();
            let mime = attachment::mime(&path);
            if mime.starts_with("image/") && attachment::is_binary(mime) && file.is_file() {
                let number = self.next_image_number();
                self.pasted_images.push((number, path));
                self.editor.insert(&format!("[Image #{number}] "));
                tokenized = true;
            } else {
                leftovers.push(path);
            }
        }

        if !leftovers.is_empty() {
            self.paste(&leftovers.join("\n")).await;
        } else if tokenized {
            self.sync_menus().await;
        }
    }

    /// The image half of [`App::paste_from_clipboard`]: saves what the
    /// clipboard holds as PNG and attaches it through the same mention
    /// pipeline an `@file` reaches — the path joins `mentions` at send time,
    /// earning the same submit-time wire-degradation warning
    /// (`App::degraded`); no second attachment channel (**F3**). What the
    /// *composer* shows moved on 2026-08-15: Claude Code's own `[Image #N]`
    /// token rather than the scratch path, which is machinery, not prose.
    async fn paste_clipboard_image(&mut self) {
        match self.clipboard.read_image() {
            Ok(image) => match self.save_clipboard_image(&image) {
                Ok(path) => {
                    let number = self.next_image_number();
                    self.pasted_images.push((number, path));
                    self.editor.insert(&format!("[Image #{number}] "));
                    self.sync_menus().await;
                }
                Err(reason) => self.status.set_notice(Some(reason)),
            },
            Err(clipboard::Error::NoImage) => {
                self.status.set_notice(Some(CLIPBOARD_EMPTY.to_owned()));
            }
            Err(error) => self.status.set_notice(Some(error.to_string())),
        }
    }

    /// Encodes `image` to PNG and writes it under [`App::clipboard_scratch`]
    /// — or, absent a test override, [`App::default_clipboard_scratch_dir`]
    /// — as `clipboard-<n>.png`: upstream's own `filename: "clipboard"`
    /// (`index.tsx:384`), numbered because one session may paste more than
    /// one image. Answers the absolute path on success, or the reason
    /// nothing was saved: there is nowhere to resolve a scratch directory, it
    /// could not be created, the encode failed, or the write did.
    ///
    /// The counter advances even on a write failure, never reusing a name a
    /// failed attempt may have partly written.
    fn save_clipboard_image(&mut self, image: &clipboard::Image) -> Result<String, String> {
        let dir = match &self.clipboard_scratch {
            Some(dir) => dir.clone(),
            None => Self::default_clipboard_scratch_dir()
                .ok_or_else(|| "no home directory to save the pasted image under".to_owned())?,
        };
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let bytes = Self::encode_clipboard_png(image)?;

        self.clipboard_pastes += 1;
        let path = dir.join(format!("clipboard-{}.png", self.clipboard_pastes));
        std::fs::write(&path, bytes).map_err(|error| error.to_string())?;

        Ok(path.display().to_string())
    }

    /// The saved clipboard images `text`'s `[Image #N]` tokens still name
    /// (2026-08-15): each one pasted this session and present in the text at
    /// send time attaches its file exactly as an `@` mention would. A token
    /// whose number nothing pasted — typed by hand, or recalled from history
    /// into a fresh session — stays literal text, the same fate a mistyped
    /// `@` path meets.
    /// Answers the images the transcript's last frame wanted cells for:
    /// each path loaded once (thumbnailed, any of the four formats),
    /// transmitted under a fresh id, given its **virtual** placement, and
    /// handed back to the chat so the next frame draws its placeholder
    /// cells. The APCs are position-independent — the cells say where the
    /// picture goes — so nothing here moves a cursor, which is exactly what
    /// makes the scheme safe under tmux. Written to the real stdout the way
    /// `flush_osc` writes; guarded on the emitter, so a test's frame writes
    /// nothing.
    fn flush_graphics(&mut self) {
        use std::io::Write as _;

        let Some(emitter) = self.graphics else {
            return;
        };
        let wanted: Vec<String> = self.chat.images_wanting_cells().to_vec();
        if wanted.is_empty() {
            return;
        }

        let mut wire = String::new();
        for path in wanted {
            // Joined against the root the way the engine resolves the
            // mention itself: a dropped path may be project-relative, and a
            // join with an absolute one is that absolute path.
            let resolved = self.root.join(&path).display().to_string();
            let (id, width, height) = match self.transmitted.get(&resolved) {
                Some(&known) => known,
                None => {
                    let loaded = graphics::load(&resolved).map_or((0, 0, 0), |preview| {
                        self.image_id += 1;
                        wire.push_str(&emitter.transmit(self.image_id, &preview.png));
                        (self.image_id, preview.width, preview.height)
                    });
                    self.transmitted.insert(resolved, loaded);
                    loaded
                }
            };
            let columns = if id == 0 {
                0
            } else {
                let columns = graphics::columns_for(width, height, chat::IMAGE_ROWS).min(60);
                wire.push_str(&emitter.virtual_placement(id, columns, chat::IMAGE_ROWS));
                columns
            };
            self.chat.set_image_cell(&path, id, columns);
            self.dirty = true;
        }

        let mut stdout = std::io::stdout();
        if let Err(error) = stdout.write_all(wire.as_bytes()) {
            tracing::warn!(%error, "the transcript's graphics escapes could not be written");
        }
        let _ = stdout.flush();
    }

    /// Reverses the `[Image #N]` token the cursor sits on — Claude Code's
    /// own composer rendering — and clears the reverse when it sits
    /// elsewhere. The number is unique per paste, so the widget's search
    /// highlight matches exactly the one token.
    fn highlight_image_token(&mut self) {
        let text = self.editor.text();
        let (row, column) = self.editor.cursor();
        let token = image_token_at(&text, char_offset(&text, row, column));
        self.editor
            .set_token_highlight(token.map(|(_, _, number)| number));
    }

    /// Backspace over an `[Image #N]` token takes the whole token — Claude
    /// Code's own composer rule, widened from
    /// cursor-after to anywhere the highlight lights: the token is atomic to
    /// the eye, so it is atomic to the delete. At the token's very front the
    /// answer is no, because backspace there is about the character before
    /// the token.
    fn delete_image_token(&mut self) -> bool {
        let text = self.editor.text();
        let (row, column) = self.editor.cursor();
        let offset = char_offset(&text, row, column);
        let Some((start, end, _)) = image_token_at(&text, offset) else {
            return false;
        };
        if offset == start {
            return false;
        }

        // A token never spans lines, so its `[` sits on the cursor's own
        // row, `offset - start` columns back.
        self.editor
            .delete_span(row, column - (offset - start), end + 1 - start);

        true
    }

    /// Claude Code's own focus-time nudge (observed behaviour):
    /// coming back to the terminal with an image on the clipboard earns one
    /// status-line hint, at most every thirty seconds. The one-second
    /// debounce upstream wraps around its subprocess check is dropped — one
    /// in-process read at a focus boundary is cheap enough to take inline.
    fn hint_clipboard_image(&mut self) {
        const EVERY: Duration = Duration::from_secs(30);
        if self
            .image_hint_last
            .is_some_and(|last| last.elapsed() < EVERY)
        {
            return;
        }
        if self.clipboard.read_image().is_ok() {
            self.image_hint_last = Some(Instant::now());
            self.status
                .set_notice(Some("Image in clipboard \u{b7} ctrl+v to paste".to_owned()));
            self.dirty = true;
        }
    }

    /// The number the next `[Image #N]` token carries: one past however many
    /// images this session has pasted, whichever door they came through —
    /// the scratch-PNG counter names files, this names tokens.
    fn next_image_number(&self) -> u32 {
        u32::try_from(self.pasted_images.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    }

    fn pasted_images_in(&self, text: &str) -> Vec<ganja_protocol::Mention> {
        self.pasted_images
            .iter()
            .filter(|(number, _)| text.contains(&format!("[Image #{number}]")))
            .map(|(_, path)| ganja_protocol::Mention {
                path: path.clone(),
                start: None,
                end: None,
            })
            .collect()
    }

    /// `<XDG data>/ganja/clipboard`, or [`None`] when there is no home to
    /// resolve it against — the same `<XDG data>/ganja` the prompt history
    /// and the theme pick already agree on (`history.rs::default_path`).
    fn default_clipboard_scratch_dir() -> Option<PathBuf> {
        let base = Xdg::new().ok()?;

        Some(base.data_dir().join("ganja").join("clipboard"))
    }

    /// `image`'s RGBA pixels as PNG bytes, encoded in-process — the ganja-
    /// shaped equivalent of upstream's per-OS shell-out to grab the same
    /// bytes (`clipboard.ts:31-72`): the same observable output through a
    /// different mechanism (deviation: **D449**, clipboard-png-in-process).
    fn encode_clipboard_png(image: &clipboard::Image) -> Result<Vec<u8>, String> {
        let width = u32::try_from(image.width)
            .map_err(|_| "the pasted image is too wide to encode".to_owned())?;
        let height = u32::try_from(image.height)
            .map_err(|_| "the pasted image is too tall to encode".to_owned())?;

        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(&image.rgba)
            .map_err(|error| error.to_string())?;
        drop(writer);

        Ok(bytes)
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.exits(key) {
            self.quit = true;
            return Ok(());
        }

        // Esc Esc is a *sequence*, so anything at all in between ends it —
        // including the key that opens a modal, which is why this sits above
        // every dialog's own handler rather than beside the Esc arm below.
        if key.code != KeyCode::Esc {
            self.last_esc = None;
        }

        // The backtrack walk (**D467**) claims exactly two keys while it is
        // up: Esc steps the highlight one user message older, Enter reverts
        // to the one it is on. Anything else is a person doing something else
        // — the walk exits silently and the key then lands wherever it would
        // have without it, which is why this only sometimes returns. Above
        // the dialogs on the same reasoning as the sequence break: no dialog
        // can be open while the walk is (both need the composer idle), and a
        // key that would open one must exit the walk first.
        if self.backtrack.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.step_backtrack();
                    return Ok(());
                }
                KeyCode::Enter if !key.modifiers.intersects(NEWLINE_MODIFIERS) => {
                    self.confirm_backtrack().await;
                    return Ok(());
                }
                _ => self.close_backtrack(),
            }
        }

        match &self.permission {
            Some(PendingDialog::Permission(permission)) => {
                // Every other key is swallowed while the modal is open: the
                // editor and the transcript beneath it are not what the user
                // is acting on right now.
                if let Some(reply) = permission_reply(key.code) {
                    let id = permission.id().clone();
                    // A teammate's dialog is answered on the channel it
                    // arrived on, because the turn waiting on it is another
                    // engine's (**D-5**). One `if`, and the same keys either
                    // way: which conversation raised a question is not
                    // something a person answering it should have to know.
                    if !self.answer_teammate_dialog(&id, reply) {
                        self.engine
                            .send(Command::ReplyPermission { id, reply })
                            .await?;
                    }
                }

                return Ok(());
            }
            Some(PendingDialog::Held(held)) => {
                // The same swallow-everything posture; what differs is the
                // road the answer takes. A settle rides `SettleHeld`, never
                // the permission wait registry, and the dialog stays up
                // until its own `PeerHoldSettled` — a settle that raced the
                // deadline is ignored by the engine, and the event closes
                // this either way (**D524**).
                if let Some(decision) = held_decision(key.code) {
                    let id = held.id().clone();
                    self.engine
                        .send(Command::SettleHeld { id, decision })
                        .await?;
                }

                return Ok(());
            }
            None => {}
        }

        if let Some(question) = &mut self.question {
            // Like permission, every key belongs to the open request until its
            // terminal event arrives; the editor beneath is not the target.
            if key.code == KeyCode::Enter {
                let id = question.id().clone();
                let answer = question.submit();
                if let Some(answer) = answer {
                    self.engine
                        .send(Command::ReplyQuestion {
                            id,
                            answers: vec![vec![answer]],
                        })
                        .await?;
                }

                return Ok(());
            }

            if question.is_editing() {
                match key.code {
                    KeyCode::Esc => question.cancel_edit(),
                    KeyCode::Backspace => question.backspace(),
                    // The answer editor owns printable j and k too; navigation
                    // resumes only after the edit has closed.
                    KeyCode::Char(character) if !key.modifiers.intersects(SHORTCUT_MODIFIERS) => {
                        question.push(character);
                    }
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => question.move_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') => question.move_selection(1),
                    KeyCode::Esc => {
                        let id = question.id().clone();
                        self.engine.send(Command::RejectQuestion { id }).await?;
                    }
                    _ => {}
                }
            }

            return Ok(());
        }

        if let Some(help) = &mut self.help {
            // Nothing to choose, so both of the keys that mean "done" close
            // it; the movement keys reach the rows the window cannot show at
            // once, and everything else is swallowed like any other modal.
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.help = None,
                KeyCode::Up | KeyCode::Char('k') => help.scroll(-1),
                KeyCode::Down | KeyCode::Char('j') => help.scroll(1),
                KeyCode::PageUp => help.scroll(-HELP_PAGE),
                KeyCode::PageDown => help.scroll(HELP_PAGE),
                KeyCode::Home => help.scroll_to_top(),
                // Further than the card can ever be; the render clamps it to
                // the last row rather than this having to know how many rows
                // there are at this width.
                KeyCode::End => help.scroll(isize::MAX),
                _ => {}
            }

            return Ok(());
        }

        if let Some(inspector) = &mut self.inspector {
            // The toggle's other half: Ctrl+T opened it, and closes it again
            // from anywhere inside — the same "from anywhere idle-or-
            // streaming" reach the chord opens it from (**F2**). `q` closes
            // it too, Codex's own binding for its transcript overlay
            // (screenshot-sourced, see `component/inspector.rs`'s module
            // doc) — nothing else in this block claims the letter.
            if key.code == KeyCode::Esc
                || key.code == KeyCode::Char('q')
                || self.keys.binds(keybind::Action::TranscriptOpen, key)
            {
                self.inspector = None;
                return Ok(());
            }
            // vim's half-page pair beside its `j`/`k` (2026-08-25): the
            // overlay's own measured rows, not `HELP_PAGE`'s fixed step.
            // Ctrl+D is also an exit chord, which `exits` yields to this
            // overlay while it is open.
            if let Some(direction) = half_page(key) {
                inspector.scroll_half_page(direction);
                return Ok(());
            }
            match key.code {
                KeyCode::Left => inspector.previous_tab(),
                KeyCode::Right => inspector.next_tab(),
                KeyCode::Char('1') => inspector.select_index(0),
                KeyCode::Char('2') => inspector.select_index(1),
                KeyCode::Char('3') => inspector.select_index(2),
                KeyCode::Up | KeyCode::Char('k') => inspector.scroll(-1),
                KeyCode::Down | KeyCode::Char('j') => inspector.scroll(1),
                KeyCode::PageUp => inspector.scroll(-HELP_PAGE),
                KeyCode::PageDown => inspector.scroll(HELP_PAGE),
                KeyCode::Home => inspector.scroll_to_top(),
                KeyCode::End => inspector.scroll(isize::MAX),
                _ => {}
            }

            return Ok(());
        }

        if self.sessions.is_some() {
            self.handle_picker_key(key.code).await;

            return Ok(());
        }

        if self.theme_list.is_some() {
            self.handle_theme_key(key.code);

            return Ok(());
        }

        if self.history_search.is_some() {
            self.handle_history_search_key(key).await;

            return Ok(());
        }

        if self.rewind.is_some() {
            self.handle_rewind_key(key.code).await;

            return Ok(());
        }

        if self.mcp_dialog.is_some() {
            self.handle_mcp_key(key.code).await;

            return Ok(());
        }

        if self.held_dialog.is_some() {
            self.handle_held_key(key.code).await;

            return Ok(());
        }

        // The whole event, not just the code: the free-text step takes
        // printable characters, the way the question dialog's editor does.
        if self.plugin_dialog.is_some() {
            self.handle_plugin_key(key);

            return Ok(());
        }

        // The same, for the same reason: `/team spawn` and a message to a
        // member are both typed into a step of the dialog (**D504**).
        if self.team_dialog.is_some() {
            self.handle_team_key(key).await;

            return Ok(());
        }

        // The two read-only panels: modal like every dialog, but with nothing
        // to steer — Esc closes, everything else is claimed and ignored.
        if self.context_dialog.is_some() {
            if key.code == KeyCode::Esc {
                self.context_dialog = None;
            }

            return Ok(());
        }

        if self.usage_dialog.is_some() {
            if key.code == KeyCode::Esc {
                self.usage_dialog = None;
            }

            return Ok(());
        }

        if self.chooser.is_some() {
            self.handle_chooser_key(key.code).await;

            return Ok(());
        }

        if self.palette.is_some() {
            self.handle_palette_key(key).await;

            return Ok(());
        }

        // Not a modal: the menus sit over the transcript while the editor keeps
        // the cursor, so they claim only the keys that steer them and let every
        // other one through to carry on typing.
        if self.dropdown.is_some() && self.handle_dropdown_key(key).await {
            return Ok(());
        }
        if self.files.is_some() && self.handle_files_key(key).await {
            return Ok(());
        }
        if self.skill_menu.is_some() && self.handle_skill_menu_key(key) {
            return Ok(());
        }

        // Upstream's gate exactly: cursor at the very start, in the ordinary
        // mode, with no menu open. The `!` itself is never inserted — what
        // runs is the raw buffer, and a prefix nobody typed would end up in it
        // (`component/prompt/index.tsx:815-840`).
        if key.code == KeyCode::Char('!')
            && !key.modifiers.intersects(SHORTCUT_MODIFIERS)
            && self.editor.mode() == Mode::Prompt
            && self.editor.cursor() == (0, 0)
        {
            self.set_shell(true);
            return Ok(());
        }

        match self.keys.action(key) {
            Some(keybind::Action::PaletteOpen) => {
                self.open_palette();
                return Ok(());
            }
            Some(keybind::Action::SessionsOpen) => {
                self.open_picker().await;
                return Ok(());
            }
            Some(keybind::Action::ThemesOpen) => {
                self.open_themes();
                return Ok(());
            }
            Some(keybind::Action::HistorySearch) => {
                self.open_history_search();
                return Ok(());
            }
            Some(keybind::Action::TranscriptOpen) => {
                self.open_inspector();
                return Ok(());
            }
            // Something else may have drawn over the screen without this
            // process knowing — the same situation the external-editor
            // return path already repairs (`compose_externally`) — so the
            // next frame forces a full `terminal.clear()` rather than a diff.
            Some(keybind::Action::Redraw) => {
                self.stale = true;
                return Ok(());
            }
            // Tab means "next agent" on an empty buffer only; with something
            // typed it is the editor's own key, as it is in every editor.
            Some(keybind::Action::AgentCycle) if self.editor.is_empty() => {
                self.cycle_agent().await;
                return Ok(());
            }
            // Resolved here, before the bare Enter below can read as a submit:
            // every chord on this row breaks the line instead. `ctrl+j` is the
            // one every terminal delivers; the `*+enter` chords need the kitty
            // protocol (see the keybind row). The cursor moved, so the inline
            // menus, which are about where it is, are re-synced.
            Some(keybind::Action::InputNewline) => {
                self.editor.insert_newline();
                self.sync_menus().await;
                return Ok(());
            }
            // Including an exit binding whose gate said no, which falls
            // through to the editor below and deletes forward there.
            _ => {}
        }

        // After the bindings, so a user who binds ctrl+v to something else
        // gets what they asked for. Not a binding of its own (deviation:
        // ctrl-v-not-a-bound-action): bracketed paste is the path that
        // actually runs, and this is the fallback for terminals that do not
        // speak it — an editing key rather than a command.
        if key.code == KeyCode::Char('v') && key.modifiers == KeyModifiers::CONTROL {
            self.paste_from_clipboard().await;
            return Ok(());
        }

        match key.code {
            // Shell mode's way out, which outranks the cancel: there is no
            // turn to stop while the user is typing a command at their own
            // prompt.
            KeyCode::Esc if self.editor.mode() == Mode::Shell => self.set_shell(false),
            // Esc alone cancels — a no-op while idle, which is exactly what it
            // should do there — and **Esc Esc at an idle composer** enters the
            // backtrack walk (**D467**, `esc-esc-backtrack-codex`; the OpenAI
            // Codex CLI's gesture, with no upstream counterpart): the newest
            // user message lights up in the transcript, each further Esc
            // steps one older, and Enter takes the conversation back to
            // before it with the prompt handed back for editing. **D452**
            // amended: the gesture used to open the rewind picker — that
            // Claude-style two-step (checkpoint, then scope) stays reachable
            // as `/rewind`, so the split is Esc Esc = Codex backtrack,
            // `/rewind` = Claude picker.
            //
            // Hardcoded here rather than bound: [`keybind`]'s table maps one
            // chord to one action and cannot express a sequence, and teaching
            // it to would be a rewrite in service of a single gesture. The
            // guard is deliberately "idle at *both* presses": while a turn
            // streams Esc stays the cancel and forgets any first press, so a
            // double-press racing a turn's end cancels and then does nothing,
            // rather than starting a walk over a conversation the user was
            // still watching. A modal's Esc never reaches here at all — every
            // dialog returns above.
            KeyCode::Esc if self.turn_running => {
                self.last_esc = None;
                self.engine.send(Command::CancelTurn).await?;
            }
            KeyCode::Esc
                if self
                    .last_esc
                    .is_some_and(|pressed| pressed.elapsed() <= ESC_CHORD) =>
            {
                self.last_esc = None;
                self.open_backtrack();
            }
            KeyCode::Esc => {
                self.last_esc = Some(Instant::now());
                self.engine.send(Command::CancelTurn).await?;
            }
            // Backspacing off the front of a shell command is the other way
            // out, and it deletes nothing on the way (`:850-859`).
            KeyCode::Backspace
                if self.editor.mode() == Mode::Shell && self.editor.cursor() == (0, 0) =>
            {
                self.set_shell(false);
            }
            KeyCode::Enter if key.modifiers.intersects(NEWLINE_MODIFIERS) => {
                self.editor.insert_newline();
                self.sync_menus().await;
            }
            KeyCode::Enter => self.submit().await,
            KeyCode::PageUp => self.chat.scroll_pages(-1),
            KeyCode::PageDown => self.chat.scroll_pages(1),
            // The two ends of the line while there is a line, and the two ends
            // of the conversation while there is not. Upstream layers these on
            // whether the composer has focus; ganja's composer always has it,
            // so what is left of the distinction is whether it holds anything.
            KeyCode::Home if self.editor.is_empty() => self.chat.scroll_to_top(),
            // Both of these move the cursor, and both menus are about where
            // the cursor is rather than about what was typed — jumping out of
            // a mention has to close the list offering to complete it.
            KeyCode::Home => {
                self.editor.line_home();
                self.sync_menus().await;
            }
            KeyCode::End if self.editor.is_empty() => self.chat.follow_tail(),
            KeyCode::End => {
                self.editor.line_end();
                self.sync_menus().await;
            }
            // History only at the edges, the way upstream reaches it from
            // `input_move_up`/`down`: on the first line an Up walks back through
            // remembered prompts, and on a lower line it is an ordinary cursor
            // move. When the walk moves nothing — a dirty buffer, or the top of
            // the history — the arrow falls through to the widget unchanged, so
            // Up on a one-line draft the user has edited still just moves within
            // it.
            // The queue sits in front of the history: with something waiting
            // and nothing typed, Up takes the newest queued message back for
            // editing rather than walking past it into last week's prompts
            // (**F4**). Once the strip is empty the walk below is reached
            // unchanged.
            KeyCode::Up if self.editor.is_empty() && self.withdraw_queued() => {
                self.sync_menus().await;
            }
            KeyCode::Up
                if self.editor.on_first_line() && self.recall(history::Direction::Older) =>
            {
                self.sync_menus().await;
            }
            KeyCode::Down
                if self.editor.on_last_line() && self.recall(history::Direction::Newer) =>
            {
                self.sync_menus().await;
            }
            // Ahead of the widget's own single-character delete: a token the
            // cursor lights deletes whole (2026-08-15).
            KeyCode::Backspace
                if key.modifiers == KeyModifiers::NONE && self.delete_image_token() =>
            {
                self.sync_menus().await;
            }
            _ => {
                self.editor.input(key);
                self.sync_menus().await;
            }
        }

        Ok(())
    }

    /// Walks the prompt history one step and, if it moved, shows the recalled
    /// prompt in the composer.
    ///
    /// Returns whether the walk moved: the guard is [`History::step`]'s (a walk
    /// is refused while the buffer holds something the user typed rather than a
    /// recalled entry), so a `false` here means the arrow should behave as an
    /// ordinary cursor key instead. The recalled text replaces the buffer with
    /// the cursor left at its end, so a second Up keeps climbing; the caller
    /// re-syncs the inline menus because the recalled line may itself hold an
    /// `@mention` or a `/command`.
    fn recall(&mut self, direction: history::Direction) -> bool {
        match self.history.step(direction, &self.editor.text()) {
            Some(entry) => {
                self.editor.set_text(&entry.input);
                true
            }
            None => false,
        }
    }

    /// Switches the composer between sending prompts and running commands.
    fn set_shell(&mut self, shell: bool) {
        self.editor
            .set_mode(if shell { Mode::Shell } else { Mode::Prompt });
        self.status.set_shell(shell);
        // A shell command is neither a slash command nor a mention, so
        // whatever was being offered is not being offered any more.
        self.dropdown = None;
        self.files = None;
        self.skill_menu = None;
    }

    /// Whether `key` quits.
    ///
    /// A bound key the editor also uses only quits on an empty buffer, so
    /// Ctrl-D deletes forward while there is something to delete and leaves
    /// once there is not; and inside the inspector the same key is vim's
    /// half page ([`half_page`]), so it scrolls while the overlay is open and
    /// leaves once it is closed. Ctrl-C and Ctrl-Q quit from anywhere.
    fn exits(&self, key: KeyEvent) -> bool {
        self.keys.binds(keybind::Action::AppExit, key)
            && (!edits(key) || self.editor.is_empty())
            && !(self.inspector.is_some() && half_page(key).is_some())
    }

    /// Whether a modal is claiming the keys and the wheel.
    fn modal_open(&self) -> bool {
        self.permission.is_some()
            || self.question.is_some()
            || self.sessions.is_some()
            || self.theme_list.is_some()
            || self.history_search.is_some()
            || self.rewind.is_some()
            || self.mcp_dialog.is_some()
            || self.held_dialog.is_some()
            || self.plugin_dialog.is_some()
            || self.team_dialog.is_some()
            || self.context_dialog.is_some()
            || self.usage_dialog.is_some()
            || self.chooser.is_some()
            || self.palette.is_some()
            || self.help.is_some()
            || self.inspector.is_some()
    }

    /// Runs the command a palette row or a menu row named.
    async fn run_command(&mut self, action: command::Action) {
        match action {
            command::Action::Sessions => self.open_picker().await,
            command::Action::New => self.start_session().await,
            command::Action::Compact => self.compact().await,
            command::Action::Editor => self.compose_externally(),
            command::Action::Models => self.open_models(),
            command::Action::Effort => self.open_effort(),
            command::Action::Agents => self.open_agents(),
            command::Action::Themes => self.open_themes(),
            command::Action::Mcp => self.open_mcp(),
            command::Action::Skills => self.open_skills(),
            command::Action::Context => self.open_context().await,
            command::Action::Usage => self.open_usage(),
            command::Action::Plugin => self.open_plugin(),
            command::Action::Team => self.open_team(),
            command::Action::Held => self.open_held(),
            command::Action::Help => self.help = Some(Help::new(self.keys.clone())),
            command::Action::Exit => self.quit = true,
            command::Action::Copy => self.copy_transcript(),
            command::Action::CopyMessage => self.copy_last_reply(),
            command::Action::Undo => self.undo().await,
            command::Action::Redo => self.redo().await,
            command::Action::Rewind => self.open_rewind(),
            // Bare `/rename` names nothing to rename to — reached only
            // through a dropdown Tab-complete that stops at the name, since
            // `command::rename` intercepts an argument-carrying line before
            // this dispatch is ever reached (D527, the `/team` precedent) —
            // so it answers with the missing-name notice, spelled once.
            command::Action::Rename => self.run_rename_line(command::Rename::Missing).await,
        }
    }

    /// Takes back the last prompt and the file changes its turn made.
    ///
    /// Nothing is hidden here: the engine answers with `RevertChanged`, and
    /// that event is the only thing that moves the transcript — the same rule
    /// every other entry follows. What a refusal costs is a line of the status
    /// bar: the engine's own words for a turn still streaming (**D119**), a
    /// session that takes no snapshots, or a conversation with nothing left to
    /// take back.
    async fn undo(&mut self) {
        // An undo never produces a cleared revert, so this only matters for
        // whatever arrives after it — and after an undo, the next clear is a
        // redo's unless a prompt intervenes and says otherwise.
        self.cleared = Cleared::Unhide;
        if let Err(refusal) = self.engine.send(Command::Undo).await {
            self.status.set_notice(Some(refusal.to_string()));
        }
    }

    /// Steps forward through what an undo took back.
    async fn redo(&mut self) {
        self.cleared = Cleared::Unhide;
        if let Err(refusal) = self.engine.send(Command::Redo).await {
            self.status.set_notice(Some(refusal.to_string()));
        }
    }

    /// Puts the whole conversation on the clipboard, as markdown.
    ///
    /// The transcript is built from what is on screen rather than from the
    /// store: they hold the same messages — every entry arrived as an engine
    /// event — and the one the user is looking at is the one they mean.
    fn copy_transcript(&mut self) {
        let Some(session) = self.engine.current_session() else {
            self.status.set_notice(Some(NOTHING_TO_COPY.to_owned()));
            return;
        };

        let text = transcript::format(&session, &self.chat.messages());
        self.copy(&text, TRANSCRIPT_COPIED, TRANSCRIPT_COPY_FAILED);
    }

    /// Puts the model's last reply on the clipboard.
    fn copy_last_reply(&mut self) {
        match transcript::last_reply(&self.chat.messages()) {
            Ok(text) => self.copy(&text, MESSAGE_COPIED, MESSAGE_COPY_FAILED),
            // Upstream's three refusals, spelled its way; see
            // [`transcript::Missing`].
            Err(missing) => self.status.set_notice(Some(missing.to_string())),
        }
    }

    /// Hands `text` to the clipboard and says which way it went.
    ///
    /// Both channels upstream writes go out together, and independently: the
    /// terminal's OSC 52 escape is queued first and unconditionally (upstream
    /// writes it before it tries a system method, and a headless or SSH
    /// session — where arboard has no display to reach — is exactly where the
    /// escape is the one channel that still delivers), then the system
    /// clipboard through the trait. [`App::draw`] flushes the queue with the
    /// next frame. The notice reports only the system half, which is the only
    /// half that can say it failed.
    fn copy(&mut self, text: &str, done: &str, failed: &str) {
        self.pending_osc.push(clipboard::osc52::sequence(text));
        let notice = match self.clipboard.write(text) {
            Ok(()) => done.to_owned(),
            Err(error) => format!("{failed}: {error}"),
        };

        self.status.set_notice(Some(notice));
    }

    /// Leaves this conversation for a fresh one.
    ///
    /// The screen is emptied only once the engine has actually let go of the
    /// session: a refusal — mid-turn — has to leave the user looking at the
    /// conversation they are still in. Nothing stored is touched either way,
    /// so the old session is still in the picker.
    async fn start_session(&mut self) {
        match self.engine.send(Command::NewSession).await {
            Ok(()) => {
                self.chat.clear();
                self.chooser = None;
                self.status.set_activity(Activity::Ready);
                self.status.set_notice(None);
                // The list was the old conversation's own state; the fresh
                // session starts without one.
                self.status.set_todos(None);
            }
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// Asks the engine to summarize what has been said and carry on from it.
    async fn compact(&mut self) {
        match self.engine.send(Command::Compact).await {
            Ok(()) => self.status.set_notice(None),
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// Hands the buffer to the user's own editor and takes back whatever they
    /// left in it.
    ///
    /// The terminal changes hands here, so the next frame cannot be a diff
    /// against what this process last drew — the editor drew over it.
    fn compose_externally(&mut self) {
        let composed = external::edit(&self.editor.text(), self.kitty_keys);
        self.stale = true;

        match composed {
            Ok(text) => {
                self.editor.set_text(&text);
                self.status.set_notice(None);
            }
            Err(refusal) => self.status.set_notice(Some(format!("{refusal:#}"))),
        }
    }

    /// Opens the palette on whatever filter it was last closed with.
    fn open_palette(&mut self) {
        self.palette = Some(Palette::reopened(
            self.keys.clone(),
            self.palette_filter.clone(),
        ));
    }

    /// Closes the palette, remembering what had been typed into it.
    fn close_palette(&mut self) {
        if let Some(palette) = self.palette.take() {
            self.palette_filter = palette.filter().to_owned();
        }
    }

    /// One keypress while the palette is open, which owns every key: its
    /// filter line is what the keyboard is pointed at.
    async fn handle_palette_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_palette(),
            KeyCode::Up => self.move_palette(-1),
            KeyCode::Down => self.move_palette(1),
            KeyCode::Backspace => {
                if let Some(palette) = &mut self.palette {
                    palette.backspace();
                }
            }
            KeyCode::Enter => {
                let action = self.palette.as_ref().and_then(Palette::selected);
                self.close_palette();
                if let Some(action) = action {
                    self.run_command(action).await;
                }
            }
            // Everything printable is filter text — j and k included, which is
            // why this dialog does not take them as movement the way the ones
            // without a filter line do.
            KeyCode::Char(character) if !key.modifiers.intersects(SHORTCUT_MODIFIERS) => {
                if let Some(palette) = &mut self.palette {
                    palette.push(character);
                }
            }
            _ => {}
        }
    }

    /// Moves the palette's cursor by `delta` commands.
    fn move_palette(&mut self, delta: isize) {
        if let Some(palette) = &mut self.palette {
            palette.move_selection(delta);
        }
    }

    /// One keypress while the model or agent list is open.
    async fn handle_chooser_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.chooser = None,
            KeyCode::Up | KeyCode::Char('k') => self.move_chooser(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_chooser(1),
            KeyCode::Enter => self.apply_chooser().await,
            _ => {}
        }
    }

    /// Moves the open list's cursor by `delta` rows.
    fn move_chooser(&mut self, delta: isize) {
        if let Some((_, chooser)) = &mut self.chooser {
            chooser.move_selection(delta);
        }
    }

    /// Sends what the open list has under its cursor.
    async fn apply_chooser(&mut self) {
        let Some((kind, value)) = self
            .chooser
            .as_ref()
            .and_then(|(kind, chooser)| chooser.selected().map(|value| (*kind, value.to_owned())))
        else {
            // An empty list has nothing under the cursor; Enter means nothing.
            return;
        };

        match kind {
            Chooser::Models => self.switch_model(value).await,
            Chooser::Effort => self.switch_effort(value).await,
            Chooser::Agents => self.switch_agent(value).await,
            Chooser::Skills => {
                // The switches above close the dialog on their own paths;
                // an insertion has no such path, so it closes here.
                self.chooser = None;
                self.editor.insert(&format!("${value} "));
                self.dirty = true;
            }
        }
    }

    /// One keypress while the command menu is up, and whether it was one of
    /// the menu's own.
    async fn handle_dropdown_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            // Closes the menu and **keeps what was typed**. Upstream deletes
            // the whole `/xyz` here, which is the one sharp edge in its
            // autocomplete that nobody would ask for (**D11**).
            KeyCode::Esc => {
                self.dropdown = None;
                self.completion = None;

                true
            }
            KeyCode::Up => {
                if let Some(dropdown) = &mut self.dropdown {
                    dropdown.move_selection(-1);
                }

                true
            }
            KeyCode::Down => {
                if let Some(dropdown) = &mut self.dropdown {
                    dropdown.move_selection(1);
                }

                true
            }
            KeyCode::Enter if !key.modifiers.intersects(NEWLINE_MODIFIERS) => {
                let choice = self.dropdown.as_ref().and_then(Dropdown::selected);
                self.dropdown = None;

                match choice {
                    Some(command::Choice::Ui(entry)) => {
                        // The command runs, so the text that named it has done
                        // its job; leaving it would mean the next Enter sent
                        // the command's own name to the model.
                        self.editor.clear();
                        self.run_command(entry.action).await;
                    }
                    // An engine command takes arguments, so choosing it types
                    // its name and waits — upstream rewrites the buffer here
                    // for the same reason (`autocomplete.tsx:456-462`).
                    Some(command::Choice::Engine(command)) => {
                        self.editor.set_text(&format!("/{} ", command.name));
                    }
                    // A slot value replaces the word under the cursor and
                    // waits, like an engine command's name: the line is
                    // still being composed (**D519**).
                    Some(command::Choice::Value(completion)) => {
                        self.complete_value(&completion.text);
                    }
                    None => {}
                }

                true
            }
            // Completes without running, for *both* populations — Claude Code
            // screenshots: `/ex` + Tab fills `/exit ` and sends nothing.
            // Upstream's own Tab binding (`prompt.autocomplete.complete`) runs
            // a UI command exactly as Enter does (`keymap.tsx:286`
            // `onSelect: () => keymap.dispatchCommand(...)`); ganja's Tab
            // instead always just fills the buffer, which is a genuine
            // divergence for the UI half of the roster (deviation **D446**,
            // tab-dropdown-completes-without-running). The engine half already
            // types-and-waits on Enter, so Tab changes nothing there.
            KeyCode::Tab => {
                let choice = self.dropdown.as_ref().and_then(Dropdown::selected);
                self.dropdown = None;

                match choice {
                    Some(command::Choice::Value(completion)) => {
                        self.complete_value(&completion.text);
                    }
                    Some(choice) => self.editor.set_text(&format!("{} ", choice.slash())),
                    None => {}
                }

                true
            }
            _ => false,
        }
    }

    /// Puts `value` where the partial word of the current `/team` slot was,
    /// followed by the space that ends the slot (**D519**): only the word
    /// under the cursor goes, so a line completed mid-sentence keeps its tail.
    fn complete_value(&mut self, value: &str) {
        let Some(slot) = self.completion.take() else {
            return;
        };
        self.editor
            .delete_span(0, slot.start, slot.partial.chars().count());
        self.editor.insert(&format!("{value} "));
    }

    /// One keypress while the file menu is up, and whether it was one of the
    /// menu's own.
    async fn handle_files_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            // Keeps the text, exactly as the command menu does (**D11**).
            KeyCode::Esc => {
                self.files = None;

                true
            }
            KeyCode::Up => {
                if let Some(files) = &mut self.files {
                    files.move_selection(-1);
                }

                true
            }
            KeyCode::Down => {
                if let Some(files) = &mut self.files {
                    files.move_selection(1);
                }

                true
            }
            // Tab completes exactly as Enter does — upstream's own binding
            // for this menu (`prompt.autocomplete.complete`) falls through to
            // the same `select()` Enter uses whenever the row is not a
            // directory (`autocomplete.tsx:624-631`), and ganja's walker
            // never yields one (`glob.rs` filters to `is_file()`), so the two
            // keys are simply two names for the one outcome here.
            KeyCode::Enter | KeyCode::Tab if !key.modifiers.intersects(NEWLINE_MODIFIERS) => {
                let chosen = self.files.as_ref().and_then(Files::selected).cloned();
                match chosen {
                    Some(MenuRow::File(path)) => self.insert_mention(&path),
                    // A roster mention is never ambiguous — the roster
                    // wins any collision at resolution (**D528**) — so a
                    // teammate's completion is always the bare name.
                    Some(MenuRow::Teammate { name, .. }) => self.insert_at_mention(&name),
                    Some(MenuRow::Session {
                        name,
                        address,
                        colliding,
                        ..
                    }) => {
                        if colliding {
                            self.insert_at_mention(&address);
                        } else {
                            self.insert_at_mention(&name);
                        }
                    }
                    None => {
                        // Nothing matched, so there is nothing to insert; the
                        // menu still goes away rather than swallowing every
                        // Enter.
                        self.files = None;
                    }
                }

                true
            }
            _ => false,
        }
    }

    /// Replaces the `@fragment` the file menu was opened for with `path`,
    /// keeping any `#line-range` the fragment carried.
    ///
    /// The literal `@path` **stays in the prompt**, with a space after it so
    /// the mention is closed: it is what the user wrote, and the engine
    /// resolves the file's content separately when it builds the request.
    fn insert_mention(&mut self, path: &str) {
        let Some(files) = self.files.take() else {
            return;
        };
        let fragment = files.fragment();
        // The range the user typed onto the fragment survives the completion,
        // normalized (`#5-` → `#5`, `#20-10` → `#20`): upstream re-appends it
        // to the chosen path the same way (`autocomplete.tsx:250,302-307`).
        let (_, start, end) = mention::split_range(&fragment.text);

        self.splice_token(fragment, mention::token(path, start, end));
    }

    /// Splices a roster or live-session mention through the same accept tail
    /// [`App::insert_mention`] uses (**D529**): the bare name for a teammate
    /// or a unique live session, and — for a colliding session row — its
    /// `uds:` spelling, `@`-prefixed and snapshot-pinned byte-for-byte
    /// (**ADJ-3**), so the person's exact choice cannot be reassigned by a
    /// later resolution.
    fn insert_at_mention(&mut self, token: &str) {
        let Some(files) = self.files.take() else {
            return;
        };

        self.splice_token(files.fragment(), format!("@{token}"));
    }

    /// Replaces the composer fragment a menu was opened for with `token` —
    /// the accept tail the file menu and the skill menu share, cursor left
    /// after what was inserted.
    ///
    /// A space after the token closes it, so the menu does not reopen on what
    /// was just chosen — but only when there is not one already, or completing
    /// mid-sentence would widen the gap every time (upstream
    /// `autocomplete.tsx:172-240` makes the same exception).
    fn splice_token(&mut self, fragment: &mention::Fragment, token: String) {
        let mut lines: Vec<String> = self.editor.text().split('\n').map(str::to_owned).collect();
        let Some(line) = lines.get_mut(fragment.row) else {
            return;
        };

        let characters: Vec<char> = line.chars().collect();
        let head: String = characters[..fragment.start.min(characters.len())]
            .iter()
            .collect();
        let rest = characters
            .get(fragment.start + fragment.width()..)
            .unwrap_or_default();
        let tail: String = rest.iter().collect();
        let inserted = match rest.first() {
            Some(next) if next.is_whitespace() => token,
            _ => format!("{token} "),
        };
        let column = head.chars().count() + inserted.chars().count();
        *line = format!("{head}{inserted}{tail}");

        let row = fragment.row;
        self.editor.set_text_at(&lines.join("\n"), row, column);
    }

    /// One keypress while the skill menu is up, and whether it was one of the
    /// menu's own — the file menu's contract, key for key.
    fn handle_skill_menu_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            // Keeps the text, exactly as the other two menus do (**D11**).
            KeyCode::Esc => {
                self.skill_menu = None;

                true
            }
            KeyCode::Up => {
                if let Some(menu) = &mut self.skill_menu {
                    menu.move_selection(-1);
                }

                true
            }
            KeyCode::Down => {
                if let Some(menu) = &mut self.skill_menu {
                    menu.move_selection(1);
                }

                true
            }
            KeyCode::Enter | KeyCode::Tab if !key.modifiers.intersects(NEWLINE_MODIFIERS) => {
                let chosen = self
                    .skill_menu
                    .as_ref()
                    .and_then(SkillMenu::selected)
                    .map(str::to_owned);
                if let Some(name) = chosen {
                    self.insert_skill(&name);
                } else {
                    self.skill_menu = None;
                }

                true
            }
            _ => false,
        }
    }

    /// Replaces the `$fragment` the skill menu was opened for with `$name `,
    /// keeping the literal token in the prompt: it is what the user wrote,
    /// and the engine loads the skill separately when it builds the request.
    fn insert_skill(&mut self, name: &str) {
        let Some(menu) = self.skill_menu.take() else {
            return;
        };
        let fragment = menu.fragment();

        self.splice_token(fragment, format!("${name}"));
    }

    /// Opens the `/skills` dialog: one row per discovered skill — name,
    /// description, origin root — with Enter inserting `$name ` into the
    /// composer rather than switching anything (**D491**; the Codex CLI's
    /// own `/skills` listing beside its `$` selector).
    fn open_skills(&mut self) {
        let roots = self.engine.skill_roots();
        let rows = ganja_tool::skill::discover(&roots)
            .into_iter()
            .map(|skill| {
                let origin = ganja_tool::skill::origin(&roots, &skill);
                let source =
                    origin.map_or_else(|| "user".to_owned(), ganja_core::plugin::skill_source);
                let origin = origin.map(|dir| dir.display().to_string());
                let detail = match (skill.description.as_deref(), origin) {
                    (Some(description), Some(origin)) => {
                        format!("({source}) {description} — {origin}")
                    }
                    (Some(description), None) => format!("({source}) {description}"),
                    (None, Some(origin)) => format!("({source}) — {origin}"),
                    (None, None) => format!("({source})"),
                };

                list::Row {
                    value: skill.name.clone(),
                    label: skill.name,
                    detail: (!detail.is_empty()).then_some(detail),
                    active: false,
                }
            })
            .collect();

        self.chooser = Some((Chooser::Skills, ListDialog::new(" skills ", rows)));
        self.dirty = true;
    }

    /// Opens, re-narrows or closes the `$` skill menu — the `@` menu's rule
    /// with `discover` in place of the project walk. The roots are a handful
    /// of directories, so the walk happens on the keystroke itself rather
    /// than in the background, and the menu closes rather than sitting empty
    /// when a non-empty fragment matches nothing: `costs $5 each` is prose,
    /// not an invocation nobody can complete.
    fn sync_skill_menu(&mut self, text: &str, cursor: (usize, usize)) {
        let Some(fragment) = mention::skill_trigger(text, cursor) else {
            self.skill_menu = None;
            return;
        };
        if self
            .skill_menu
            .as_ref()
            .is_some_and(|menu| menu.answers(&fragment))
        {
            return;
        }

        let roots = self.engine.skill_roots();
        let skills: Vec<(ganja_tool::skill::Skill, String)> = ganja_tool::skill::discover(&roots)
            .into_iter()
            .map(|skill| {
                let source = ganja_tool::skill::origin(&roots, &skill)
                    .map_or_else(|| "user".to_owned(), ganja_core::plugin::skill_source);

                (skill, source)
            })
            .collect();
        let narrowing = !fragment.text.is_empty();
        let menu = SkillMenu::new(fragment, &skills);
        self.skill_menu = (!(menu.is_empty() && narrowing)).then_some(menu);
    }

    /// Opens, re-narrows or closes the two inline menus after the buffer
    /// changed.
    ///
    /// At most one is ever up. A leading `/` is checked first because it is the
    /// cheaper question and because a buffer that starts with one is a command
    /// being typed, whatever else is in it.
    async fn sync_menus(&mut self) {
        let text = self.editor.text();
        let cursor = self.editor.cursor();

        // A shell command is neither: it is going to the user's own shell.
        if self.editor.mode() == Mode::Shell {
            self.dropdown = None;
            self.files = None;
            self.skill_menu = None;
            self.cancel_file_walk();
            return;
        }

        if dropdown::triggered(&text, cursor) {
            self.files = None;
            self.skill_menu = None;
            self.cancel_file_walk();
            match &mut self.dropdown {
                Some(dropdown) => dropdown.refresh(&text),
                None => {
                    self.dropdown = Some(Dropdown::new(&text, self.engine_commands.clone()));
                }
            }
            self.completion = None;
            return;
        }
        // A `/team` argument slot raises the same box over what could fill
        // it (**D519**), rebuilt per keystroke because the slot itself moves
        // with the cursor.
        if let Some(slot) = command::team_completion(&text, cursor, &self.agent_kinds) {
            self.files = None;
            self.skill_menu = None;
            self.cancel_file_walk();
            self.dropdown = Some(Dropdown::values(&slot));
            self.completion = Some(slot);
            return;
        }
        self.completion = None;
        self.dropdown = None;

        let Some(fragment) = mention::trigger(&text, cursor) else {
            self.files = None;
            self.cancel_file_walk();
            self.sync_skill_menu(&text, cursor);
            return;
        };
        // An `@` fragment under the cursor is a file being mentioned, not a
        // skill being invoked.
        self.skill_menu = None;
        // The list depends on the fragment and on nothing else, so a keystroke
        // that left it alone must not walk the project again.
        if self
            .files
            .as_ref()
            .is_some_and(|files| files.answers(&fragment))
        {
            self.cancel_file_walk();
            return;
        }
        if self
            .file_walk
            .as_ref()
            .is_some_and(|walk| walk.fragment == fragment)
        {
            return;
        }

        // Spawned rather than awaited (2026-08-15): a keystroke that waited on
        // the walk stalled the whole loop for the walk's duration. A menu that
        // is already up keeps its rows — stale beats stalled — until the reap
        // on Tick installs the new ones.
        self.spawn_file_walk(fragment);
    }

    /// Ends an in-flight `@`-menu walk: the token stops the blocking walk
    /// between its batches, and nothing will reap the handle.
    fn cancel_file_walk(&mut self) {
        if let Some(walk) = self.file_walk.take() {
            walk.cancel.cancel();
            walk.task.abort();
        }
    }

    /// Installs a finished walk's rows — while the composer still shows the
    /// fragment they answer. Polled on Tick exactly as `wire_fetch` is.
    async fn poll_file_walk(&mut self) {
        if !self
            .file_walk
            .as_ref()
            .is_some_and(|walk| walk.task.is_finished())
        {
            return;
        }
        let walk = self.file_walk.take().expect("checked finished above");
        let Ok((paths, listing)) = walk.task.await else {
            return;
        };

        // Cached past the menu's own close: submit-time classification
        // reads this after Enter has already torn `self.files` down.
        let (sessions, incomplete) = match listing {
            lister::Listing::Complete(sessions) => (sessions, None),
            lister::Listing::Partial { rows, error } => (rows, Some(error)),
        };
        self.session_listing = sessions.clone();

        // The editor may have moved on while the project was being walked; a
        // menu for a fragment nobody is typing any more would be a lie.
        let (text, cursor) = (self.editor.text(), self.editor.cursor());
        if self.editor.mode() != Mode::Shell
            && mention::trigger(&text, cursor).is_some_and(|current| current == walk.fragment)
        {
            let rows = self.assemble_at_rows(paths, sessions);
            self.files = Some(Files::new(walk.fragment, rows, incomplete));
            self.dirty = true;
        }
    }

    /// Assembles the `@` menu's full row list (**D529**): the walked file
    /// paths in the walk's own order, then roster teammates (lead-assigned,
    /// so a completion never resolves ambiguously — **D528**), then live
    /// sessions off the injected lister, this session's own excluded.
    ///
    /// A session row whose name is held by another row too — teammate or
    /// session, under the registry's own case-insensitive fold — is marked
    /// `colliding`, so its completion splices the `uds:` spelling rather
    /// than the bare name (ADJ-3); one shadowed by a same-named real file
    /// is still shown, marked, because the file wins at submit regardless
    /// (**F12**).
    fn assemble_at_rows(
        &self,
        paths: Vec<String>,
        sessions: Vec<lister::LiveSession>,
    ) -> Vec<MenuRow> {
        let own_session = self.engine.session_id();
        let sessions: Vec<lister::LiveSession> = sessions
            .into_iter()
            .filter(|session| session.session_id != own_session.as_str())
            .collect();

        let teammates: Vec<MenuRow> = self
            .team_roster()
            .map(|view| team::rows(&view))
            .unwrap_or_default()
            .into_iter()
            .map(|row| MenuRow::Teammate {
                name: row.name,
                lead: row.is_lead,
            })
            .collect();

        // Every name in play, so a session's collision check sees teammates
        // and other sessions alike ("among themselves or with the other
        // kind", D529). Owned, not borrowed: `teammates` and `sessions`
        // both move into `rows` right below.
        let all_names: Vec<String> = teammates
            .iter()
            .filter_map(|row| match row {
                MenuRow::Teammate { name, .. } => Some(name.clone()),
                MenuRow::File(_) | MenuRow::Session { .. } => None,
            })
            .chain(sessions.iter().map(|session| session.name.clone()))
            .collect();

        let mut rows: Vec<MenuRow> = paths.iter().cloned().map(MenuRow::File).collect();
        rows.extend(teammates);
        rows.extend(sessions.into_iter().map(|session| {
            let colliding = all_names
                .iter()
                .filter(|name| registry::same_name(name, &session.name))
                .count()
                > 1;
            let shadowed = paths.contains(&session.name);

            MenuRow::Session {
                address: format!("{ADDRESS_SCHEME}{}", session.socket.display()),
                name: session.name,
                cwd: session.cwd,
                stem: session.stem,
                colliding,
                shadowed,
            }
        }));

        rows
    }

    /// The files a mention fragment offers, relative to [`App::cwd`].
    ///
    /// Driven through the tool registry rather than through a walker of its
    /// own, so the menu offers exactly the files `glob` would find: the same
    /// ignore rules, the same hidden-file rule, the same order. It is also the
    /// reason a mention is not a read — the context carries a
    /// [`FileTimes`] of its own, so nothing this walk touches is recorded
    /// against the session and `edit` still refuses a file the model has not
    /// opened.
    ///
    /// The walk is synchronous work on a blocking thread and runs once per
    /// change to the fragment. On a very large tree that is a visible pause
    /// while typing; upstream answers from an index it keeps warm, which is a
    /// P6-sized piece of machinery this build does not have.
    fn spawn_file_walk(&mut self, fragment: mention::Fragment) {
        self.cancel_file_walk();
        let Some(glob) = self.tools.get("glob").cloned() else {
            return;
        };
        let cancel = CancellationToken::new();
        let ctx = ToolCtx {
            cwd: self.cwd.clone(),
            cancel: cancel.clone(),
            call_id: MENTION_CALL.to_owned(),
            files: Arc::new(FileTimes::default()),
            // The menu is a file walk, not a conversation: it has no
            // credentials to guard, nothing to delegate to, and nobody to ask.
            credentials: Credentials::Unguarded,
            spawn: None,
            postbox: None,
            ask: None,
            switch: None,
            jobs: None,
        };
        let cwd = self.cwd.clone();
        let wanted = pattern(&fragment.text);
        // The live-session listing rides the same spawn (**D529** Axis 5):
        // a session with no lister — a pane member, a headless build —
        // fetches nothing, which is the graceful absence AC-27 pins.
        let sessions = self.lister.as_ref().map(|lister| lister.list());

        let task = tokio::spawn(async move {
            // A fragment is typed, not written: half of one is a pattern that
            // does not parse yet, and a menu is not the place to say so.
            let paths = match glob
                .run(serde_json::json!({ "pattern": wanted }), &ctx)
                .await
            {
                Ok(found) => relative_paths(&cwd, &found.output),
                Err(_) => Vec::new(),
            };
            let listing = match sessions {
                Some(sessions) => sessions.await,
                None => lister::Listing::Complete(Vec::new()),
            };

            (paths, listing)
        });
        self.file_walk = Some(FileWalk {
            fragment,
            cancel,
            task,
        });
    }

    /// Opens the model list over the roster this session's *wire* owns where
    /// there is one, and over this provider's catalog entries otherwise.
    ///
    /// The wire wins where it answers, and that is now a decision rather than
    /// an accident of an empty table: cursor has no catalog rows to lose, but a
    /// ChatGPT seat's provider has plenty and its offering is still the pinned
    /// five (**D476**) — offering a session the vendor's whole catalog would
    /// list models its own backend refuses. `wire_lists_models` is the seam's
    /// own decision asked synchronously, because this opens a dialog or spawns
    /// a fetch and cannot await to find out which.
    ///
    /// This provider's only: a switch is same-provider by construction, so a
    /// row for anything else would be a refusal with a nice label on it.
    ///
    /// The wire path runs off the render loop: the fetch is spawned, the tick
    /// that reaps it opens the list, and until then a slow endpoint costs a
    /// status line rather than a frozen frame. That the seat's arm answers
    /// instantly buys it no shortcut — one lane for both keeps one code path.
    fn open_models(&mut self) {
        if !provider::wire_lists_models(&self.provider) {
            let rows: Vec<list::Row> = catalog::models()
                .filter(|model| model.provider_id == self.provider)
                .map(|model| list::Row {
                    value: model.id.to_owned(),
                    label: model.id.to_owned(),
                    detail: Some(model.name.to_owned()),
                    active: model.id == self.model,
                })
                .collect();
            if !rows.is_empty() {
                self.chooser = Some((Chooser::Models, ListDialog::new(" models ", rows)));
                return;
            }
        }

        // The wire's listing answers — from the App-lifetime cache when a
        // fetch already landed it.
        if let Some(models) = &self.wire_models {
            let rows = wire_rows(models, &self.model);
            self.chooser = Some((Chooser::Models, ListDialog::new(" models ", rows)));
            return;
        }
        // One fetch at a time: the tick that reaps the one in flight opens
        // the list, and a second `/model` before then changes nothing.
        if self.wire_fetch.is_some() {
            return;
        }

        // The task owns its provider id because the seam borrows.
        let id = self.provider.clone();
        self.wire_fetch = Some(tokio::spawn(async move {
            provider::wire_model_listing(&id).await
        }));
        self.status
            .set_notice(Some(format!("fetching {} models…", self.provider)));
    }

    /// Opens the flat effort picker over the active model's catalog names.
    ///
    /// A model the catalog gives no efforts — every uncataloged provider's,
    /// and most cataloged rows — gets ganja's reworded refusal sentence in the
    /// status bar instead of an empty dialog (`app.tsx:717`, the `variant.list`
    /// command's toast).
    fn open_effort(&mut self) {
        // Provider-scoped for the engine's reason (`catalog::model_for`): the
        // names offered here must be the names the engine will accept.
        let names: Vec<String> = catalog::model_for(&self.provider, &self.model)
            .map(|info| info.variants.keys().cloned().collect())
            .unwrap_or_default();
        if names.is_empty() {
            self.status.set_notice(Some(NO_EFFORTS.to_owned()));
            return;
        }

        let rows = effort::rows(names.iter().map(String::as_str), self.effort.as_deref());
        self.chooser = Some((Chooser::Effort, ListDialog::new(" effort ", rows)));
    }

    /// Opens the agent list over the agents a user may switch to.
    ///
    /// Subagents and hidden agents are left out: the first are the task tool's
    /// to spawn and the second exist precisely so as not to be offered.
    fn open_agents(&mut self) {
        let rows: Vec<list::Row> = self
            .selectable_agents()
            .into_iter()
            .map(|agent| list::Row {
                value: agent.name.clone(),
                label: agent.name.clone(),
                detail: agent.description.clone(),
                active: self.agent.as_deref() == Some(agent.name.as_str()),
            })
            .collect();

        self.chooser = Some((Chooser::Agents, ListDialog::new(" agents ", rows)));
    }

    /// The agents a user may switch to, in registry order — the one filter
    /// [`App::open_agents`] and [`App::cycle_agent`] both apply, so the list
    /// and the cycle cannot drift over who is offered.
    fn selectable_agents(&self) -> Vec<&ganja_core::agent::Agent> {
        self.engine
            .agents()
            .map(|registry| {
                registry
                    .agents()
                    .iter()
                    .filter(|agent| agent.selectable())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Moves to the next agent a user may switch to, wrapping.
    ///
    /// Wrapping where every list here clamps, because this is not a cursor in
    /// a list: it is one key pressed repeatedly to get somewhere, and stopping
    /// at the end would mean reaching for the mouse.
    async fn cycle_agent(&mut self) {
        let names: Vec<String> = self
            .selectable_agents()
            .into_iter()
            .map(|agent| agent.name.clone())
            .collect();
        if names.is_empty() {
            return;
        }

        let next = self
            .agent
            .as_deref()
            .and_then(|current| names.iter().position(|name| name == current))
            .map_or(0, |index| (index + 1) % names.len());

        self.switch_agent(names[next].clone()).await;
    }

    /// Runs the rest of the session as `name`.
    ///
    /// A refusal — a switch mid-turn, a name the registry does not hold —
    /// lands in the status bar and leaves the list open, so the user still has
    /// what they were choosing from.
    async fn switch_agent(&mut self, name: String) {
        match self
            .engine
            .send(Command::SwitchAgent { name: name.clone() })
            .await
        {
            Ok(()) => {
                // Redundant but harmless: `AgentChanged` is now the source of
                // truth for every adoption, including this manual one, so its
                // handler would keep the indicator correct without this eager
                // frontend update.
                self.agent = Some(name);
                self.status.set_agent(self.agent.clone());
                // An agent may name a model of its own, which the engine
                // adopts on the switch; pricing follows the engine's answer
                // rather than what the frontend last remembered.
                self.model = self.engine.model();
                self.status.set_model(Some(self.model.clone()));
                self.chooser = None;
                self.status.set_notice(None);
            }
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// Asks the rest of the session of `model`.
    async fn switch_model(&mut self, model: String) {
        match self
            .engine
            .send(Command::SwitchModel {
                model: model.clone(),
            })
            .await
        {
            Ok(()) => {
                self.model = model;
                self.status.set_model(Some(self.model.clone()));
                // A model that kept the effort keeps the segment, naming the
                // new model; one that lost it is announced by the engine's
                // `EffortChanged`, whose handler clears the segment then.
                self.sync_effort_status();
                self.chooser = None;
                self.status.set_notice(None);
            }
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// Runs the rest of the session under the picker's choice — a catalog
    /// name, or [`effort::DEFAULT`] for none.
    ///
    /// A refusal — a switch mid-turn, a name the row does not carry — lands in
    /// the status bar and leaves the list open, exactly as the model list's
    /// does.
    async fn switch_effort(&mut self, value: String) {
        let effort = (value != effort::DEFAULT).then_some(value);
        match self.engine.send(Command::SwitchEffort { effort }).await {
            Ok(()) => {
                // Redundant but harmless, the way `switch_agent`'s eager
                // update is: `EffortChanged` is the source of truth for
                // every adoption, this manual one included.
                self.effort = self.engine.effort();
                self.sync_effort_status();
                self.chooser = None;
                self.status.set_notice(None);
            }
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// Re-renders the status bar's `model (effort)` segment from what this
    /// frontend currently believes, which is the one place the pair is put
    /// together — an effort shown against the wrong model would name a
    /// selection that does not exist.
    fn sync_effort_status(&mut self) {
        self.status.set_effort(
            self.effort
                .as_ref()
                .map(|effort| (self.model.clone(), effort.clone())),
        );
    }

    /// Opens the theme picker with the cursor on the theme already in use.
    fn open_themes(&mut self) {
        self.theme_list = Some(ThemeList::new(self.themes.names(), self.themes.active()));
    }

    /// Opens the Ctrl+T inspector overlay, always on its first tab (**F2**).
    fn open_inspector(&mut self) {
        self.inspector = Some(Inspector::new());
    }

    /// One keypress while the theme picker is open, which owns every key.
    fn handle_theme_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.close_themes(true),
            KeyCode::Up | KeyCode::Char('k') => self.move_theme(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_theme(1),
            KeyCode::Enter => self.close_themes(false),
            _ => {}
        }
    }

    /// Moves the cursor by `delta` rows and applies what it lands on.
    ///
    /// Applying on the way past is the point of the dialog: a theme is
    /// something you recognize on screen, not something you recognize by name.
    fn move_theme(&mut self, delta: isize) {
        let Some(list) = &mut self.theme_list else {
            return;
        };
        list.move_selection(delta);

        if let Some(name) = list.selected().map(str::to_owned) {
            self.apply_theme(&name);
        }
    }

    /// Closes the picker, either putting back the theme it opened on or
    /// keeping — and storing — the one under the cursor.
    ///
    /// A pick that cannot be stored still applies for this run; the notice says
    /// only that it will not survive it.
    fn close_themes(&mut self, revert: bool) {
        let Some(list) = self.theme_list.take() else {
            return;
        };

        if revert {
            self.apply_theme(list.initial());
            return;
        }
        if let Err(refusal) = self.themes.persist() {
            self.status.set_notice(Some(refusal.to_string()));
        }
    }

    /// Installs `name` everywhere the active theme is held.
    ///
    /// Not one assignment: the editor holds the styles it was built with, and
    /// the transcript caches lines with their styles baked in. The first is
    /// repainted here; the second notices at its next frame, because the theme
    /// carries a revision the cache compares against.
    fn apply_theme(&mut self, name: &str) {
        let Some(theme) = self.themes.select(name) else {
            return;
        };

        self.editor.restyle(&theme);
        self.theme = theme;
    }

    /// Opens the sessions picker over what this project's store holds.
    ///
    /// A refusal — an engine running without storage, a store that will not
    /// read — lands in the status bar: there is nothing to pick from, so a
    /// dialog would have nothing to say that the notice does not.
    async fn open_picker(&mut self) {
        match self.engine.sessions().await {
            Ok(entries) => {
                // Roots only. A child session belongs to the task call that
                // spawned it — it is rendered on that call's row, its title is
                // the description the model wrote, and resuming into one would
                // put the user inside a delegated turn with no way to see what
                // asked for it (upstream lists `roots: true` here too).
                let roots = entries
                    .into_iter()
                    .filter(|info| info.parent.is_none())
                    .collect();

                self.sessions = Some(Sessions::new(roots, sessions::now()));
            }
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// One keypress while the picker is open, which owns every key: the
    /// editor and the transcript beneath it are not what the user is acting
    /// on right now.
    async fn handle_picker_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.sessions = None,
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(sessions) = &mut self.sessions {
                    sessions.move_selection(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(sessions) = &mut self.sessions {
                    sessions.move_selection(1);
                }
            }
            KeyCode::Enter => self.resume_selected().await,
            _ => {}
        }
    }

    /// Switches to the session the picker is on.
    ///
    /// The screen changes only once the engine has actually resumed: a resume
    /// it refuses — mid-turn, or a session that vanished between listing and
    /// choosing — leaves the current conversation up, with the refusal in the
    /// status bar. The picker stays open so the user still has the list they
    /// were choosing from.
    async fn resume_selected(&mut self) {
        let Some(id) = self
            .sessions
            .as_ref()
            .and_then(|sessions| sessions.selected())
            .map(|info| info.id.clone())
        else {
            // An empty list has nothing under the cursor; Enter means nothing.
            return;
        };

        match self.engine.resume(&id).await {
            Ok(transcript) => {
                self.sessions = None;
                self.chat.clear();
                // The bar's todo list belonged to the conversation being
                // left; the resumed one's next `todowrite` refills it.
                self.status.set_todos(None);
                self.seed(transcript);
                // A stored session carries the agent and the model it was left
                // on, and the engine restores both; the bar would otherwise go
                // on naming whatever the previous session was using.
                self.agent = self.engine.agent();
                self.status.set_agent(self.agent.clone());
                self.model = self.engine.model();
                self.status.set_model(Some(self.model.clone()));
                // The effort rides the same stored row the agent and the
                // model do, filtered by the engine against the resumed model.
                self.effort = self.engine.effort();
                self.sync_effort_status();
                self.status.set_notice(None);
            }
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// Opens the Ctrl+R search over remembered prompts, capturing the
    /// composer's exact buffer for an Esc to restore untouched.
    fn open_history_search(&mut self) {
        self.history_search = Some(HistorySearch::new(
            self.history.entries(),
            sessions::now(),
            self.editor.text(),
            self.editor.cursor(),
        ));
    }

    /// One keypress while the history search is open, which owns every key:
    /// its query line is what the keyboard is pointed at.
    async fn handle_history_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_history_search(),
            KeyCode::Up => self.move_history_search(-1),
            KeyCode::Down => self.move_history_search(1),
            KeyCode::Backspace => {
                if let Some(search) = &mut self.history_search {
                    search.backspace();
                }
            }
            KeyCode::Enter => self.fill_from_history_search().await,
            // Everything printable narrows the query — j and k included, the
            // same reading the palette's own filter line gives them.
            KeyCode::Char(character) if !key.modifiers.intersects(SHORTCUT_MODIFIERS) => {
                if let Some(search) = &mut self.history_search {
                    search.push(character);
                }
            }
            _ => {}
        }
    }

    /// Moves the search's cursor by `delta` rows.
    fn move_history_search(&mut self, delta: isize) {
        if let Some(search) = &mut self.history_search {
            search.move_selection(delta);
        }
    }

    /// Closes the search, putting back exactly the buffer it opened over —
    /// text and cursor both, byte for byte.
    fn close_history_search(&mut self) {
        let Some(search) = self.history_search.take() else {
            return;
        };
        let (row, column) = search.origin_cursor();
        self.editor.set_text_at(search.origin_text(), row, column);
    }

    /// Puts the entry under the cursor into the composer and closes the
    /// search — an Enter here fills the buffer, it never submits it.
    async fn fill_from_history_search(&mut self) {
        let Some(input) = self
            .history_search
            .as_ref()
            .and_then(HistorySearch::selected)
            .map(|prompt| prompt.input.clone())
        else {
            // An empty list has nothing under the cursor; Enter means nothing.
            return;
        };

        self.history_search = None;
        self.editor.set_text(&input);
        // The recalled entry may itself hold a `/command` or an `@mention`,
        // just as an Up-arrow recall's can — see `App::recall`.
        self.sync_menus().await;
    }

    /// Opens the rewind picker over the checkpoints the transcript holds
    /// (**F7**).
    ///
    /// Reached as `/rewind`, from the palette or the `/` menu, like `/undo`
    /// and for the same reason (**D4**: there is no leader key here). The Esc
    /// Esc gesture that once landed here is the backtrack walk's now
    /// (**D467**, [`App::open_backtrack`]); this picker stays the door to the
    /// `Files` and `Both` scopes.
    fn open_rewind(&mut self) {
        self.rewind = Some(Rewind::new(self.chat.checkpoints()));
    }

    /// Enters the backtrack walk (**D467**): the newest user message lights
    /// up, and the status bar says what the two claimed keys do.
    ///
    /// A transcript with no user message has nothing to walk, so the gesture
    /// is a no-op there rather than a mode with nothing highlighted.
    fn open_backtrack(&mut self) {
        let candidates: Vec<MessageId> = self
            .chat
            .checkpoints()
            .into_iter()
            .map(|checkpoint| checkpoint.message_id)
            .collect();
        let Some(newest) = candidates.first().cloned() else {
            return;
        };

        self.chat.set_backtrack(Some(newest));
        self.status.set_notice(Some(BACKTRACK_HINT.to_owned()));
        self.backtrack = Some(Backtrack {
            candidates,
            index: 0,
        });
    }

    /// One more Esc in the walk: the highlight steps one user message older,
    /// holding at the oldest rather than wrapping — past the top, another
    /// press means "further back" and there is no further back to offer.
    fn step_backtrack(&mut self) {
        if let Some(backtrack) = &mut self.backtrack {
            backtrack.index = (backtrack.index + 1).min(backtrack.candidates.len() - 1);
            self.chat
                .set_backtrack(Some(backtrack.candidates[backtrack.index].clone()));
        }
    }

    /// Enter in the walk: revert the conversation to before the highlighted
    /// message.
    ///
    /// Conversation-only on purpose — Codex's backtrack forks the chat and
    /// leaves the working tree alone; `/rewind` remains the door to the file
    /// scopes. The composer prefill is not done here: the engine's
    /// `RevertChanged` carries the whole prompt back, and the one handler of
    /// that event is the one prefill mechanism.
    async fn confirm_backtrack(&mut self) {
        let Some(backtrack) = self.backtrack.take() else {
            return;
        };
        self.chat.set_backtrack(None);
        self.status.set_notice(None);

        let message_id = backtrack.candidates[backtrack.index].clone();
        self.rewind_to(message_id, RevertScope::Conversation).await;
    }

    /// Leaves the walk without reverting anything: highlight and hint both
    /// come down, and nothing is sent.
    fn close_backtrack(&mut self) {
        if self.backtrack.take().is_some() {
            self.chat.set_backtrack(None);
            // The hint is the only notice a live walk can be showing, so
            // clearing outright cannot eat somebody else's message.
            self.status.set_notice(None);
        }
    }

    /// One keypress while the rewind picker is open, which owns every key: its
    /// list is what the keyboard is pointed at.
    ///
    /// Esc closes from either step rather than stepping back to the first: the
    /// picker is two views of one question, and a person who wants out of the
    /// scope choice wants out of the picker.
    async fn handle_rewind_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.rewind = None,
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(rewind) = &mut self.rewind {
                    rewind.move_selection(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(rewind) = &mut self.rewind {
                    rewind.move_selection(1);
                }
            }
            KeyCode::Enter => self.advance_rewind().await,
            _ => {}
        }
    }

    /// Enter in the rewind picker: the first one opens the scope choice, the
    /// second sends the rewind.
    async fn advance_rewind(&mut self) {
        let Some(rewind) = &mut self.rewind else {
            return;
        };

        if !rewind.is_choosing_scope() {
            // `(Current)` is where the session already is, so choosing it is a
            // person deciding not to rewind at all.
            if !rewind.advance() {
                self.rewind = None;
            }

            return;
        }

        let chosen = rewind.chosen();
        self.rewind = None;
        if let Some((message_id, scope)) = chosen {
            self.rewind_to(message_id, scope).await;
        }
    }

    /// Asks the engine to take the session back to `message_id`.
    ///
    /// Nothing is hidden or restored here: the engine answers with
    /// `RevertChanged`, and that event is the only thing that moves the
    /// transcript — the same rule `/undo` follows. What a refusal costs is a
    /// line of the status bar: a turn still streaming (**D119**), a session
    /// that takes no snapshots, or a checkpoint that is not one.
    async fn rewind_to(&mut self, message_id: MessageId, scope: RevertScope) {
        // The same reading an undo leaves behind: what this hid can be stepped
        // back through, so the next cleared revert means "show them again".
        // A code-only rewind hides nothing and clears nothing, so it must not
        // touch the reading a standing undo left.
        if scope.touches_conversation() {
            self.cleared = Cleared::Unhide;
        }
        self.code_only_rewind = !scope.touches_conversation();

        if let Err(refusal) = self
            .engine
            .send(Command::RevertTo { message_id, scope })
            .await
        {
            // Nothing was reverted, so no event is coming to consume the flag.
            self.code_only_rewind = false;
            self.status.set_notice(Some(refusal.to_string()));
        }
    }

    /// Opens the `/mcp` dialog over every configured server's current status
    /// and tool count (**F5**).
    fn open_mcp(&mut self) {
        self.mcp_dialog = Some(mcp::Mcp::new(self.mcp_dialog_rows()));
    }

    /// Opens the `/context` panel over the engine's on-demand breakdown
    /// (**D470**) — computed now, from the same state the next request would
    /// be assembled from, which is what makes the panel answer on a fresh
    /// session and immediately after a revert. The catalog display name is
    /// resolved here, beside the one caller that holds the breakdown, so the
    /// component never reads the compiled-in catalog itself (W7); an
    /// uncataloged model passes no name and renders its id once.
    async fn open_context(&mut self) {
        let breakdown = self.engine.context_breakdown().await;
        let display = catalog::model(&breakdown.model).map(|model| model.name.clone());
        self.context_dialog = Some(context::Context::new(display, breakdown));
    }

    /// Opens the `/usage` panel over what this side already holds (**D471**):
    /// the status bar's totals, the inspector's per-turn rows, and the same
    /// context estimate the bar's meter polls. The cache and reasoning splits
    /// come from the session record where one exists — it accumulates every
    /// turn, resumes included — and from summing the in-memory turn rows on
    /// an engine that stores nothing.
    fn open_usage(&mut self) {
        let splits = self
            .engine
            .current_session()
            .map(|session| session.usage)
            .unwrap_or_else(|| {
                self.turn_usages
                    .iter()
                    .fold(Usage::default(), |sum, row| Usage {
                        input_tokens: sum.input_tokens.saturating_add(row.usage.input_tokens),
                        output_tokens: sum.output_tokens.saturating_add(row.usage.output_tokens),
                        reasoning_tokens: sum
                            .reasoning_tokens
                            .saturating_add(row.usage.reasoning_tokens),
                        cache_read_tokens: sum
                            .cache_read_tokens
                            .saturating_add(row.usage.cache_read_tokens),
                        cache_write_tokens: sum
                            .cache_write_tokens
                            .saturating_add(row.usage.cache_write_tokens),
                    })
            });
        let estimate = self.engine.context_estimate();

        self.usage_dialog = Some(usage::Usage::new(usage::Data {
            totals: self.totals,
            splits,
            context: estimate.window.map(|window| (estimate.tokens, window)),
            turns: self.turn_usages.iter().cloned().collect(),
            // The app's own wall clock (W7): the one duration this side
            // truly measures. A resumed session's earlier processes left no
            // clock behind, so this is honestly the app's lifetime, not the
            // stored session's.
            duration: Some(self.session_start.elapsed()),
            // Read at open time like everything else on this panel — a
            // snapshot, not a view. Empty for a wire that has heard no such
            // headers, which renders no section at all (**D484**).
            rates: self.engine.rate_windows(),
            // The plan buckets beside them, read at the same moment
            // (**D485**). Empty renders no section and the honest tail
            // instead, which is the panel's own rule rather than this
            // caller's.
            plans: self.engine.plan_windows(),
            // The panel judges expiry against the moment it was opened.
            now: Some(std::time::SystemTime::now()),
        }));
    }

    /// The `/mcp` dialog's rows, fresh off the engine.
    ///
    /// Driven by [`Engine::mcp_names`] rather than by [`Engine::mcp_status`]'s
    /// map alone, so a server still on its first dial gets a "dialling" row
    /// instead of not existing yet — the same distinction
    /// [`ganja_core::mcp::Status`]'s own doc draws.
    fn mcp_dialog_rows(&self) -> Vec<mcp::Row> {
        let status = self.engine.mcp_status();
        let counts = self.engine.mcp_tool_counts();

        self.engine
            .mcp_names()
            .into_iter()
            .map(|name| {
                let (label, detail, mut actions) = match status.get(&name) {
                    Some(ganja_core::McpStatus::Connected) => {
                        ("Connected".to_owned(), None, Vec::new())
                    }
                    Some(ganja_core::McpStatus::Disabled) => {
                        ("Disabled".to_owned(), None, Vec::new())
                    }
                    Some(ganja_core::McpStatus::Failed { error }) => (
                        "Failed".to_owned(),
                        Some(error.lines().next().unwrap_or(error).trim().to_owned()),
                        vec![mcp::Action::Reconnect],
                    ),
                    // Absent is what "still dialling" looks like without a
                    // fourth `Status` variant to mean it.
                    None => ("dialling".to_owned(), None, Vec::new()),
                };
                // Login belongs on an `oauth`-configured server whatever its
                // status — unlike Reconnect above, gated on `Failed` alone —
                // and a login already in flight overrides the row entirely
                // with the URL a person has to open, poll-refreshed the same
                // way the status itself is.
                let (label, detail) = match self.engine.mcp_login_url(&name) {
                    Some(url) if !url.is_empty() => {
                        ("Logging in".to_owned(), Some(format!("go to: {url}")))
                    }
                    Some(_) => ("Logging in".to_owned(), None),
                    None => (label, detail),
                };
                if self.engine.mcp_has_oauth(&name) {
                    actions.push(mcp::Action::Login);
                }
                let tools = counts.get(&name).copied();

                mcp::Row {
                    name,
                    status: label,
                    tools,
                    detail,
                    actions,
                }
            })
            .collect()
    }

    /// Looks at where the MCP servers stand and refreshes the `/mcp` dialog's
    /// rows, while it is open. Status stays poll-driven — the same tick
    /// [`App::poll_mcp`] already rides, no new protocol event involved.
    fn poll_mcp_dialog(&mut self) {
        if self.mcp_dialog.is_none() {
            return;
        }

        let rows = self.mcp_dialog_rows();
        if let Some(dialog) = &mut self.mcp_dialog {
            dialog.refresh(rows);
        }
        self.dirty = true;
    }

    /// One keypress while the `/mcp` dialog is open, which owns every key —
    /// the same shape [`App::handle_rewind_key`] answers to, Esc closing from
    /// either of the dialog's two steps rather than stepping back to the
    /// first.
    async fn handle_mcp_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.mcp_dialog = None,
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(dialog) = &mut self.mcp_dialog {
                    dialog.move_selection(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(dialog) = &mut self.mcp_dialog {
                    dialog.move_selection(1);
                }
            }
            KeyCode::Enter => self.advance_mcp().await,
            _ => {}
        }
    }

    /// Enter in the `/mcp` dialog: on the server step, opens the row's
    /// actions where it has any; on the action step, runs the chosen one and
    /// returns to the server list — the dialog stays open so the outcome
    /// shows up on the row the next poll refreshes.
    async fn advance_mcp(&mut self) {
        let Some(dialog) = &mut self.mcp_dialog else {
            return;
        };

        if !dialog.is_choosing_action() {
            // A row with nothing to choose leaves the dialog exactly as it
            // was — see `Mcp::advance`'s own doc for why this differs from
            // the rewind picker's "(Current)" close.
            dialog.advance();

            return;
        }

        let Some((name, action)) = dialog.chosen() else {
            return;
        };
        let name = name.to_owned();
        match action {
            mcp::Action::Reconnect => self.reconnect_mcp(name).await,
            mcp::Action::Login => self.login_mcp(name).await,
        }
    }

    /// Asks the engine to re-dial `name`, and reflects the outcome on the
    /// dialog immediately rather than waiting for the next tick's poll.
    async fn reconnect_mcp(&mut self, name: String) {
        if let Some(dialog) = &mut self.mcp_dialog {
            dialog.back_to_servers();
        }

        if let Err(refusal) = self.engine.reconnect_mcp(&name).await {
            self.status.set_notice(Some(refusal));
        }

        let rows = self.mcp_dialog_rows();
        if let Some(dialog) = &mut self.mcp_dialog {
            dialog.refresh(rows);
        }
    }

    /// Starts an OAuth login for `name`. A different call shape from
    /// [`App::reconnect_mcp`]: [`ganja_core::Engine::login_mcp`] returns as
    /// soon as the browser URL is ready rather than once the login finishes,
    /// so what this awaits is bounded by one discovery-and-registration round
    /// trip and not by somebody completing a login in a browser — the wait
    /// for that runs in the background, and [`App::poll_mcp_dialog`]'s next
    /// tick is what shows the URL and, later, the outcome.
    async fn login_mcp(&mut self, name: String) {
        if let Some(dialog) = &mut self.mcp_dialog {
            dialog.back_to_servers();
        }

        if let Err(refusal) = self.engine.login_mcp(&name).await {
            self.status.set_notice(Some(refusal));
        }

        let rows = self.mcp_dialog_rows();
        if let Some(dialog) = &mut self.mcp_dialog {
            dialog.refresh(rows);
        }
    }

    /// Opens the `/held` listing over what the admission gate holds right now
    /// (**D524**) — the only review surface an explicit or mode-unknown hold
    /// has, and the second one a parity hold does.
    fn open_held(&mut self) {
        self.held_dialog = Some(held::HeldList::new(self.held_dialog_rows()));
    }

    /// The `/held` listing's rows, fresh off [`Engine::held_messages`]:
    /// sender, cause, age and a one-line preview — the summary where the
    /// sender wrote one, the body's first line otherwise, display-capped by
    /// the row builder (the engine caps the preview and deliberately not the
    /// summary).
    fn held_dialog_rows(&self) -> Vec<held::Row> {
        self.engine
            .held_messages()
            .into_iter()
            .map(|entry| {
                held::Row::new(
                    entry.id,
                    entry.from,
                    entry.cause,
                    entry.age,
                    entry.summary.as_ref().map(|summary| summary.as_str()),
                    entry.preview.as_str(),
                )
            })
            .collect()
    }

    /// Polls the held count onto the status bar — the D462 posture: read off
    /// engine state, never tracked as a tally — and refreshes the `/held`
    /// listing while it is open. Also keeps the approval modal's countdown
    /// moving, since nothing else redraws an idle screen under it.
    fn poll_held(&mut self) {
        let held = self.engine.held_messages();
        self.status.set_held(held.len());
        if self.held_dialog.is_some() {
            let rows = self.held_dialog_rows();
            if let Some(dialog) = &mut self.held_dialog {
                dialog.refresh(rows);
            }
            self.dirty = true;
        } else if matches!(&self.permission, Some(PendingDialog::Held(_))) {
            self.dirty = true;
        }
    }

    /// One keypress while the `/held` listing is open, which owns every
    /// key — [`App::handle_mcp_key`]'s shape, Esc closing from either step.
    /// Closing reviews nothing: unlike the approval modal's Esc, the listing
    /// is a window over the buffer, and leaving it decides nothing.
    async fn handle_held_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.held_dialog = None,
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(dialog) = &mut self.held_dialog {
                    dialog.move_selection(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(dialog) = &mut self.held_dialog {
                    dialog.move_selection(1);
                }
            }
            KeyCode::Enter => self.advance_held().await,
            _ => {}
        }
    }

    /// Enter in the `/held` listing: on the row step, opens the entry's
    /// Release/Deny choice; on the action step, settles the entry and
    /// returns to the rows — the dialog stays open, and the next poll
    /// retires the settled row.
    async fn advance_held(&mut self) {
        let Some(dialog) = &mut self.held_dialog else {
            return;
        };

        if !dialog.is_choosing_action() {
            dialog.advance();

            return;
        }

        let Some((id, action)) = dialog.chosen() else {
            return;
        };
        let id = id.clone();
        let decision = action.decision();
        dialog.back_to_rows();
        if let Err(error) = self.engine.send(Command::SettleHeld { id, decision }).await {
            self.status
                .set_notice(Some(format!("the settle was refused: {error}")));
        }
        self.poll_held();
    }

    /// Opens the `/plugin` dialog over what the store holds right now.
    ///
    /// The rows come off [`ganja_core::plugin::Store::list`] — the same call
    /// `ganja plugin list` prints — so the dialog and the CLI are two views
    /// of one answer, which is what keeps them from disagreeing (**D472**'s
    /// one-collector rule). A store that cannot be read opens the dialog
    /// anyway, with the refusal on its notice line: an unreadable state file
    /// is worth a person's attention, not a silent no-op.
    /// A dialog opened while a store action from an earlier one is still
    /// running is told so: the lane is the app's, not the dialog's, and a
    /// fresh dialog that showed Add as live would be inviting a refusal.
    fn open_plugin(&mut self) {
        let (rows, notice) = self.plugin_rows();
        let mut dialog = plugin::Plugin::new(rows);
        if let Some(notice) = notice {
            dialog.set_notice(notice);
        }
        dialog.set_busy(self.plugin_task.is_some());
        self.plugin_dialog = Some(dialog);
    }

    /// The store the dialog acts on: the one a test handed in, or the real
    /// one under the config home — resolved per action rather than held,
    /// so an environment that changes homes between sessions is never read
    /// through a stale path.
    fn resolve_plugin_store(&self) -> Option<ganja_core::plugin::Store> {
        self.plugin_store
            .clone()
            .or_else(ganja_core::plugin::Store::discover)
    }

    /// The `/plugin` dialog's rows, fresh off the store, with anything the
    /// read had to complain about.
    fn plugin_rows(&self) -> (Vec<plugin::Row>, Option<String>) {
        let Some(store) = self.resolve_plugin_store() else {
            return (
                Vec::new(),
                Some(
                    "no config home could be resolved, so there is nowhere to keep plugins"
                        .to_owned(),
                ),
            );
        };
        match store.list() {
            Ok(listings) => (
                listings
                    .into_iter()
                    .map(|listing| plugin::Row {
                        summary: plugin::summarize(&listing.components),
                        name: listing.name,
                        enabled: listing.enabled,
                        marketplace: listing.marketplace,
                    })
                    .collect(),
                None,
            ),
            Err(error) => (Vec::new(), Some(error.to_string())),
        }
    }

    /// One keypress while the `/plugin` dialog is open, which owns every key —
    /// [`drive_two_step`], the same driver the `/team` dialog reads.
    fn handle_plugin_key(&mut self, key: KeyEvent) {
        let Some(dialog) = &mut self.plugin_dialog else {
            return;
        };
        match drive_two_step(dialog, key) {
            Driven::Close => self.plugin_dialog = None,
            Driven::Run(effect) => self.run_plugin_effect(effect),
            Driven::Stay => {}
        }
    }

    /// Starts what the dialog decided, and says on its notice line what is
    /// happening — never a silent state. The outcome lands later, through
    /// [`App::poll_plugin_task`].
    ///
    /// The store calls run under [`tokio::task::spawn_blocking`] and are
    /// **not awaited here**, because two of them are not quick — a
    /// marketplace add may be a `git clone` over a network, an install copies
    /// a tree — and an event loop that awaits one draws nothing and answers
    /// no key until it returns (`zus`). The others ride the same lane rather
    /// than earning a second one, and one lane is also what keeps two writers
    /// off the same `plugins.json`: a second action while one runs is refused
    /// with [`plugin::BUSY`] rather than queued, since a person who can see
    /// the first one running can choose when to ask again.
    ///
    /// The reload is the one action that stays here: it touches no store
    /// file, it swaps the engine seams through `&self`, and it is a config
    /// read rather than a network call.
    fn run_plugin_effect(&mut self, effect: plugin::Effect) {
        if effect == plugin::Effect::Reload {
            let notice = self.reload_plugins();
            let (rows, read_failure) = self.plugin_rows();
            if let Some(dialog) = &mut self.plugin_dialog {
                dialog.refresh(rows);
                dialog.set_notice(read_failure.unwrap_or(notice));
            }

            return;
        }

        if self.plugin_task.is_some() {
            // The dialog refuses its own two store-writing actions before
            // they ever get here; this catches the rest — a row's
            // enable/disable/remove chosen while a clone runs.
            if let Some(dialog) = &mut self.plugin_dialog {
                dialog.set_notice(plugin::BUSY);
            }

            return;
        }

        let Some(store) = self.resolve_plugin_store() else {
            if let Some(dialog) = &mut self.plugin_dialog {
                dialog.set_notice(
                    "no config home could be resolved, so there is nowhere to keep plugins",
                );
            }

            return;
        };

        let pending = pending_notice(&effect);
        self.plugin_task = Some(tokio::task::spawn_blocking(move || {
            run_store_effect(&store, effect)
        }));
        if let Some(dialog) = &mut self.plugin_dialog {
            dialog.set_busy(true);
            dialog.set_notice(pending);
        }
    }

    /// Reaps a finished `/plugin` store action and reflects it on the dialog:
    /// the rows re-read from the store, the result — a confirmation, a
    /// refusal, or a failed clone's captured git stderr — on the notice line.
    ///
    /// [`App::poll_wire_models`]'s shape, and for its reason: polled on the
    /// tick, awaited only once the handle reports finished, so the loop never
    /// waits on the clone it started.
    ///
    /// A dialog closed while the action ran simply has nowhere to put the
    /// answer, and that is the whole of the cleanup: the result is polled
    /// state rather than a channel into a closed dialog, and the store's own
    /// stage-validate-move is what guarantees a killed or refused add left
    /// nothing half-written behind.
    async fn poll_plugin_task(&mut self) {
        if !self
            .plugin_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            return;
        }
        let handle = self.plugin_task.take().expect("checked finished above");
        let notice = handle
            .await
            // A panic inside the store task; its message is all there is.
            .unwrap_or_else(|error| format!("the store task failed: {error}"));

        let (rows, read_failure) = self.plugin_rows();
        if let Some(dialog) = &mut self.plugin_dialog {
            dialog.set_busy(false);
            dialog.refresh(rows);
            dialog.set_notice(read_failure.unwrap_or(notice));
        }
        self.dirty = true;
    }

    /// Re-reads the plugin store through a fresh config load and rebuilds
    /// what can rebuild in-session — **D474** (`plugin-reload-honesty`),
    /// declared here, at the action itself.
    ///
    /// The honest split, and why it falls where it does:
    ///
    /// - **Hooks** rebuild: the fresh config's `hooks` table — its own tiers
    ///   plus every enabled plugin's contributions, merged by the same
    ///   `plugin::apply` the startup load ran — replaces the engine's table
    ///   whole, effective at the next fire.
    /// - **Skill roots** rebuild: the base tool registry is recomposed the
    ///   way the startup path composed it — builtins, the webfetch opt-in,
    ///   and a skill tool over `instruction::skill_roots` — and the prompt's
    ///   environment half is recomposed with it, so `<available_skills>`
    ///   and the loadable roots move together.
    /// - **Agents, MCP dials and LSP servers do not**: the roster, the
    ///   dials and the spawns are assembled at startup, and half-reloading
    ///   any of them — an agent list that changed under a running roster, a
    ///   server redialled mid-session — would be a lie about what this
    ///   session is running. The dialog *names* them restart-required
    ///   instead ([`RELOAD_SPLIT`]).
    ///
    /// One caveat, owned rather than hidden: the reload re-reads the config
    /// the way a fresh start would — discovered tiers plus the `GANJA_CONFIG`
    /// environment file — but a `--config` *flag* lives in the process's own
    /// argv, which this frontend was deliberately not handed. A session
    /// launched with that flag reloads without the flagged file's hooks and
    /// skills; the restart the dialog already recommends for the other three
    /// surfaces is the accurate remedy for that edge too.
    fn reload_plugins(&mut self) -> String {
        let config = match ganja_core::config::Config::load(&self.cwd) {
            Ok(config) => config,
            Err(error) => return format!("reload failed: {error}"),
        };

        self.engine
            .replace_hooks(ganja_core::hook::Hooks::new(&config.hooks, &self.root));

        // The startup path's own composition, re-run: `crate::run` builds
        // exactly these three layers before handing the registry over.
        let mut tools = ganja_tool::Registry::with_builtins();
        if config.webfetch_allows_private() {
            tools = tools.with(Arc::new(
                ganja_tool::webfetch::WebfetchTool::allowing_private(),
            ));
        }
        let skill_roots = ganja_core::instruction::skill_roots(&config, &self.cwd);
        tools = tools.with(Arc::new(ganja_tool::skill::SkillTool::over(
            skill_roots.clone(),
        )));
        self.engine.replace_base_tools(Arc::new(tools));
        // Swapped beside the registry so the next turn's `$` invocations read
        // the same list its rebuilt skill tool does.
        self.engine.replace_skill_roots(skill_roots);

        let cwd = self.cwd.clone();
        self.engine.replace_environment(move |model| {
            ganja_core::instruction::suffix(&config, &cwd, model)
        });

        RELOAD_SPLIT.to_owned()
    }

    /// Hands the editor's contents to the engine.
    ///
    /// The prompt reaches the transcript as an engine event rather than being
    /// pushed here, so what the screen shows is exactly what the engine will
    /// send back to the model.
    async fn submit(&mut self) {
        // Checked before anything else, as upstream checks it: the shell
        // branch runs ahead of the slash branch, because in shell mode a `/`
        // starts a path (`component/prompt/index.tsx:1058-1069`).
        if self.editor.mode() == Mode::Shell {
            self.submit_shell().await;
            return;
        }

        let Some(prompt) = self.editor.prompt() else {
            return;
        };

        // Checked before the engine hears about it, as upstream checks it:
        // `exit` on its own is a person leaving, not a question about the word.
        if command::is_bare_exit(&prompt) {
            self.quit = true;
            return;
        }

        // A buffer naming a UI command by itself runs it, exactly as choosing
        // it from the dropdown would have. The menu is not the only door: it
        // closes the moment a space follows the name, and Tab completion
        // leaves `/exit ` behind on purpose (**D446**) — so submit reads the
        // text itself, as Claude Code and Codex both do. Ahead of the steer
        // branch because a UI action is the frontend's, not the model's: the
        // palette already dispatches any of these mid-turn.
        if let Some(entry) = command::submitted(&prompt) {
            self.clear_composer();
            self.run_command(entry.action).await;
            return;
        }

        // `/team`'s own grammar, read here because it is the one UI command
        // that takes arguments: `command::Action` is `Copy` and carries none, so
        // a bare `/team` reaches `run_command` above while `/team spawn w1
        // --backend ganja` reaches this (**D504**, AC-11's own spelling). Both
        // doors end up in the same dialog, which is what keeps the palette and
        // the typed line one thing rather than two.
        if let Some(line) = command::team(&prompt) {
            self.clear_composer();
            // Remembered whatever the line turns out to mean — accepted or
            // refused by the grammar — because the words are out of the
            // composer either way and the history is where an Up-arrow finds
            // them again: a long spawn prompt behind a mistyped flag is
            // edited, not retyped (user directive, 2026-08-20). Unlike a
            // prompt, which is remembered only once the engine took it, there
            // is no engine here to take or refuse it first. A bare `/team`
            // never reaches this: it is the palette's own door above, and is
            // remembered no more than `/help` is.
            self.history.append(history::PromptInfo::text(&prompt));
            self.run_team_line(line).await;
            return;
        }

        // `/rename`'s own grammar, for `/team`'s exact reason: it is the
        // other UI command that carries an argument, so a bare `/rename`
        // reaches `run_command` above while `/rename fresh` reaches this
        // (**D527**).
        if let Some(line) = command::rename(&prompt) {
            self.clear_composer();
            self.history.append(history::PromptInfo::text(&prompt));
            self.run_rename_line(line).await;
            return;
        }

        // A turn already holds the engine, so what was typed is not a prompt:
        // it is a message for the turn that is running (**F4**). See
        // [`App::enqueue`].
        if self.turn_running {
            self.enqueue(prompt).await;
            return;
        }

        match self.start_turn_with(prompt.clone()).await {
            Ok(()) => self.clear_composer(),
            // A turn started between the event that said none was running and
            // this send. Nothing is lost and nothing is refused at the user:
            // the message joins the queue the steer would have joined, and the
            // fallback lane replays it when the engine is idle again.
            Err(EngineError::Busy) => {
                let id = self.mint_steer_id();
                self.queue.push_fallback(id, prompt);
                self.clear_composer();
                self.sync_queue_status();
            }
            // The editor keeps the text, so a refused prompt is never lost.
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }
    }

    /// Hands `prompt` to the engine as a turn of its own and reports what the
    /// engine said about it.
    ///
    /// The tail of [`App::submit`], shared with the fallback lane's replay so
    /// that a queued message reaches the engine through exactly the path a
    /// freshly typed one does — its `@` mentions resolved *now*, when it is
    /// sent, rather than when it was queued.
    ///
    /// The composer is deliberately not touched here. Whether refused text
    /// stays on screen is the caller's decision, and the two callers make it
    /// differently: a person's own keystrokes stay where they can see them,
    /// while a replayed entry goes back to the queue that owns it.
    async fn start_turn_with(&mut self, prompt: String) -> Result<(), EngineError> {
        // The raw buffer is what history remembers — slash and `@` tokens
        // included, upstream stores the literal input — so it is captured
        // here, before `prompt` is moved into the send below, and only
        // committed once the engine accepts it.
        let remembered = history::PromptInfo::text(&prompt);

        // Set before the send rather than after it: a prompt after an undo is
        // the user keeping what the undo did, and the engine deletes those
        // messages *inside* this call — before the event announcing it can be
        // read back. A refusal restores it below, because a refused prompt
        // truncated nothing.
        let previously = std::mem::replace(&mut self.cleared, Cleared::Drop);

        // A buffer naming one of the engine's commands runs it; anything else
        // starting with a slash is text, because a command this build does not
        // have is not one the UI should intercept on the model's behalf.
        let mut degraded: Vec<String> = Vec::new();
        let sent = match self.engine_command(&prompt) {
            Some((name, args)) => self.engine.send(Command::RunCommand { name, args }).await,
            None => {
                // Only the tokens that name a file which is really there
                // (**D113**). `@alice` in a sentence is a person, and
                // attaching her would put an attachment-error block in front
                // of the model instead of what the user wrote.
                let mut mentions = mention::attachable(&prompt, &self.root);
                mentions.extend(self.pasted_images_in(&prompt));
                degraded = self.degraded(&mentions);

                let skills = self.requested_skills(&prompt);
                let session_mentions = self.session_mention_tokens(&prompt, &mentions);
                self.engine
                    .send(Command::SendPrompt {
                        // The `@path`, `[Image #N]`, `$skill` and
                        // `@session`/`@teammate` tokens all stay in the
                        // text: they are what the user wrote, and the
                        // engine reads the files `mentions` names, loads
                        // the skills `skills` names, and resolves
                        // `session_mentions` into a reminder — none of it
                        // sent — when it builds the request (**D529**).
                        text: prompt,
                        mentions,
                        skills,
                        session_mentions,
                        peers: Vec::new(),
                    })
                    .await
            }
        };

        match sent {
            Ok(()) => {
                // Remembered only once the engine took it: a refused prompt
                // stays in the composer, and one it never accepted is not one
                // an Up-arrow should bring back. Consecutive duplicates are
                // suppressed inside `append`, so re-sending a recalled prompt
                // does not fill the history with copies of it.
                self.history.append(remembered);
                self.warn_degraded(&degraded);

                Ok(())
            }
            Err(refusal) => {
                self.cleared = previously;

                Err(refusal)
            }
        }
    }

    /// Takes what was typed while a turn was running: hands it to that turn
    /// where the turn can take it, and holds it for the end of the turn where
    /// it cannot.
    ///
    /// Steering is the primary lane and the queue is the fallback, which is
    /// the design all three surveyed implementations converge on — Codex
    /// injects into the running turn and keeps `queued_user_messages` for what
    /// cannot be injected, and Claude Code does the same split. The refusal
    /// path here is the frontend half of `EngineError::NotStreaming`: the
    /// engine answers a steer that lost its turn with a type rather than a
    /// guess, and the answer decides which lane owns the message.
    async fn enqueue(&mut self, prompt: String) {
        let id = self.mint_steer_id();

        // An engine command never steers. It is not a message the model reads
        // — it acts on the engine between turns — so the fallback lane runs it
        // once this turn ends, which is Claude Code's own split.
        if self.engine_command(&prompt).is_some() {
            self.queue.push_fallback(id, prompt);
            self.clear_composer();
            self.sync_queue_status();
            return;
        }

        let mut mentions = mention::attachable(&prompt, &self.root);
        mentions.extend(self.pasted_images_in(&prompt));
        let degraded = self.degraded(&mentions);
        let skills = self.requested_skills(&prompt);
        let session_mentions = self.session_mention_tokens(&prompt, &mentions);
        let sent = self
            .engine
            .send(Command::Steer {
                id: id.clone(),
                text: prompt.clone(),
                mentions,
                skills,
                session_mentions,
                peers: Vec::new(),
            })
            .await;

        match sent {
            Ok(()) => {
                self.history.append(history::PromptInfo::text(&prompt));
                self.queue.push_steered(id, prompt);
                self.clear_composer();
                self.warn_degraded(&degraded);
            }
            // The turn ended between the event that said it was running and
            // this send. Nothing steers an idle engine, so the fallback lane
            // takes the message and replays it as a prompt.
            Err(EngineError::NotStreaming) => {
                self.queue.push_fallback(id, prompt);
                self.clear_composer();
            }
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }

        self.sync_queue_status();
    }

    /// Sends the oldest message the fallback lane owns, once the engine is
    /// idle again.
    ///
    /// One per idle moment, deliberately: the send it makes starts a turn, and
    /// the next entry waits for that turn to end — which is the FIFO the lane
    /// exists to be. Run after every engine event and on every tick, so the
    /// end of a turn and the retry of a lost race reach it through the same
    /// door.
    async fn replay_queued(&mut self) {
        if self.turn_running || self.revert_pending {
            return;
        }
        let Some(entry) = self.queue.take_next_fallback() else {
            return;
        };

        match self.start_turn_with(entry.text.clone()).await {
            Ok(()) => {}
            // A turn started underneath the replay: the entry keeps its place
            // at the front of the queue and the next tick tries again. The
            // text was never in the composer, so nothing had to survive a
            // refusal for this to be lossless.
            Err(EngineError::Busy) => self.queue.requeue_front(entry),
            // Anything else is an answer that will not change by being asked
            // again — a `/command` this build does not have, a session with
            // nothing to undo — so the entry is dropped rather than retried
            // forever, and the bar says what happened to it.
            Err(refusal) => self.status.set_notice(Some(refusal.to_string())),
        }

        self.sync_queue_status();
        // A tick-driven replay changes the strip and the bar without any of
        // the paths that already mark the frame dirty having run.
        self.dirty = true;
    }

    /// Takes the newest queued message back into the composer for editing.
    ///
    /// Returns whether there was one, so an Up arrow falls through to the
    /// history walk when the queue is empty: the queue sits *in front of* the
    /// history and nothing else about that walk changes.
    ///
    /// A steered entry cannot be un-sent — there is no command that takes a
    /// steer back — so a withdrawal that races the engine's own
    /// `SteerConsumed` leaves the message in the transcript exactly once and
    /// the recalled copy in the composer, where the person decides whether to
    /// send it again. Nothing here resends anything.
    ///
    /// **A teammate's row is never what comes back** ([`Queue::withdraw_newest`]
    /// passes it over): this is the composer, where Enter resolves `@`
    /// mentions, loads `$` skills and runs `/` commands, and words nobody at
    /// this terminal typed may not be put in front of it (§7-5). A strip
    /// holding only peers' messages therefore answers `false` and the Up arrow
    /// walks the history, exactly as an empty one does.
    fn withdraw_queued(&mut self) -> bool {
        let Some(entry) = self.queue.withdraw_newest() else {
            return false;
        };
        self.editor.set_text(&entry.text);
        self.sync_queue_status();

        true
    }

    /// The next correlation id for a queued message.
    ///
    /// Per session and nothing more: no other frontend mints one, and the
    /// engine only ever echoes it back in `Event::SteerConsumed`.
    fn mint_steer_id(&mut self) -> String {
        self.steers = self.steers.saturating_add(1);

        format!("steer-{}", self.steers)
    }

    /// Shows how many messages are waiting, or clears the segment.
    fn sync_queue_status(&mut self) {
        self.status.set_queued(self.queue.depth());
    }

    /// Answers every permission request a yolo session took on itself
    /// (**D479**).
    ///
    /// A loop rather than a single reply because a step that fanned children
    /// out raises several at once, and each is answered by id: what a
    /// non-bypassed session shows one at a time, this retires all of — which
    /// is what stops a queued request from waiting on a dialog that will never
    /// be drawn.
    ///
    /// # Errors
    ///
    /// Propagates an engine that would not take the reply, the same posture
    /// the dialog's own keystroke has: a request nothing answers is a turn
    /// stopped forever, and silence about it would be worse than the error.
    async fn answer_for_the_absent(&mut self) -> Result<()> {
        while let Some(id) = self.auto_permissions.pop_front() {
            self.engine
                .send(Command::ReplyPermission {
                    id,
                    reply: PermissionReply::Once,
                })
                .await?;
        }

        Ok(())
    }

    /// Tells the bar how many dialogs are waiting behind the one on screen.
    fn sync_dialog_status(&mut self) {
        self.status
            .set_queued_dialogs(self.queued_permissions.len());
    }

    /// Tells the bar how many delegated children the running turn has in
    /// flight.
    ///
    /// Counted off the transcript rather than tracked as a number of its own:
    /// the parts already carry the answer — a `task` part is `Running` from the
    /// moment its call starts until its child comes home — and a counter kept
    /// beside them would be a second source of truth to keep in step with
    /// resume, revert and every other path that rewrites the chat.
    fn sync_task_status(&mut self) {
        self.status.set_running_tasks(self.chat.running_tasks());
    }

    /// Empties the composer and the menus that were about what was in it.
    fn clear_composer(&mut self) {
        self.editor.clear();
        self.dropdown = None;
        self.files = None;
        self.skill_menu = None;
        self.cancel_file_walk();
    }

    /// The submit-time half of graceful degradation: a mention the wire cannot
    /// carry is named *before* the turn, and the engine-side text block will
    /// name it again inside.
    fn warn_degraded(&mut self, degraded: &[String]) {
        self.status.set_notice((!degraded.is_empty()).then(|| {
            format!(
                "attached by name only — this provider's wire does not carry: {}",
                degraded.join(", ")
            )
        }));
    }

    /// The `$name` invocations `text` carries, validated against a fresh
    /// discovery of the engine's own skill roots — the same walk the `skill`
    /// tool runs, so what a token invokes is what a call could load, and a
    /// `$word` nothing answers to stays literal.
    fn requested_skills(&self, text: &str) -> Vec<String> {
        let roots = self.engine.skill_roots();

        ganja_tool::skill::requested_in(text, &ganja_tool::skill::discover(&roots))
    }

    /// The `@` tokens `text` carries that name a teammate or a live session
    /// rather than a file (**D529**, AC-22): every mention [`mention::scan`]
    /// finds, in fixed order — resolved to a real file ⇒ already in
    /// `mentions`, and skipped here (the D113 rule, file wins, kept first);
    /// else matching a roster name or a name the last `@` menu listed (the
    /// registry's own fold), or carrying the `uds:` scheme ⇒ collected here,
    /// as typed; anything else is left where [`mention::scan`] found it —
    /// literal text, exactly as a mistyped path is.
    ///
    /// A bare `/path` staying literal is deliberate (**AC-22**): only the
    /// menu's own `uds:` completion, or a hand-typed one, carries the intent
    /// unambiguously enough to route here.
    fn session_mention_tokens(&self, text: &str, mentions: &[Mention]) -> Vec<String> {
        let roster: Vec<String> = self
            .team_roster()
            .map(|view| view.members.into_iter().map(|member| member.name).collect())
            .unwrap_or_default();

        mention::scan(text)
            .into_iter()
            .filter(|token| !mentions.iter().any(|mention| mention.path == token.path))
            .filter(|token| {
                token.path.starts_with(ADDRESS_SCHEME)
                    || roster
                        .iter()
                        .any(|name| registry::same_name(name, &token.path))
                    || self
                        .session_listing
                        .iter()
                        .any(|session| registry::same_name(&session.name, &token.path))
            })
            .map(|token| token.path)
            .collect()
    }

    /// The mentions whose bytes the selected provider will not carry, as
    /// `@path (mime)` labels for the status line.
    ///
    /// The same two questions the engine asks when it builds the request — the
    /// mime `ganja_core::attachment` derives and the wire's
    /// [`Engine::accepts_attachment`] answer — asked here at submit so the
    /// warning lands before the turn rather than inside it.
    fn degraded(&self, mentions: &[Mention]) -> Vec<String> {
        mentions
            .iter()
            .filter_map(|mention| {
                let mime = attachment::mime(&mention.path);
                (attachment::is_binary(mime) && !self.engine.accepts_attachment(mime))
                    .then(|| format!("@{} ({mime})", mention.path))
            })
            .collect()
    }

    /// Runs the buffer in the shell on the user's own behalf.
    ///
    /// Ungated by design (**D13**): this is the person at the terminal typing a
    /// command, not the model asking to run one, and upstream runs it without a
    /// dialog for exactly that reason.
    async fn submit_shell(&mut self) {
        let Some(command) = self.editor.prompt() else {
            return;
        };

        // Captured before the move, and marked as the shell submission it is:
        // upstream tags a shell prompt with `mode: "shell"`, so a recalled
        // shell command reads back as one.
        let remembered = history::PromptInfo {
            input: command.clone(),
            mode: Some(history::Mode::Shell),
            parts: Vec::new(),
        };

        // A shell command after an undo makes it permanent too; see
        // [`App::submit`] for why this is set before the send.
        let previously = std::mem::replace(&mut self.cleared, Cleared::Drop);

        match self.engine.send(Command::RunShell { command }).await {
            Ok(()) => {
                self.history.append(remembered);
                self.editor.clear();
                self.set_shell(false);
                self.status.set_notice(None);
            }
            // Text and mode both kept: a refused command is one to try again,
            // and putting the composer back into prompt mode under it would
            // send it to the model instead.
            Err(refusal) => {
                self.cleared = previously;
                self.status.set_notice(Some(refusal.to_string()));
            }
        }
    }

    /// The engine command `prompt` names, and everything typed after it.
    ///
    /// Nothing is parsed out of the arguments here: the command's own template
    /// decides what `$1` and `$ARGUMENTS` make of them.
    fn engine_command(&self, prompt: &str) -> Option<(String, String)> {
        let rest = prompt.strip_prefix('/')?;
        let (name, args) = rest
            .find(char::is_whitespace)
            .map_or((rest, ""), |index| (&rest[..index], &rest[index..]));

        self.engine_commands
            .iter()
            .any(|command| command.name == name)
            .then(|| (name.to_owned(), args.trim_start().to_owned()))
    }

    fn handle_core(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::MessageStarted {
                session_id: _,
                message,
            } => {
                if message.role == Role::Assistant {
                    self.status.set_activity(Activity::Streaming);
                    // The turn now holds the engine's slot, which is what
                    // makes the next Enter a steer rather than a prompt.
                    self.turn_running = true;
                    // And the transcript's tail says so until it settles
                    // (**D487**). The token figure is read once, here: the
                    // provider reports usage when the turn ends, so nothing
                    // moves it while the turn runs.
                    self.turns = self.turns.saturating_add(1);
                    // One arrival is not a streaming turn's opening: a
                    // compaction's summary comes in already complete, and
                    // while the compacting dress is up it is the gauge's
                    // finish line — the bar snaps full instead of being
                    // replaced by a verb (2026-08-25).
                    let snapped = message.time.completed.is_some() && self.chat.finish_compacting();
                    if !snapped {
                        self.chat.set_working(Some(Working {
                            started: Instant::now(),
                            turn: self.turns,
                            output_tokens: self.totals.output_tokens,
                            compaction: None,
                        }));
                    }
                }
                self.chat.start_message(message);
            }
            CoreEvent::PartStarted {
                session_id: _,
                message_id,
                part,
            } => self.chat.start_part(&message_id, part),
            CoreEvent::PartDelta {
                session_id: _,
                message_id,
                part_id,
                delta,
            } => self.chat.append_delta(&message_id, &part_id, &delta),
            CoreEvent::PartUpdated {
                session_id: _,
                message_id,
                part,
            } => {
                if let PartBody::Tool { tool, state, .. } = &part.body {
                    self.status.set_activity(match state {
                        ToolState::Pending { .. } | ToolState::Running { .. } => {
                            Activity::Tool(tool.clone())
                        }
                        ToolState::Completed { .. } | ToolState::Error { .. } => {
                            Activity::Streaming
                        }
                    });
                    // The bar's `todos` element reads the copy the tool
                    // publishes for frontends; nothing else knows how to
                    // update the list, so it stays whatever the last write
                    // said (**D469**).
                    if tool == "todowrite"
                        && let ToolState::Completed { metadata, .. } = state
                    {
                        self.status.set_todos(todo_progress(metadata));
                    }
                }
                self.chat.update_part(&message_id, part);
                self.sync_task_status();
            }
            CoreEvent::PermissionRequested {
                session_id,
                id,
                call_id,
                tool,
                title,
                args,
                directories,
            } => {
                // A yolo session stands in for the person before any of the
                // rest happens (**D479**): nobody is about to be asked, so
                // there is nothing to announce, nothing to show and nothing to
                // queue. The id goes to the one caller that may await, and the
                // reply is `Once` — never `Always`, which would write a rule
                // into this project's store on the strength of a flag.
                //
                // Only an *Ask* ever reaches here: a denial refuses the call
                // inside the engine and raises no request at all, which is
                // what keeps a config's standing "no" standing.
                if self.yolo {
                    self.auto_permissions.push_back(id);

                    return;
                }
                // A pane teammate forwarding to its lead (D-5, AC-8) shows no
                // dialog either: the question travels to the lead's screen as
                // §5's frame and the answer comes back through the inbox. The
                // bar still says the turn is waiting on somebody, and on whom.
                if self.forwards_asks_to_lead() {
                    self.status
                        .set_notice(Some(format!("asked the lead to allow: {title}")));
                    self.status.set_activity(Activity::Permission);
                    self.member_asks.push(CoreEvent::PermissionRequested {
                        session_id,
                        id,
                        call_id,
                        tool,
                        title,
                        args,
                        directories,
                    });

                    return;
                }
                // A dialog raised is a person needed, shown now or queued
                // behind the one already up (**D462**, **D468**) — see
                // [`App::raise_permission`].
                let summary = format!("approval requested: {title}");
                let asked = Permission::new(id, tool, title, args, directories);
                self.raise_permission(&summary, asked);
            }
            // The engine took a queued message into the running turn, so the
            // strip entry has done its job: what it stood for is about to
            // arrive as the ordinary user message this event precedes. An id
            // nothing answers to is the withdrawal race, and is not an error —
            // see [`App::withdraw_queued`].
            CoreEvent::SteerConsumed { id, .. } => {
                self.queue.consume(&id);
                // And for a peer's message this is the consumption fact
                // [`Delivery::Acknowledged`] was waiting for: the turn has the
                // whole batch this id stood for, so the mailbox may finally let
                // all of it go (**D503**).
                if let Some(batch) = self.peer_steers.remove(&id) {
                    self.settled.extend(batch);
                }
                self.sync_queue_status();
            }
            CoreEvent::PermissionReplied { id, .. } => {
                // A forwarded ask answered by any route — the lead's frame,
                // or a cancel refusing every open dialog — is no longer
                // waiting on the lead (D-5).
                if self
                    .member
                    .as_ref()
                    .is_some_and(|inbox| inbox.asks().retire(&id))
                {
                    self.settle_member_activity();
                }
                let names_open_request = self
                    .permission
                    .as_ref()
                    .is_some_and(|dialog| dialog.permission_id() == Some(&id));
                if names_open_request {
                    // The next question, if this turn's children raised one
                    // while this dialog was up. The activity only goes back to
                    // streaming once nobody is waiting on anybody.
                    self.permission = self.queued_permissions.pop_front();
                    if self.permission.is_none() {
                        self.status.set_activity(Activity::Streaming);
                    }
                } else {
                    // A queued request that was answered without being shown —
                    // a cancel refusing every open dialog is the way that
                    // happens — retires from the queue rather than being asked
                    // about after the fact. A held item can never match: it
                    // has no permission id to answer by (B1).
                    self.queued_permissions
                        .retain(|waiting| waiting.permission_id() != Some(&id));
                }
                self.sync_dialog_status();
            }
            // The one event that moves the transcript backwards. What it does
            // not say — and does not have to — is whether a cleared revert
            // means "show those messages again" or "they are gone": this
            // frontend sent the command that decides it, and remembered which
            // (**R10**).
            CoreEvent::RevertChanged {
                session_id: _,
                revert,
                prompt,
            } => {
                // A code-only rewind announces a revert the engine does not
                // hold: the files moved and nothing was hidden, so there is
                // nothing outstanding for the fallback lane to wait on — and
                // whatever revert *was* standing before it still is, which is
                // why this leaves the flag alone rather than clearing it.
                let code_only = std::mem::take(&mut self.code_only_rewind);
                // While one stands, the fallback lane holds: a replayed prompt
                // would commit the undo the user just made, and doing that on
                // their behalf with a message they queued beforehand is not a
                // decision this frontend gets to take.
                if !code_only {
                    self.revert_pending = revert.is_some();
                }
                match revert {
                    // Nothing to hide, and the files that came back are worth
                    // saying: the transcript is whole, so the marker row the
                    // other reverts draw would be a claim about a hidden range
                    // that does not exist.
                    Some(info) if code_only => self.status.set_notice(Some(restored(&info.files))),
                    Some(info) => self.chat.revert(info.message_id, info.files),
                    None => match self.cleared {
                        Cleared::Unhide => self.chat.unrevert(),
                        Cleared::Drop => self.chat.drop_reverted(),
                    },
                }
                // Upstream repopulates the composer with the message it just
                // took back, so undoing and retyping a prompt is editing it.
                // A resumed session carries no prompt on purpose: reopening a
                // conversation is not the moment to put words in somebody's
                // editor.
                if let Some(prompt) = prompt {
                    // Out of shell mode first: what came back is a prompt, and
                    // dropping it into a composer that runs its contents in a
                    // shell would change what Enter does to it.
                    self.set_shell(false);
                    self.editor.set_text(&prompt);
                }
            }
            CoreEvent::QuestionAsked { id, questions, .. } => {
                // The other dialog that blocks a turn on a person, so the
                // same **D468** moment as a permission request.
                self.announce(
                    NotificationEvent::ApprovalRequested,
                    "a question is waiting for an answer",
                );
                self.question = questions
                    .into_iter()
                    .next()
                    .map(|question| Question::new(id, question));
            }
            CoreEvent::QuestionReplied { id, .. } | CoreEvent::QuestionRejected { id, .. } => {
                let names_open_request = self
                    .question
                    .as_ref()
                    .is_some_and(|question| *question.id() == id);
                if names_open_request {
                    self.question = None;
                    self.status.set_activity(Activity::Streaming);
                }
            }
            CoreEvent::AgentChanged { agent, model, .. } => {
                self.agent = Some(agent);
                self.status.set_agent(self.agent.clone());
                self.model = model;
                self.status.set_model(Some(self.model.clone()));
                // The segment names the model, so a switch that kept the
                // effort re-renders it against the model now active; a
                // switch that cleared it is followed by its own
                // `EffortChanged { effort: None }`.
                self.sync_effort_status();
            }
            CoreEvent::EffortChanged { effort, .. } => {
                self.effort = effort;
                self.sync_effort_status();
            }
            // Taken and drawn nowhere (**D496**): no frontend paints the
            // posture this announces and no test pins one — its place would be
            // beside the agent and the effort in the status bar. The arm
            // exists so the match stays exhaustive.
            CoreEvent::PermissionModeChanged { .. } => {}
            // An inbound peer message was held for review (**D524**). The
            // parity causes raise the approval modal — a deadline is
            // counting down on those, so a person is put in front of it now,
            // through the same one-on-screen queue every dialog rides. An
            // explicit or mode-unknown hold raises nothing: no timer races
            // anybody, and its review surface is the `/held` listing the
            // `N held` segment points at.
            //
            // **No yolo branch, on purpose (B1)**: a bypass-classed session
            // is exactly the one whose every unset-policy inbound holds, so
            // an auto-answer here would convert the gate's holds into
            // accepts wholesale. The hold rides [`PendingDialog::Held`] —
            // a variant the drain that answers [`PermissionId`]s cannot
            // name — and unattended inbound is spelled
            // `cross_session_inbound: "accept"`, never a flag.
            CoreEvent::PeerHeld {
                id,
                from,
                cause,
                summary,
                preview,
                expires_in_ms,
                ..
            } => {
                match cause {
                    HoldCause::ModeMismatch | HoldCause::NoModeAsserted => {
                        let notice = format!("held for review: a message from {from}");
                        let dialog = held::HeldApproval::new(
                            id,
                            from,
                            cause,
                            summary.map(|summary| summary.as_str().to_owned()),
                            preview.as_str().to_owned(),
                            expires_in_ms,
                        );
                        self.raise_dialog(&notice, PendingDialog::Held(dialog));
                    }
                    HoldCause::Explicit { .. } | HoldCause::ModeUnknown => {}
                }
                self.poll_held();
            }
            // A hold ended — by whatever settled it, a person's answer or
            // the deadline's — so the modal it raised retires, shown or
            // still queued, and the listing's next poll drops its row.
            CoreEvent::PeerHoldSettled { id, .. } => {
                let names_open_dialog = self
                    .permission
                    .as_ref()
                    .is_some_and(|dialog| dialog.held_id() == Some(&id));
                if names_open_dialog {
                    self.permission = self.queued_permissions.pop_front();
                    if self.permission.is_none() {
                        self.status.set_activity(if self.turn_running {
                            Activity::Streaming
                        } else {
                            Activity::Ready
                        });
                    }
                } else {
                    // A hold settled while its dialog was still queued — the
                    // deadline, a mode change, or `/held` — retires from the
                    // queue rather than being asked about after the fact.
                    self.queued_permissions
                        .retain(|waiting| waiting.held_id() != Some(&id));
                }
                self.sync_dialog_status();
                self.poll_held();
            }
            // A settlement receipt for a message this session sent (D534).
            // No behavior yet — the frontend notice is W3/L3b's own work —
            // named here only to keep this match exhaustive.
            CoreEvent::PeerReceipt { .. } => {}
            // A compaction reporting how far its summary has streamed (user
            // directive, 2026-08-25): the strip flips to the compacting
            // dress — armed here even before any message opens, which is how
            // the automatic trigger at a turn's start gets to show itself —
            // and the status bar spins, because a summarize request is
            // streaming even though no part is.
            CoreEvent::CompactionProgress { tokens, budget, .. } => {
                self.status.set_activity(Activity::Streaming);
                self.chat.set_compacting(tokens, budget);
            }
            CoreEvent::MessageFinished {
                message_id,
                reason,
                usage,
                error,
                ..
            } => {
                // A pane teammate's lead hears about every turn's end, and
                // how it ended (§10.3-3). Carried rather than written: this
                // arm cannot reach the disk.
                if self.member.is_some() {
                    self.member_finished = Some((reason, error.clone()));
                }
                self.status.set_activity(match reason {
                    FinishReason::Completed => Activity::Ready,
                    FinishReason::Cancelled => Activity::Stopped,
                    FinishReason::Failed => Activity::Failed,
                });
                // The tail that flips the app out of streaming is the turn's
                // one end, which is what makes it **D468**'s turn-complete
                // moment: once per finish event, never per frame.
                self.announce(
                    NotificationEvent::TurnComplete,
                    match reason {
                        FinishReason::Completed => "turn complete",
                        FinishReason::Cancelled => "turn cancelled",
                        FinishReason::Failed => "turn failed",
                    },
                );
                // The slot is free, and every steer this turn did not take is
                // one no turn ever will: a finished turn drains no mailbox, so
                // whatever is still on the strip becomes the fallback lane's
                // to replay. A cancelled turn converges here too — its
                // unconsumed messages were never announced, and this is where
                // they are re-owned.
                self.turn_running = false;
                // Whatever ended it, the tail stops claiming work is under
                // way (**D487**) — with one settle window: a finished
                // compaction's full gauge is held a beat first, so the 100%
                // is seen rather than cleared in the frame it appeared.
                self.chat.settle_working();
                // Peers first, because the two are stranded in opposite
                // directions: a typed message becomes the fallback lane's to
                // replay, and a peer's goes back to the mailbox it was never
                // pruned from (**D503**, and §7-5 for why it may not take the
                // replay lane).
                self.strand_peers();
                self.queue.strand();
                self.sync_queue_status();
                // A finished turn has no children left, however its parts
                // ended: cancelled and failed calls reach a terminal state too,
                // and the count follows the transcript rather than guessing.
                self.sync_task_status();
                if let Some(usage) = usage {
                    self.record(&message_id, &usage);
                }
                if let Some(error) = error {
                    // The transcript is where a person is looking when a turn
                    // dies, so the provider's words land there, under the
                    // reply they cut short; the status bar keeps only its
                    // `failed` activity state. A failure so early that no
                    // reply entry exists falls back to the notice — somewhere
                    // beats nowhere.
                    if !self.chat.set_error(&message_id, error.clone()) {
                        self.status.set_notice(Some(error));
                    }
                }
            }
        }
    }

    /// Adds what a turn spent to the session totals, and keeps its own row
    /// for the Ctrl+T inspector's per-turn token tab (**F2**).
    ///
    /// Tokens accumulate whatever the model is, so a run against the fake
    /// provider still shows counts; dollars only appear once the catalog can
    /// price the model, because a made-up figure is worse than none.
    fn record(&mut self, message_id: &MessageId, usage: &Usage) {
        // The three input counters are disjoint — `Usage` says so, and each
        // provider is what normalizes to it — so what a turn spent on the way
        // in is their sum rather than `input_tokens` alone.
        let input = usage
            .input_tokens
            .saturating_add(usage.cache_read_tokens)
            .saturating_add(usage.cache_write_tokens);

        self.totals.input_tokens = self.totals.input_tokens.saturating_add(input);
        self.totals.output_tokens = self
            .totals
            .output_tokens
            .saturating_add(usage.output_tokens);

        if let Some(model) = catalog::model(&self.model) {
            *self.totals.cost_usd.get_or_insert(0.0) += catalog::cost(usage, &model).total_usd;
        }

        self.status.set_totals(self.totals);

        // Retained whole, reasoning and cache splits included, where the
        // totals above collapse them: the inspector's job is to show what
        // the running totals throw away.
        self.turn_usages.push_back(TurnUsage {
            message_id: message_id.clone(),
            model: self.model.clone(),
            usage: *usage,
        });
        if self.turn_usages.len() > MAX_TURN_USAGE {
            self.turn_usages.pop_front();
        }
    }

    fn needs_draw(&self) -> bool {
        if self.dirty {
            self.urgent || self.last_draw.elapsed() >= FRAME
        } else {
            // The spinner animates on its own while a turn streams, and so
            // does the transcript's working line — which outlives the
            // streaming state, since a turn spends most of itself inside tool
            // calls and its clock has to keep counting there (**D487**).
            self.animating() && self.last_draw.elapsed() >= FRAME
        }
    }

    /// Whether something on screen moves without an event arriving.
    ///
    /// The status bar's spinner and the transcript's working line are the two,
    /// and they are not the same window: the spinner runs while the reply
    /// streams, the working line for the whole turn.
    fn animating(&self) -> bool {
        self.status.is_streaming() || self.turn_running
    }

    /// Whether the loop should wake itself rather than wait for something to
    /// happen.
    ///
    /// Two clocks answer to this, and [`App::until_next_wakeup`] is where they
    /// are told apart: almost everything here wants the **next frame**, and one
    /// arm — a lead with a mailbox — wants the next §6.2 pass a whole second
    /// away.
    fn wants_wakeup(&self) -> bool {
        self.wants_frame()
            // A lead has to keep waking, because the thing it is waiting for is
            // a file another process writes: nothing here would ever hear a
            // teammate's message otherwise, and an idle session is exactly when
            // one is most likely to arrive (**D503**).
            || self.lead_inbox.is_some()
            // And a member has to keep waking for the same file-shaped reason,
            // pointed the other way: its lead writes into an inbox nothing
            // else here would ever read (§10.3-1).
            || self.member.is_some()
    }

    /// Whether something wants the **next frame**, as against the next mailbox
    /// pass.
    ///
    /// The third arm is the MCP dial: nothing else would wake an idle app
    /// while servers connect in the background, so without it a failed server
    /// would sit unreported until the user's next keystroke. The fourth is
    /// the model-listing fetch, for the same reason: the tick is what reaps
    /// it, and without this arm the finished fetch would sit unopened until
    /// an unrelated keypress.
    fn wants_frame(&self) -> bool {
        self.dirty
            || self.animating()
            || self.pending_mcp()
            // The dialog polls status/tool-counts on every tick while it is
            // open, exactly as the status bar's own MCP notice does.
            || self.mcp_dialog.is_some()
            || self.wire_fetch.is_some()
            // The `@` menu's walk resolves on the tick that reaps it, and an
            // idle loop would otherwise leave a finished walk uninstalled
            // until the next keystroke.
            || self.file_walk.is_some()
            // A running store action has no event of its own either, and the
            // dialog is waiting on exactly the tick that reaps it.
            || self.plugin_task.is_some()
            // A spawn in flight is reaped by the tick and by nothing else, and
            // while it runs it may be waiting on a dialog only the tick raises.
            || self.team_spawn.is_some()
            // The last is the fallback lane: a queued message whose replay
            // lost a race has nothing else to wake the loop and try again.
            // Only while the lane could actually act — a turn in flight is
            // woken by its own events, and a paused lane waiting on a revert
            // is waiting on a person, so neither is a reason to keep the loop
            // spinning at frame rate.
            || (self.queue.has_fallback() && !self.turn_running && !self.revert_pending)
            // The `/team` dialog polls the roster and each member's ring on
            // every tick, exactly as the `/mcp` dialog polls its statuses.
            || self.team_dialog.is_some()
            // A teammate that is running may hand this loop a permission
            // dialog down a channel nothing else wakes for, and somebody
            // waiting on a teammate is somebody watching for that dialog. A
            // team with nobody in it hands over nothing, which is what keeps
            // the ordinary case — a session that leads a team and has spawned
            // into none — off this arm entirely (**D503**).
            || self.teammates > 0
            // A shutdown waiting on a turn is bounded by a clock, and the
            // tick is what reads it.
            || self.member_shutdown.is_some()
    }

    /// How long the loop may sleep before it has to do something.
    ///
    /// The frame clock when anything is about to be **drawn**, and the team
    /// clock when the only reason to wake at all is the lead's §6.2 pass.
    /// Sharing one timer was a real cost rather than a tidiness one: a
    /// registry is installed for every session that has a config home, so an
    /// idle terminal woke sixty times a second forever, ran the whole tick
    /// body each time, and found the pass not yet due on fifty-nine of them.
    fn until_next_wakeup(&self) -> Duration {
        if self.wants_frame() {
            return self.until_next_frame();
        }

        self.until_team_poll()
    }

    fn until_next_frame(&self) -> Duration {
        FRAME.saturating_sub(self.last_draw.elapsed())
    }

    /// How long until the mailbox pass is due again — zero when it has never
    /// run, which is a first tick rather than a wait.
    ///
    /// The lead's §6.2 cadence, or the member's §6.1 one on a pane teammate:
    /// the two are never both installed, and the member's is the shorter
    /// because it is the side that has to notice a shutdown promptly.
    fn until_team_poll(&self) -> Duration {
        let (poll, last) = if self.member.is_some() {
            (member::POLL, self.member_polled)
        } else {
            (ganja_core::teammate::lead_inbox::POLL, self.team_polled)
        };

        last.map_or(Duration::ZERO, |last| poll.saturating_sub(last.elapsed()))
    }
}

/// The glob a mention fragment searches with.
///
/// Anchored on whatever directories the fragment already names and matching
/// the rest anywhere below them, which is the shape a path being typed has.
/// `**/` in front lets the named directories sit at any depth, so `src/app`
/// finds `crates/ganja-tui/src/app.rs` without the user spelling the way there.
///
/// Ported behavior stops at the trigger; the matching itself is ganja's own.
/// Upstream scores whole paths through a purpose-built index; this matches file
/// *names* under the directories named, which finds what a person is usually
/// typing and cannot be mistaken for the same ranking (deviation:
/// mention-matches-under-the-path-typed).
/// The character offset of the editor's `(row, column)` cursor in its text,
/// lines rejoined the way [`Editor::text`] joins them.
fn char_offset(text: &str, row: usize, column: usize) -> usize {
    text.split('\n')
        .take(row)
        .map(|line| line.chars().count() + 1)
        .sum::<usize>()
        + column
}

/// The `[Image #N]` token `offset` sits on — inside it, or directly after
/// its closing bracket, which is where the cursor lands the moment a paste
/// inserts one: the char offsets of its `[` and its `]`, and its number.
fn image_token_at(text: &str, offset: usize) -> Option<(usize, usize, u32)> {
    const HEAD: [char; 8] = ['[', 'I', 'm', 'a', 'g', 'e', ' ', '#'];
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0;
    while start + HEAD.len() < chars.len() {
        if chars[start..start + HEAD.len()] != HEAD {
            start += 1;
            continue;
        }
        let digits = start + HEAD.len();
        let mut end = digits;
        while end < chars.len() && chars[end].is_ascii_digit() {
            end += 1;
        }
        if end == digits || chars.get(end) != Some(&']') {
            start += 1;
            continue;
        }
        if (start..=end + 1).contains(&offset) {
            let number = chars[digits..end].iter().collect::<String>().parse().ok()?;

            return Some((start, end, number));
        }
        start = end + 1;
    }

    None
}

/// What one keypress did to a two-step dialog, as [`drive_two_step`] answers
/// it: consumed inside the dialog, an effect for the caller to run, or the
/// dialog's own close.
enum Driven<E> {
    /// The key was consumed inside the dialog.
    Stay,
    /// Enter resolved to an effect the caller has to run.
    Run(E),
    /// Esc on a step where Esc means leave.
    Close,
}

/// One keypress while a two-step dialog owns every key. The free-text step
/// takes the printable characters, the way the question dialog's editor does;
/// everywhere else the keys are the `/mcp` dialog's, Esc closing the dialog
/// except where the input step consumes it as "cancel the edit". Written once
/// over [`crate::component::TwoStep`] so the `/plugin` and `/team` dialogs
/// cannot drift apart key by key.
fn drive_two_step<D: crate::component::TwoStep>(
    dialog: &mut D,
    key: KeyEvent,
) -> Driven<D::Effect> {
    if dialog.is_typing() {
        match key.code {
            KeyCode::Esc => {
                dialog.cancel();
            }
            KeyCode::Backspace => dialog.backspace(),
            KeyCode::Enter => {
                if let Some(effect) = dialog.submit() {
                    return Driven::Run(effect);
                }
            }
            KeyCode::Char(character) if !key.modifiers.intersects(SHORTCUT_MODIFIERS) => {
                dialog.push(character);
            }
            _ => {}
        }

        return Driven::Stay;
    }

    match key.code {
        KeyCode::Esc => {
            if !dialog.cancel() {
                return Driven::Close;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => dialog.move_selection(-1),
        KeyCode::Down | KeyCode::Char('j') => dialog.move_selection(1),
        KeyCode::Enter => {
            if let Some(effect) = dialog.submit() {
                return Driven::Run(effect);
            }
        }
        _ => {}
    }

    Driven::Stay
}

/// One spawn waiting on a person, and where the answer goes back.
///
/// A pair rather than a struct: it never leaves this file, and the two halves
/// are exactly what [`ganja_core::SpawnAsker`] is handed and hands back.
type SpawnQuestion = (
    ganja_core::SpawnAsk,
    tokio::sync::oneshot::Sender<PermissionReply>,
);

/// What a session leading no team answers to every `/team` action.
///
/// One sentence rather than a silence: `/team` on a build with no config home
/// is a person asking about something that genuinely is not there, and a dialog
/// that simply refused to open would look like a broken key.
const NO_TEAM: &str = "this session leads no team \u{b7} there is no config home to keep one in";

/// What `/team shutdown` answers when the team is only the lead.
const NOBODY_TO_STOP: &str = "this team has no teammates to stop";

/// What the status bar says while a pane teammate waits for its turn to end
/// before answering the lead's shutdown request.
const SHUTTING_DOWN: &str =
    "the lead asked this teammate to shut down \u{b7} waiting for the turn to end";

/// A `shutdown_request` waiting on a running turn (§10.3-4).
#[derive(Debug)]
struct MemberShutdown {
    /// What the `shutdown_approved` quotes back.
    request_id: String,
    /// When it arrived, against [`ganja_core::teammate::SETTLE`].
    since: Instant,
    /// Whether the turn has already been told to stop, so it is told once.
    cancelled: bool,
}

/// What one write into a teammate's inbox is reported as, whichever way it
/// went.
///
/// One function for both outcomes so a fan-out and a single ask cannot word
/// the same fact two ways. `to` is only reached for on the failing side: a
/// write that landed reports the **canonical** name the roster resolved, which
/// is not always the spelling that was asked for.
fn said(
    to: &str,
    outcome: Result<&ganja_tool::team::Sent, &ganja_tool::team::Undelivered>,
) -> String {
    match outcome {
        Ok(sent) => format!("{}: {}", sent.to, sent.note),
        Err(undelivered) => format!("{to}: {}", undelivered_reason(undelivered)),
    }
}

/// What a spawn's own dialog is filed under, where a call's dialog names its
/// tool. Not a registry id — no tool raised this — and named for the door it
/// came through so a person reading the dialog knows it is not a call.
const SPAWN_TOOL: &str = "/team spawn";

/// How many spawn dialogs may be waiting to be raised.
///
/// Small, and it can be: only one spawn runs at a time
/// ([`App::team_spawn`] is the guard), so the queue holds the one question that
/// spawn asks plus room for a tick that has not drained it yet.
const SPAWN_ASKS: usize = 4;

/// Puts a `/team spawn`'s own permission dialog in front of the person who
/// typed it (**D-5**, Resolution 4).
///
/// The engine's spawn gate is `async` and asks through this seam, which is what
/// lets the asking side be a *frontend* rather than a turn: the question goes
/// down a channel the event loop drains on its next tick, is drawn as an
/// ordinary permission dialog, and comes back on the oneshot. Nothing here
/// touches the app — an asker that held one could not be moved into the spawn's
/// own task.
#[derive(Debug)]
struct DialogAsker {
    asks: tokio::sync::mpsc::Sender<SpawnQuestion>,
}

/// Written in the shape `async_trait` desugars to, because this crate does not
/// depend on that macro and should not start. Two impls here are spelled out
/// this way — this one and [`crate::component::team`]'s test double — and two
/// boxed futures written out are still smaller than a build dependency added
/// to hide them.
impl ganja_core::SpawnAsker for DialogAsker {
    /// Asks, and treats every way of not being answered as a refusal.
    ///
    /// A full queue, a receiver that went away with the app, a dropped
    /// sender — none of them is a yes, and the trait's own doc says so: a spawn
    /// nobody could be asked about is one nobody approved.
    ///
    /// `try_send` rather than `send`, which is what makes that true of the
    /// full queue as well: awaiting a slot would leave the spawn hanging on a
    /// loop that has stopped draining rather than refusing it, and this seam's
    /// whole contract is that not being answered *is* an answer.
    fn ask<'a, 'b>(
        &'a self,
        request: ganja_core::SpawnAsk,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PermissionReply> + Send + 'b>>
    where
        'a: 'b,
        Self: 'b,
    {
        Box::pin(async move {
            let (reply, answered) = tokio::sync::oneshot::channel();
            if self.asks.try_send((request, reply)).is_err() {
                return PermissionReply::Reject;
            }

            answered.await.unwrap_or(PermissionReply::Reject)
        })
    }
}

/// Why a message did not reach anybody, in one sentence.
///
/// The tool's own refusal constants are `send_message`'s and are written for a
/// model to retry on; this is the same information for a person looking at a
/// dialog, which is why it is spelled here rather than reached for.
fn undelivered_reason(undelivered: &ganja_tool::team::Undelivered) -> String {
    match undelivered {
        ganja_tool::team::Undelivered::Unknown => {
            "nobody on this team answers to that name".to_owned()
        }
        ganja_tool::team::Undelivered::NoTransport { reason }
        | ganja_tool::team::Undelivered::Ambiguous { reason }
        | ganja_tool::team::Undelivered::NameMoved { reason }
        | ganja_tool::team::Undelivered::Failed { reason } => reason.clone(),
    }
}

/// One teammate's message as the wire carries it (**D495**).
///
/// The one place a [`Delivered`] becomes a [`PeerPayload`], so both delivery
/// lanes — a prompt and a steer — put exactly the same four fields on the wire.
/// The colour is passed through as the team file recorded it: whether it is one
/// the roster really assigned was decided where the record was written, and a
/// frontend re-deciding it would be a second opinion about somebody else's
/// roster.
fn payload(message: &Delivered) -> ganja_protocol::team::PeerPayload {
    ganja_protocol::team::PeerPayload::new(
        &message.from,
        message.summary.clone(),
        message.color.clone(),
        &message.body,
    )
}

/// One in-flight `@`-menu walk: the fragment it answers, the token that
/// supersedes it, and the walk itself — the file paths and the live-session
/// listing fetched together (**D529**), so opening `@` costs one spawn
/// rather than two and both are reaped by the one poll.
struct FileWalk {
    fragment: mention::Fragment,
    cancel: CancellationToken,
    task: JoinHandle<(Vec<String>, lister::Listing)>,
}

fn pattern(fragment: &str) -> String {
    // While a range is being typed the fragment carries it (`@src/app#10-20`),
    // and the walk knows nothing about lines: the query is what sits before
    // the last `#`, upstream's `baseQuery` (`autocomplete.tsx:32-44`), taken
    // whether or not the tail parses yet so the list stays up while `#12` is
    // still `#1`.
    let fragment = fragment.rsplit_once('#').map_or(fragment, |(base, _)| base);

    let Some((directory, leaf)) = fragment.rsplit_once('/') else {
        return if fragment.is_empty() {
            "**/*".to_owned()
        } else {
            format!("**/*{fragment}*")
        };
    };

    match (directory.is_empty(), leaf.is_empty()) {
        // A leading slash names no directory, so there is nothing to anchor on.
        (true, true) => "**/*".to_owned(),
        (true, false) => format!("**/*{leaf}*"),
        // A trailing slash: everything under what was named.
        (false, true) => format!("**/{directory}/**"),
        (false, false) => format!("**/{directory}/**/*{leaf}*"),
    }
}

/// The paths `output` names, relative to `cwd` and capped to what a menu can
/// show.
///
/// `glob` answers with absolute paths and, when it capped its own result, with
/// a sentence saying so. Keeping only the lines that are under `cwd` drops that
/// sentence and the "No files found" line without either of them having to be
/// recognized by their text.
fn relative_paths(cwd: &Path, output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| Path::new(line).strip_prefix(cwd).ok())
        .map(|path| path.display().to_string())
        .filter(|path| !path.is_empty())
        .take(MAX_FILES)
        .collect()
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
