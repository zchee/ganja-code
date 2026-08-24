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
    Engine, EngineError, attachment, catalog,
    config::{NotificationEvent, StatuslineConfig},
    provider,
    teammate::{
        Delivery,
        lead_inbox::{Delivered, LeadInbox},
        posture::{Forwarded, Posture},
    },
};
use ganja_protocol::{
    Command, Event as CoreEvent, FinishReason, Mention, Message, MessageId, PartBody, PermissionId,
    PermissionReply, RevertScope, Role, ToolState, Usage,
};
use ganja_tool::{Credentials, FileTimes, ToolCtx, job::Jobs as _};
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
        files::Files,
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
    member, mention, notify,
    theme::{Theme, Themes},
    transcript,
};

/// Shortest gap between frames: roughly 60 FPS.
pub const FRAME: Duration = Duration::from_millis(16);

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
    /// The tool call currently waiting on the user's decision, if any.
    permission: Option<Permission>,
    /// Dialogs that arrived while another was already on screen, in arrival
    /// order (**D462**).
    ///
    /// One at a time is still what a person is shown — two modals over each
    /// other is not a design, and the answer keys are the same three either
    /// way — so a second request queues rather than replacing the first, and
    /// the bar counts what is behind it. Only concurrent children can produce
    /// one: a single call is a turn blocked inside it, which is what made the
    /// engine's own registry a single cell until this wave.
    queued_permissions: VecDeque<Permission>,
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
    /// The inline command menu, while the buffer is a command being typed.
    dropdown: Option<Dropdown>,
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
        let theme = themes.theme();
        let agent = engine.agent();
        let model = engine.model();
        let engine_commands = command::EngineCommand::roster(engine.commands());
        let mut status = Status::new(notice);
        status.set_agent(agent.clone());
        status.set_model(Some(model.clone()));
        // Both halves of the lead side, claimed here because this is the one
        // frontend and both are take-once: the mailbox pass, and the queue a
        // teammate's dialogs cross on. A session leading no team gets neither,
        // which is what makes every test that builds a bare engine cost
        // nothing (**D503**).
        let lead_inbox = engine
            .teammates()
            .map(|team| LeadInbox::new(Arc::clone(team.registry())));
        let teammate_dialogs = engine.teammate_dialogs();
        // Built unconditionally, because a channel nobody sends on costs one
        // allocation and a branch here would be a second thing to keep in step
        // with whether a team was installed.
        let (spawn_asker, spawn_asks) = tokio::sync::mpsc::channel(SPAWN_ASKS);

        Self {
            engine: Arc::new(engine),
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
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: Box<dyn clipboard::Clipboard>) -> Self {
        self.clipboard = clipboard;

        self
    }

    /// Saves a pasted clipboard image under `dir` instead of the real
    /// `<XDG data>/ganja/clipboard` — a test seam, so a paste never reaches a
    /// real person's data directory.
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
        // The socket first, whichever way the loop ended: nobody here will
        // read a peer's message once the loop is over, and the file is what
        // `ganja sessions --live` would otherwise list as dead. Held here for
        // the reason `session_end` is — `run` consumes the app.
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
        let Some(socket) = &mut self.socket else {
            return;
        };
        match socket.sync(&self.engine).await {
            binder::Synced::Unchanged | binder::Synced::Bound(_) => {}
            binder::Synced::Refused(sentence) => {
                self.status.set_notice(Some(sentence));
                self.dirty = true;
            }
        }
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
        let postbox = ganja_core::Postbox::lead(teammates.registry());
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
    async fn deliver_peers(&mut self, messages: Vec<Delivered>) -> bool {
        if messages.is_empty() {
            return true;
        }
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
                if let Some(permission) = &self.permission {
                    permission.render(transcript, buffer, &self.theme);
                }
                if self.inspector.is_none() {
                    self.editor.render(prompt, buffer);
                    self.status.render(status, buffer, &self.theme);
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

        if let Some(permission) = &self.permission {
            // Every other key is swallowed while the modal is open: the
            // editor and the transcript beneath it are not what the user is
            // acting on right now.
            if let Some(reply) = permission_reply(key.code) {
                let id = permission.id().clone();
                // A teammate's dialog is answered on the channel it arrived
                // on, because the turn waiting on it is another engine's
                // (**D-5**). One `if`, and the same keys either way: which
                // conversation raised a question is not something a person
                // answering it should have to know.
                if !self.answer_teammate_dialog(&id, reply) {
                    self.engine
                        .send(Command::ReplyPermission { id, reply })
                        .await?;
                }
            }

            return Ok(());
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
    /// once there is not.
    fn exits(&self, key: KeyEvent) -> bool {
        self.keys.binds(keybind::Action::AppExit, key) && (!edits(key) || self.editor.is_empty())
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
            command::Action::Help => self.help = Some(Help::new(self.keys.clone())),
            command::Action::Exit => self.quit = true,
            command::Action::Copy => self.copy_transcript(),
            command::Action::CopyMessage => self.copy_last_reply(),
            command::Action::Undo => self.undo().await,
            command::Action::Redo => self.redo().await,
            command::Action::Rewind => self.open_rewind(),
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

                if let Some(choice) = choice {
                    self.editor.set_text(&format!("{} ", choice.slash()));
                }

                true
            }
            _ => false,
        }
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
                let chosen = self
                    .files
                    .as_ref()
                    .and_then(Files::selected)
                    .map(str::to_owned);
                if let Some(path) = chosen {
                    self.insert_mention(&path);
                } else {
                    // Nothing matched, so there is nothing to insert; the menu
                    // still goes away rather than swallowing every Enter.
                    self.files = None;
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
        // A space after the mention closes it, so the menu does not reopen on
        // the path that was just chosen — but only when there is not one
        // already, or completing mid-sentence would widen the gap every time
        // (upstream `autocomplete.tsx:172-240` makes the same exception).
        let token = mention::token(path, start, end);
        let mention = match rest.first() {
            Some(next) if next.is_whitespace() => token,
            _ => format!("{token} "),
        };
        let column = head.chars().count() + mention.chars().count();
        *line = format!("{head}{mention}{tail}");

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
        // A space after the token closes it, so the menu does not reopen on
        // the name that was just chosen — the file menu's own exception when
        // one is already there.
        let token = format!("${name}");
        let inserted = match rest.first() {
            Some(next) if next.is_whitespace() => token,
            _ => format!("{token} "),
        };
        let column = head.chars().count() + inserted.chars().count();
        *line = format!("{head}{inserted}{tail}");

        let row = fragment.row;
        self.editor.set_text_at(&lines.join("\n"), row, column);
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
            return;
        }
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
        let Ok(paths) = walk.task.await else {
            return;
        };

        // The editor may have moved on while the project was being walked; a
        // menu for a fragment nobody is typing any more would be a lie.
        let (text, cursor) = (self.editor.text(), self.editor.cursor());
        if self.editor.mode() != Mode::Shell
            && mention::trigger(&text, cursor).is_some_and(|current| current == walk.fragment)
        {
            self.files = Some(Files::new(walk.fragment, paths));
            self.dirty = true;
        }
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

        let task = tokio::spawn(async move {
            // A fragment is typed, not written: half of one is a pattern that
            // does not parse yet, and a menu is not the place to say so.
            match glob
                .run(serde_json::json!({ "pattern": wanted }), &ctx)
                .await
            {
                Ok(found) => relative_paths(&cwd, &found.output),
                Err(_) => Vec::new(),
            }
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
            .engine
            .agents()
            .map(|registry| {
                registry
                    .agents()
                    .iter()
                    .filter(|agent| agent.selectable())
                    .map(|agent| list::Row {
                        value: agent.name.clone(),
                        label: agent.name.clone(),
                        detail: agent.description.clone(),
                        active: self.agent.as_deref() == Some(agent.name.as_str()),
                    })
                    .collect()
            })
            .unwrap_or_default();

        self.chooser = Some((Chooser::Agents, ListDialog::new(" agents ", rows)));
    }

    /// Moves to the next agent a user may switch to, wrapping.
    ///
    /// Wrapping where every list here clamps, because this is not a cursor in
    /// a list: it is one key pressed repeatedly to get somewhere, and stopping
    /// at the end would mean reaching for the mouse.
    async fn cycle_agent(&mut self) {
        let names: Vec<String> = self
            .engine
            .agents()
            .map(|registry| {
                registry
                    .agents()
                    .iter()
                    .filter(|agent| agent.selectable())
                    .map(|agent| agent.name.clone())
                    .collect()
            })
            .unwrap_or_default();
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
                self.engine
                    .send(Command::SendPrompt {
                        // The `@path`, `[Image #N]` and `$skill` tokens stay
                        // in the text: they are what the user wrote, and the
                        // engine reads the files `mentions` names — and loads
                        // the skills `skills` names — when it builds the
                        // request.
                        text: prompt,
                        mentions,
                        skills,
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
        let sent = self
            .engine
            .send(Command::Steer {
                id: id.clone(),
                text: prompt.clone(),
                mentions,
                skills,
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
                    self.chat.set_working(Some(Working {
                        started: Instant::now(),
                        turn: self.turns,
                        output_tokens: self.totals.output_tokens,
                    }));
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
                    .is_some_and(|permission| *permission.id() == id);
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
                    // about after the fact.
                    self.queued_permissions
                        .retain(|waiting| *waiting.id() != id);
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
                // way (**D487**).
                self.chat.set_working(None);
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
/// supersedes it, and the walk itself.
struct FileWalk {
    fragment: mention::Fragment,
    cancel: CancellationToken,
    task: JoinHandle<Vec<String>>,
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
mod tests {
    use std::{
        fs,
        sync::Arc,
        time::{Duration, Instant},
    };

    use futures::{FutureExt as _, StreamExt as _, stream::BoxStream};
    use ganja_core::{
        Engine, SessionId, SessionInfo, Storage,
        provider::{FakeProvider, fake},
        storage::VERSION,
    };
    use ganja_protocol::{
        Event as CoreEvent, FinishReason, Message, Part, PartBody, PartId, PermissionId,
        PermissionReply, QuestionId, QuestionInfo, QuestionOption, ToolState, Usage,
    };
    use ratatui::{
        Terminal,
        backend::{Backend, ClearType, TestBackend},
        crossterm::event::{
            Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
            MouseEvent, MouseEventKind,
        },
        style::{Color, Modifier},
    };
    use tempfile::TempDir;

    use super::{
        App, BACKTRACK_HINT, Chooser, Cleared, Dropdown, ESC_CHORD, FRAME, Help, JoinHandle,
        ListDialog, MAX_EVENT_LOG, MessageId, Mode, NO_EFFORTS, Palette, Permission, RevertScope,
        Rewind, WireListing, permission_reply,
    };

    /// The session every hand-built fixture event happens in. One pinned id,
    /// used consistently, so a test that one day cares which session an event
    /// named has something stable to assert on.
    fn session() -> SessionId {
        SessionId::from("ses_fixture".to_owned())
    }
    use crate::{
        clipboard, command,
        component::{self, effort, mcp, sessions},
        escrepair::EscRepair,
        event::AppEvent,
        history,
        theme::{DEFAULT_THEME, Themes},
    };

    fn engine() -> Engine {
        engine_asking(fake::MODEL)
    }

    /// The same, asking for a model of the caller's choosing — which is what
    /// the pricing tests need, since prices come from a catalog the fake
    /// model is not in.
    fn engine_asking(model: &str) -> Engine {
        Engine::new(
            Arc::new(FakeProvider::default()),
            model,
            Arc::new(ganja_tool::Registry::new(Vec::new())),
            ganja_permission::Permissions::default(),
        )
    }

    /// An app over the builtin themes: no disk is read, so a test never
    /// sees the machine's own theme directory or stored pick.
    fn app() -> App {
        App::new(engine(), None, Themes::builtin())
    }

    /// An app whose engine writes into, and lists from, a store in `directory`.
    ///
    /// The picker asks the engine what it holds, so a picker test needs the
    /// real storage path rather than a stub: what it renders is what
    /// [`Engine::sessions`] read off the disk.
    fn persistent_app(directory: &TempDir) -> App {
        App::new(
            Engine::persistent(
                Arc::new(FakeProvider::default()),
                fake::MODEL,
                Arc::new(ganja_tool::Registry::new(Vec::new())),
                ganja_permission::Permissions::default(),
                Storage::open(directory.path().join("storage")),
            ),
            None,
            Themes::builtin(),
        )
    }

    /// Stores one session under `id`, last touched `ago` milliseconds before
    /// `now`, carrying `tokens` of accumulated input and one message.
    fn store_session(
        directory: &TempDir,
        id: &str,
        title: Option<&str>,
        now: u64,
        ago: u64,
        tokens: u64,
    ) {
        let storage = Storage::open(directory.path().join("storage"));
        let updated = now.saturating_sub(ago);
        let info = SessionInfo {
            effort: None,
            id: SessionId::from(id.to_owned()),
            version: VERSION,
            title: title.map(str::to_owned),
            created: updated,
            updated,
            usage: Usage {
                input_tokens: tokens,
                ..Usage::default()
            },
            context_tokens: 0,
            summary: None,
            agent: None,
            model: None,
            activated_tools: std::collections::BTreeSet::new(),
            parent: None,
            revert: None,
        };
        let message = Message::user("what the picker is choosing between");

        storage.save_info(&info).expect("the info stores");
        storage
            .save_message(&info.id, &message)
            .expect("the envelope stores");
        for part in &message.parts {
            storage
                .save_part(&info.id, &message.id, part)
                .expect("the part stores");
        }
    }

    /// Stores one session under `id` that a task call on `parent` spawned.
    fn store_child(directory: &TempDir, id: &str, parent: &str) {
        let storage = Storage::open(directory.path().join("storage"));
        let info = SessionInfo {
            effort: None,
            id: SessionId::from(id.to_owned()),
            version: VERSION,
            title: Some("find the parser (@explore subagent)".to_owned()),
            created: 1_000,
            updated: 1_000,
            usage: Usage::default(),
            context_tokens: 0,
            summary: None,
            agent: Some("explore".to_owned()),
            model: None,
            activated_tools: std::collections::BTreeSet::new(),
            parent: Some(SessionId::from(parent.to_owned())),
            revert: None,
        };

        storage.save_info(&info).expect("the info stores");
    }

    /// The three sessions the picker snapshots render: one titled and recent,
    /// one that never earned a title, and one old enough to be listed in
    /// hours.
    ///
    /// Ages are written as offsets from the clock the picker itself reads when
    /// it opens, because a row says how long ago a session was touched. Fixed
    /// timestamps would re-age every day and rot the snapshot; an offset
    /// renders the same interval forever.
    fn store_pickable_sessions(directory: &TempDir) {
        const MINUTE: u64 = 60 * 1_000;
        const HOUR: u64 = 60 * MINUTE;

        let now = sessions::now();

        store_session(
            directory,
            "0198f2c4-a1b0-7000-8000-000000000011",
            Some("porting the session store"),
            now,
            30 * 1_000,
            12_400,
        );
        store_session(
            directory,
            "0198f2c4-a1b0-7000-8000-000000000012",
            None,
            now,
            5 * MINUTE,
            1_234,
        );
        store_session(
            directory,
            "0198f2c4-a1b0-7000-8000-000000000013",
            Some("a first look at the tool registry"),
            now,
            3 * HOUR,
            42,
        );
    }

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
    }

    /// An app plus the engine stream its own loop would read, for the tests
    /// that need a prompt to travel the whole way and come back.
    async fn wired() -> (App, BoxStream<'static, CoreEvent>) {
        let engine = engine();
        let events = engine.subscribe().await.expect("the test subscribes first");

        (App::new(engine, None, Themes::builtin()), events)
    }

    /// An app whose real turn is stopped inside the `question` tool, plus the
    /// stream the frontend would be reading while the dialog is open.
    async fn questioning() -> (TempDir, App, BoxStream<'static, CoreEvent>) {
        let directory = temporary();
        let script = directory.path().join("question.json");
        fs::write(
            &script,
            r#"{
                "cadence_ms": 0,
                "turns": [
                    {
                        "tool_calls": [{
                            "name": "question",
                            "args": {
                                "questions": [{
                                    "question": "Which database should the service use?",
                                    "header": "Database",
                                    "options": [
                                        {"label": "Postgres", "description": "Relational database"},
                                        {"label": "SQLite", "description": "One local file"}
                                    ]
                                }]
                            }
                        }]
                    },
                    {"text": "Thanks."}
                ]
            }"#,
        )
        .expect("the fake-provider script writes");

        let engine = Engine::new(
            Arc::new(FakeProvider::new("", Duration::ZERO).with_script(&script)),
            fake::MODEL,
            Arc::new(ganja_tool::Registry::new(vec![Arc::new(
                ganja_tool::question::QuestionTool,
            )])),
            ganja_permission::Permissions::default(),
        );
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, None, Themes::builtin());
        app.engine
            .send(ganja_protocol::Command::SendPrompt {
                text: "ask me".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts the prompt");

        for _ in 0..64 {
            let event = next_event(&mut events).await;
            let asked = matches!(event, CoreEvent::QuestionAsked { .. });
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
            if asked {
                return (directory, app, events);
            }
        }

        panic!("the scripted turn did not ask its question");
    }

    /// The next engine event, bounded so a broken dialog test fails rather
    /// than hanging the whole suite in the condition it is meant to prevent.
    async fn next_event(events: &mut BoxStream<'static, CoreEvent>) -> CoreEvent {
        tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .expect("the engine reports before the dialog timeout")
            .expect("the engine keeps reporting")
    }

    /// Feeds the app the next `count` engine events.
    async fn pump(app: &mut App, events: &mut BoxStream<'static, CoreEvent>, count: usize) {
        for _ in 0..count {
            let event = events.next().await.expect("the engine keeps reporting");
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
        }
    }

    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(width, height)).expect("a test terminal is buildable")
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area();

        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A screen dump carrying style as well as text.
    ///
    /// [`screen`] reads `.symbol()` only, which is what lets every layout
    /// snapshot survive a change of palette — and what makes it useless for
    /// pinning one. This one emits each row as the runs of cells that share a
    /// style, with the colors and modifiers the backend was actually handed.
    fn styled_screen(terminal: &Terminal<TestBackend>) -> String {
        /// One run of same-styled cells.
        fn run(text: &str, (fg, bg, modifier): (Color, Color, Modifier)) -> String {
            let mut described = format!("{text:?} {fg:?} on {bg:?}");
            if !modifier.is_empty() {
                described.push_str(&format!(" {modifier:?}"));
            }

            format!("[{described}]")
        }

        let buffer = terminal.backend().buffer();
        let area = buffer.area();

        (0..area.height)
            .map(|row| {
                let mut runs = Vec::new();
                let mut text = String::new();
                let mut style: Option<(Color, Color, Modifier)> = None;

                for column in 0..area.width {
                    let cell = &buffer[(column, row)];
                    let cell_style = (cell.fg, cell.bg, cell.modifier);

                    if style != Some(cell_style) {
                        if let Some(previous) = style {
                            runs.push(run(&text, previous));
                        }
                        text.clear();
                        style = Some(cell_style);
                    }
                    text.push_str(cell.symbol());
                }
                if let Some(previous) = style {
                    runs.push(run(&text, previous));
                }

                format!("{row:>2} {}", runs.join(" "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// An app drawing in the builtin theme `name`.
    fn themed_app(name: &str) -> App {
        let mut themes = Themes::builtin();
        themes
            .select(name)
            .unwrap_or_else(|| panic!("{name} should be a builtin theme"));

        App::new(engine(), None, themes)
    }

    /// The transcript the per-theme snapshots are taken over: a prompt, a
    /// reply, an edit carrying a diff and a call that was refused — between
    /// them every role the transcript paints.
    fn palette_transcript(app: &mut App) {
        app.chat
            .start_message(Message::user("show me every color you have"));

        let mut reply = Message::assistant("canned");
        reply
            .parts
            .push(Part::text("One edit applied, one command refused."));
        reply.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "edit".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"filePath": "theme.rs"}),
                    output: "edited theme.rs".to_owned(),
                    title: "theme.rs".to_owned(),
                    metadata: serde_json::json!({
                        "diff": "@@ -1,2 +1,2 @@\n-let theme = Theme::default();\n+let theme = themes.theme();\n context"
                    }),
                    started: 0,
                    completed: 1,
                },
            },
        });
        reply.parts.push(Part {
            id: PartId::from("prt_2".to_owned()),
            body: PartBody::Tool {
                call_id: "call_2".to_owned(),
                tool: "bash".to_owned(),
                state: ToolState::Error {
                    input: serde_json::json!({"command": "rm -rf /"}),
                    error: "refused: destructive command".to_owned(),
                    started: 0,
                    completed: 1,
                },
            },
        });
        app.chat.start_message(reply);
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
        AppEvent::Term(TermEvent::Key(KeyEvent::new(code, modifiers)))
    }

    fn typing(text: &str) -> impl Iterator<Item = AppEvent> + use<'_> {
        text.chars()
            .map(|character| key(KeyCode::Char(character), KeyModifiers::NONE))
    }

    /// **D516.** The repaired stream drives the same editor a direct key
    /// would: a split Left arrow lands as cursor movement, never as `[D`
    /// text, and the phantom Esc never reaches the key handler at all.
    #[tokio::test]
    async fn a_split_arrow_repaired_by_the_machine_edits_the_composer() {
        let mut app = app();
        for event in typing("ab") {
            app.handle(event).await.expect("typing is handled");
        }

        let mut machine = EscRepair::active();
        let base = std::time::Instant::now();
        let split = [
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('['), KeyModifiers::NONE),
            // crossterm marks uppercase ASCII with SHIFT, so the final does.
            (KeyCode::Char('D'), KeyModifiers::SHIFT),
        ];
        let mut repaired = Vec::new();
        for (at, (code, modifiers)) in split.into_iter().enumerate() {
            repaired.extend(machine.accept(
                TermEvent::Key(KeyEvent::new(code, modifiers)),
                base + std::time::Duration::from_millis(at as u64),
            ));
        }

        for event in repaired {
            app.handle(AppEvent::Term(event))
                .await
                .expect("the repaired key is handled");
        }
        for event in typing("X") {
            app.handle(event).await.expect("typing is handled");
        }

        assert_eq!(app.editor.text(), "aXb");
    }

    /// The prompt reaches the transcript through the engine, not through the
    /// editor: the frontend never invents an entry.
    #[tokio::test]
    async fn enter_submits_the_prompt_and_the_engine_puts_it_in_the_transcript() {
        let (mut app, mut events) = wired().await;
        for event in typing("hello") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.editor.prompt().is_none(), "the editor should be empty");

        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            !screen(&terminal).contains("hello"),
            "the transcript should wait for the engine:\n{}",
            screen(&terminal)
        );

        pump(&mut app, &mut events, 1).await;

        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("hello"),
            "the prompt should be in the transcript:\n{}",
            screen(&terminal)
        );
    }

    #[tokio::test]
    async fn modified_enter_inserts_a_newline_instead_of_submitting() {
        for modifier in [
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
        ] {
            let mut app = app();
            for event in typing("one") {
                app.handle(event).await.expect("typing is handled");
            }
            app.handle(key(KeyCode::Enter, modifier))
                .await
                .expect("modified enter is handled");
            for event in typing("two") {
                app.handle(event).await.expect("typing is handled");
            }

            assert_eq!(
                app.editor.prompt().as_deref(),
                Some("one\ntwo"),
                "{modifier:?}+Enter should break the line"
            );
        }
    }

    /// Ctrl+J is the universal line break even when a terminal cannot report modified Enter.
    #[tokio::test]
    async fn ctrl_j_inserts_a_newline_and_submits_nothing() {
        let mut app = app();
        for event in typing("one") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl+j is handled");
        for event in typing("two") {
            app.handle(event).await.expect("typing is handled");
        }

        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("one\ntwo"),
            "ctrl+j should break the line, not submit; a submit would have cleared the buffer"
        );
    }

    #[tokio::test]
    async fn a_bare_q_types_instead_of_quitting() {
        let mut app = app();

        app.handle(key(KeyCode::Char('q'), KeyModifiers::NONE))
            .await
            .expect("typing is handled");

        assert!(!app.quit);
        assert_eq!(app.editor.prompt().as_deref(), Some("q"));
    }

    #[tokio::test]
    async fn control_c_and_control_q_quit() {
        for code in [KeyCode::Char('c'), KeyCode::Char('q')] {
            let mut app = app();

            app.handle(key(code, KeyModifiers::CONTROL))
                .await
                .expect("a quit key is handled");

            assert!(app.quit, "{code:?} with Control should quit");
        }
    }

    /// **F4.** What used to be refused is now steered: a second Enter while a
    /// turn holds the engine hands the text to *that* turn and shows it
    /// waiting, rather than bouncing it off the Busy contract — which the
    /// engine still keeps, and which this frontend simply stops asking about.
    #[tokio::test]
    async fn a_second_prompt_mid_turn_is_steered_into_the_running_turn() {
        let (mut app, mut events) = wired().await;
        for event in typing("first") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        // Both message envelopes, so the turn is visibly under way.
        pump(&mut app, &mut events, 2).await;
        assert!(app.turn_running, "the fixture needs a turn in flight");

        for event in typing("second") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(
            app.editor.is_empty(),
            "a steered message leaves the composer, like any accepted one"
        );
        assert_eq!(app.queue.depth(), 1);
        assert!(
            app.queue.entries()[0].is_steered(),
            "the engine took it, so the strip waits on SteerConsumed"
        );

        let mut terminal = terminal(100, 12);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(
            !screen.contains("already streaming"),
            "nothing was refused, so nothing should say so:\n{screen}"
        );
        assert!(screen.contains("second"), "got:\n{screen}");
        assert!(screen.contains("1 queued"), "got:\n{screen}");
    }

    /// Drives the real engine, so this covers the whole path a keystroke takes:
    /// Esc becomes a command, the engine stops the turn, and the finish event
    /// turns the status bar over.
    #[tokio::test]
    async fn escape_stops_a_streaming_turn_inside_the_budget() {
        const CANCEL_BUDGET: Duration = Duration::from_millis(100);

        let (mut app, mut events) = wired().await;

        for event in typing("hello") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        // Both envelopes, the reply's part, and a fragment in it: the turn is
        // actually streaming before it gets interrupted.
        pump(&mut app, &mut events, 4).await;
        assert!(app.status.is_streaming());

        let issued = Instant::now();
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        while app.status.is_streaming() {
            let event = events.next().await.expect("the engine keeps reporting");
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
        }
        let elapsed = issued.elapsed();

        assert!(
            elapsed < CANCEL_BUDGET,
            "the turn took {elapsed:?} to stop, budget is {CANCEL_BUDGET:?}"
        );

        let mut terminal = terminal(80, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("stopped"),
            "the status bar should report the cancel:\n{}",
            screen(&terminal)
        );
    }

    #[tokio::test]
    async fn escape_while_idle_changes_nothing() {
        let mut app = app();

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert!(!app.status.is_streaming());
        assert!(!app.quit);
        assert!(app.editor.prompt().is_none());
    }

    #[tokio::test]
    async fn streamed_fragments_land_in_one_entry() {
        let mut app = app();
        let reply = Message::assistant("canned");
        let part = Part::text("");

        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            session_id: session(),
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::core(CoreEvent::PartStarted {
            session_id: session(),
            message_id: reply.id.clone(),
            part: part.clone(),
        }))
        .await
        .expect("a part start is handled");
        for fragment in ["stream", "ed ", "reply"] {
            app.handle(AppEvent::core(CoreEvent::PartDelta {
                session_id: session(),
                message_id: reply.id.clone(),
                part_id: part.id.clone(),
                delta: fragment.to_owned(),
            }))
            .await
            .expect("a fragment is handled");
        }

        assert!(app.status.is_streaming());

        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("streamed reply"),
            "fragments should join:\n{}",
            screen(&terminal)
        );

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            session_id: session(),
            message_id: reply.id,
            reason: FinishReason::Cancelled,
            usage: None,
            error: None,
            completed: 0,
        }))
        .await
        .expect("a turn end is handled");
        assert!(!app.status.is_streaming());
    }

    /// The provider's dying words land in the transcript, where the person is
    /// looking, and not in the status bar — whose one line keeps only the
    /// failed state. A failure whose reply never started still lands
    /// somewhere: the notice.
    #[tokio::test]
    async fn a_provider_error_lands_in_the_transcript_and_not_the_status_bar() {
        let mut app = app();
        let reply = Message::assistant("canned");
        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            session_id: session(),
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            session_id: session(),
            message_id: reply.id,
            reason: FinishReason::Failed,
            usage: None,
            error: Some("Our servers are currently overloaded.".to_owned()),
            completed: 0,
        }))
        .await
        .expect("a turn end is handled");

        let mut terminal = terminal(120, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("[error] Our servers are currently overloaded."),
            "the transcript carries the provider's words, got:\n{}",
            screen(&terminal)
        );
        assert!(
            !status_line(&mut app).contains("overloaded"),
            "the status bar does not repeat them: {}",
            status_line(&mut app)
        );

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            session_id: session(),
            message_id: MessageId::from("msg_ghost".to_owned()),
            reason: FinishReason::Failed,
            usage: None,
            error: Some("dead before a word".to_owned()),
            completed: 0,
        }))
        .await
        .expect("a turn end is handled");
        assert!(
            status_line(&mut app).contains("dead before a word"),
            "a failure with no reply entry still lands somewhere: {}",
            status_line(&mut app)
        );
    }

    /// A turn the provider refused ends the same way any other does, with the
    /// reason on screen.
    #[tokio::test]
    async fn a_failed_turn_reports_why_in_the_status_bar() {
        let mut app = app();

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            session_id: session(),
            message_id: Message::assistant("canned").id,
            reason: FinishReason::Failed,
            usage: None,
            error: Some("no usable credentials".to_owned()),
            completed: 0,
        }))
        .await
        .expect("a turn end is handled");

        let mut terminal = terminal(80, 12);
        app.draw(&mut terminal).expect("a frame draws");

        assert!(!app.status.is_streaming());
        assert!(
            screen(&terminal).contains("no usable credentials"),
            "the failure should be explained:\n{}",
            screen(&terminal)
        );
    }

    /// Builds the finish event a provider that reported its usage produces.
    fn finished(model: &str, usage: Usage) -> AppEvent {
        AppEvent::core(CoreEvent::MessageFinished {
            session_id: session(),
            message_id: Message::assistant(model).id,
            reason: FinishReason::Completed,
            usage: Some(usage),
            error: None,
            completed: 0,
        })
    }

    /// Spend is per session, not per turn, so two turns add up — and the sum
    /// counts cache traffic, which is billed and is otherwise invisible.
    #[tokio::test]
    async fn usage_accumulates_across_turns_and_reaches_the_status_bar() {
        const MODEL: &str = "claude-sonnet-5";

        let mut app = App::new(engine_asking(MODEL), None, Themes::builtin());
        let usage = Usage {
            input_tokens: 6_000,
            output_tokens: 400,
            reasoning_tokens: 200,
            cache_read_tokens: 6_000,
            cache_write_tokens: 300,
        };

        for _ in 0..2 {
            app.handle(finished(MODEL, usage))
                .await
                .expect("a turn end is handled");
        }

        assert_eq!(
            app.totals.input_tokens, 24_600,
            "6,000 + 6,000 + 300, twice"
        );
        assert_eq!(app.totals.output_tokens, 800);

        let mut terminal = terminal(100, 12);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        assert!(screen.contains("24.6k in"), "got:\n{screen}");
        assert!(screen.contains("800 out"), "got:\n{screen}");
        // Two turns of $0.01795: input 6k at $2, 6k cached at $0.20, 300
        // written at $2.50, output 400 at $10, all per million tokens.
        assert!(screen.contains("$0.0359"), "got:\n{screen}");
    }

    /// **Non-vacuity target for asking the engine which model it is asking.**
    /// The default agent names a model of its own, so the engine has already
    /// left the one the process was launched with by the time the screen is
    /// built. Handing the launch model to the app instead — what the startup
    /// path used to do — prices this turn against a model nothing asked for,
    /// and the fake one has no price at all, so the dollars vanish.
    #[tokio::test]
    async fn a_default_agents_own_model_is_what_the_first_turn_is_priced_against() {
        const MODEL: &str = "claude-sonnet-5";

        let config: ganja_core::config::Config = serde_json::from_value(serde_json::json!({
            "default_agent": "review",
            "agent": { "review": { "mode": "primary", "model": format!("anthropic/{MODEL}") } }
        }))
        .expect("the fixture is a config");
        let engine = engine().with_agents(Arc::new(
            ganja_core::AgentRegistry::from_config(&config).expect("the fixture resolves an agent"),
        ));
        assert_ne!(
            engine.model(),
            fake::MODEL,
            "the fixture only proves anything while the agent moves the engine off the launch model"
        );

        let mut app = App::new(engine, None, Themes::builtin());
        app.handle(finished(
            MODEL,
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 0,
                ..Usage::default()
            },
        ))
        .await
        .expect("a turn end is handled");

        assert_eq!(app.model, MODEL);
        // A million input tokens at $2 per million.
        assert_eq!(app.totals.cost_usd, Some(2.0));
    }

    /// The fake provider reports usage against a model with no price, which is
    /// exactly the case that must show counts and no dollars.
    #[tokio::test]
    async fn an_unpriced_model_reports_its_tokens_and_invents_no_price() {
        let mut app = app();

        app.handle(finished(
            fake::MODEL,
            Usage {
                input_tokens: 40,
                output_tokens: 7,
                ..Usage::default()
            },
        ))
        .await
        .expect("a turn end is handled");

        assert_eq!(app.totals.cost_usd, None);

        let mut terminal = terminal(100, 12);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        assert!(screen.contains("40 in"), "got:\n{screen}");
        assert!(screen.contains("7 out"), "got:\n{screen}");
        assert!(!screen.contains('$'), "got:\n{screen}");
    }

    /// A turn that died part-way through still spent what it spent. Since a
    /// provider failure became a terminal `Failed` event, a finish can carry
    /// both an error and the usage reported before the stream broke, and the
    /// bill has to survive the failure rather than being written off with it.
    #[tokio::test]
    async fn a_failed_turn_still_bills_for_what_it_spent() {
        const MODEL: &str = "claude-sonnet-5";

        let mut app = App::new(engine_asking(MODEL), None, Themes::builtin());

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            session_id: session(),
            message_id: Message::assistant(MODEL).id,
            reason: FinishReason::Failed,
            usage: Some(Usage {
                input_tokens: 2_000,
                output_tokens: 150,
                ..Usage::default()
            }),
            error: Some("the provider answered 500: overloaded".to_owned()),
            completed: 0,
        }))
        .await
        .expect("a failed turn is handled");

        assert_eq!(app.totals.input_tokens, 2_000);
        assert_eq!(app.totals.output_tokens, 150);

        let mut terminal = terminal(120, 12);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        assert!(screen.contains("failed"), "got:\n{screen}");
        assert!(screen.contains("2.0k in"), "got:\n{screen}");
        // 2,000 in at $2 plus 150 out at $10, per million tokens.
        assert!(screen.contains("$0.0055"), "got:\n{screen}");
        assert!(
            screen.contains("overloaded"),
            "the reason must survive beside the bill:\n{screen}"
        );
    }

    /// A turn that ends without usage — a cancel, or a provider that reports
    /// none — leaves the totals alone rather than resetting them.
    #[tokio::test]
    async fn a_turn_without_usage_does_not_disturb_the_totals() {
        const MODEL: &str = "claude-sonnet-5";

        let mut app = App::new(engine_asking(MODEL), None, Themes::builtin());
        app.handle(finished(
            MODEL,
            Usage {
                input_tokens: 1_000,
                output_tokens: 100,
                ..Usage::default()
            },
        ))
        .await
        .expect("a turn end is handled");

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            session_id: session(),
            message_id: Message::assistant(MODEL).id,
            reason: FinishReason::Cancelled,
            usage: None,
            error: None,
            completed: 0,
        }))
        .await
        .expect("a cancel is handled");

        assert_eq!(app.totals.input_tokens, 1_000);
        assert_eq!(app.totals.output_tokens, 100);
    }

    /// The wire's reasoning and cache splits survive into the Ctrl+T
    /// inspector's own per-turn row (**F2**), where the running totals above
    /// collapse them into two numbers.
    #[tokio::test]
    async fn message_finished_retains_a_turn_usage_row_with_its_full_split() {
        const MODEL: &str = "claude-sonnet-5";

        let mut app = App::new(engine_asking(MODEL), None, Themes::builtin());
        let reply_id = Message::assistant(MODEL).id;
        let usage = Usage {
            input_tokens: 3,
            output_tokens: 4,
            reasoning_tokens: 5,
            cache_read_tokens: 6,
            cache_write_tokens: 7,
        };

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            session_id: session(),
            message_id: reply_id.clone(),
            reason: FinishReason::Completed,
            usage: Some(usage),
            error: None,
            completed: 0,
        }))
        .await
        .expect("a turn end is handled");

        let row = app.turn_usages.back().expect("a row should have been kept");
        assert_eq!(row.message_id, reply_id);
        assert_eq!(row.model, MODEL);
        assert_eq!(row.usage, usage);
    }

    /// The complement of [`a_turn_without_usage_does_not_disturb_the_totals`]:
    /// a turn that reported nothing has nothing to add a row about either.
    #[tokio::test]
    async fn a_turn_without_usage_adds_no_turn_usage_row() {
        let mut app = app();

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            session_id: session(),
            message_id: Message::assistant(fake::MODEL).id,
            reason: FinishReason::Cancelled,
            usage: None,
            error: None,
            completed: 0,
        }))
        .await
        .expect("a cancel is handled");

        assert!(
            app.turn_usages.is_empty(),
            "a turn that reported no usage should add no row"
        );
    }

    /// The raw-log ring buffer is capped rather than growing without bound
    /// (**F2**) — a synchronous unit test on `App::tee_event` directly,
    /// rather than routing thousands of events through `handle`, which would
    /// pay for a `replay_queued` call this behavior has nothing to do with.
    #[test]
    fn the_raw_log_caps_rather_than_growing_without_bound() {
        let mut app = app();
        let event = CoreEvent::AgentChanged {
            session_id: session(),
            agent: "build".to_owned(),
            model: fake::MODEL.to_owned(),
        };

        for _ in 0..(MAX_EVENT_LOG + 10) {
            app.tee_event(&event);
        }

        assert_eq!(app.event_log.len(), MAX_EVENT_LOG);
    }

    #[tokio::test]
    async fn the_wheel_and_the_page_keys_move_the_viewport() {
        let mut app = app();
        for index in 0..60 {
            app.chat
                .start_message(Message::user(format!("entry {index}")));
        }
        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");

        app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })))
        .await
        .expect("a wheel event is handled");
        assert!(!app.chat.is_following_tail());

        app.handle(key(KeyCode::End, KeyModifiers::NONE))
            .await
            .expect("End is handled");
        assert!(app.chat.is_following_tail());

        app.handle(key(KeyCode::PageUp, KeyModifiers::NONE))
            .await
            .expect("PageUp is handled");
        assert!(!app.chat.is_following_tail());

        app.handle(key(KeyCode::PageDown, KeyModifiers::NONE))
            .await
            .expect("PageDown is handled");
        assert!(app.chat.is_following_tail());
    }

    #[tokio::test]
    async fn unrelated_events_do_not_disturb_the_editor() {
        let mut app = app();
        for event in typing("draft") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })))
        .await
        .expect("a click is handled");
        app.handle(AppEvent::Term(TermEvent::Key(KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ))))
        .await
        .expect("a key release is handled");
        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert_eq!(app.editor.prompt().as_deref(), Some("draft"));
    }

    #[tokio::test]
    async fn engine_bursts_are_coalesced_but_keystrokes_are_not() {
        let mut app = app();
        let reply = Message::assistant("canned");
        let part = Part::text("");
        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            session_id: session(),
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::core(CoreEvent::PartStarted {
            session_id: session(),
            message_id: reply.id.clone(),
            part: part.clone(),
        }))
        .await
        .expect("a part start is handled");

        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");

        app.handle(AppEvent::core(CoreEvent::PartDelta {
            session_id: session(),
            message_id: reply.id,
            part_id: part.id,
            delta: "burst".to_owned(),
        }))
        .await
        .expect("a fragment is handled");
        assert!(
            !app.needs_draw(),
            "a fragment arriving inside the frame budget should wait"
        );
        assert!(app.wants_wakeup(), "the pending frame must be woken up for");
        assert!(app.until_next_frame() <= FRAME);

        app.handle(key(KeyCode::Char('a'), KeyModifiers::NONE))
            .await
            .expect("typing is handled");
        assert!(app.needs_draw(), "a keystroke should redraw immediately");
    }

    #[test]
    fn a_resize_storm_rewraps_without_panicking() {
        let mut app = app();
        for index in 0..40 {
            app.chat.start_message(Message::user(format!(
                "entry {index} carries enough words to wrap at every width tried here"
            )));
        }

        let mut terminal = terminal(80, 24);
        for width in [80_u16, 12, 200, 1, 61, 3, 120, 40] {
            terminal.backend_mut().resize(width, 24);
            app.draw(&mut terminal)
                .expect("a frame draws after a resize");

            assert!(
                app.chat
                    .cached_widths()
                    .iter()
                    .all(|cached| *cached == Some(width)),
                "the wrap cache should be invalidated by a resize to {width}"
            );
        }

        terminal.backend_mut().resize(1, 1);
        app.draw(&mut terminal)
            .expect("a frame draws into a one-cell terminal");
    }

    #[test]
    #[ignore = "timing-sensitive: run at phase gates with `cargo test -- --ignored`"]
    fn a_five_thousand_line_transcript_draws_inside_the_frame_budget() {
        const ENTRIES: usize = 250;
        const LINES_PER_ENTRY: usize = 20;
        const FRAMES: usize = 200;

        let mut app = app();
        for entry in 0..ENTRIES {
            let body = (0..LINES_PER_ENTRY)
                .map(|line| format!("entry {entry} line {line} of plain streamed transcript text"))
                .collect::<Vec<_>>()
                .join("\n");
            app.chat.start_message(Message::user(body));
        }

        let mut terminal = terminal(120, 40);
        // The first frame wraps everything; the budget is about the rest.
        app.draw(&mut terminal).expect("the warm-up frame draws");
        assert!(
            app.chat.line_count() >= 5_000,
            "the transcript should be at least 5,000 lines, got {}",
            app.chat.line_count()
        );

        let mut samples = Vec::with_capacity(FRAMES);
        for frame in 0..FRAMES {
            app.chat
                .scroll_lines(if frame.is_multiple_of(2) { -7 } else { 5 });

            let started = Instant::now();
            app.draw(&mut terminal).expect("a frame draws");
            samples.push(started.elapsed());
        }

        samples.sort_unstable();
        let p50 = samples[FRAMES / 2];
        let p95 = samples[FRAMES * 95 / 100];
        let worst = samples.last().copied().unwrap_or_default();

        // The gate runner reads these numbers off the test output.
        println!(
            "{} lines, {FRAMES} frames: p50 {p50:?}, p95 {p95:?}, worst {worst:?}",
            app.chat.line_count()
        );

        assert!(
            p95 < FRAME,
            "p95 frame time was {p95:?} (worst {worst:?}), budget is {FRAME:?}"
        );
    }

    /// A stored conversation of `count` messages with the shape a real one
    /// has: prompts of a few lines, replies of a paragraph or two, and every
    /// fifth reply carrying a finished tool call between its text parts.
    ///
    /// Deliberately not made of one-word messages. The cost the frontend pays
    /// on a resume is wrapping every line of every entry, so a transcript of
    /// stubs would measure nothing.
    fn stored_transcript(count: usize) -> Vec<Message> {
        (0..count)
            .map(|index| {
                if index.is_multiple_of(2) {
                    return Message::user(format!(
                        "turn {index}: each part of a message is written to its own file, so a\n\
                         streaming text part can be rewritten as it grows without rewriting the\n\
                         whole envelope. Walk me through what a resume has to rebuild from that."
                    ));
                }

                let mut reply = Message::assistant(fake::MODEL);
                reply.parts.push(Part::text(format!(
                    "turn {index}: a resume reads the info file first, because it names the\n\
                     summary the live window starts from. Everything from that message onward\n\
                     is read back envelope by envelope, and each envelope's parts are read from\n\
                     the part directory keyed by the message id.\n\
                     \n\
                     Assistant messages that carry no content stay in the transcript but are\n\
                     left out of the request window, since some providers reject an empty\n\
                     message; they render as interrupted rather than as complete."
                )));

                if index.is_multiple_of(5) {
                    reply.parts.push(Part {
                        id: PartId::from(format!("prt_{index}")),
                        body: PartBody::Tool {
                            call_id: format!("call_{index}"),
                            tool: "read".to_owned(),
                            state: ToolState::Completed {
                                input: serde_json::json!({"filePath": "crates/ganja-core/src/storage.rs"}),
                                output: "read 412 lines".to_owned(),
                                title: "storage.rs".to_owned(),
                                metadata: serde_json::json!({}),
                                started: 0,
                                completed: 1,
                            },
                        },
                    });
                    reply.parts.push(Part::text(
                        "The tool call above is closed as an error on resume when the previous\n\
                         process died before it finished, because the next request has to answer\n\
                         every call the model opened.",
                    ));
                }

                reply.complete();
                reply
            })
            .collect()
    }

    /// P4 acceptance: a 200-message session reaches its first frame inside
    /// 150ms.
    ///
    /// The measured window is [`App::seed`] plus the first [`App::draw`], over
    /// messages already in memory. Reading them off the disk is the engine's
    /// cost and is timed where it happens; what the frontend owes is turning a
    /// resumed transcript into a screen. The first frame is the expensive one
    /// because it wraps every entry — later frames reuse the cache.
    #[test]
    fn a_two_hundred_message_session_reaches_its_first_frame_inside_the_budget() {
        const BUDGET: Duration = Duration::from_millis(150);
        const MESSAGES: usize = 200;

        // Built outside the window: composing fixtures is not what a resume
        // pays for.
        let transcript = stored_transcript(MESSAGES);
        let mut app = app();
        let mut terminal = terminal(100, 30);

        let started = Instant::now();
        app.seed(transcript);
        app.draw(&mut terminal).expect("the first frame draws");
        let elapsed = started.elapsed();

        // The gate runner reads this number off the test log rather than
        // trusting a green assertion to mean it was measured.
        eprintln!(
            "{MESSAGES} messages, {} wrapped lines: first frame in {elapsed:?} (budget {BUDGET:?})",
            app.chat.line_count()
        );

        assert!(
            app.chat.line_count() >= 1_000,
            "a transcript this budget is worth measuring should be at least 1,000 lines, got {}",
            app.chat.line_count()
        );
        assert!(
            elapsed < BUDGET,
            "the first frame took {elapsed:?}, budget is {BUDGET:?}"
        );
    }

    /// One reply of the frame-time fixture, **markdown-heavy by construction**.
    ///
    /// Every construct the renderer has a code path for is in here on purpose:
    /// a heading, a paragraph long enough to wrap at 120 columns, a fenced
    /// **Rust** block (so syntect actually parses and the nine `syntax*` slots
    /// actually resolve), a grid table, a nested list carrying inline emphasis
    /// and code, and a blockquote. A plain-text transcript would measure the
    /// wrap and call it the renderer (R16(4)).
    fn markdown_reply(index: usize) -> String {
        format!(
            "## Section {index}: what the step decided\n\
             \n\
             The engine hands the frontend an ordered event stream and nothing else, so a \
             transcript that applied every event holds exactly what the next request will \
             carry — which is the property a remote client will lean on, and the reason \
             every message type is serde-derived from the first day rather than the day \
             a socket appears.\n\
             \n\
             ```rust\n\
             /// Counts the bytes that matter in `input`.\n\
             fn weigh_{index}(input: &str) -> Result<usize, Error> {{\n\
             \x20   let total = input.chars().filter(|c| !c.is_whitespace()).count();\n\
             \x20   if total == 0 {{\n\
             \x20       return Err(Error::Empty);\n\
             \x20   }}\n\
             \x20   Ok(total * {index})\n\
             }}\n\
             ```\n\
             \n\
             | field | kind | what it carries |\n\
             | --- | --- | --- |\n\
             | id | PartId | the part this body was written under |\n\
             | body | PartBody | text, a tool call, a file, or a step marker |\n\
             \n\
             - the outer claim for step {index}\n\
             \x20 - a nested one with **emphasis** and `inline_code()`\n\
             \x20 - and another, so the marker column is exercised twice\n\
             - a second outer claim, long enough that it has to wrap at a hundred and \
             twenty columns and hang under its own marker\n\
             \n\
             > What the step above settled, said once more so the quote path runs.\n"
        )
    }

    /// The transcript the frame-time accept measures.
    fn markdown_transcript(replies: usize) -> Vec<Message> {
        (0..replies)
            .flat_map(|index| {
                let mut reply = Message::assistant(fake::MODEL);
                reply.parts.push(Part::text(markdown_reply(index)));
                reply.complete();

                [
                    Message::user(format!("walk me through step {index}")),
                    reply,
                ]
            })
            .collect()
    }

    /// P6 acceptance (R16(4)): a 10,000-line markdown transcript scrolls at
    /// 30 FPS or better on a 120×40 screen.
    ///
    /// `#[ignore]`d because it is a stopwatch, not a claim about behavior, and
    /// a loaded CI box would make it flap. Run it with
    /// `cargo nextest run -p ganja-tui --run-ignored all -E 'test(scrolls_a_ten_thousand_line)'`
    /// and read the numbers off the log rather than off the green tick.
    ///
    /// What is measured is a frame: the transcript's cached-wrap walk, the
    /// viewport blit and the backend's diff, over a transcript whose assistant
    /// text is markdown (see [`markdown_reply`]). The first frame is called out
    /// separately because it is the one that parses every block and runs
    /// syntect over every fence; the ones after it are what scrolling costs.
    #[test]
    #[ignore = "a timing measurement, not a behavior"]
    fn scrolls_a_ten_thousand_line_markdown_transcript_at_thirty_frames_a_second() {
        const BUDGET: Duration = Duration::from_millis(33);
        const LINES: usize = 10_000;
        const REPLIES: usize = 320;
        const STEPS: usize = 200;

        let transcript = markdown_transcript(REPLIES);
        let mut app = app();
        let mut terminal = terminal(120, 40);
        let mut frames = Vec::with_capacity(STEPS + 1);

        app.seed(transcript);
        let started = Instant::now();
        app.draw(&mut terminal).expect("the first frame draws");
        let first = started.elapsed();
        frames.push(first);

        let lines = app.chat.line_count();
        assert!(
            lines >= LINES,
            "the accept is over {LINES} lines, and this fixture wrapped to {lines}"
        );

        // Top to bottom in even steps, so the walk crosses every entry rather
        // than re-drawing one screenful that is already warm.
        app.chat.scroll_to_top();
        let step = isize::try_from(lines / STEPS).unwrap_or(1).max(1);
        for _ in 0..STEPS {
            app.chat.scroll_lines(step);
            let started = Instant::now();
            app.draw(&mut terminal).expect("a frame draws");
            frames.push(started.elapsed());
        }

        frames.sort_unstable();
        let at = |percent: usize| frames[(frames.len() - 1) * percent / 100];
        let (p50, p95, worst) = (at(50), at(95), at(100));

        eprintln!(
            "{lines} markdown lines over {REPLIES} replies at 120x40, \
             {frames} frames: first {first:?}, p50 {p50:?}, p95 {p95:?}, max {worst:?} \
             (budget p95 {BUDGET:?})",
            frames = frames.len()
        );

        assert!(
            p95 <= BUDGET,
            "p95 frame time was {p95:?}, budget is {BUDGET:?}"
        );
    }

    #[test]
    fn a_fresh_app_wants_its_first_frame() {
        let app = app();

        assert!(app.needs_draw());
        assert!(app.until_next_frame() <= FRAME);
    }

    fn permission_event(id: &str) -> CoreEvent {
        CoreEvent::PermissionRequested {
            session_id: session(),
            id: PermissionId::from(id.to_owned()),
            call_id: "call_1".to_owned(),
            tool: "shell".to_owned(),
            title: "cargo test".to_owned(),
            args: serde_json::json!({"command": "cargo test"}),
            directories: Vec::new(),
        }
    }

    /// The wiring's own mapping (**D469**): only `completed` counts as done,
    /// and the in-progress entry's title is what the element shows working.
    #[test]
    fn the_bar_counts_only_completed_todos_as_done() {
        let progress = super::todo_progress(&serde_json::json!({"todos": [
            {"content": "landed", "status": "completed", "priority": "high"},
            {"content": "underway", "status": "in_progress", "priority": "medium"},
            {"content": "waiting", "status": "pending", "priority": "low"},
            {"content": "dropped", "status": "cancelled", "priority": "low"},
        ]}))
        .expect("a well-formed list maps");
        assert_eq!(progress.done, 1);
        assert_eq!(progress.total, 4);
        assert_eq!(progress.current.as_deref(), Some("underway"));

        assert!(
            super::todo_progress(&serde_json::json!({})).is_none(),
            "metadata without a list clears the element rather than inventing one"
        );
    }

    /// The lead wiring for the roster's `todos` element (**D469**): a
    /// finished `todowrite` fills it from the metadata the tool published.
    #[tokio::test]
    async fn a_finished_todowrite_fills_the_bars_todo_element() {
        let statusline: ganja_core::config::StatuslineConfig =
            serde_json::from_value(serde_json::json!({"elements": ["todos"]}))
                .expect("the fixture is a statusline table");
        let mut app = app().with_statusline(Some(&statusline));

        app.handle(AppEvent::core(CoreEvent::PartUpdated {
            session_id: session(),
            message_id: MessageId::from("msg_1".to_owned()),
            part: Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "todowrite".to_owned(),
                    state: ToolState::Completed {
                        input: serde_json::json!({}),
                        output: "[]".to_owned(),
                        title: "1 todos".to_owned(),
                        metadata: serde_json::json!({"todos": [
                            {"content": "landed", "status": "completed", "priority": "high"},
                            {"content": "underway", "status": "in_progress", "priority": "medium"},
                        ]}),
                        started: 0,
                        completed: 1,
                    },
                },
            },
        }))
        .await
        .expect("the event applies");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("todos:1/2"),
            "got:\n{}",
            screen(&terminal)
        );
    }

    #[tokio::test]
    async fn a_tool_call_moves_through_its_lifecycle_on_screen() {
        let mut app = app();
        let reply = Message::assistant("canned");
        let part = Part::tool("call_1", "shell");

        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            session_id: session(),
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::core(CoreEvent::PartStarted {
            session_id: session(),
            message_id: reply.id.clone(),
            part: part.clone(),
        }))
        .await
        .expect("a part start is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("\u{25cf} Shell"),
            "got:\n{}",
            screen(&terminal)
        );

        app.handle(AppEvent::core(CoreEvent::PartUpdated {
            session_id: session(),
            message_id: reply.id.clone(),
            part: Part {
                id: part.id.clone(),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "shell".to_owned(),
                    state: ToolState::Running {
                        input: serde_json::json!({"command": "cargo test"}),
                        metadata: serde_json::Value::Null,
                        started: 0,
                    },
                },
            },
        }))
        .await
        .expect("a running update is handled");
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("cargo test"),
            "got:\n{}",
            screen(&terminal)
        );

        app.handle(AppEvent::core(CoreEvent::PartUpdated {
            session_id: session(),
            message_id: reply.id.clone(),
            part: Part {
                id: part.id.clone(),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "shell".to_owned(),
                    state: ToolState::Completed {
                        input: serde_json::json!({"command": "cargo test"}),
                        output: "ok".to_owned(),
                        title: "cargo test".to_owned(),
                        metadata: serde_json::json!({}),
                        started: 0,
                        completed: 1,
                    },
                },
            },
        }))
        .await
        .expect("a completed update is handled");
        app.draw(&mut terminal).expect("a frame draws");
        let screen_text = screen(&terminal);
        assert!(
            screen_text.contains("\u{25cf} Shell(command: \"cargo test\")"),
            "got:\n{screen_text}"
        );
        assert!(
            screen_text.contains("\u{23bf} cargo test"),
            "got:\n{screen_text}"
        );
        assert!(screen_text.contains("ok"), "got:\n{screen_text}");
    }

    /// **AC4.** The tail says a turn is under way for exactly as long as the
    /// engine holds one, and the loop keeps waking itself while it does — the
    /// elapsed figure is a clock, and a clock nobody redraws is a wrong one.
    #[tokio::test]
    async fn the_working_line_lives_exactly_as_long_as_the_turn_does() {
        let mut app = app();
        let reply = Message::assistant("canned");

        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            session_id: session(),
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("\u{273b} "),
            "got:\n{}",
            screen(&terminal)
        );
        assert!(app.animating(), "the loop has a reason to wake itself");

        app.handle(AppEvent::core(CoreEvent::MessageFinished {
            session_id: session(),
            message_id: reply.id.clone(),
            reason: FinishReason::Completed,
            usage: None,
            error: None,
            completed: 0,
        }))
        .await
        .expect("a finish is handled");
        app.draw(&mut terminal).expect("a frame draws");

        assert!(
            !screen(&terminal).contains("\u{273b} "),
            "a settled turn leaves no working line:\n{}",
            screen(&terminal)
        );
        assert!(
            !app.animating(),
            "and nothing left on screen moves on its own"
        );
    }

    #[tokio::test]
    async fn a_part_updated_for_an_unseen_id_is_appended_not_dropped() {
        let mut app = app();
        let reply = Message::assistant("canned");
        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            session_id: session(),
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");

        // No PartStarted for this id: a frontend that joined mid-stream still
        // has to converge on the same transcript.
        app.handle(AppEvent::core(CoreEvent::PartUpdated {
            session_id: session(),
            message_id: reply.id.clone(),
            part: Part {
                id: PartId::from("prt_orphan".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "read".to_owned(),
                    state: ToolState::Running {
                        input: serde_json::json!({"filePath": "a.rs"}),
                        metadata: serde_json::Value::Null,
                        started: 0,
                    },
                },
            },
        }))
        .await
        .expect("an update for an unseen id is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("a.rs"),
            "an update for an id the transcript never saw start should still append, got:\n{}",
            screen(&terminal)
        );
    }

    #[test]
    fn permission_keys_map_to_the_right_reply() {
        assert_eq!(
            permission_reply(KeyCode::Char('y')),
            Some(PermissionReply::Once)
        );
        assert_eq!(
            permission_reply(KeyCode::Char('a')),
            Some(PermissionReply::Always)
        );
        assert_eq!(
            permission_reply(KeyCode::Char('n')),
            Some(PermissionReply::Reject)
        );
        assert_eq!(
            permission_reply(KeyCode::Esc),
            Some(PermissionReply::Reject)
        );
        assert_eq!(permission_reply(KeyCode::Char('x')), None);
    }

    #[tokio::test]
    async fn keys_while_the_dialog_is_open_are_sent_as_replies_not_typed() {
        let mut app = app();
        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");

        app.handle(key(KeyCode::Char('y'), KeyModifiers::NONE))
            .await
            .expect("y is handled");

        assert!(
            app.permission.is_some(),
            "the dialog waits for PermissionReplied before closing"
        );
        assert!(
            app.editor.prompt().is_none(),
            "the keystroke must not reach the editor"
        );
    }

    #[tokio::test]
    async fn a_permission_request_opens_the_dialog() {
        let mut app = app();
        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");

        assert!(app.permission.is_some());

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("shell"), "got:\n{screen}");
        assert!(screen.contains("cargo test"), "got:\n{screen}");
    }

    #[tokio::test]
    async fn a_matching_reply_closes_the_dialog_but_a_stray_one_does_not() {
        let mut app = app();
        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");
        assert!(app.permission.is_some());

        app.handle(AppEvent::core(CoreEvent::PermissionReplied {
            session_id: session(),
            id: PermissionId::from("perm_other".to_owned()),
            reply: PermissionReply::Reject,
        }))
        .await
        .expect("a stray reply is handled");
        assert!(
            app.permission.is_some(),
            "a reply naming a different request must not close this dialog"
        );

        app.handle(AppEvent::core(CoreEvent::PermissionReplied {
            session_id: session(),
            id: PermissionId::from("perm_1".to_owned()),
            reply: PermissionReply::Once,
        }))
        .await
        .expect("the matching reply is handled");
        assert!(app.permission.is_none());
    }

    /// Two children asking at once are two dialogs, shown one at a time
    /// (**D462**).
    ///
    /// The engine holds both open and routes each reply by id, so the frontend
    /// may not drop the first when the second arrives — nor stack them, which
    /// is not a design. It queues, says how many are behind, and asks the next
    /// one the moment this one is answered.
    #[tokio::test]
    async fn a_second_request_queues_behind_the_open_dialog_and_is_asked_next() {
        let mut app = app();
        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("the first request is handled");
        app.handle(AppEvent::core(permission_event("perm_2")))
            .await
            .expect("the second request is handled");

        assert_eq!(
            app.permission.as_ref().map(|open| open.id().as_str()),
            Some("perm_1"),
            "the dialog on screen is still the one that was asked first"
        );
        assert_eq!(app.queued_permissions.len(), 1);

        let mut terminal = terminal(100, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(
            screen.contains("1 dialog queued"),
            "the bar says how many are behind it:\n{screen}"
        );

        app.handle(AppEvent::core(CoreEvent::PermissionReplied {
            session_id: session(),
            id: PermissionId::from("perm_1".to_owned()),
            reply: PermissionReply::Once,
        }))
        .await
        .expect("the first reply is handled");

        assert_eq!(
            app.permission.as_ref().map(|open| open.id().as_str()),
            Some("perm_2"),
            "answering one asks the next rather than leaving the queue stranded"
        );
        assert!(app.queued_permissions.is_empty());

        app.handle(AppEvent::core(CoreEvent::PermissionReplied {
            session_id: session(),
            id: PermissionId::from("perm_2".to_owned()),
            reply: PermissionReply::Once,
        }))
        .await
        .expect("the second reply is handled");
        assert!(app.permission.is_none(), "and then nobody is being asked");
    }

    /// A cancel answers every open request, including the ones nobody was
    /// shown. Those retire from the queue rather than being put in front of
    /// somebody after the turn they belonged to has ended.
    #[tokio::test]
    async fn a_reply_to_a_queued_request_retires_it_without_ever_showing_it() {
        let mut app = app();
        for id in ["perm_1", "perm_2"] {
            app.handle(AppEvent::core(permission_event(id)))
                .await
                .expect("a request is handled");
        }

        app.handle(AppEvent::core(CoreEvent::PermissionReplied {
            session_id: session(),
            id: PermissionId::from("perm_2".to_owned()),
            reply: PermissionReply::Reject,
        }))
        .await
        .expect("the queued request's own refusal is handled");

        assert!(
            app.queued_permissions.is_empty(),
            "the refused request left the queue"
        );
        assert_eq!(
            app.permission.as_ref().map(|open| open.id().as_str()),
            Some("perm_1"),
            "and the one on screen is untouched by it"
        );
    }

    /// What the scripted shell call echoes, so "the tool ran" is a question
    /// about the tool's own output rather than about the screen.
    const ECHOED: &str = "yolo-ran-zarquon";

    /// The scripted turn after the tool call. Its arrival is what says every
    /// call before it was resolved without anything still waiting on a person.
    const CLOSING: &str = "script-finished-zarquon";

    /// An app whose scripted turn makes one `bash` call — a tool the builtin
    /// defaults ask about (`ganja_permission::permission::ASK_BY_DEFAULT`) —
    /// and then says [`CLOSING`].
    ///
    /// A real engine and a real tool rather than hand-built events, because
    /// what **D479** is about is a turn that runs to its end with nobody
    /// answering anything: a fixture that only replays a `PermissionRequested`
    /// could not tell an answered request from a wedged one.
    async fn shelling(
        yolo: bool,
        permissions: ganja_permission::Permissions,
    ) -> (TempDir, App, BoxStream<'static, CoreEvent>) {
        let directory = temporary();
        let script = directory.path().join("shell.json");
        fs::write(
            &script,
            format!(
                r#"{{
                    "cadence_ms": 0,
                    "turns": [
                        {{"tool_calls": [{{
                            "name": "bash",
                            "args": {{"command": "echo {ECHOED}"}}
                        }}]}},
                        {{"text": "{CLOSING}"}}
                    ]
                }}"#
            ),
        )
        .expect("the fake-provider script writes");

        let engine = Engine::new(
            Arc::new(FakeProvider::new("", Duration::ZERO).with_script(&script)),
            fake::MODEL,
            Arc::new(ganja_tool::Registry::new(vec![Arc::new(
                ganja_tool::shell::ShellTool::new(),
            )])),
            permissions,
        );
        let events = engine.subscribe().await.expect("the test subscribes first");
        let app = App::new(engine, None, Themes::builtin()).with_yolo(yolo);
        app.engine
            .send(ganja_protocol::Command::SendPrompt {
                text: "run it".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts the prompt");

        (directory, app, events)
    }

    /// Feeds the app every event of a two-turn script and hands back what it
    /// was fed, asserting on the way that no dialog was ever on screen.
    ///
    /// The assertion is *inside* the loop on purpose: a dialog that opened and
    /// closed again between two events would be invisible to a check made
    /// after the turn, and a dialog nobody can see is exactly the failure a
    /// bypass has to be checked for.
    async fn pump_turn(
        app: &mut App,
        events: &mut BoxStream<'static, CoreEvent>,
    ) -> Vec<CoreEvent> {
        let mut seen = Vec::new();

        for _ in 0..128 {
            let event = next_event(events).await;
            let finished = matches!(event, CoreEvent::MessageFinished { .. });
            seen.push(event.clone());
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
            assert!(
                app.permission.is_none() && app.queued_permissions.is_empty(),
                "no dialog may be raised in a bypassed session"
            );
            // One assistant message carries the whole turn — the step that
            // made the call and the step that said the closing word — so its
            // close is the end of everything this script had to play.
            if finished {
                return seen;
            }
        }

        panic!("the scripted turn never ran to its end: {seen:#?}");
    }

    /// What a completed `bash` part carries, or [`None`] while nothing has
    /// completed one.
    fn completed_shell(events: &[CoreEvent]) -> Option<String> {
        events.iter().find_map(|event| match event {
            CoreEvent::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    tool,
                    state: ToolState::Completed { output, .. },
                    ..
                } if tool == "bash" => Some(output.clone()),
                _ => None,
            },
            _ => None,
        })
    }

    /// The lead behavior of **D479**: the dialog the rules raised is answered
    /// for the person who asked not to be asked, and the call runs.
    #[tokio::test]
    async fn a_yolo_session_answers_a_raised_dialog_once_and_never_draws_it() {
        let (_directory, mut app, mut events) =
            shelling(true, ganja_permission::Permissions::default()).await;

        let seen = pump_turn(&mut app, &mut events).await;

        let asked = seen
            .iter()
            .filter(|event| matches!(event, CoreEvent::PermissionRequested { .. }))
            .count();
        assert_eq!(asked, 1, "the rules still raised the request: {seen:#?}");

        let replies: Vec<_> = seen
            .iter()
            .filter_map(|event| match event {
                CoreEvent::PermissionReplied { reply, .. } => Some(*reply),
                _ => None,
            })
            .collect();
        assert_eq!(
            replies,
            vec![PermissionReply::Once],
            "answered once, and never `Always` — nothing may be written to \
             this project's rules on the strength of a flag"
        );

        assert!(
            completed_shell(&seen).is_some_and(|output| output.contains(ECHOED)),
            "the call the dialog was about actually ran: {seen:#?}"
        );
    }

    /// The pre-mortem's first scenario, pinned: a bypass answers the dialogs
    /// the rules *raise*, and a denial raises none.
    #[tokio::test]
    async fn a_denied_call_stays_denied_in_a_yolo_session() {
        let mut permissions = ganja_permission::Permissions::default();
        permissions.set_baseline(vec![ganja_permission::permission::Rule {
            permission: "bash".to_owned(),
            pattern: "*".to_owned(),
            action: ganja_permission::Action::Deny,
        }]);
        let (_directory, mut app, mut events) = shelling(true, permissions).await;

        let seen = pump_turn(&mut app, &mut events).await;

        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, CoreEvent::PermissionRequested { .. })),
            "a denial is decided inside the engine and asks nobody: {seen:#?}"
        );
        assert!(
            completed_shell(&seen).is_none(),
            "the denied command never ran: {seen:#?}"
        );
        // And the turn carried on regardless: a refusal is information the
        // model reads, never a turn this frontend ended.
        assert!(
            seen.iter().any(|event| matches!(
                event,
                CoreEvent::PartUpdated { part, .. }
                    if matches!(&part.body, PartBody::Tool { state: ToolState::Error { .. }, .. })
            )),
            "the refusal reached the model as an error result: {seen:#?}"
        );
    }

    /// The other half of the bypass's boundary: what a *person* is asked is
    /// not what a *rule* asks. `question` — and the two plan doors, which ride
    /// the same seam (`ganja_tool::plan`, `ctx.ask`) — arrive as
    /// [`CoreEvent::QuestionAsked`] and keep their dialog.
    #[tokio::test]
    async fn a_yolo_session_still_raises_its_questions() {
        let mut app = app().with_yolo(true);

        app.handle(AppEvent::core(CoreEvent::QuestionAsked {
            session_id: session(),
            id: QuestionId::from("qst_1".to_owned()),
            questions: vec![QuestionInfo {
                question: "Which database should the service use?".to_owned(),
                header: "Database".to_owned(),
                options: vec![QuestionOption {
                    label: "Postgres".to_owned(),
                    description: "Relational database".to_owned(),
                }],
                multiple: None,
                custom: None,
            }],
            source: None,
        }))
        .await
        .expect("the question is handled");

        assert!(
            app.question.is_some(),
            "a bypass covers permission, never conversation"
        );
    }

    #[tokio::test]
    async fn a_question_dialog_offers_the_options_and_enter_replies_the_selected_label() {
        let (_directory, mut app, mut events) = questioning().await;

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        for expected in [
            "Database",
            "Which database should the service use?",
            "Postgres",
            "Relational database",
            "SQLite",
            "One local file",
        ] {
            assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
        }

        app.handle(key(KeyCode::Down, KeyModifiers::NONE))
            .await
            .expect("down is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert!(
            app.question.is_some(),
            "the dialog waits for QuestionReplied before closing"
        );

        let mut answered = None;
        for _ in 0..64 {
            let event = next_event(&mut events).await;
            if let CoreEvent::QuestionReplied { answers, .. } = &event {
                answered = Some(answers.clone());
            }
            let finished = matches!(event, CoreEvent::MessageFinished { .. });
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
            if finished {
                break;
            }
        }

        assert_eq!(answered, Some(vec![vec!["SQLite".to_owned()]]));
        assert!(app.question.is_none());
    }

    #[tokio::test]
    async fn esc_rejects_the_question_and_the_turn_reads_it_as_dismissal() {
        let (_directory, mut app, mut events) = questioning().await;

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(
            app.question.is_some(),
            "the dialog waits for QuestionRejected before closing"
        );

        let mut rejected = false;
        let mut dismissed = false;
        for _ in 0..64 {
            let event = next_event(&mut events).await;
            match &event {
                CoreEvent::QuestionRejected { .. } => rejected = true,
                CoreEvent::PartUpdated { part, .. } => {
                    if let PartBody::Tool { tool, state, .. } = &part.body
                        && tool == "question"
                        && matches!(state, ToolState::Error { error, .. } if error == ganja_tool::question::DISMISSED)
                    {
                        dismissed = true;
                    }
                }
                _ => {}
            }
            let finished = matches!(event, CoreEvent::MessageFinished { .. });
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
            if finished {
                break;
            }
        }

        assert!(rejected, "the engine should announce the dismissal");
        assert!(
            dismissed,
            "the question tool should read its dismissal text"
        );
        assert!(app.question.is_none());
    }

    #[tokio::test]
    async fn control_c_still_quits_while_the_dialog_is_open() {
        let mut app = app();
        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");

        app.handle(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .expect("control-c is handled");

        assert!(app.quit);
    }

    /// Drives a real turn to a streaming state, opens the dialog by hand
    /// (nothing in this build yet gates a real tool call on one), presses
    /// Esc, and proves the turn was never cancelled: it runs to a natural
    /// `Completed` finish rather than the `Cancelled` the sibling escape test
    /// gets when Esc is allowed to reach `CancelTurn`.
    #[tokio::test]
    async fn escape_with_the_dialog_open_does_not_cancel_the_turn() {
        let engine = Engine::new(
            Arc::new(FakeProvider::new("one two three", Duration::from_millis(2))),
            fake::MODEL,
            Arc::new(ganja_tool::Registry::new(Vec::new())),
            ganja_permission::Permissions::default(),
        );
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, None, Themes::builtin());

        for event in typing("hello") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        pump(&mut app, &mut events, 4).await;
        assert!(app.status.is_streaming());

        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");
        assert!(app.permission.is_some());

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(
            app.permission.is_some(),
            "Esc should reject the dialog, not close it before PermissionReplied"
        );

        let mut finished = None;
        while finished.is_none() {
            let event = events.next().await.expect("the engine keeps reporting");
            if let CoreEvent::MessageFinished { reason, .. } = &event {
                finished = Some(*reason);
            }
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
        }

        assert_eq!(
            finished,
            Some(FinishReason::Completed),
            "Esc must not have cancelled the turn"
        );
        assert!(
            app.permission.is_some(),
            "the dialog should not self-close on an unrelated event"
        );
    }

    /// **AC3.** A reply that thought before it answered, as the pane draws it:
    /// the thinking behind its own marker and dimmed into italics, the answer
    /// behind the bullet every reply block has.
    #[test]
    fn snapshot_thinking() {
        let mut app = app();
        app.chat.start_message(Message::user("say hello"));
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part::reasoning_text(
            "The user wants a greeting and nothing more, so a short one is \
             the whole of the job here.",
        ));
        reply.parts.push(Part::text("Hello, world!"));
        app.chat.start_message(reply);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// The read row the user pinned by screenshot: the path and the range it
    /// asked for on the header, a count as the whole of the result, and none
    /// of the envelope the tool writes for the model.
    #[test]
    fn snapshot_read_row() {
        let mut app = app();
        let mut message = Message::assistant("canned");
        message.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "read".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({
                        "filePath": "/repo/crates/ganja-tui/src/component/chat.rs",
                        "offset": 1158,
                        "limit": 60,
                    }),
                    output: "<path>/repo/crates/ganja-tui/src/component/chat.rs</path>\n\
                             <type>file</type>\n<content>\n1158: fn wrap() {}\n</content>"
                        .to_owned(),
                    title: "crates/ganja-tui/src/component/chat.rs".to_owned(),
                    metadata: serde_json::json!({
                        "display": {
                            "type": "file",
                            "path": "/repo/crates/ganja-tui/src/component/chat.rs",
                            "lineStart": 1158,
                            "lineEnd": 1217,
                            "totalLines": 2500,
                        },
                    }),
                    started: 0,
                    completed: 1,
                },
            },
        });
        app.chat.start_message(message);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn snapshot_tool_pending() {
        let mut app = app();
        let mut message = Message::assistant("canned");
        message.parts.push(Part::tool("call_1", "shell"));
        app.chat.start_message(message);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn snapshot_tool_running() {
        let mut app = app();
        let mut message = Message::assistant("canned");
        message.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                state: ToolState::Running {
                    input: serde_json::json!({"command": "cargo test"}),
                    metadata: serde_json::Value::Null,
                    started: 0,
                },
            },
        });
        app.chat.start_message(message);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn snapshot_tool_completed_with_a_diff() {
        let mut app = app();
        let mut message = Message::assistant("canned");
        message.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "edit".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"filePath": "a.rs"}),
                    output: "edited a.rs".to_owned(),
                    title: "a.rs".to_owned(),
                    metadata: serde_json::json!({
                        "diff": "@@ -1,2 +1,2 @@\n-old line\n+new line\n context line"
                    }),
                    started: 0,
                    completed: 1,
                },
            },
        });
        app.chat.start_message(message);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn snapshot_tool_error() {
        let mut app = app();
        let mut message = Message::assistant("canned");
        message.parts.push(Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                state: ToolState::Error {
                    input: serde_json::json!({"command": "rm -rf /"}),
                    error: "refused: destructive command\nsee policy for details".to_owned(),
                    started: 0,
                    completed: 1,
                },
            },
        });
        app.chat.start_message(message);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn snapshot_permission_dialog_open() {
        let mut app = app();
        app.permission = Some(Permission::new(
            PermissionId::from("perm_1".to_owned()),
            "shell".to_owned(),
            "cargo test".to_owned(),
            serde_json::json!({"command": "cargo test"}),
            Vec::new(),
        ));

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// The modal is bounded, so a command can be longer than it can draw. What
    /// the user must never be handed is a call that simply stops mid-word with
    /// `y` sitting under it: here the tail that pipes a download into a shell
    /// is off the bottom, and the dialog says so rather than letting the
    /// visible half read as the whole thing.
    #[test]
    fn snapshot_permission_dialog_with_a_call_too_long_to_fit() {
        let command = format!(
            "cargo test --workspace --all-features --no-fail-fast -- --nocapture {}; \
             curl -fsSL http://ganja.example/install.sh | sh -",
            "--skip live --skip golden --skip pty --skip slow --skip flaky ".repeat(12),
        );
        let mut app = app();
        app.permission = Some(Permission::new(
            PermissionId::from("perm_1".to_owned()),
            "shell".to_owned(),
            command.clone(),
            serde_json::json!({ "command": command }),
            Vec::new(),
        ));

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// Opens the picker the way a user does — Ctrl-S, through `App::handle` —
    /// so what is snapshotted is the dialog over the list the engine actually
    /// read back, not a `Sessions` assembled by the test.
    #[tokio::test]
    async fn snapshot_sessions_picker_open() {
        let directory = temporary();
        store_pickable_sessions(&directory);
        let mut app = persistent_app(&directory);

        app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .expect("control-s is handled");

        assert!(
            app.sessions.is_some(),
            "the picker must be open, or the snapshot is of a bare screen"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// Live preview is what the dialog is for: the theme under the cursor is
    /// applied as the cursor reaches it, before anything is confirmed.
    #[tokio::test]
    async fn moving_the_cursor_in_the_theme_picker_applies_what_it_lands_on() {
        let mut app = app();

        // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
        // own default chord is gone, so these fixtures open it the way
        // `snapshot_help_dialog_open` opens the reference card.
        app.run_command(command::Action::Themes).await;

        assert!(app.theme_list.is_some(), "the picker should be open");
        assert_eq!(
            app.theme.name(),
            DEFAULT_THEME,
            "opening previews nothing: the cursor starts where the user is"
        );

        app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .await
            .expect("j is handled");

        assert_eq!(
            app.theme.name(),
            "terminal",
            "the row below opencode should already be applied"
        );
        assert_eq!(app.themes.active(), "terminal");
        assert!(app.theme_list.is_some(), "moving must not close the picker");
    }

    /// The other half of preview: browsing has to cost nothing.
    #[tokio::test]
    async fn cancelling_the_theme_picker_puts_back_the_theme_it_opened_on() {
        let mut app = app();
        // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
        // own default chord is gone, so these fixtures open it the way
        // `snapshot_help_dialog_open` opens the reference card.
        app.run_command(command::Action::Themes).await;
        app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .await
            .expect("j is handled");
        assert_ne!(app.theme.name(), DEFAULT_THEME, "a preview must have run");

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert!(app.theme_list.is_none(), "escape closes the picker");
        assert_eq!(app.theme.name(), DEFAULT_THEME);
        assert_eq!(app.themes.active(), DEFAULT_THEME);
    }

    #[tokio::test]
    async fn keeping_a_theme_closes_the_picker_and_leaves_it_applied() {
        let mut app = app();
        // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
        // own default chord is gone, so these fixtures open it the way
        // `snapshot_help_dialog_open` opens the reference card.
        app.run_command(command::Action::Themes).await;
        app.handle(key(KeyCode::Char('k'), KeyModifiers::NONE))
            .await
            .expect("k is handled");
        let previewed = app.theme.name().to_owned();

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.theme_list.is_none());
        assert_eq!(app.theme.name(), previewed);
    }

    /// The picker owns every key while it is open, exactly as the sessions one
    /// does — otherwise `j` would be typed into the prompt behind it.
    #[tokio::test]
    async fn keys_while_the_theme_picker_is_open_do_not_reach_the_editor() {
        let mut app = app();
        // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
        // own default chord is gone, so these fixtures open it the way
        // `snapshot_help_dialog_open` opens the reference card.
        app.run_command(command::Action::Themes).await;

        for event in typing("jkx") {
            app.handle(event).await.expect("typing is handled");
        }

        assert!(app.editor.prompt().is_none());
        assert!(app.theme_list.is_some());
    }

    /// The wheel belongs to whatever modal is up, like it does for the other
    /// two: scrolling the transcript out from under a dialog is never what the
    /// notch meant.
    #[tokio::test]
    async fn the_wheel_does_not_reach_the_transcript_while_the_theme_picker_is_open() {
        let mut app = app();
        for index in 0..60 {
            app.chat
                .start_message(Message::user(format!("entry {index}")));
        }
        app.draw(&mut terminal(40, 12)).expect("a frame draws");

        // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
        // own default chord is gone, so these fixtures open it the way
        // `snapshot_help_dialog_open` opens the reference card.
        app.run_command(command::Action::Themes).await;
        app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })))
        .await
        .expect("a wheel event is handled");

        assert!(
            app.chat.is_following_tail(),
            "the wheel should be swallowed"
        );
    }

    /// An app whose prompt history already holds `entries`, submitted in the
    /// order given — the last one named is the newest.
    fn app_with_history(directory: &TempDir, entries: &[&str]) -> App {
        let mut history =
            history::History::load_from(directory.path().join("prompt-history.jsonl"));
        for entry in entries {
            history.append(history::PromptInfo::text(*entry));
        }

        app().with_history(history)
    }

    /// Ctrl+R opens the search over whatever the store holds.
    #[tokio::test]
    async fn control_r_opens_the_history_search() {
        let directory = temporary();
        let mut app = app_with_history(&directory, &["first prompt", "second prompt"]);

        app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .expect("control-r is handled");

        assert!(app.history_search.is_some(), "the search should be open");
    }

    /// Fuzzy narrowing survives the whole way from a keystroke down to what
    /// is under the cursor.
    #[tokio::test]
    async fn typing_into_the_history_search_narrows_to_a_fuzzy_match() {
        let directory = temporary();
        let mut app = app_with_history(&directory, &["commit the fix", "git status"]);

        app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .expect("control-r is handled");
        for event in typing("ommi") {
            app.handle(event).await.expect("typing narrows the query");
        }

        assert_eq!(
            app.history_search
                .as_ref()
                .and_then(component::search::HistorySearch::selected)
                .map(|prompt| prompt.input.as_str()),
            Some("commit the fix")
        );
    }

    /// Enter fills the composer with the entry under the cursor and closes
    /// the search — an Enter here is a fill, never a submit, so no engine
    /// event should ever arrive.
    #[tokio::test]
    async fn enter_in_history_search_fills_the_composer_and_sends_nothing() {
        let (mut app, mut events) = wired().await;
        let directory = temporary();
        let mut history =
            history::History::load_from(directory.path().join("prompt-history.jsonl"));
        history.append(history::PromptInfo::text("what does this crate do"));
        app.history = history;

        app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .expect("control-r is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.history_search.is_none(), "enter closes the search");
        assert_eq!(app.editor.text(), "what does this crate do");

        let arrived = tokio::time::timeout(Duration::from_millis(50), events.next()).await;
        assert!(
            arrived.is_err(),
            "no engine event should have fired — a fill is not a submit"
        );
    }

    /// Esc restores exactly the buffer the search opened over — text and
    /// cursor both — even after the query narrowed the list and the
    /// selection moved.
    #[tokio::test]
    async fn esc_restores_the_pre_search_buffer_byte_for_byte() {
        let directory = temporary();
        let mut app = app_with_history(&directory, &["remembered prompt"]);
        for event in typing("draft in progress") {
            app.handle(event).await.expect("typing is handled");
        }
        let text_before = app.editor.text();
        let cursor_before = app.editor.cursor();

        app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .expect("control-r is handled");
        for event in typing("remem") {
            app.handle(event).await.expect("typing narrows the query");
        }
        app.handle(key(KeyCode::Down, KeyModifiers::NONE))
            .await
            .expect("down is handled");
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert!(app.history_search.is_none(), "escape closes the search");
        assert_eq!(
            app.editor.text(),
            text_before,
            "the buffer is restored byte for byte"
        );
        assert_eq!(app.editor.cursor(), cursor_before, "and the cursor too");
    }

    /// The chord is data like the six existing ones: rebinding it opens the
    /// search from its new key, and the old default stops working.
    #[tokio::test]
    async fn a_rebound_history_search_reaches_its_new_key_and_the_default_stops_working() {
        let directory = temporary();
        let mut history =
            history::History::load_from(directory.path().join("prompt-history.jsonl"));
        history.append(history::PromptInfo::text("remembered"));

        let configured: std::collections::BTreeMap<String, String> =
            [("history_search".to_owned(), "f7".to_owned())].into();
        let keys = crate::keybind::Keybinds::from_config(&configured).expect("a legible binding");
        let mut app = App::new(engine(), None, Themes::builtin())
            .with_history(history)
            .with_keybinds(keys);

        app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .expect("control-r is handled");
        assert!(
            app.history_search.is_none(),
            "the replaced default should be inert"
        );

        app.handle(key(KeyCode::F(7), KeyModifiers::NONE))
            .await
            .expect("f7 is handled");
        assert!(app.history_search.is_some(), "and f7 should open it");
    }

    /// The picker owns every key while it is open, exactly as the sessions
    /// and theme pickers do — otherwise `j` would be typed into the query
    /// and nowhere else, or worse, leak past it into the editor.
    #[tokio::test]
    async fn keys_while_the_history_search_is_open_do_not_reach_the_editor() {
        let directory = temporary();
        let mut app = app_with_history(&directory, &["remembered prompt"]);

        app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .expect("control-r is handled");
        for event in typing("jkx") {
            app.handle(event).await.expect("typing is handled");
        }

        assert!(app.editor.prompt().is_none());
        assert!(app.history_search.is_some());
    }

    /// The wheel belongs to the search modal too, like every other one.
    #[tokio::test]
    async fn the_wheel_does_not_reach_the_transcript_while_the_history_search_is_open() {
        let directory = temporary();
        let mut app = app_with_history(&directory, &["remembered prompt"]);
        for index in 0..60 {
            app.chat
                .start_message(Message::user(format!("entry {index}")));
        }
        app.draw(&mut terminal(40, 12)).expect("a frame draws");

        app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .expect("control-r is handled");
        app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })))
        .await
        .expect("a wheel event is handled");

        assert!(
            app.chat.is_following_tail(),
            "the wheel should be swallowed"
        );
    }

    /// An empty store still opens — the modal says so honestly rather than
    /// refusing to open at all.
    #[tokio::test]
    async fn control_r_over_an_empty_store_still_opens() {
        let mut app = app();

        app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .expect("control-r is handled");

        assert!(app.history_search.is_some());
    }

    #[tokio::test]
    async fn snapshot_history_search_open() {
        let directory = temporary();
        let mut app = app_with_history(
            &directory,
            &["what does this crate do", "commit the fix", "git status"],
        );

        app.handle(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .expect("control-r is handled");

        assert!(
            app.history_search.is_some(),
            "the search must be open, or the snapshot is of a bare screen"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// A pick has to outlive the run that made it, and the theme it names has
    /// to be the one the next run opens on.
    #[tokio::test]
    async fn a_kept_theme_is_stored_and_reopened_next_run() {
        let directory = temporary();
        let store = directory.path().join("tui.json");

        let mut themes = Themes::builtin();
        themes.adopt_store(store.clone());
        let mut app = App::new(engine(), None, themes);

        // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
        // own default chord is gone, so these fixtures open it the way
        // `snapshot_help_dialog_open` opens the reference card.
        app.run_command(command::Action::Themes).await;
        app.handle(key(KeyCode::Char('k'), KeyModifiers::NONE))
            .await
            .expect("k is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        let kept = app.theme.name().to_owned();

        let mut reopened = Themes::builtin();
        reopened.adopt_store(store);
        let next_run = App::new(engine(), None, reopened);

        assert_eq!(next_run.theme.name(), kept);
        assert_ne!(kept, DEFAULT_THEME, "the fixture must have changed it");
    }

    /// A cancel must not leave the previewed name behind in the store.
    #[tokio::test]
    async fn a_cancelled_preview_is_never_written_down() {
        let directory = temporary();
        let store = directory.path().join("tui.json");

        let mut themes = Themes::builtin();
        themes.adopt_store(store.clone());
        let mut app = App::new(engine(), None, themes);

        // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
        // own default chord is gone, so these fixtures open it the way
        // `snapshot_help_dialog_open` opens the reference card.
        app.run_command(command::Action::Themes).await;
        app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .await
            .expect("j is handled");
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert!(
            !store.exists(),
            "nothing was confirmed, so nothing is stored"
        );
    }

    /// A theme switch has to reach the editor and the cached transcript, not
    /// just the panes redrawn from scratch every frame.
    #[tokio::test]
    async fn a_theme_switch_repaints_the_whole_screen() {
        let mut app = themed_app("aura");
        palette_transcript(&mut app);

        let mut screen_buffer = terminal(80, 24);
        app.draw(&mut screen_buffer).expect("a frame draws");
        let (glyphs_before, styles_before) =
            (screen(&screen_buffer), styled_screen(&screen_buffer));

        app.apply_theme("gruvbox");
        app.draw(&mut screen_buffer).expect("a frame draws");

        assert_eq!(
            glyphs_before,
            screen(&screen_buffer),
            "a theme switch must not move a single glyph"
        );
        assert_ne!(
            styles_before,
            styled_screen(&screen_buffer),
            "nothing was repainted"
        );
    }

    #[test]
    fn snapshot_theme_opencode() {
        let mut app = themed_app(DEFAULT_THEME);
        palette_transcript(&mut app);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    #[test]
    fn snapshot_theme_tokyonight() {
        let mut app = themed_app("tokyonight");
        palette_transcript(&mut app);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    #[test]
    fn snapshot_theme_gruvbox() {
        let mut app = themed_app("gruvbox");
        palette_transcript(&mut app);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    #[test]
    fn snapshot_theme_aura() {
        let mut app = themed_app("aura");
        palette_transcript(&mut app);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    /// Opened the way a user opens it, and dumped with styles: this is what
    /// pins the selected row's fill and the panel surface behind the list.
    #[tokio::test]
    async fn snapshot_themes_dialog_open() {
        let mut app = app();
        palette_transcript(&mut app);

        // `ctrl+t` opens the Ctrl+T inspector now (**D453**); the picker's
        // own default chord is gone, so these fixtures open it the way
        // `snapshot_help_dialog_open` opens the reference card.
        app.run_command(command::Action::Themes).await;

        assert!(
            app.theme_list.is_some(),
            "the picker must be open, or the snapshot is of a bare screen"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_sessions_picker_after_moving_the_selection() {
        let directory = temporary();
        store_pickable_sessions(&directory);
        let mut app = persistent_app(&directory);

        app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .expect("control-s is handled");
        app.handle(key(KeyCode::Char('j'), KeyModifiers::NONE))
            .await
            .expect("j is handled");

        assert_eq!(
            app.sessions
                .as_ref()
                .and_then(|sessions| sessions.selected())
                .map(|info| info.id.as_str()),
            Some("0198f2c4-a1b0-7000-8000-000000000012"),
            "j should move down one row rather than reaching the editor"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    // ---- D505: the session socket follows the slot through the app's doors ----

    /// The socket a lead serves follows the engine's session slot through
    /// this app's own two doors (**D505**): bound on the first pass under the
    /// id the engine was minted with, moved to the stored session the picker
    /// resumes — the old one shut down first — moved again by `/new`, and
    /// shut down on the exit path. The fake binder records the ids; the real
    /// one is proved end to end in `ganja-cli/tests/session_socket.rs`.
    #[tokio::test]
    async fn the_session_socket_follows_the_picker_and_new_through_the_apps_doors() {
        let directory = temporary();
        store_pickable_sessions(&directory);
        let recording = Arc::new(crate::binder::fake::Recording::default());
        let mut app = persistent_app(&directory).with_socket(
            Box::new(Arc::clone(&recording)),
            crate::binder::fake::served(),
        );
        let minted = app.engine.session_id();

        // The first pass, whatever event carries it, binds under the minted id.
        app.handle(AppEvent::Tick).await.expect("a tick is handled");
        assert_eq!(
            recording.bound.lock().expect("not poisoned").as_slice(),
            std::slice::from_ref(&minted),
            "bound once, under the id the engine started on"
        );

        // The picker: Ctrl-S opens it and Enter resumes the row under the
        // cursor, which is a stored session with an id of its own.
        app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .expect("control-s is handled");
        let chosen = app
            .sessions
            .as_ref()
            .and_then(|sessions| sessions.selected())
            .map(|info| info.id.clone())
            .expect("the picker has a row under the cursor");
        assert_ne!(chosen, minted);
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert_eq!(app.engine.session_id(), chosen, "the resume moved the slot");
        assert_eq!(
            recording.bound.lock().expect("not poisoned").as_slice(),
            &[minted.clone(), chosen.clone()],
            "the resume rebound under the stored session's id"
        );
        assert_eq!(
            recording.closed.lock().expect("not poisoned").as_slice(),
            &[crate::binder::fake::Recording::path_for(&minted)],
            "and the minted session's socket was shut down first"
        );

        // `/new` through the composer, as a person types it.
        for event in typing("/new") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        let fresh = app.engine.session_id();
        assert_ne!(fresh, chosen, "/new re-minted the id");
        assert_eq!(
            recording.bound.lock().expect("not poisoned").as_slice(),
            &[minted.clone(), chosen.clone(), fresh.clone()],
            "and the socket followed it"
        );
        assert_eq!(
            recording.closed.lock().expect("not poisoned").len(),
            2,
            "the resumed session's socket was shut down before the fresh bind"
        );

        // The exit path: what `App::run` does after the loop.
        app.socket
            .as_mut()
            .expect("this app serves a socket")
            .shutdown()
            .await;
        assert_eq!(
            recording.closed.lock().expect("not poisoned").last(),
            Some(&crate::binder::fake::Recording::path_for(&fresh)),
            "exit shuts the bound socket down"
        );
    }

    /// An engine carrying the four builtin agents, which is what the agent
    /// list and Tab both read.
    fn agentic_app() -> App {
        let registry = Arc::new(
            ganja_core::AgentRegistry::from_config(&ganja_core::config::Config::default())
                .expect("the builtin agents resolve"),
        );
        let engine = Engine::new(
            Arc::new(FakeProvider::default()),
            fake::MODEL,
            Arc::new(ganja_tool::Registry::new(Vec::new())),
            ganja_permission::Permissions::default(),
        )
        .with_agents(registry);

        App::new(engine, None, Themes::builtin())
    }

    /// The whole point of `ctrl+p`: it reaches the list of everything else.
    #[tokio::test]
    async fn control_p_opens_the_palette_and_escape_closes_it() {
        let mut app = app();

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        assert!(app.palette.is_some(), "the palette should be open");

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.palette.is_none(), "escape should close it");
    }

    #[tokio::test]
    async fn typing_in_the_palette_filters_it_rather_than_reaching_the_editor() {
        let mut app = app();
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");

        // `j` and `k` are movement in the dialogs with no filter line; here
        // they have to be text, or half the alphabet cannot be searched for.
        for event in typing("jk") {
            app.handle(event).await.expect("typing is handled");
        }

        assert_eq!(
            app.palette.as_ref().map(Palette::filter),
            Some("jk"),
            "the keys should have reached the filter"
        );
        assert!(
            app.editor.prompt().is_none(),
            "and not the editor underneath"
        );
    }

    #[tokio::test]
    async fn the_palette_runs_the_command_under_its_cursor() {
        let mut app = app();
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        for event in typing("themes") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(
            app.palette.is_none(),
            "running a command closes the palette"
        );
        assert!(app.theme_list.is_some(), "and opens what it named");
    }

    /// Closing is not forgetting: a glance at the screen mid-search should not
    /// cost the search.
    #[tokio::test]
    async fn a_reopened_palette_still_holds_what_was_typed_into_it() {
        let mut app = app();
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        for event in typing("mo") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");

        assert_eq!(app.palette.as_ref().map(Palette::filter), Some("mo"));
    }

    #[tokio::test]
    async fn the_palette_reaches_every_command_it_lists() {
        let cases = [
            ("help", (|app: &App| app.help.is_some()) as fn(&App) -> bool),
            ("themes", |app: &App| app.theme_list.is_some()),
            ("models", |app: &App| app.chooser.is_some()),
            ("agents", |app: &App| app.chooser.is_some()),
            ("exit", |app: &App| app.quit),
        ];

        for (typed, opened) in cases {
            let mut app = agentic_app().with_provider("anthropic");
            app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
                .await
                .expect("control-p is handled");
            for event in typing(typed) {
                app.handle(event).await.expect("typing is handled");
            }
            app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .expect("enter is handled");

            assert!(opened(&app), "/{typed} should have done something");
        }
    }

    /// The trigger, at the level the user meets it: a slash that starts the
    /// buffer raises the menu and a slash anywhere else does not.
    #[tokio::test]
    async fn the_command_menu_opens_on_a_leading_slash_and_on_nothing_else() {
        let mut leading = app();
        for event in typing("/") {
            leading.handle(event).await.expect("typing is handled");
        }
        assert!(leading.dropdown.is_some(), "a leading slash should open it");

        let mut midway = app();
        for event in typing("what about /tmp") {
            midway.handle(event).await.expect("typing is handled");
        }
        assert!(
            midway.dropdown.is_none(),
            "a slash mid-sentence is a path, not a command"
        );
    }

    #[tokio::test]
    async fn the_command_menu_closes_once_the_slash_is_backspaced_away() {
        let mut app = app();
        for event in typing("/mo") {
            app.handle(event).await.expect("typing is handled");
        }
        assert!(app.dropdown.is_some());

        for _ in 0..3 {
            app.handle(key(KeyCode::Backspace, KeyModifiers::NONE))
                .await
                .expect("backspace is handled");
        }

        assert!(app.dropdown.is_none(), "an empty buffer is not a command");
    }

    /// **D11**: upstream deletes the typed `/xyz` whenever the menu closes.
    /// Reverting that divergence — wiping the buffer in `handle_dropdown_key`'s
    /// escape arm — fails this test.
    #[tokio::test]
    async fn escape_closes_the_command_menu_and_keeps_what_was_typed() {
        let mut app = app();
        for event in typing("/models") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert!(app.dropdown.is_none(), "the menu should have closed");
        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("/models"),
            "the text must survive, where upstream deletes it"
        );
    }

    #[tokio::test]
    async fn enter_on_the_command_menu_runs_the_command_and_empties_the_editor() {
        let mut app = app();
        for event in typing("/themes") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.theme_list.is_some(), "the command should have run");
        assert!(
            app.editor.prompt().is_none(),
            "the text that named it has done its job"
        );
    }

    /// **F1**, **D446**: Tab completes the buffer and closes the menu without
    /// running anything, for a UI command — the one place Tab and Enter part
    /// ways, since upstream's own Tab binding dispatches a UI command exactly
    /// as Enter does.
    #[tokio::test]
    async fn tab_on_the_command_menu_completes_the_buffer_without_running_it() {
        let mut app = app();
        for event in typing("/the") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .expect("tab is handled");

        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("/themes "),
            "the selected row's name, not the fragment that narrowed to it"
        );
        assert!(app.dropdown.is_none(), "choosing closes the menu");
        assert!(
            app.theme_list.is_none(),
            "Tab must not run the command the way Enter does"
        );
    }

    /// The same claim from the engine's side of the wire: nothing reaches it.
    #[tokio::test]
    async fn tab_on_a_ui_command_never_reaches_the_engine() {
        let (mut app, mut events) = wired().await;
        for event in typing("/new") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .expect("tab is handled");

        assert_eq!(app.editor.prompt().as_deref(), Some("/new "));
        assert!(
            events.next().now_or_never().is_none(),
            "nothing should have been sent to the engine"
        );
    }

    #[tokio::test]
    async fn the_arrow_keys_steer_the_command_menu_instead_of_the_transcript() {
        let mut app = app();
        for event in typing("/") {
            app.handle(event).await.expect("typing is handled");
        }
        let first = app
            .dropdown
            .as_ref()
            .and_then(Dropdown::selected)
            .expect("a menu with rows");

        app.handle(key(KeyCode::Down, KeyModifiers::NONE))
            .await
            .expect("down is handled");

        assert_ne!(
            app.dropdown.as_ref().and_then(Dropdown::selected),
            Some(first),
            "down should have moved the cursor"
        );
    }

    /// **F6**, **D445**. The flag is the whole feature: `App::draw` already
    /// turns it into a `terminal.clear()` for the external-editor return
    /// path, so pinning that Ctrl+L sets it is what pins the key to the
    /// behavior without re-testing `draw` itself.
    #[tokio::test]
    async fn ctrl_l_marks_the_next_frame_stale() {
        let mut app = app();
        assert!(!app.stale, "nothing has asked for a repaint yet");

        app.handle(key(KeyCode::Char('l'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl-l is handled");

        assert!(app.stale, "the next draw should force a full repaint");

        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(!app.stale, "the flag is consumed by the draw it triggered");
    }

    /// The exact bug `stale` exists to prevent: something else — not this
    /// `Terminal` — wrote over the screen, and an ordinary draw diffs against
    /// what it still thinks is there, so it writes nothing and the
    /// corruption survives. Ctrl+L is the hint that forces the real
    /// `terminal.clear()` a plain `draw` has no reason to call on its own.
    #[tokio::test]
    async fn ctrl_l_forces_a_full_repaint_instead_of_a_diff_against_a_stale_screen() {
        let mut app = app();
        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("the first frame draws");
        assert!(
            !screen(&terminal).trim().is_empty(),
            "the fixture app should have drawn something to corrupt"
        );

        // Something else wrote over the screen without this `Terminal`
        // knowing, the way an external editor does.
        terminal
            .backend_mut()
            .clear_region(ClearType::All)
            .expect("the backend clears");
        assert!(screen(&terminal).trim().is_empty(), "the corruption landed");

        // Nothing changed from this `Terminal`'s point of view, so an
        // ordinary draw trusts its own cache and never touches the
        // corrupted backend.
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).trim().is_empty(),
            "an undirected draw should not have repainted over the corruption"
        );

        app.handle(key(KeyCode::Char('l'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl-l is handled");
        app.draw(&mut terminal).expect("the redraw repaints");

        assert!(
            !screen(&terminal).trim().is_empty(),
            "ctrl+l should have forced a real repaint over the corruption"
        );
    }

    #[tokio::test]
    async fn the_model_list_holds_the_running_providers_models_and_marks_the_active_one() {
        let mut served = app().with_provider("anthropic");
        served.open_models();

        let dialog = served.chooser.as_ref().expect("the list should be open");
        assert_eq!(dialog.0, Chooser::Models);
        assert!(
            !dialog.1.is_empty(),
            "anthropic has models in the compiled-in catalog"
        );
        assert!(
            served.wire_fetch.is_none(),
            "a cataloged provider never consults the wire"
        );
    }

    /// The fake model has no catalog row, so `/effort` has nothing to list
    /// — and says ganja's sentence instead of opening an empty dialog.
    #[tokio::test]
    async fn the_effort_picker_refuses_a_model_with_nothing_to_offer() {
        let mut app = app();
        app.run_command(command::Action::Effort).await;

        assert!(app.chooser.is_none(), "there is nothing to choose from");
        assert!(
            status_line(&mut app).contains(NO_EFFORTS),
            "got {:?}",
            status_line(&mut app)
        );
    }

    /// Enter on the picker routes through [`Command::SwitchEffort`]; the
    /// engine's refusal — the fake provider has no catalog rows — lands in
    /// the status bar and leaves the list open, exactly as a refused model
    /// switch does.
    #[tokio::test]
    async fn choosing_an_effort_sends_the_switch_and_surfaces_the_refusal() {
        let mut app = app();
        app.chooser = Some((
            Chooser::Effort,
            ListDialog::new(" effort ", effort::rows(["max"], None)),
        ));
        app.move_chooser(1);

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(
            status_line(&mut app).contains("not in the catalog"),
            "the engine's refusal should be readable: {:?}",
            status_line(&mut app)
        );
        assert!(
            app.chooser.is_some(),
            "a refused switch keeps the list the user was choosing from"
        );
    }

    /// The announcement is what moves the status line — whichever frontend
    /// issued the switch — and clearing takes the segment away whole, so a
    /// session back on Default renders the bar it always had.
    #[tokio::test]
    async fn an_effort_change_event_moves_the_status_line_and_a_clear_empties_it() {
        let mut app = app();
        assert!(!status_line(&mut app).contains("(max)"));

        app.handle(AppEvent::core(CoreEvent::EffortChanged {
            session_id: session(),
            effort: Some("max".to_owned()),
        }))
        .await
        .expect("the announcement is handled");
        assert_eq!(app.effort.as_deref(), Some("max"));
        assert!(
            status_line(&mut app).contains("canned (max)"),
            "the model and its effort belong together: {:?}",
            status_line(&mut app)
        );

        app.handle(AppEvent::core(CoreEvent::EffortChanged {
            session_id: session(),
            effort: None,
        }))
        .await
        .expect("the clear is handled");
        assert!(
            !status_line(&mut app).contains("canned (max)"),
            "Default has no segment: {:?}",
            status_line(&mut app)
        );
    }

    /// A wire-listed model row, in the shape the seam hands back.
    fn listed(id: &str, name: &str) -> ganja_core::provider::ListedModel {
        ganja_core::provider::ListedModel {
            id: id.to_owned(),
            name: name.to_owned(),
        }
    }

    /// A whole seam answer around `models`. The notice is the seam's to write
    /// and nothing in this crate renders it — the chooser shows rows — so a
    /// fixture supplies any of them.
    fn served(models: Vec<ganja_core::provider::ListedModel>) -> ganja_core::provider::WireModels {
        ganja_core::provider::WireModels {
            models,
            notice: "a listing fixture",
        }
    }

    /// Ticks until the in-flight listing fetch has been reaped.
    ///
    /// The loop the real select loop runs, minus the frame budget: the fetch
    /// finishes on its own schedule and the tick is what notices.
    async fn reap_wire_fetch(app: &mut App) {
        for _ in 0..400 {
            app.handle(AppEvent::Tick).await.expect("a tick is handled");
            if app.wire_fetch.is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        panic!("the listing fetch never finished");
    }

    /// A provider with no catalog rows goes to the wire's listing, and one
    /// the seam does not serve either ends where it always did: the empty
    /// list, one tick later.
    #[tokio::test]
    async fn a_provider_no_listing_serves_still_opens_the_empty_list_it_always_did() {
        let mut unknown = app().with_provider("a-provider-nothing-ships");
        unknown.open_models();
        assert!(
            unknown.chooser.is_none(),
            "nothing opens until the fetch lands"
        );

        reap_wire_fetch(&mut unknown).await;
        assert!(
            unknown
                .chooser
                .as_ref()
                .is_some_and(|(_, list)| list.is_empty()),
            "a provider with no catalog entries has nothing to offer"
        );
    }

    /// The wire path's row shape, from the cache: value and label are the id,
    /// the display name rides beside it, and the session's model is the
    /// active row the cursor starts on.
    #[tokio::test]
    async fn a_cached_wire_listing_opens_the_chooser_with_its_rows_and_marks_the_active_model() {
        let mut app = app().with_provider("cursor");
        app.model = "claude-4.5-opus".to_owned();
        app.wire_models = Some(vec![
            listed("gpt-5.3-codex", "Codex 5.3"),
            listed("claude-4.5-opus", "Claude 4.5 Opus"),
        ]);

        app.open_models();

        let (kind, dialog) = app.chooser.as_ref().expect("the cache opens the list");
        assert_eq!(*kind, Chooser::Models);
        assert_eq!(
            dialog.selected(),
            Some("claude-4.5-opus"),
            "the cursor starts on the model the session is on"
        );
        assert!(app.wire_fetch.is_none(), "a cache hit fetches nothing");

        let mut terminal = terminal(80, 14);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("gpt-5.3-codex"), "{screen}");
        assert!(
            screen.contains("Codex 5.3"),
            "the display name rides beside the id: {screen}"
        );
    }

    /// While the fetch is out, the status bar says so instead of the frame
    /// freezing on the RPC or an empty dialog opening early.
    #[tokio::test]
    async fn an_uncataloged_model_list_says_it_is_fetching_while_the_wire_answers() {
        let mut app = app().with_provider("a-provider-nothing-ships");
        app.open_models();

        assert!(app.wire_fetch.is_some(), "the fetch is in flight");
        assert!(
            status_line(&mut app).contains("fetching a-provider-nothing-ships models"),
            "got: {}",
            status_line(&mut app)
        );
    }

    /// The guard the fetch slot doubles as: after a second `/model`, the
    /// planted fetch is still the one the tick reaps, so completing it fills
    /// the cache — a second spawn would have replaced it and answered
    /// differently.
    #[tokio::test]
    async fn a_second_model_list_while_the_fetch_is_in_flight_does_not_spawn_another() {
        let mut app = app().with_provider("cursor");
        let (landing, planted) = tokio::sync::oneshot::channel();
        app.wire_fetch = Some(tokio::spawn(async move {
            planted.await.expect("the test completes the fetch")
        }));

        app.open_models();
        assert!(app.chooser.is_none(), "nothing to show yet");

        landing
            .send(Some(Ok(served(vec![listed("planted-one", "Planted One")]))))
            .expect("the fetch is still listening");
        reap_wire_fetch(&mut app).await;

        assert!(
            app.wire_models
                .as_ref()
                .is_some_and(|models| models[0].id == "planted-one"),
            "the planted fetch is the one that landed"
        );
        assert!(
            app.chooser
                .as_ref()
                .is_some_and(|(kind, list)| *kind == Chooser::Models && !list.is_empty()),
            "and its rows are what opened"
        );
    }

    /// Without this arm the tick never fires on an idle app and the finished
    /// fetch waits for an unrelated keypress — the same reason the MCP dial
    /// keeps the loop waking.
    #[tokio::test]
    async fn an_in_flight_wire_fetch_keeps_the_loop_waking_up() {
        let mut app = app().with_provider("cursor");
        let (_landing, planted) = tokio::sync::oneshot::channel::<WireListing>();
        app.wire_fetch = Some(tokio::spawn(async move { planted.await.unwrap_or(None) }));
        app.draw(&mut terminal(80, 24)).expect("a frame draws");

        assert!(!app.dirty, "the frame above cleared it");
        assert!(app.wants_wakeup(), "the fetch is what keeps it awake");
    }

    /// Rows landing under an open modal wait in the cache rather than
    /// stealing the keys the modal claimed, and the next `/model` opens
    /// instantly from what was kept.
    #[tokio::test]
    async fn a_listing_that_lands_under_a_modal_waits_in_the_cache() {
        let mut app = app().with_provider("cursor");
        app.help = Some(Help::new(app.keys.clone()));
        app.wire_fetch = Some(tokio::spawn(async {
            Some(Ok(served(vec![listed("planted-one", "Planted One")])))
        }));

        reap_wire_fetch(&mut app).await;
        assert!(app.chooser.is_none(), "the modal keeps its keys");
        assert!(app.wire_models.is_some(), "but the rows are kept");

        app.help = None;
        app.open_models();
        assert!(
            app.chooser.is_some(),
            "the next `/model` opens instantly from the cache"
        );
    }

    /// An empty roster is an answer, and an empty dialog is not a way to
    /// show it.
    #[tokio::test]
    async fn a_wire_that_serves_no_models_says_so_instead_of_opening_an_empty_dialog() {
        let mut app = app().with_provider("cursor");
        app.wire_fetch = Some(tokio::spawn(async { Some(Ok(served(Vec::new()))) }));

        reap_wire_fetch(&mut app).await;
        assert!(app.chooser.is_none(), "no dialog opens over nothing");
        assert!(
            status_line(&mut app).contains("the cursor wire served no models"),
            "got: {}",
            status_line(&mut app)
        );
    }

    /// A failure is a status line, never a dialog — and the cleared slot is
    /// what makes the next `/model` a retry rather than a dead end.
    #[tokio::test]
    async fn a_failed_wire_listing_lands_in_the_status_bar_and_clears_the_slot_for_a_retry() {
        let mut app = app().with_provider("cursor");
        app.wire_fetch = Some(tokio::spawn(async {
            Some(Err(ganja_core::provider::ProviderError::Auth(
                "no cursor credential is stored; run `ganja auth login cursor`".to_owned(),
            )))
        }));

        reap_wire_fetch(&mut app).await;
        assert!(app.chooser.is_none(), "a failure opens nothing");
        assert!(app.wire_models.is_none(), "and caches nothing");
        assert!(
            status_line(&mut app).contains("ganja auth login cursor"),
            "the wire's own words reach the status bar: {}",
            status_line(&mut app)
        );
    }

    #[tokio::test]
    async fn choosing_a_model_switches_the_session_to_it() {
        let mut app = app().with_provider("anthropic");
        app.open_models();

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.chooser.is_none(), "a switch that took closes the list");
        assert_eq!(
            app.model,
            app.engine.model(),
            "the frontend prices what the engine will ask for"
        );
        assert_ne!(app.model, fake::MODEL, "and it is no longer the old model");
    }

    /// A switch mid-turn is exactly what the engine refuses, and a refusal has
    /// one place to be seen.
    #[tokio::test]
    async fn a_refused_model_switch_reaches_the_status_bar_and_leaves_the_list_up() {
        let (mut app, mut events) = wired().await;
        app = app.with_provider("anthropic");
        for event in typing("a turn to be busy with") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        app.open_models();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(
            app.chooser.is_some(),
            "a refused switch keeps the list the user was choosing from"
        );
        let mut terminal = terminal(120, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("already streaming"),
            "the refusal should be readable:\n{}",
            screen(&terminal)
        );

        // Drain the turn so the test does not leave one streaming.
        pump(&mut app, &mut events, 1).await;
    }

    /// Subagents are the task tool's to spawn; a picker offering them would be
    /// offering a switch the engine refuses.
    #[tokio::test]
    async fn the_agent_list_holds_only_the_agents_a_user_may_switch_to() {
        let mut app = agentic_app();
        app.open_agents();

        let (kind, dialog) = app.chooser.as_ref().expect("the list should be open");
        assert_eq!(*kind, Chooser::Agents);

        let mut listed = Vec::new();
        let mut cursor = dialog.clone();
        cursor.move_selection(-99);
        for _ in 0..8 {
            if let Some(value) = cursor.selected() {
                listed.push(value.to_owned());
            }
            cursor.move_selection(1);
        }
        listed.dedup();

        assert_eq!(
            listed,
            vec!["build".to_owned(), "plan".to_owned()],
            "general and explore are subagents"
        );
    }

    #[tokio::test]
    async fn choosing_an_agent_switches_the_session_and_the_status_bar_says_so() {
        let mut app = agentic_app();
        assert_eq!(
            app.agent.as_deref(),
            Some("build"),
            "sessions start on build"
        );

        app.open_agents();
        app.handle(key(KeyCode::Down, KeyModifiers::NONE))
            .await
            .expect("down is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(app.agent.as_deref(), Some("plan"));
        assert_eq!(app.engine.agent().as_deref(), Some("plan"));

        let mut terminal = terminal(80, 8);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("plan"),
            "the bar should name the agent:\n{}",
            screen(&terminal)
        );
    }

    #[tokio::test]
    async fn an_agent_change_the_engine_announces_moves_the_indicator() {
        let mut app = app();

        app.handle(AppEvent::core(CoreEvent::AgentChanged {
            session_id: session(),
            agent: "plan".to_owned(),
            model: "provider/planner".to_owned(),
        }))
        .await
        .expect("an agent change is handled");

        assert_eq!(app.agent.as_deref(), Some("plan"));
        assert_eq!(app.model, "provider/planner");

        let mut terminal = terminal(80, 8);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("plan"),
            "the status bar should name the announced agent:\n{}",
            screen(&terminal)
        );
    }

    #[tokio::test]
    async fn tab_on_an_empty_editor_cycles_the_agents_and_wraps() {
        let mut app = agentic_app();
        let mut seen = vec![app.agent.clone()];

        for _ in 0..3 {
            app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
                .await
                .expect("tab is handled");
            seen.push(app.agent.clone());
        }

        assert_eq!(
            seen,
            vec![
                Some("build".to_owned()),
                Some("plan".to_owned()),
                Some("build".to_owned()),
                Some("plan".to_owned()),
            ],
            "two selectable agents, cycled and wrapped"
        );
    }

    #[tokio::test]
    async fn tab_with_something_typed_reaches_the_editor_instead_of_the_agents() {
        let mut app = agentic_app();
        for event in typing("half a thought") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .expect("tab is handled");

        assert_eq!(
            app.agent.as_deref(),
            Some("build"),
            "a Tab inside a sentence is not a request to change agent"
        );
        assert_ne!(
            app.editor.prompt().as_deref(),
            Some("half a thought"),
            "it should have reached the editor"
        );
    }

    /// The one key that means two things, and the gate that decides which.
    #[tokio::test]
    async fn control_d_exits_on_an_empty_editor_and_deletes_forward_otherwise() {
        let mut bare = app();
        bare.handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .await
            .expect("control-d is handled");
        assert!(bare.quit, "an empty editor has nothing to delete");

        let mut typed = app();
        for event in typing("abc") {
            typed.handle(event).await.expect("typing is handled");
        }
        typed
            .handle(key(KeyCode::Home, KeyModifiers::NONE))
            .await
            .expect("home is handled");
        typed
            .handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .await
            .expect("control-d is handled");

        assert!(!typed.quit, "there was something to delete");
        assert_eq!(
            typed.editor.prompt().as_deref(),
            Some("bc"),
            "and it was deleted"
        );
    }

    /// Ganja's own interrupts, which stay unconditional where Ctrl-D is gated.
    #[tokio::test]
    async fn control_c_and_control_q_quit_whatever_is_typed() {
        for code in [KeyCode::Char('c'), KeyCode::Char('q')] {
            let mut app = app();
            for event in typing("a draft nobody asked to keep") {
                app.handle(event).await.expect("typing is handled");
            }

            app.handle(key(code, KeyModifiers::CONTROL))
                .await
                .expect("a quit key is handled");

            assert!(app.quit, "{code:?} with Control should quit");
        }
    }

    #[tokio::test]
    async fn home_and_end_move_in_the_buffer_while_there_is_one_and_in_the_transcript_otherwise() {
        let mut written = app();
        for event in typing("a line to move around in") {
            written.handle(event).await.expect("typing is handled");
        }

        written
            .handle(key(KeyCode::Home, KeyModifiers::NONE))
            .await
            .expect("home is handled");
        assert_eq!(
            written.editor.cursor(),
            (0, 0),
            "home reached the line's start"
        );

        written
            .handle(key(KeyCode::End, KeyModifiers::NONE))
            .await
            .expect("end is handled");
        assert_eq!(
            written.editor.cursor(),
            (0, "a line to move around in".chars().count()),
            "end reached the line's end"
        );

        // Empty editor: the same keys are the transcript's.
        let mut empty = app();
        empty.seed(stored_transcript(80));
        let mut terminal = terminal(40, 12);
        empty.draw(&mut terminal).expect("a frame draws");

        empty
            .handle(key(KeyCode::Home, KeyModifiers::NONE))
            .await
            .expect("home is handled");
        assert!(
            !empty.chat.is_following_tail(),
            "home should have gone to the oldest message"
        );

        empty
            .handle(key(KeyCode::End, KeyModifiers::NONE))
            .await
            .expect("end is handled");
        assert!(
            empty.chat.is_following_tail(),
            "end should have come back to the newest"
        );
    }

    #[tokio::test]
    async fn a_bare_exit_word_submitted_on_its_own_quits() {
        for word in ["exit", "quit", ":q"] {
            let (mut app, _events) = wired().await;
            for event in typing(word) {
                app.handle(event).await.expect("typing is handled");
            }

            app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .expect("enter is handled");

            assert!(app.quit, "{word:?} on its own should quit");
        }
    }

    #[tokio::test]
    async fn an_exit_word_inside_a_sentence_is_a_prompt_like_any_other() {
        let (mut app, mut events) = wired().await;
        for event in typing("does exit mean anything here") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(!app.quit, "the word only quits when it is the whole prompt");
        pump(&mut app, &mut events, 1).await;
    }

    /// The keys the config file moved are the keys that work; the ones it
    /// replaced are not.
    #[tokio::test]
    async fn a_rebound_key_opens_what_it_was_bound_to_and_the_default_stops_working() {
        let configured: std::collections::BTreeMap<String, String> =
            [("palette_open".to_owned(), "f5".to_owned())].into();
        let keys = crate::keybind::Keybinds::from_config(&configured).expect("a legible binding");
        let mut app = app().with_keybinds(keys);

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        assert!(
            app.palette.is_none(),
            "the replaced default should be inert"
        );

        app.handle(key(KeyCode::F(5), KeyModifiers::NONE))
            .await
            .expect("f5 is handled");
        assert!(app.palette.is_some(), "and f5 should open it");
    }

    #[tokio::test]
    async fn the_help_card_opens_from_the_palette_and_closes_on_escape() {
        let mut app = app();
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        for event in typing("help") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert!(app.help.is_some());

        // Every other key is swallowed, like any modal here.
        for event in typing("x") {
            app.handle(event).await.expect("typing is handled");
        }
        assert!(app.help.is_some(), "typing should not close it");
        assert!(app.editor.prompt().is_none(), "nor reach the editor");

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.help.is_none());
    }

    /// Ctrl+T opens the inspector overlay (**F2**, **D453**) and both Esc and
    /// Ctrl+T itself close it again — a toggle, not a one-way door.
    #[tokio::test]
    async fn ctrl_t_opens_the_inspector_and_either_esc_or_ctrl_t_closes_it() {
        let mut app = app();
        assert!(app.inspector.is_none());

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl+t is handled");
        assert!(app.inspector.is_some());

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.inspector.is_none(), "escape should close it");

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl+t is handled");
        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl+t is handled again");
        assert!(app.inspector.is_none(), "ctrl+t itself should close it too");
    }

    /// `q` closes the overlay too — Codex's own binding for its transcript
    /// overlay, screenshot-sourced (see `component/inspector.rs`'s module
    /// doc) — beside Esc and Ctrl+T rather than instead of them.
    #[tokio::test]
    async fn q_closes_the_inspector_too() {
        let mut app = app();

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl+t is handled");
        assert!(app.inspector.is_some());

        app.handle(key(KeyCode::Char('q'), KeyModifiers::NONE))
            .await
            .expect("q is handled");
        assert!(app.inspector.is_none(), "q should close it");
    }

    /// Left/Right cycle the tabs and the digit keys jump straight to one, all
    /// reflected in what actually renders rather than just in which key was
    /// pressed.
    #[tokio::test]
    async fn digit_keys_and_arrows_switch_the_inspectors_tab() {
        let mut app = app();
        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl+t opens the overlay");

        let mut terminal = terminal(120, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("no session yet"),
            "the overlay opens on the transcript tab:\n{}",
            screen(&terminal)
        );

        app.handle(key(KeyCode::Char('2'), KeyModifiers::NONE))
            .await
            .expect("2 jumps to the log tab");
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("no events yet"),
            "got:\n{}",
            screen(&terminal)
        );

        app.handle(key(KeyCode::Left, KeyModifiers::NONE))
            .await
            .expect("left cycles back a tab");
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("no session yet"),
            "left from the log tab should land back on the transcript tab:\n{}",
            screen(&terminal)
        );
    }

    /// A view, not a mode (**F2**): opening the overlay does not pause a
    /// streaming turn, and the transcript underneath keeps growing exactly as
    /// it would with nothing open.
    #[tokio::test]
    async fn the_inspector_does_not_pause_a_streaming_turn() {
        let mut app = app();
        let reply = Message::assistant("canned");
        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            session_id: session(),
            message: reply.clone(),
        }))
        .await
        .expect("the reply starts");
        assert!(app.status.is_streaming());

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl+t opens the overlay");

        let part = Part::text("");
        app.handle(AppEvent::core(CoreEvent::PartStarted {
            session_id: session(),
            message_id: reply.id.clone(),
            part: part.clone(),
        }))
        .await
        .expect("a part starts");
        app.handle(AppEvent::core(CoreEvent::PartDelta {
            session_id: session(),
            message_id: reply.id.clone(),
            part_id: part.id.clone(),
            delta: "still streaming".to_owned(),
        }))
        .await
        .expect("a fragment is handled");

        assert!(
            app.status.is_streaming(),
            "the overlay must not pause the turn"
        );
        let grew = app.chat.messages().iter().any(|(_, parts)| {
            parts
                .iter()
                .any(|part| part.as_text() == Some("still streaming"))
        });
        assert!(
            grew,
            "the transcript should keep growing while the overlay is open"
        );
    }

    /// Opened the way a user opens it, over a real transcript — this is what
    /// pins the overlay's banner, its tab strip and its footer, full-terminal
    /// (screenshot-sourced, see `component/inspector.rs`'s module doc) rather
    /// than boxed inside the three-pane frame the way it used to be.
    #[tokio::test]
    async fn snapshot_inspector_dialog_open() {
        let mut app = app();
        palette_transcript(&mut app);

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl+t opens the overlay");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// Full-terminal takeover means what it says: the composer and the
    /// status bar — both still drawn beneath every other dialog — disappear
    /// under the inspector while it is open, and come back the moment it
    /// closes (screenshot-sourced, see `component/inspector.rs`'s module
    /// doc).
    #[tokio::test]
    async fn the_inspector_covers_the_composer_and_the_status_bar() {
        let mut app = app();
        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let closed = screen(&terminal);
        assert!(closed.contains("Ask ganja something"), "{closed}");
        // The idle footer carries no key reminders, so the state label is what
        // says the status bar is drawn at all.
        assert!(closed.contains("ready"), "{closed}");

        app.handle(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .expect("ctrl+t opens the overlay");
        app.draw(&mut terminal).expect("a frame draws");
        let open = screen(&terminal);
        assert!(
            !open.contains("Ask ganja something"),
            "the composer should be covered while the overlay is open:\n{open}"
        );
        assert!(
            !open.contains("ready"),
            "the status bar should be covered too:\n{open}"
        );

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape closes the overlay");
        app.draw(&mut terminal).expect("a frame draws");
        let reclosed = screen(&terminal);
        assert!(
            reclosed.contains("Ask ganja something"),
            "closing the overlay should bring the composer back:\n{reclosed}"
        );
    }

    #[tokio::test]
    async fn the_wheel_does_not_reach_the_transcript_while_the_palette_is_open() {
        let mut app = app();
        app.seed(stored_transcript(80));
        let mut terminal = terminal(40, 12);
        app.draw(&mut terminal).expect("a frame draws");

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        app.handle(AppEvent::Term(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })))
        .await
        .expect("the wheel is handled");

        assert!(
            app.chat.is_following_tail(),
            "a modal claims the wheel as well as the keys"
        );
    }

    #[tokio::test]
    async fn snapshot_palette_open() {
        let mut app = app();
        palette_transcript(&mut app);

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");

        assert!(
            app.palette.is_some(),
            "the palette must be open, or the snapshot is of a bare screen"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_palette_filtered() {
        let mut app = app();
        palette_transcript(&mut app);

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        for event in typing("s") {
            app.handle(event).await.expect("typing is handled");
        }

        assert_eq!(
            app.palette.as_ref().map(Palette::filter),
            Some("s"),
            "the fragment must have landed, or the snapshot is of an unfiltered list"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// The selected row is filled rather than tinted, which is the one part of
    /// the palette a symbol-only dump cannot pin.
    #[tokio::test]
    async fn snapshot_palette_selection_styling() {
        let mut app = themed_app("tokyonight");
        palette_transcript(&mut app);

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        app.handle(key(KeyCode::Down, KeyModifiers::NONE))
            .await
            .expect("down is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(styled_screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_command_menu_open() {
        let mut app = app();
        palette_transcript(&mut app);

        for event in typing("/s") {
            app.handle(event).await.expect("typing is handled");
        }

        assert!(
            app.dropdown.is_some(),
            "the menu must be open, or the snapshot is of a bare screen"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_agents_dialog_open() {
        let mut app = agentic_app();
        palette_transcript(&mut app);
        app.open_agents();

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_help_dialog_open() {
        let mut app = app();
        palette_transcript(&mut app);
        app.run_command(crate::command::Action::Help).await;

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// A project on disk for the `@` menu to walk, with one file in a
    /// subdirectory so a mention has a path to complete rather than a name.
    fn project() -> TempDir {
        let directory = temporary();
        std::fs::create_dir_all(directory.path().join("src")).expect("the fixture tree is made");
        for path in ["README.md", "src/lib.rs", "src/app.rs"] {
            std::fs::write(directory.path().join(path), "// a file worth mentioning\n")
                .expect("the fixture file writes");
        }

        directory
    }

    /// A skills root holding two named skills, for the `$` menu to offer.
    fn skill_root() -> TempDir {
        let directory = temporary();
        for (name, description) in [("porting", "How to port a module."), ("tdd", "Red, green.")] {
            let dir = directory.path().join(name);
            std::fs::create_dir_all(&dir).expect("the fixture tree is made");
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
            )
            .expect("the fixture file writes");
        }

        directory
    }

    /// An app whose engine holds `root` as its skill roots — the same seam
    /// the real assembly wires (`with_skill_roots`).
    fn app_with_skills(root: &TempDir) -> App {
        let app = app();
        app.engine.replace_skill_roots(
            ganja_tool::skill::Roots::none().with_paths([root.path().to_path_buf()]),
        );

        app
    }

    #[tokio::test]
    async fn a_dollar_raises_the_skill_menu_and_the_fragment_narrows_it() {
        let root = skill_root();
        let mut app = app_with_skills(&root);

        for event in typing("use $port") {
            app.handle(event).await.expect("typing is handled");
        }

        assert!(app.skill_menu.is_some(), "the skill menu should be open");
        assert_eq!(
            app.skill_menu.as_ref().and_then(|menu| menu.selected()),
            Some("porting"),
            "the fragment narrowed the list to the matching name"
        );
    }

    /// The exact trigger, at the level a person meets it: prose dollars are
    /// prose, and only a token a skill answers to keeps a menu up.
    #[tokio::test]
    async fn prose_dollars_raise_no_skill_menu() {
        let cases = [
            ("costs $5 each", false),
            ("$ cargo build", false),
            ("mail me$porting", false),
            ("$port", true),
            ("$", true),
        ];

        for (text, expected) in cases {
            let root = skill_root();
            let mut app = app_with_skills(&root);
            for event in typing(text) {
                app.handle(event).await.expect("typing is handled");
            }

            assert_eq!(
                app.skill_menu.is_some(),
                expected,
                "{text:?} should {}have raised the menu",
                if expected { "" } else { "not " }
            );
        }
    }

    #[tokio::test]
    async fn tab_completes_the_invocation_without_submitting() {
        let root = skill_root();
        let mut app = app_with_skills(&root);
        for event in typing("use $port") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .expect("tab is handled");

        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("use $porting "),
            "the fragment became the whole name, with room after it"
        );
        assert!(app.skill_menu.is_none(), "completing closes the menu");
        assert_eq!(
            app.editor.cursor(),
            (0, "use $porting ".chars().count()),
            "the cursor follows what was inserted"
        );
    }

    #[tokio::test]
    async fn enter_on_the_skill_menu_completes_the_same_as_tab() {
        let root = skill_root();
        let mut app = app_with_skills(&root);
        for event in typing("$td") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("$tdd "),
            "enter completes rather than submitting"
        );
        assert!(app.skill_menu.is_none());
    }

    /// Escape closes the menu and keeps the text (**D11**), like the other
    /// two inline menus.
    #[tokio::test]
    async fn esc_closes_the_skill_menu_and_keeps_the_text() {
        let root = skill_root();
        let mut app = app_with_skills(&root);
        for event in typing("$port") {
            app.handle(event).await.expect("typing is handled");
        }
        assert!(app.skill_menu.is_some());

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("esc is handled");

        assert!(app.skill_menu.is_none());
        assert_eq!(app.editor.prompt().as_deref(), Some("$port"));
    }

    /// The submit half: the names a buffer's tokens invoke are exactly what
    /// rides `Command::SendPrompt::skills`, validated against the same
    /// discovery the menu offered from.
    #[tokio::test]
    async fn requested_skills_reads_the_tokens_a_submit_would_send() {
        let root = skill_root();
        let app = app_with_skills(&root);

        assert_eq!(
            app.requested_skills("use $porting then $tdd, not $PATH or $5"),
            vec!["porting".to_owned(), "tdd".to_owned()]
        );
        assert!(app.requested_skills("nothing invoked").is_empty());
    }

    /// The `/skills` dialog is a listing with an insertion, not a switch:
    /// Enter puts `$name ` at the cursor and the session changes nothing.
    #[tokio::test]
    async fn the_skills_dialog_lists_and_enter_inserts_the_token() {
        let root = skill_root();
        let mut app = app_with_skills(&root);
        app.run_command(command::Action::Skills).await;

        let (kind, dialog) = app.chooser.as_ref().expect("the dialog is open");
        assert!(matches!(kind, Chooser::Skills));
        assert_eq!(dialog.selected(), Some("porting"));

        // The row names the description; the origin root rides beside it but
        // is a temporary path too wide for this terminal, so the sentence is
        // the part a screen this size proves.
        let mut terminal = terminal(120, 20);
        app.draw(&mut terminal).expect("a frame draws");
        let drawn = screen(&terminal);
        assert!(
            drawn.contains("How to port a module."),
            "the dialog shows the skill's sentence: {drawn}"
        );

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(app.editor.prompt().as_deref(), Some("$porting "));
        assert!(app.chooser.is_none(), "choosing closes the dialog");
    }

    #[tokio::test]
    async fn snapshot_skill_menu() {
        let root = skill_root();
        let mut app = app_with_skills(&root);
        for event in typing("use $") {
            app.handle(event).await.expect("typing is handled");
        }

        let mut terminal = terminal(80, 16);
        app.draw(&mut terminal).expect("a frame draws");
        insta::assert_snapshot!(screen(&terminal));
    }

    /// An app whose `@` menu walks `directory`.
    fn app_in(directory: &TempDir) -> App {
        app().with_cwd(directory.path())
    }

    /// The paths the file menu is currently offering.
    fn offered(app: &App) -> Vec<String> {
        let mut listed = Vec::new();
        let Some(files) = &app.files else {
            return listed;
        };
        let mut cursor = files.clone();
        cursor.move_selection(-99);
        for _ in 0..16 {
            if let Some(path) = cursor.selected() {
                listed.push(path.to_owned());
            }
            cursor.move_selection(1);
        }
        listed.dedup();

        listed
    }

    /// Types `text` into `app`, one key at a time, the way a person does —
    /// then settles the `@` menu's background walk, which a real run reaps
    /// on its next ticks (2026-08-15: the walk left the keystroke's own
    /// handling).
    async fn typed(app: &mut App, text: &str) {
        for event in typing(text) {
            app.handle(event).await.expect("typing is handled");
        }
        settle_file_menu(app).await;
    }

    /// Waits for an in-flight `@` walk to finish and installs it, standing in
    /// for the tick loop.
    async fn settle_file_menu(app: &mut App) {
        for _ in 0..500 {
            if app.file_walk.is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            app.poll_file_walk().await;
        }
        panic!("the file walk never settled");
    }

    #[tokio::test]
    async fn an_at_raises_the_file_menu_over_what_the_project_holds() {
        let directory = project();
        let mut app = app_in(&directory);

        typed(&mut app, "look at @lib").await;

        assert!(app.files.is_some(), "the file menu should be open");
        assert_eq!(offered(&app), vec!["src/lib.rs".to_owned()]);
    }

    /// A fragment naming directories anchors on them, which is what makes a
    /// path typed from the root reach what is under it.
    #[tokio::test]
    async fn a_fragment_naming_a_directory_offers_what_is_inside_it() {
        let directory = project();
        let mut app = app_in(&directory);

        typed(&mut app, "@src/").await;

        let mut offered = offered(&app);
        offered.sort();
        assert_eq!(
            offered,
            vec!["src/app.rs".to_owned(), "src/lib.rs".to_owned()]
        );
    }

    /// The exact trigger, at the level a person meets it. The shapes
    /// themselves are pinned in `mention`; what this covers is that the app
    /// asks the same question the scan will ask on submit.
    #[tokio::test]
    async fn the_file_menu_opens_on_exactly_the_mentions_a_submit_would_read() {
        let cases = [
            // Typed, and whether the menu should be up at the end of it.
            ("@lib", true),
            ("look at @lib", true),
            ("mail me@example.com", false),
            ("@lib and then", false),
            ("nothing at all", false),
            ("@", true),
        ];

        for (text, expected) in cases {
            let directory = project();
            let mut app = app_in(&directory);
            typed(&mut app, text).await;

            assert_eq!(
                app.files.is_some(),
                expected,
                "{text:?} should {}have raised the menu",
                if expected { "" } else { "not " }
            );
        }
    }

    /// Moving the cursor back out of a mention closes the menu, and moving it
    /// back in opens it again — the trigger is about where the cursor is, not
    /// about what was typed.
    #[tokio::test]
    async fn moving_the_cursor_out_of_a_mention_closes_the_menu() {
        let directory = project();
        let mut app = app_in(&directory);
        typed(&mut app, "@lib").await;
        assert!(app.files.is_some());

        app.handle(key(KeyCode::Home, KeyModifiers::NONE))
            .await
            .expect("home is handled");

        assert!(app.files.is_none(), "the cursor is in front of the `@` now");
    }

    #[tokio::test]
    async fn choosing_a_file_writes_the_mention_into_the_buffer() {
        let directory = project();
        let mut app = app_in(&directory);
        typed(&mut app, "compare @lib").await;

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("compare @src/lib.rs "),
            "the fragment should have become the whole path, with room after it"
        );
        assert!(app.files.is_none(), "choosing closes the menu");
        assert_eq!(
            app.editor.cursor(),
            (0, "compare @src/lib.rs ".chars().count()),
            "the cursor follows what was inserted"
        );
    }

    /// **F1**: Tab reaches the identical outcome as Enter in the file menu —
    /// upstream's own binding falls through to the same selection unless the
    /// row is a directory, and this build's `@` walker never yields one
    /// (`glob.rs` filters to `is_file()`).
    #[tokio::test]
    async fn tab_on_the_file_menu_completes_the_mention_the_same_as_enter() {
        let directory = project();
        let mut app = app_in(&directory);
        typed(&mut app, "compare @lib").await;

        app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .expect("tab is handled");

        assert_eq!(app.editor.prompt().as_deref(), Some("compare @src/lib.rs "),);
        assert!(app.files.is_none(), "choosing closes the menu");
    }

    /// A mention in the middle of a sentence is replaced in place, and the
    /// cursor stays where the user was writing rather than jumping to the end.
    #[tokio::test]
    async fn a_mention_mid_sentence_is_replaced_without_moving_the_rest() {
        let directory = project();
        let mut app = app_in(&directory);
        typed(&mut app, "look at @lib and say why").await;
        // Back into the mention: nine characters from the end.
        for _ in 0.."and say why".chars().count() + 1 {
            app.handle(key(KeyCode::Left, KeyModifiers::NONE))
                .await
                .expect("left is handled");
        }
        settle_file_menu(&mut app).await;
        assert!(
            app.files.is_some(),
            "the cursor is inside the mention again"
        );

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("look at @src/lib.rs and say why"),
            "the space already after the mention is not doubled"
        );
        assert_eq!(
            app.editor.cursor(),
            (0, "look at @src/lib.rs".chars().count())
        );
    }

    /// **Non-vacuity target for the submit scan.** Dropping `mention::scan`
    /// from `submit` — sending `Vec::new()` the way the composer did before
    /// mentions existed — fails this test on the `File` part.
    ///
    /// The fixture project is what both mentions name, because the scan is now
    /// filtered by whether the file is there (**D113**): before that, this
    /// test read whichever directory the runner happened to start in.
    #[tokio::test]
    async fn a_submitted_prompt_carries_its_mentions_and_keeps_their_text() {
        let directory = project();
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, None, Themes::builtin())
            .with_cwd(directory.path())
            .with_root(directory.path());

        typed(&mut app, "compare @src/lib.rs with @README.md").await;
        // The menu is still up over the mention the cursor is in, and it owns
        // Enter — so the way to send this is the way a person sends it.
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let CoreEvent::MessageStarted {
            session_id: _,
            message,
        } = events.next().await.expect("the engine reports the prompt")
        else {
            panic!("the first event of a turn is the user's message");
        };

        let attached: Vec<&str> = message
            .parts
            .iter()
            .filter_map(|part| match &part.body {
                PartBody::File { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            attached,
            vec!["src/lib.rs", "README.md"],
            "both mentions should have reached the engine"
        );

        let text: String = message
            .parts
            .iter()
            .filter_map(ganja_protocol::Part::as_text)
            .collect();
        assert_eq!(
            text, "compare @src/lib.rs with @README.md",
            "the literal tokens stay in the prompt, as upstream leaves them"
        );
    }

    /// The range rides the fragment while it is typed, and the walk sees only
    /// the path half — so the menu keeps offering the file the range is for.
    #[tokio::test]
    async fn a_range_being_typed_does_not_narrow_the_file_menu() {
        let directory = project();
        let mut app = app_in(&directory);

        typed(&mut app, "@lib#10-20").await;

        assert_eq!(offered(&app), vec!["src/lib.rs".to_owned()]);
    }

    /// Choosing a file keeps the typed range, normalized: upstream re-appends
    /// `#start` or `#start-end` to the chosen path (`autocomplete.tsx:250`),
    /// dropping an empty end and a reversed one.
    #[tokio::test]
    async fn choosing_a_file_keeps_the_normalized_typed_range() {
        let cases = [
            ("compare @lib#10-20", "compare @src/lib.rs#10-20 "),
            ("compare @lib#5-", "compare @src/lib.rs#5 "),
            ("compare @lib#20-10", "compare @src/lib.rs#20 "),
        ];

        for (text, expected) in cases {
            let directory = project();
            let mut app = app_in(&directory);
            typed(&mut app, text).await;

            app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .expect("enter is handled");

            assert_eq!(app.editor.prompt().as_deref(), Some(expected), "{text:?}");
        }
    }

    /// The submit-time half of graceful degradation: a binary mention the
    /// selected wire cannot carry is named in the status line before the
    /// turn, so the engine-side text block is never the first the user hears
    /// of it.
    #[tokio::test]
    async fn submitting_a_binary_mention_the_wire_refuses_warns_in_the_status_line() {
        let directory = project();
        fs::write(directory.path().join("shot.png"), b"png-bytes").expect("the fixture writes");

        let (provider, _requests) = ganja_testkit::ScriptedProvider::text_only(Vec::new());
        let engine = Engine::new(
            provider,
            "recorder-model",
            Arc::new(ganja_tool::Registry::new(Vec::new())),
            ganja_permission::Permissions::default(),
        );
        let mut app = App::new(engine, None, Themes::builtin())
            .with_cwd(directory.path())
            .with_root(directory.path());

        typed(&mut app, "look at @shot.png").await;
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let bar = status_line(&mut app);
        assert!(
            bar.contains("@shot.png (image/png)"),
            "the warning names the file and its mime: {bar}"
        );
        assert!(
            bar.contains("does not carry"),
            "and says why the bytes will not travel: {bar}"
        );
    }

    /// The other half: a wire that carries the mime warns about nothing.
    #[tokio::test]
    async fn submitting_a_binary_mention_the_wire_carries_warns_about_nothing() {
        let directory = project();
        fs::write(directory.path().join("shot.png"), b"png-bytes").expect("the fixture writes");

        let mut app = App::new(engine(), None, Themes::builtin())
            .with_cwd(directory.path())
            .with_root(directory.path());

        typed(&mut app, "look at @shot.png").await;
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let bar = status_line(&mut app);
        assert!(
            !bar.contains("attached by name only"),
            "the fake carries every mime, so there is nothing to warn about: {bar}"
        );
    }

    /// **D11** again, on the other menu: closing is not deleting.
    #[tokio::test]
    async fn escape_closes_the_file_menu_and_keeps_what_was_typed() {
        let directory = project();
        let mut app = app_in(&directory);
        typed(&mut app, "look at @lib").await;

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert!(app.files.is_none());
        assert_eq!(app.editor.prompt().as_deref(), Some("look at @lib"));
    }

    /// **Non-vacuity target for the `!` consume.** Letting the keystroke fall
    /// through to the editor — dropping the flip arm — leaves a `!` in the
    /// buffer and fails the second assertion here.
    #[tokio::test]
    async fn an_exclamation_at_the_start_flips_to_shell_mode_and_is_never_typed() {
        let mut app = app();

        typed(&mut app, "!").await;

        assert_eq!(app.editor.mode(), Mode::Shell);
        assert!(
            app.editor.is_empty(),
            "the `!` is the mode switch, not a character: {:?}",
            app.editor.text()
        );
    }

    #[tokio::test]
    async fn an_exclamation_anywhere_else_is_a_character_like_any_other() {
        let mut app = app();

        typed(&mut app, "wow!").await;

        assert_eq!(app.editor.mode(), Mode::Prompt);
        assert_eq!(app.editor.prompt().as_deref(), Some("wow!"));
    }

    /// Upstream's gate is the cursor, not the buffer: a `!` typed in front of
    /// existing text flips the mode and keeps the text as the command.
    #[tokio::test]
    async fn an_exclamation_in_front_of_existing_text_still_flips() {
        let mut app = app();
        typed(&mut app, "ls -la").await;
        app.handle(key(KeyCode::Home, KeyModifiers::NONE))
            .await
            .expect("home is handled");

        typed(&mut app, "!").await;

        assert_eq!(app.editor.mode(), Mode::Shell);
        assert_eq!(app.editor.prompt().as_deref(), Some("ls -la"));
    }

    #[tokio::test]
    async fn escape_and_backspace_at_the_start_are_the_two_ways_out_of_shell_mode() {
        for leaving in [KeyCode::Esc, KeyCode::Backspace] {
            let mut app = app();
            typed(&mut app, "!").await;
            typed(&mut app, "ls").await;
            app.handle(key(KeyCode::Home, KeyModifiers::NONE))
                .await
                .expect("home is handled");

            app.handle(key(leaving, KeyModifiers::NONE))
                .await
                .expect("the way out is handled");

            assert_eq!(app.editor.mode(), Mode::Prompt, "{leaving:?}");
            assert_eq!(
                app.editor.prompt().as_deref(),
                Some("ls"),
                "{leaving:?} should leave the mode, not eat the command"
            );
        }
    }

    /// Backspace anywhere but the front is still backspace.
    #[tokio::test]
    async fn backspace_inside_a_shell_command_deletes_rather_than_leaving() {
        let mut app = app();
        typed(&mut app, "!").await;
        typed(&mut app, "lsx").await;

        app.handle(key(KeyCode::Backspace, KeyModifiers::NONE))
            .await
            .expect("backspace is handled");

        assert_eq!(app.editor.mode(), Mode::Shell);
        assert_eq!(app.editor.prompt().as_deref(), Some("ls"));
    }

    /// The whole passthrough, through the real engine: the command runs, the
    /// synthetic user message upstream writes lands in the transcript, and the
    /// composer comes back for the next prompt.
    #[tokio::test]
    async fn submitting_in_shell_mode_runs_the_command_and_comes_back() {
        let (mut app, mut events) = wired().await;
        typed(&mut app, "!").await;
        typed(&mut app, "echo hello").await;

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(
            app.editor.mode(),
            Mode::Prompt,
            "a command that was accepted leaves the composer ready for a prompt"
        );
        assert!(app.editor.is_empty());

        let CoreEvent::MessageStarted {
            session_id: _,
            message,
        } = events.next().await.expect("the engine reports the command")
        else {
            panic!("the first event of a passthrough is the synthetic user message");
        };
        let text: String = message
            .parts
            .iter()
            .filter_map(ganja_protocol::Part::as_text)
            .collect();
        assert_eq!(text, "The following tool was executed by the user");

        // Drain the rest so the test does not leave a turn streaming.
        while let Some(event) = events.next().await {
            if matches!(event, CoreEvent::MessageFinished { .. }) {
                break;
            }
        }
    }

    /// A refusal has to leave both halves alone: the text so the command can
    /// be tried again, and the mode so trying it again does not send it to the
    /// model instead.
    #[tokio::test]
    async fn a_refused_shell_command_keeps_the_text_and_the_mode() {
        let (mut app, mut events) = wired().await;
        typed(&mut app, "a turn to be busy with").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        pump(&mut app, &mut events, 2).await;

        typed(&mut app, "!").await;
        typed(&mut app, "echo hello").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(app.editor.mode(), Mode::Shell);
        assert_eq!(app.editor.prompt().as_deref(), Some("echo hello"));

        let mut terminal = terminal(120, 12);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("already streaming"),
            "the refusal should be readable:\n{}",
            screen(&terminal)
        );
    }

    /// Neither menu belongs in shell mode: a `/` there starts a path and an
    /// `@` is whatever the shell makes of it.
    #[tokio::test]
    async fn shell_mode_offers_neither_commands_nor_files() {
        let directory = project();
        let mut app = app_in(&directory);
        typed(&mut app, "!").await;

        typed(&mut app, "/usr").await;
        assert!(app.dropdown.is_none());

        app.editor.clear();
        typed(&mut app, "cat @lib").await;
        assert!(app.files.is_none());
    }

    /// An engine command expects arguments, so choosing it types its name and
    /// waits rather than running something with none.
    #[tokio::test]
    async fn choosing_an_engine_command_types_its_name_instead_of_running_it() {
        let (mut app, _events) = wired().await;
        typed(&mut app, "/init").await;

        assert_eq!(
            app.dropdown.as_ref().and_then(Dropdown::selected),
            Some(crate::command::Choice::Engine(
                crate::command::EngineCommand {
                    name: "init".to_owned(),
                    description: Some("guided AGENTS.md setup".to_owned()),
                    hint: None,
                }
            )),
            "the engine's own command should be under the cursor"
        );

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(
            app.editor.text(),
            "/init ",
            "the name is typed, with room for the arguments it takes"
        );
        assert!(app.dropdown.is_none());
    }

    /// Tab reaches the identical outcome as Enter for an engine command:
    /// Tab's own "complete without running" only changes anything for the UI
    /// half of the roster, which already types-and-waits on Enter.
    #[tokio::test]
    async fn tab_on_an_engine_command_types_its_name_the_same_as_enter() {
        let (mut app, _events) = wired().await;
        typed(&mut app, "/init").await;

        app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .expect("tab is handled");

        assert_eq!(app.editor.text(), "/init ");
        assert!(app.dropdown.is_none());
    }

    /// And the second Enter runs it, arguments and all.
    #[tokio::test]
    async fn submitting_an_engine_command_runs_it_with_what_was_typed_after_it() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, None, Themes::builtin());

        typed(&mut app, "/init focus on the test suite").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let CoreEvent::MessageStarted {
            session_id: _,
            message,
        } = events.next().await.expect("the engine reports the prompt")
        else {
            panic!("the first event of a turn is the user's message");
        };
        let text: String = message
            .parts
            .iter()
            .filter_map(ganja_protocol::Part::as_text)
            .collect();

        assert!(
            text.contains("AGENTS.md"),
            "the template should have been expanded, got: {text}"
        );
        assert!(
            text.contains("focus on the test suite"),
            "and the arguments should be in it, got: {text}"
        );
        assert!(
            app.editor.is_empty(),
            "a command that ran clears the composer"
        );
    }

    /// A slash this build does not know is not a command, so it is text. The
    /// engine has its own answer for an unknown command on the wire; the UI
    /// simply does not intercept what it cannot name.
    #[tokio::test]
    async fn an_unknown_slash_command_is_sent_as_the_text_it_is() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, None, Themes::builtin());

        // Trailing space, so the menu is closed and Enter reaches the submit.
        typed(&mut app, "/nonesuch please ").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let CoreEvent::MessageStarted {
            session_id: _,
            message,
        } = events.next().await.expect("the engine reports the prompt")
        else {
            panic!("the first event of a turn is the user's message");
        };
        let text: String = message
            .parts
            .iter()
            .filter_map(ganja_protocol::Part::as_text)
            .collect();

        assert_eq!(text, "/nonesuch please ");
    }

    /// The dropdown is not the only door to a built-in: the menu closes the
    /// moment a space follows the name, so the Enter that follows must read
    /// the text itself — Claude Code and Codex both dispatch on the submitted
    /// line, not on the menu's state.
    #[tokio::test]
    async fn a_ui_command_with_a_trailing_space_still_runs_on_enter() {
        let (mut app, _events) = wired().await;

        typed(&mut app, "/exit ").await;
        assert!(app.dropdown.is_none(), "the space closed the menu");

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.quit, "the command should have run, not been sent");
        assert!(
            app.editor.is_empty(),
            "a command that ran clears the composer"
        );
    }

    /// The sharpest spelling of the same edge: Tab fills `/exit ` without
    /// running it (**D446**), so the Enter that follows has to mean the
    /// command that was just completed.
    #[tokio::test]
    async fn tab_completion_then_enter_runs_the_command_it_completed() {
        let (mut app, _events) = wired().await;
        typed(&mut app, "/exi").await;

        app.handle(key(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .expect("tab is handled");
        assert_eq!(app.editor.text(), "/exit ");

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.quit, "tab completed the command and enter ran it");
    }

    /// A built-in takes no arguments, so a name followed by more words is
    /// prose — the same ruling that sends an unknown slash command to the
    /// model rather than intercepting it.
    #[tokio::test]
    async fn a_ui_command_followed_by_arguments_is_sent_as_the_text_it_is() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, None, Themes::builtin());

        typed(&mut app, "/exit now ").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(!app.quit, "words after the name make it prose");
        let CoreEvent::MessageStarted {
            session_id: _,
            message,
        } = events.next().await.expect("the engine reports the prompt")
        else {
            panic!("the first event of a turn is the user's message");
        };
        let text: String = message
            .parts
            .iter()
            .filter_map(ganja_protocol::Part::as_text)
            .collect();

        assert_eq!(text, "/exit now ");
    }

    /// A UI command acts on the frontend, not on the running turn, so it runs
    /// now rather than steering its own name into the conversation — the
    /// palette already dispatches any of these mid-turn.
    #[tokio::test]
    async fn a_ui_command_submitted_mid_turn_runs_instead_of_steering() {
        let (mut app, _events) = wired().await;
        app.turn_running = true;

        typed(&mut app, "/exit ").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.quit, "the command should have run instead of queueing");
    }

    #[tokio::test]
    async fn new_session_empties_the_screen_the_old_one_filled() {
        let mut app = app();
        palette_transcript(&mut app);
        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(screen(&terminal).contains("show me every color"));

        app.run_command(crate::command::Action::New).await;

        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            !screen(&terminal).contains("show me every color"),
            "the previous conversation should be off the screen:\n{}",
            screen(&terminal)
        );
    }

    /// A refused reset leaves the user looking at the conversation they are
    /// still in, rather than at a blank screen the engine never agreed to.
    #[tokio::test]
    async fn a_refused_new_session_leaves_the_transcript_alone() {
        let (mut app, mut events) = wired().await;
        typed(&mut app, "a turn to be busy with").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        pump(&mut app, &mut events, 2).await;

        app.run_command(crate::command::Action::New).await;

        let mut terminal = terminal(120, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("a turn to be busy with"), "got:\n{screen}");
        assert!(screen.contains("already streaming"), "got:\n{screen}");
    }

    /// A compaction is a turn like any other from out here, so what proves the
    /// command reached the engine is that the engine ran one. This session has
    /// nothing to summarize yet, which is why the turn it runs is one that
    /// simply ends.
    #[tokio::test]
    async fn compact_reaches_the_engine_as_a_turn_of_its_own() {
        let (mut app, mut events) = wired().await;

        app.run_command(crate::command::Action::Compact).await;

        let event = events.next().await.expect("the engine runs the turn");
        assert!(
            matches!(event, CoreEvent::MessageFinished { .. }),
            "got {event:?}"
        );
        assert!(!app.status.is_streaming());
    }

    /// Both of the commands the palette gained reach something.
    #[tokio::test]
    async fn the_palette_reaches_the_commands_this_wave_added() {
        // `/new` empties the screen the old conversation filled.
        let (mut app, _events) = wired().await;
        palette_transcript(&mut app);
        app.draw(&mut terminal(80, 24)).expect("a frame draws");
        assert_ne!(
            app.chat.line_count(),
            0,
            "there has to be something to clear"
        );

        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        typed(&mut app, "new").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert_eq!(app.chat.line_count(), 0, "/new should have cleared it");

        // `/compact` reaches the engine, which answers with a turn.
        let (mut app, mut events) = wired().await;
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");
        typed(&mut app, "compact").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(
            events.next().await.is_some(),
            "/compact should have reached the engine"
        );
    }

    /// A child session belongs to the task call that spawned it, and is
    /// rendered on that call's row; offering it here would be offering a
    /// resume into the middle of a delegated turn.
    #[tokio::test]
    async fn the_picker_lists_roots_only() {
        let directory = temporary();
        store_session(
            &directory,
            "0198f2c4-a1b0-7000-8000-000000000014",
            Some("the conversation"),
            1_000,
            0,
            10,
        );
        store_child(
            &directory,
            "0198f2c4-a1b0-7000-8000-000000000015",
            "0198f2c4-a1b0-7000-8000-000000000014",
        );
        let mut app = persistent_app(&directory);

        app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .expect("control-s is handled");

        let listed: Vec<String> = app
            .sessions
            .as_ref()
            .map(|sessions| {
                let mut cursor = sessions.clone();
                let mut seen = Vec::new();
                cursor.move_selection(-99);
                for _ in 0..8 {
                    if let Some(info) = cursor.selected() {
                        seen.push(info.id.as_str().to_owned());
                    }
                    cursor.move_selection(1);
                }
                seen.dedup();
                seen
            })
            .unwrap_or_default();

        assert_eq!(
            listed,
            vec!["0198f2c4-a1b0-7000-8000-000000000014".to_owned()],
            "a delegated turn's session is not one to resume into"
        );

        // And the row it drew is the root's, by the title a person picks by.
        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("the conversation"), "got:\n{screen}");
        assert!(
            !screen.contains("@explore subagent"),
            "the child's own title has no business in the picker:\n{screen}"
        );
    }

    /// A task tool part, in whichever state, on the message the engine
    /// streamed it on.
    async fn task_part(app: &mut App, state: ToolState) {
        let reply = Message::assistant("canned");
        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            session_id: session(),
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::core(CoreEvent::PartUpdated {
            session_id: session(),
            message_id: reply.id,
            part: Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "task".to_owned(),
                    state,
                },
            },
        }))
        .await
        .expect("a task update is handled");
    }

    /// The progress the task tool republishes on its own part is the only
    /// window a frontend has into a child, so it has to reach the screen.
    #[tokio::test]
    async fn a_delegated_turns_progress_reaches_the_transcript() {
        let mut app = app();
        task_part(
            &mut app,
            ToolState::Running {
                input: serde_json::json!({
                    "description": "find the parser",
                    "subagent_type": "explore",
                }),
                metadata: serde_json::json!({"current_tool": "grep parser", "toolcalls": 3}),
                started: 0,
            },
        )
        .await;

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        assert!(
            screen.contains("\u{25cf} Task(agent: \"explore\""),
            "got:\n{screen}"
        );
        assert!(screen.contains("find the parser"), "got:\n{screen}");
        assert!(screen.contains("\u{23bf} grep parser"), "got:\n{screen}");
    }

    #[tokio::test]
    async fn snapshot_file_menu_open() {
        let directory = project();
        let mut app = app_in(&directory);
        palette_transcript(&mut app);

        typed(&mut app, "compare @lib").await;

        assert!(
            app.files.is_some(),
            "the menu must be open, or the snapshot is of a bare screen"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_shell_mode() {
        let mut app = app();
        palette_transcript(&mut app);

        typed(&mut app, "!").await;
        typed(&mut app, "cargo nextest run --workspace").await;

        assert_eq!(app.editor.mode(), Mode::Shell, "the mode must have flipped");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_shell_output_streaming() {
        let mut app = app();
        let reply = Message::assistant("canned");
        app.handle(AppEvent::core(CoreEvent::MessageStarted {
            session_id: session(),
            message: reply.clone(),
        }))
        .await
        .expect("a message start is handled");
        app.handle(AppEvent::core(CoreEvent::PartUpdated {
            session_id: session(),
            message_id: reply.id,
            part: Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "bash".to_owned(),
                    state: ToolState::Running {
                        input: serde_json::json!({"command": "cargo nextest run"}),
                        metadata: serde_json::json!({
                            "output": "    Starting 517 tests\n\
                                       PASS [   0.004s] ganja-core permission\n\
                                       PASS [   0.006s] ganja-core storage\n\
                                       PASS [   0.011s] ganja-tui app\n\
                                       PASS [   0.012s] ganja-tui chat\n\
                                       PASS [   0.019s] ganja-cli import"
                        }),
                        started: 0,
                    },
                },
            },
        }))
        .await
        .expect("a running update is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_task_running() {
        let mut app = app();
        task_part(
            &mut app,
            ToolState::Running {
                input: serde_json::json!({
                    "description": "find every caller of resolve",
                    "subagent_type": "explore",
                }),
                metadata: serde_json::json!({"current_tool": "grep resolve", "toolcalls": 4}),
                started: 0,
            },
        )
        .await;

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_task_completed() {
        let mut app = app();
        task_part(
            &mut app,
            ToolState::Completed {
                input: serde_json::json!({
                    "description": "find every caller of resolve",
                    "subagent_type": "explore",
                }),
                output: "<task id=\"tsk_1\" state=\"completed\"><task_result>\
                         four callers, all in session.rs</task_result></task>"
                    .to_owned(),
                title: "find every caller of resolve".to_owned(),
                metadata: serde_json::json!({
                    "session": "ses_child",
                    "agent": "explore",
                    "model": fake::MODEL,
                    "toolcalls": 9,
                }),
                started: 1_000,
                completed: 24_500,
            },
        )
        .await;

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_permission_dialog_with_directories() {
        let mut app = app();
        app.handle(AppEvent::core(CoreEvent::PermissionRequested {
            session_id: session(),
            id: PermissionId::from("perm_1".to_owned()),
            call_id: "call_1".to_owned(),
            tool: "bash".to_owned(),
            title: "cp report.md /var/www/html".to_owned(),
            args: serde_json::json!({"command": "cp report.md /var/www/html"}),
            directories: vec!["/var/www/html".to_owned(), "/etc/nginx".to_owned()],
        }))
        .await
        .expect("a permission request is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn a_mention_fragment_anchors_on_whatever_directories_it_names() {
        let cases = [
            ("", "**/*"),
            ("lib", "**/*lib*"),
            ("src/", "**/src/**"),
            ("src/li", "**/src/**/*li*"),
            ("crates/ganja-tui/src/", "**/crates/ganja-tui/src/**"),
            // A leading slash names no directory to anchor on.
            ("/lib", "**/*lib*"),
            ("/", "**/*"),
        ];

        for (fragment, expected) in cases {
            assert_eq!(super::pattern(fragment), expected, "{fragment:?}");
        }
    }

    #[test]
    fn only_the_paths_under_the_walk_are_offered_and_only_the_first_ten() {
        let cwd = std::path::Path::new("/project");
        let listed = (0..14)
            .map(|index| format!("/project/src/file_{index:02}.rs"))
            .collect::<Vec<_>>()
            .join("\n");
        // What `glob` appends when it capped its own result, and what it says
        // when it found nothing — neither is a path.
        let output = format!(
            "{listed}\n\n(Results are truncated: showing first 100 results. \
             Consider using a more specific path or pattern.)"
        );

        let offered = super::relative_paths(cwd, &output);

        assert_eq!(offered.len(), 10, "got {offered:?}");
        assert_eq!(offered[0], "src/file_00.rs");
        assert!(
            offered.iter().all(|path| path.starts_with("src/")),
            "the sentence is not a path: {offered:?}"
        );
        assert!(super::relative_paths(cwd, "No files found").is_empty());
    }

    // ---- clipboard, paste, and the mention filter ----

    /// A project root holding `files`, each written with its own name.
    fn project_holding(files: &[&str]) -> TempDir {
        let root = temporary();

        for file in files {
            let path = root.path().join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("the parent directory is creatable");
            }
            std::fs::write(&path, file).expect("the fixture file is writable");
        }

        root
    }

    /// Everything the status bar is currently saying.
    fn status_line(app: &mut App) -> String {
        let mut terminal = terminal(120, 12);
        app.draw(&mut terminal).expect("a frame draws");

        screen(&terminal)
            .lines()
            .next_back()
            .unwrap_or_default()
            .to_owned()
    }

    /// Submits `text` and hands back every `File` part the engine put on the
    /// user message it made of it.
    async fn submitted_files(root: &TempDir, text: &str) -> Vec<String> {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, None, Themes::builtin())
            .with_cwd(root.path())
            .with_root(root.path());

        for event in typing(text) {
            app.handle(event).await.expect("typing is handled");
        }
        // A menu the last token raised owns Enter until it is closed; Esc
        // means "cancel the turn" when there is no menu, so it is only sent
        // when there is one.
        if app.files.is_some() {
            app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
                .await
                .expect("escape is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let CoreEvent::MessageStarted {
            session_id: _,
            message,
        } = events.next().await.expect("the engine reports the prompt")
        else {
            panic!("the first event of a turn is the user's message");
        };

        message
            .parts
            .iter()
            .filter_map(|part| match &part.body {
                PartBody::File { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    /// **D113 / R15(a)**, at the seam a person actually types into: `@alice`
    /// is a person, and attaching her would put an attachment-error block in
    /// front of the model in place of the sentence that was written.
    #[tokio::test]
    async fn a_word_that_names_no_file_is_submitted_as_text_and_attaches_nothing() {
        let root = project_holding(&["notes.md"]);

        assert!(
            submitted_files(&root, "ask @alice about it")
                .await
                .is_empty(),
            "a name should attach nothing"
        );
    }

    #[tokio::test]
    async fn a_mention_that_names_a_real_file_still_attaches_it() {
        let root = project_holding(&["notes.md"]);

        assert_eq!(
            submitted_files(&root, "read @notes.md please").await,
            vec!["notes.md".to_owned()]
        );
    }

    /// The degradation the ruling asks for: the typo rides into the prompt as
    /// text the model can see and act on, beside the mention that resolved.
    #[tokio::test]
    async fn a_mistyped_path_rides_as_visible_text_beside_the_one_that_resolved() {
        let root = project_holding(&["notes.md"]);
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, None, Themes::builtin())
            .with_cwd(root.path())
            .with_root(root.path());

        for event in typing("compare @notes.md with @notez.md") {
            app.handle(event).await.expect("typing is handled");
        }
        // The cursor is still inside the second mention, so the file menu owns
        // Enter — closing it is how a person sends this.
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let CoreEvent::MessageStarted {
            session_id: _,
            message,
        } = events.next().await.expect("the engine reports the prompt")
        else {
            panic!("the first event of a turn is the user's message");
        };

        let attached: Vec<&str> = message
            .parts
            .iter()
            .filter_map(|part| match &part.body {
                PartBody::File { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        let text: String = message.parts.iter().filter_map(Part::as_text).collect();

        assert_eq!(attached, vec!["notes.md"], "only the file that exists");
        assert!(
            text.contains("@notez.md"),
            "the typo has to reach the model as text: {text}"
        );
    }

    /// A pasted paragraph is content, not keystrokes — which is the whole
    /// reason bracketed paste is enabled. Both line endings a terminal may
    /// send become the line breaks they mean.
    #[tokio::test]
    async fn a_bracketed_paste_lands_at_the_cursor_with_its_line_breaks_intact() {
        let mut app = app();
        for event in typing("see: ") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(AppEvent::Term(TermEvent::Paste(
            "one\r\ntwo\rthree".to_owned(),
        )))
        .await
        .expect("a paste is handled");

        assert_eq!(
            app.editor.text(),
            "see: one\ntwo\nthree",
            "CRLF and a lone CR both mean a line break"
        );
    }

    /// The failure this replaces: fed through the key handler, the newline in
    /// the middle of a paste is an Enter, and Enter here sends the prompt.
    #[tokio::test]
    async fn a_multi_line_paste_does_not_submit_the_prompt() {
        let (mut app, mut events) = wired().await;

        app.handle(AppEvent::Term(TermEvent::Paste(
            "first line\nsecond line".to_owned(),
        )))
        .await
        .expect("a paste is handled");

        assert_eq!(app.editor.text(), "first line\nsecond line");
        assert!(
            events.next().now_or_never().is_none(),
            "nothing should have been sent to the engine"
        );
    }

    /// **F5**: a dropped path becomes a mention instead of landing as raw
    /// text — the same insertion the `@` menu's own Enter uses.
    #[tokio::test]
    async fn a_dropped_path_pastes_as_a_mention() {
        let directory = project();
        let mut app = app_in(&directory);

        app.handle(AppEvent::Term(TermEvent::Paste("src/lib.rs".to_owned())))
            .await
            .expect("a paste is handled");

        assert_eq!(app.editor.text(), "@src/lib.rs ");
    }

    /// A dropped **image** is the picture, not its path (2026-08-15): the
    /// drop inserts the same `[Image #N]` token every other door does, so
    /// the strip previews it and the composer never carries the filesystem
    /// spelling the user asked to be rid of.
    #[tokio::test]
    async fn a_dropped_image_path_becomes_an_image_token() {
        let directory = project();
        fs::write(directory.path().join("shot.png"), b"png-ish").expect("the image exists");
        let mut app = app_in(&directory);

        app.handle(AppEvent::Term(TermEvent::Paste("shot.png".to_owned())))
            .await
            .expect("a paste is handled");

        assert_eq!(
            app.editor.text(),
            "[Image #1] ",
            "the picture's token, never its path"
        );
        assert_eq!(
            app.pasted_images_in("[Image #1]")
                .into_iter()
                .map(|mention| mention.path)
                .collect::<Vec<_>>(),
            vec!["shot.png".to_owned()],
            "and the token names the dropped file"
        );
    }

    /// Several paths pasted at once — the way a terminal hands over a
    /// multi-file drag — become several mentions, in the order they arrived.
    #[tokio::test]
    async fn a_multi_file_drop_pastes_as_mentions_in_order() {
        let directory = project();
        let mut app = app_in(&directory);

        app.handle(AppEvent::Term(TermEvent::Paste(
            "README.md src/lib.rs".to_owned(),
        )))
        .await
        .expect("a paste is handled");

        assert_eq!(app.editor.text(), "@README.md @src/lib.rs ");
    }

    /// A pasted shell one-liner that happens to name a real path stays
    /// ordinary text: not every token in it is a path, and the classifier
    /// refuses to half-transform the line.
    #[tokio::test]
    async fn a_pasted_shell_one_liner_stays_raw_text() {
        let directory = project();
        let mut app = app_in(&directory);

        app.handle(AppEvent::Term(TermEvent::Paste(
            "cat src/lib.rs | grep x".to_owned(),
        )))
        .await
        .expect("a paste is handled");

        assert_eq!(app.editor.text(), "cat src/lib.rs | grep x");
    }

    #[tokio::test]
    async fn a_paste_while_a_dialog_is_up_does_not_reach_the_composer_behind_it() {
        let mut app = app();
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await
            .expect("control-p is handled");

        app.handle(AppEvent::Term(TermEvent::Paste("pasted".to_owned())))
            .await
            .expect("a paste is handled");

        assert!(app.palette.is_some(), "the palette is still up");
        assert!(app.editor.is_empty(), "the composer behind it is untouched");
    }

    #[tokio::test]
    async fn control_v_pastes_what_the_clipboard_holds() {
        let mut app = app().with_clipboard(Box::new(clipboard::Recording::holding(
            "pasted\nfrom the clipboard",
        )));

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("control-v is handled");

        assert_eq!(app.editor.text(), "pasted\nfrom the clipboard");
    }

    /// A clipboard holding neither text nor an image (**F3**, D111's image
    /// half) says so and eats no keystroke.
    #[tokio::test]
    async fn a_clipboard_holding_neither_text_nor_an_image_says_so() {
        let mut app = app().with_clipboard(Box::new(clipboard::Recording::default()));

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("control-v is handled");

        assert!(
            status_line(&mut app).contains("the clipboard holds neither text nor an image"),
            "got: {}",
            status_line(&mut app)
        );
        assert!(app.editor.is_empty(), "and nothing was inserted");
    }

    /// **F3**, lifting D111's image half — respelled 2026-08-15 to Claude
    /// Code's own composer token: a scripted clipboard image pastes as
    /// `[Image #N]` inline, and the bytes on disk are a real, decodable PNG
    /// of the scripted dimensions.
    #[tokio::test]
    async fn pasting_a_clipboard_image_shows_an_inline_image_token() {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255];
        let mut app = app()
            .with_clipboard(Box::new(clipboard::Recording::holding_image(
                3,
                1,
                rgba.clone(),
            )))
            .with_clipboard_scratch_dir(scratch.path());

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("control-v is handled");

        assert_eq!(
            app.editor.text(),
            "[Image #1] ",
            "the composer carries the token, not the scratch path"
        );

        let bytes = fs::read(scratch.path().join("clipboard-1.png")).expect("the image was saved");
        let (width, height, decoded) = decode_png(&bytes);
        assert_eq!((width, height), (3, 1), "the scripted dimensions survive");
        assert_eq!(decoded, rgba, "and so do the pixels");
    }

    /// The cursor lights the token it sits on — inside it or right after
    /// its bracket, where a fresh paste leaves it — and nothing else.
    #[test]
    fn the_token_under_the_cursor_is_found_and_prose_is_not() {
        let text = "see [Image #12] here";

        assert_eq!(
            super::image_token_at(text, 4),
            Some((4, 14, 12)),
            "its first char"
        );
        assert_eq!(super::image_token_at(text, 9), Some((4, 14, 12)), "inside");
        assert_eq!(
            super::image_token_at(text, 15),
            Some((4, 14, 12)),
            "right after the bracket"
        );
        assert_eq!(super::image_token_at(text, 3), None, "before it");
        assert_eq!(super::image_token_at(text, 16), None, "past it");
        assert_eq!(super::image_token_at("no tokens at all", 5), None);
        assert_eq!(
            super::image_token_at("[Image #] empty", 3),
            None,
            "digits are required"
        );
    }

    /// The offset math the highlight rides: rows rejoined on newlines,
    /// counted in characters, multibyte included.
    #[test]
    fn the_cursor_offset_counts_characters_across_lines() {
        assert_eq!(super::char_offset("abc\ndef", 1, 2), 6);
        assert_eq!(super::char_offset("見て\n[Image #1]", 1, 0), 3);
        assert_eq!(super::char_offset("abc", 0, 3), 3);
    }

    /// Backspace on a token the cursor lights takes the whole token —
    /// Claude Code's own composer rule, widened to
    /// anywhere inside it. At the token's very front it stays an ordinary
    /// one-character delete, because backspace there is about what sits
    /// before the token.
    #[tokio::test]
    async fn backspace_on_an_image_token_deletes_the_whole_token() {
        {
            let mut app = app();
            typed(&mut app, "see [Image #12] x").await;
            for _ in 0..3 {
                app.handle(key(KeyCode::Left, KeyModifiers::NONE))
                    .await
                    .expect("left is handled");
            }
            app.handle(key(KeyCode::Backspace, KeyModifiers::NONE))
                .await
                .expect("backspace is handled");
            assert_eq!(
                app.editor.text(),
                "see  x",
                "mid-token backspace takes the token whole"
            );
        }

        let mut app = app();
        typed(&mut app, "ab[Image #5]").await;
        for _ in 0..10 {
            app.handle(key(KeyCode::Left, KeyModifiers::NONE))
                .await
                .expect("left is handled");
        }
        app.handle(key(KeyCode::Backspace, KeyModifiers::NONE))
            .await
            .expect("backspace is handled");
        assert_eq!(
            app.editor.text(),
            "a[Image #5]",
            "at the token's front, backspace is about the character before it"
        );
    }

    /// Claude Code's focus-time nudge (observed behaviour): coming back
    /// to a terminal with an image on the clipboard says so once, and the
    /// thirty-second limit keeps a window-switching flurry to one hint.
    #[tokio::test]
    async fn regaining_focus_over_a_clipboard_image_hints_once() {
        let mut app = app().with_clipboard(Box::new(clipboard::Recording::holding_image(
            1,
            1,
            vec![1, 2, 3, 4],
        )));

        app.handle(AppEvent::Term(TermEvent::FocusGained))
            .await
            .expect("focus is handled");
        assert!(
            status_line(&mut app).contains("Image in clipboard"),
            "the hint names the ctrl+v door"
        );

        app.status.set_notice(None);
        app.handle(AppEvent::Term(TermEvent::FocusGained))
            .await
            .expect("focus is handled again");
        assert!(
            !status_line(&mut app).contains("Image in clipboard"),
            "the rate limit holds the second hint"
        );
    }

    /// Cmd+V over an image-only clipboard: the terminal has no text and
    /// sends the bracketed-paste envelope **empty**, and that emptiness
    /// routes to the clipboard chain — Claude Code's own mechanism
    /// (observed 2026-08-15) — so the system paste chord
    /// attaches the image exactly as Ctrl+V does.
    #[tokio::test]
    async fn an_empty_terminal_paste_reads_the_image_off_the_clipboard() {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let mut app = app()
            .with_clipboard(Box::new(clipboard::Recording::holding_image(
                1,
                1,
                vec![1, 2, 3, 4],
            )))
            .with_clipboard_scratch_dir(scratch.path());

        app.handle(AppEvent::Term(TermEvent::Paste(String::new())))
            .await
            .expect("an empty paste is handled");

        assert_eq!(
            app.editor.text(),
            "[Image #1] ",
            "the empty envelope reads the clipboard image instead"
        );
        assert!(scratch.path().join("clipboard-1.png").exists());
    }

    /// The pinned 2026-08-15 screenshot: a file copied in Finder rides the
    /// pasteboard as its URL *and* its bare name as text, and pasting it must
    /// consult the files first — asked for text, the paste inserted
    /// `Screenshot ….png`, a basename that resolves nowhere. An image file
    /// tokenizes as `[Image #N]` mapped to the copied file itself, no scratch
    /// copy made.
    #[tokio::test]
    async fn pasting_a_copied_image_file_tokenizes_the_file_itself() {
        let dir = tempfile::tempdir().expect("a directory for the copied file");
        let copied = dir.path().join("Screenshot 2026-08-15 at 10.29.02.png");
        fs::write(&copied, b"not-really-a-png").expect("the copied file exists");
        let mut app = app().with_clipboard(Box::new(clipboard::Recording::holding_files(vec![
            copied.clone(),
        ])));

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("control-v is handled");

        assert_eq!(
            app.editor.text(),
            "[Image #1] ",
            "the copied image is a token, not its basename as text"
        );
        assert_eq!(
            app.pasted_images_in("[Image #1]")
                .into_iter()
                .map(|mention| mention.path)
                .collect::<Vec<_>>(),
            vec![copied.display().to_string()],
            "and the token names the copied file itself"
        );
    }

    /// A copied file that is not an image goes through the same classifier a
    /// typed or dropped path does — an existing file becomes an `@` mention.
    #[tokio::test]
    async fn pasting_a_copied_non_image_file_becomes_a_mention() {
        let dir = tempfile::tempdir().expect("a directory for the copied file");
        let copied = dir.path().join("notes.rs");
        fs::write(&copied, b"fn main() {}").expect("the copied file exists");
        let mut app = app().with_clipboard(Box::new(clipboard::Recording::holding_files(vec![
            copied.clone(),
        ])));

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("control-v is handled");

        assert_eq!(
            app.editor.text(),
            format!("@{} ", copied.display()),
            "a non-image file pastes as the mention its drop would"
        );
    }

    /// The token is the composer's face for the saved file: submitting a
    /// prompt that still carries it attaches the PNG exactly as an `@`
    /// mention would, while the text the model reads keeps the token.
    #[tokio::test]
    async fn a_submitted_image_token_attaches_the_saved_png() {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, None, Themes::builtin())
            .with_clipboard(Box::new(clipboard::Recording::holding_image(
                1,
                1,
                vec![1, 2, 3, 4],
            )))
            .with_clipboard_scratch_dir(scratch.path());

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("control-v is handled");
        for event in typing("what is this") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let CoreEvent::MessageStarted { message, .. } =
            events.next().await.expect("the engine reports the prompt")
        else {
            panic!("the first event of a turn is the user's message");
        };
        let files: Vec<&str> = message
            .parts
            .iter()
            .filter_map(|part| match &part.body {
                PartBody::File { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(
            files,
            vec![scratch.path().join("clipboard-1.png").display().to_string()],
            "the token's file rides the mentions"
        );
        assert!(
            message
                .parts
                .iter()
                .any(|part| part.as_text() == Some("[Image #1] what is this")),
            "and the text keeps the token: {:?}",
            message.parts
        );
    }

    /// A second paste in the same session earns its own name rather than
    /// overwriting the first.
    #[tokio::test]
    async fn a_second_clipboard_image_paste_gets_its_own_number() {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let mut app = app()
            .with_clipboard(Box::new(clipboard::Recording::holding_image(
                1,
                1,
                vec![1, 2, 3, 4],
            )))
            .with_clipboard_scratch_dir(scratch.path());

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("the first control-v is handled");
        app.editor.clear();
        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("the second control-v is handled");

        assert_eq!(app.editor.text(), "[Image #2] ");
        assert!(scratch.path().join("clipboard-2.png").exists());
        assert!(scratch.path().join("clipboard-1.png").exists());
        assert!(scratch.path().join("clipboard-2.png").exists());
    }

    /// The submit-time degradation warning (`App::degraded`) fires for a
    /// pasted image exactly as it does for an `@file` mention: the pasted
    /// image reaches the same pipeline, and a wire without image support is
    /// told so before the turn.
    #[tokio::test]
    async fn a_clipboard_image_on_a_wire_without_image_support_warns_at_submit() {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let (provider, _requests) = ganja_testkit::ScriptedProvider::text_only(Vec::new());
        let engine = Engine::new(
            provider,
            "recorder-model",
            Arc::new(ganja_tool::Registry::new(Vec::new())),
            ganja_permission::Permissions::default(),
        );
        let mut app = App::new(engine, None, Themes::builtin())
            .with_clipboard(Box::new(clipboard::Recording::holding_image(
                1,
                1,
                vec![9, 8, 7, 6],
            )))
            .with_clipboard_scratch_dir(scratch.path());

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("control-v is handled");

        // Asserted at the data level, not through the rendered status line:
        // a real `<XDG data>/ganja/clipboard` path is long enough that the
        // rendered bar can truncate it before the mime reaches the visible
        // width — a pre-existing, unrelated property of a one-line status
        // bar, not something this test should depend on. The mention comes
        // off the `[Image #N]` token's own map (2026-08-15), since the
        // composer no longer carries the path.
        let pasted = app.editor.text();
        let mentions = app.pasted_images_in(&pasted);
        assert_eq!(mentions.len(), 1, "the pasted image resolved as a mention");
        assert_eq!(
            app.degraded(&mentions),
            vec![format!(
                "@{} (image/png)",
                scratch.path().join("clipboard-1.png").display()
            )],
            "the pasted image is exactly what the degradation warning names"
        );

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let bar = status_line(&mut app);
        assert!(
            bar.contains("attached by name only") && bar.contains("does not carry"),
            "the degradation warning fires at submit: {bar}"
        );
    }

    /// A scratch directory that cannot be created — a plain file sitting
    /// where the directory would need to be — degrades to a notice naming
    /// why, the same posture `ganja-tool`'s own spill directory takes on a
    /// write it cannot make.
    #[tokio::test]
    async fn a_clipboard_image_that_cannot_be_saved_reports_why() {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let blocked = scratch.path().join("blocked");
        fs::write(&blocked, "not a directory").expect("the fixture writes");
        let mut app = app()
            .with_clipboard(Box::new(clipboard::Recording::holding_image(
                1,
                1,
                vec![1, 2, 3, 4],
            )))
            .with_clipboard_scratch_dir(&blocked);

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("control-v is handled");

        assert!(app.editor.is_empty(), "nothing was inserted");
        assert!(
            !status_line(&mut app).is_empty(),
            "the failure is reported rather than swallowed"
        );
    }

    /// Decodes `bytes` as a PNG, answering its declared width, height and raw
    /// RGBA8 pixels.
    fn decode_png(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().expect("a valid png header");
        let mut buffer = vec![0; reader.output_buffer_size().expect("a sized frame")];
        let info = reader.next_frame(&mut buffer).expect("a valid png frame");
        buffer.truncate(info.buffer_size());

        (info.width, info.height, buffer)
    }

    /// [`App::encode_clipboard_png`] round-trips through [`decode_png`] for
    /// the two edge shapes real screenshots stress: a single pixel, and
    /// dimensions with no shared factor to hide a stride bug behind.
    #[test]
    fn encoding_a_clipboard_image_round_trips_at_1x1() {
        let image = clipboard::Image {
            width: 1,
            height: 1,
            rgba: vec![10, 20, 30, 40],
        };

        let bytes = App::encode_clipboard_png(&image).expect("the encode succeeds");
        let (width, height, decoded) = decode_png(&bytes);

        assert_eq!((width, height), (1, 1));
        assert_eq!(decoded, image.rgba);
    }

    #[test]
    fn encoding_a_clipboard_image_round_trips_at_odd_dimensions() {
        let rgba: Vec<u8> = (0..(3 * 5 * 4)).map(|byte| byte as u8).collect();
        let image = clipboard::Image {
            width: 3,
            height: 5,
            rgba: rgba.clone(),
        };

        let bytes = App::encode_clipboard_png(&image).expect("the encode succeeds");
        let (width, height, decoded) = decode_png(&bytes);

        assert_eq!((width, height), (3, 5));
        assert_eq!(decoded, rgba);
    }

    /// A machine with no clipboard costs a notice, never the keystroke.
    #[tokio::test]
    async fn a_clipboard_that_cannot_be_reached_is_a_notice_and_not_a_lost_prompt() {
        let mut app = app().with_clipboard(Box::new(clipboard::Recording::refusing_reads(
            clipboard::Error::Unavailable("no display".to_owned()),
        )));
        for event in typing("half a thought") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("control-v is handled");

        assert!(status_line(&mut app).contains("no display"));
        assert_eq!(
            app.editor.text(),
            "half a thought",
            "what was being typed survives"
        );
    }

    /// A user who bound `ctrl+v` to something else gets what they asked for:
    /// the paste fallback is checked after the bindings, not before them.
    #[tokio::test]
    async fn a_rebound_control_v_reaches_its_binding_rather_than_the_clipboard() {
        let keys = crate::keybind::Keybinds::from_config(
            &[("app_exit".to_owned(), "ctrl+v".to_owned())]
                .into_iter()
                .collect(),
        )
        .expect("the binding parses");
        let mut app = app()
            .with_keybinds(keys)
            .with_clipboard(Box::new(clipboard::Recording::holding("not this")));

        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .await
            .expect("control-v is handled");

        assert!(app.quit, "the binding wins");
        assert!(app.editor.is_empty());
    }

    /// An assistant message carrying `texts`, as the transcript holds one.
    fn replied(texts: &[&str]) -> Message {
        Message {
            id: ganja_protocol::MessageId::ascending(),
            role: ganja_protocol::Role::Assistant,
            parts: texts.iter().map(|text| Part::text(*text)).collect(),
            time: ganja_protocol::MessageTime {
                created: 1,
                completed: Some(2),
            },
            model: Some(fake::MODEL.to_owned()),
            usage: None,
        }
    }

    #[tokio::test]
    async fn copying_a_message_hands_over_the_last_reply_alone() {
        let clipboard = clipboard::Recording::default();
        let log = clipboard.log();
        let mut app = app().with_clipboard(Box::new(clipboard));
        app.seed(vec![
            replied(&["an older answer"]),
            Message::user("and then?"),
            replied(&["  the newest answer", "in two parts  "]),
        ]);

        app.run_command(command::Action::CopyMessage).await;

        assert_eq!(
            *log.lock().expect("the lock holds"),
            vec!["the newest answer\nin two parts".to_owned()],
            "the parts join on a newline and the whole is trimmed"
        );
        assert!(
            status_line(&mut app).contains("Message copied to clipboard!"),
            "got: {}",
            status_line(&mut app)
        );
    }

    #[tokio::test]
    async fn a_copy_queues_the_osc52_escape_even_when_the_system_clipboard_fails() {
        // Upstream writes the terminal escape before it tries a system
        // method: on a headless or SSH box the escape is the only channel
        // that still delivers, so a desktop refusal must not suppress it.
        let mut app = app().with_clipboard(Box::new(clipboard::Recording::refusing_writes(
            clipboard::Error::Unavailable("no display".to_owned()),
        )));
        app.seed(vec![Message::user("and then?"), replied(&["the answer"])]);

        app.run_command(command::Action::CopyMessage).await;

        assert_eq!(
            app.pending_osc,
            vec![clipboard::osc52::sequence("the answer")],
            "one escape, queued regardless of the system half"
        );
        assert!(
            status_line(&mut app).contains("Failed to copy to clipboard"),
            "got: {}",
            status_line(&mut app)
        );
    }

    /// An app announcing through a capture buffer under the `tui` table
    /// `tui` parses to, plus the handle the assertions read (**D468**).
    fn notifying_app(tui: serde_json::Value) -> (App, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let config: ganja_core::config::TuiConfig =
            serde_json::from_value(tui).expect("the fixture is a tui table");
        let capture = crate::notify::Capture::default();
        let log = capture.log();
        let app = app().with_notifier(crate::notify::Notifier::over(config, Box::new(capture)));

        (app, log)
    }

    /// Every byte the notifier has written so far.
    fn notified(log: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<u8> {
        log.lock().expect("the capture lock holds").clone()
    }

    #[tokio::test]
    async fn an_unfocused_finished_turn_writes_exactly_one_osc9_notification() {
        let (mut app, log) = notifying_app(serde_json::json!({"notifications": true}));

        app.handle(AppEvent::Term(TermEvent::FocusLost))
            .await
            .expect("a focus event is handled");
        app.handle(finished(fake::MODEL, Usage::default()))
            .await
            .expect("a finish is handled");

        let bytes = String::from_utf8(notified(&log)).expect("the escape is utf-8");
        assert_eq!(
            bytes, "\x1b]9;turn complete\x07",
            "one sequence, whole, and nothing beside it"
        );
    }

    #[tokio::test]
    async fn a_focused_finished_turn_writes_nothing() {
        let (mut app, log) = notifying_app(serde_json::json!({"notifications": true}));

        app.handle(AppEvent::Term(TermEvent::FocusLost))
            .await
            .expect("a focus event is handled");
        app.handle(AppEvent::Term(TermEvent::FocusGained))
            .await
            .expect("a focus event is handled");
        app.handle(finished(fake::MODEL, Usage::default()))
            .await
            .expect("a finish is handled");

        assert!(
            notified(&log).is_empty(),
            "a watched terminal hears nothing"
        );
    }

    /// Crossterm only learns the state from the first focus event, so a turn
    /// finishing before one arrived runs on the assumption that somebody is
    /// watching — quiet-by-default (**D468**).
    #[tokio::test]
    async fn a_turn_finishing_before_any_focus_event_is_assumed_watched() {
        let (mut app, log) = notifying_app(serde_json::json!({"notifications": true}));

        app.handle(finished(fake::MODEL, Usage::default()))
            .await
            .expect("a finish is handled");

        assert!(notified(&log).is_empty());
    }

    #[tokio::test]
    async fn the_bel_method_writes_exactly_the_bell_byte() {
        let (mut app, log) =
            notifying_app(serde_json::json!({"notifications": true, "notification_method": "bel"}));

        app.handle(AppEvent::Term(TermEvent::FocusLost))
            .await
            .expect("a focus event is handled");
        app.handle(finished(fake::MODEL, Usage::default()))
            .await
            .expect("a finish is handled");

        assert_eq!(notified(&log), b"\x07");
    }

    #[tokio::test]
    async fn an_approval_only_config_notifies_on_a_permission_dialog_and_not_at_turn_end() {
        let (mut app, log) =
            notifying_app(serde_json::json!({"notifications": ["approval-requested"]}));
        app.handle(AppEvent::Term(TermEvent::FocusLost))
            .await
            .expect("a focus event is handled");

        app.handle(finished(fake::MODEL, Usage::default()))
            .await
            .expect("a finish is handled");
        assert!(
            notified(&log).is_empty(),
            "turn end is a moment this config did not ask for"
        );

        app.handle(AppEvent::core(permission_event("perm_1")))
            .await
            .expect("a permission request is handled");
        let bytes = String::from_utf8(notified(&log)).expect("the escape is utf-8");
        assert_eq!(
            bytes, "\x1b]9;approval requested: cargo test\x07",
            "the one asked-for moment announces, once"
        );
    }

    #[tokio::test]
    async fn copying_a_message_with_nothing_to_copy_says_which_kind_of_nothing() {
        let clipboard = clipboard::Recording::default();
        let log = clipboard.log();
        let mut app = app().with_clipboard(Box::new(clipboard));

        app.run_command(command::Action::CopyMessage).await;

        assert!(
            status_line(&mut app).contains("No assistant messages found"),
            "got: {}",
            status_line(&mut app)
        );
        assert!(
            log.lock().expect("the lock holds").is_empty(),
            "nothing was copied"
        );
    }

    #[tokio::test]
    async fn copying_the_transcript_hands_over_the_whole_conversation() {
        let directory = temporary();
        store_session(
            &directory,
            "0198f2c4-a1b0-7000-8000-000000000016",
            Some("a stored talk"),
            10_000,
            0,
            0,
        );
        let clipboard = clipboard::Recording::default();
        let log = clipboard.log();
        let mut app = persistent_app(&directory).with_clipboard(Box::new(clipboard));
        let stored = app
            .engine
            .resume(&SessionId::from(
                "0198f2c4-a1b0-7000-8000-000000000016".to_owned(),
            ))
            .await
            .expect("the stored session resumes");
        app.seed(stored);

        app.run_command(command::Action::Copy).await;

        let copied = log.lock().expect("the lock holds").join("");
        assert!(copied.starts_with("# a stored talk\n\n"), "got: {copied}");
        assert!(
            copied.contains("**Session ID:** 0198f2c4-a1b0-7000-8000-000000000016\n"),
            "got: {copied}"
        );
        assert!(
            copied.contains("## User\n\nwhat the picker is choosing between\n\n---\n\n"),
            "the conversation itself has to be in it: {copied}"
        );
        assert!(
            status_line(&mut app).contains("Session transcript copied to clipboard!"),
            "got: {}",
            status_line(&mut app)
        );
    }

    /// Upstream returns silently here; a person who asked for a copy is owed
    /// an answer (deviation: copy-with-no-session-says-so).
    #[tokio::test]
    async fn copying_the_transcript_before_there_is_one_says_so() {
        let mut app = app().with_clipboard(Box::new(clipboard::Recording::default()));

        app.run_command(command::Action::Copy).await;

        assert!(
            status_line(&mut app).contains("there is no session to copy yet"),
            "got: {}",
            status_line(&mut app)
        );
    }

    #[tokio::test]
    async fn a_clipboard_that_refuses_a_copy_says_so_in_upstreams_words() {
        let mut app = app().with_clipboard(Box::new(clipboard::Recording::refusing_writes(
            clipboard::Error::Unavailable("no display".to_owned()),
        )));
        app.seed(vec![replied(&["an answer"])]);

        app.run_command(command::Action::CopyMessage).await;

        let line = status_line(&mut app);
        assert!(line.contains("Failed to copy to clipboard"), "got: {line}");
        assert!(
            line.contains("no display"),
            "and it names the reason: {line}"
        );
    }

    // ---- the MCP status notice ----

    /// An engine holding one MCP server named `broken`, whose command is a
    /// path nothing can spawn.
    fn engine_dialling_a_missing_server(root: &TempDir) -> Engine {
        let config: std::collections::BTreeMap<String, ganja_core::config::McpServer> =
            serde_json::from_value(serde_json::json!({
                "broken": { "type": "local", "command": ["/nonexistent-ganja-fixture"] }
            }))
            .expect("the fixture is a config");

        engine().with_mcp(ganja_core::McpServers::new(config, root.path()))
    }

    /// **R3's fake-provider-notice pattern.** A server that cannot be reached
    /// costs its tools and one line of the status bar, and never the session.
    #[tokio::test]
    async fn a_server_that_cannot_be_reached_is_named_in_the_status_bar() {
        let root = temporary();
        let engine = engine_dialling_a_missing_server(&root);
        engine.connect_mcp();
        let mut app = App::new(engine, None, Themes::builtin()).watching_mcp(1);

        // Nothing is said while it is still being dialled: a server with no
        // status yet is one nothing has finished trying.
        let mut line = status_line(&mut app);
        for _ in 0..400 {
            if line.contains("mcp broken") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            app.handle(AppEvent::Tick).await.expect("a tick is handled");
            line = status_line(&mut app);
        }

        assert!(
            line.contains("mcp broken"),
            "the failed server should be named: {line}"
        );
        assert!(
            !app.pending_mcp(),
            "and once it has answered there is nothing left to wait for"
        );
    }

    #[tokio::test]
    async fn a_session_with_no_mcp_servers_says_nothing_about_them() {
        let mut app = app();

        for _ in 0..8 {
            app.handle(AppEvent::Tick).await.expect("a tick is handled");
        }

        assert!(
            !status_line(&mut app).contains("mcp"),
            "got: {}",
            status_line(&mut app)
        );
        assert!(
            !app.pending_mcp(),
            "and nothing keeps the loop awake looking for one"
        );
    }

    /// Without this, an idle app never wakes: `wants_wakeup` is what schedules
    /// the tick the poll rides on, and a failed server would sit unreported
    /// until the user's next keystroke.
    #[tokio::test]
    async fn a_session_still_dialling_keeps_waking_up_to_look() {
        let root = temporary();
        let mut app = App::new(
            engine_dialling_a_missing_server(&root),
            None,
            Themes::builtin(),
        )
        .watching_mcp(1);
        app.draw(&mut terminal(80, 24)).expect("a frame draws");

        assert!(!app.dirty, "the frame above cleared it");
        assert!(app.wants_wakeup(), "the dial is what keeps it awake");
    }

    // ---- F5: the `/mcp` dialog ----

    /// Runs `app` a few ticks, until at least one MCP server has answered.
    async fn wait_for_mcp_to_settle(app: &mut App) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline && app.engine.mcp_status().is_empty() {
            app.handle(AppEvent::Tick).await.expect("a tick is handled");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// `/mcp` lists every configured server with its status; a failed one
    /// offers Reconnect, and one that never lent a tool names no count.
    #[tokio::test]
    async fn slash_mcp_lists_every_configured_server_with_its_status_and_actions() {
        let root = temporary();
        let engine = engine_dialling_a_missing_server(&root);
        engine.connect_mcp();
        let mut app = App::new(engine, None, Themes::builtin()).watching_mcp(1);
        wait_for_mcp_to_settle(&mut app).await;

        app.run_command(command::Action::Mcp).await;

        let dialog = app.mcp_dialog.as_ref().expect("/mcp opens the dialog");
        let row = dialog
            .selected()
            .expect("the one configured server has a row");
        assert_eq!(row.name, "broken");
        assert_eq!(row.status, "Failed");
        assert_eq!(row.tools, None, "a failed server lends nothing to count");
        assert!(row.detail.is_some(), "a failed row names why");
        assert_eq!(row.actions, vec![mcp::Action::Reconnect]);
    }

    /// Login belongs on a remote server configured with `oauth` whatever its
    /// status — unlike Reconnect above, gated on `Failed` alone — and running
    /// it shows the URL to open in the row until the browser finishes it.
    ///
    /// Deliberately never connects the server (no `connect_mcp()`,
    /// `wait_for_mcp_to_settle` unused): a real connect attempt would read
    /// this machine's own credential store, which is exactly what this test
    /// must not touch — see `crates/ganja-core/tests/mcp_oauth.rs` for the
    /// isolated-process test that does exercise storage. `mcp_has_oauth` and
    /// `mcp_login_url`, which this test is about, read the config and the
    /// in-memory login map only.
    #[tokio::test]
    async fn login_appears_for_an_oauth_server_and_shows_the_url_while_it_runs() {
        let address = oauth_discovery_fixture().await;
        let root = temporary();
        let config: std::collections::BTreeMap<String, ganja_core::config::McpServer> =
            serde_json::from_value(serde_json::json!({
                "hub": {
                    "type": "remote",
                    "url": format!("http://{address}/mcp"),
                    "oauth": {},
                }
            }))
            .expect("the fixture is a config");
        let engine = engine().with_mcp(ganja_core::McpServers::new(config, root.path()));
        let mut app = App::new(engine, None, Themes::builtin());

        app.run_command(command::Action::Mcp).await;
        let dialog = app.mcp_dialog.as_ref().expect("/mcp opens the dialog");
        let row = dialog
            .selected()
            .expect("the one configured server has a row");
        assert_eq!(
            row.status, "dialling",
            "nothing has attempted a connect yet"
        );
        assert_eq!(
            row.actions,
            vec![mcp::Action::Login],
            "an oauth-configured server offers Login whatever its status"
        );

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter opens the row's one action");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter runs the chosen action");

        let dialog = app.mcp_dialog.as_ref().expect("the dialog stays open");
        let row = dialog.selected().expect("still the one row");
        assert_eq!(row.status, "Logging in", "{row:?}");
        assert!(
            row.detail
                .as_deref()
                .is_some_and(|detail| detail.starts_with("go to: http")),
            "the URL a person has to open should be shown: {row:?}"
        );
    }

    /// A loopback RFC 8414 authorization-server endpoint, answering only
    /// `/.well-known/oauth-authorization-server` — enough for
    /// `Servers::start_login`'s discovery step, which is as far as the test
    /// above runs it (no `/register`, since a server naming no registration
    /// endpoint is itself a real, exercised path — the fixed fallback client
    /// id — and one this test does not need to distinguish).
    async fn oauth_discovery_fixture() -> std::net::SocketAddr {
        use tokio::{
            io::{AsyncReadExt as _, AsyncWriteExt as _},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port is available");
        let address = listener.local_addr().expect("the socket has an address");

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 4096];
                    if stream.read(&mut buffer).await.is_err() {
                        return;
                    }
                    let body = serde_json::json!({
                        "authorization_endpoint": format!("http://{address}/authorize"),
                        "token_endpoint": format!("http://{address}/token"),
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        address
    }

    /// Esc closes the dialog from either of its two steps rather than
    /// stepping back to the first — [`App::handle_rewind_key`]'s own rule,
    /// answered the same way here.
    #[tokio::test]
    async fn esc_closes_the_mcp_dialog_from_either_step() {
        let root = temporary();
        let engine = engine_dialling_a_missing_server(&root);
        engine.connect_mcp();
        let mut app = App::new(engine, None, Themes::builtin()).watching_mcp(1);
        wait_for_mcp_to_settle(&mut app).await;

        app.run_command(command::Action::Mcp).await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter opens the failed row's actions");
        assert!(
            app.mcp_dialog
                .as_ref()
                .is_some_and(mcp::Mcp::is_choosing_action),
            "the one configured server is failed, so enter must open its actions"
        );

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.mcp_dialog.is_none(), "escape closes the whole dialog");
    }

    /// A row with nothing to choose leaves the dialog exactly as it was: no
    /// close, no action step, unlike the rewind picker's `(Current)` row.
    #[tokio::test]
    async fn enter_on_a_row_with_no_actions_leaves_the_dialog_open() {
        let root = temporary();
        let config: std::collections::BTreeMap<String, ganja_core::config::McpServer> =
            serde_json::from_value(serde_json::json!({
                "off": { "type": "local", "command": ["never-run"], "enabled": false }
            }))
            .expect("the fixture is a config");
        let engine = engine().with_mcp(ganja_core::McpServers::new(config, root.path()));
        let mut app = App::new(engine, None, Themes::builtin());

        app.run_command(command::Action::Mcp).await;
        let dialog = app.mcp_dialog.as_ref().expect("/mcp opens the dialog");
        assert_eq!(
            dialog.selected().map(|row| row.status.as_str()),
            Some("Disabled")
        );

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert!(app.mcp_dialog.is_some(), "a disabled row does not close it");
        assert!(
            !app.mcp_dialog
                .as_ref()
                .is_some_and(mcp::Mcp::is_choosing_action),
            "and it has nothing to choose, so no action step opens"
        );
    }

    /// Reconnect run from the dialog is not a UI-only gesture: it drives the
    /// engine's own `reconnect_mcp`, which spawns a fresh dial — proven by
    /// counting real invocations of the fixture command rather than trusting
    /// a status string.
    #[cfg(unix)]
    #[tokio::test]
    async fn reconnect_from_the_dialog_spawns_a_fresh_dial() {
        let root = temporary();
        let counter = root.path().join("attempts");
        let config: std::collections::BTreeMap<String, ganja_core::config::McpServer> =
            serde_json::from_value(serde_json::json!({
                "flaky": {
                    "type": "local",
                    "command": ["sh", "-c", format!("echo x >> {} ; exit 1", counter.display())],
                }
            }))
            .expect("the fixture is a config");
        let engine = engine().with_mcp(ganja_core::McpServers::new(config, root.path()));
        engine.connect_mcp();
        let mut app = App::new(engine, None, Themes::builtin()).watching_mcp(1);
        wait_for_mcp_to_settle(&mut app).await;
        let attempts = |path: &std::path::Path| {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .count()
        };
        assert_eq!(
            attempts(&counter),
            1,
            "the startup dial is the first attempt"
        );

        app.run_command(command::Action::Mcp).await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter opens the failed row's actions");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter on Reconnect runs it");

        assert!(
            app.mcp_dialog
                .as_ref()
                .is_some_and(|dialog| !dialog.is_choosing_action()),
            "running the action returns to the server list"
        );
        assert_eq!(
            attempts(&counter),
            2,
            "reconnect must have spawned a real second dial"
        );
    }

    /// The dialog, over one connected and one failed server (screenshot: no
    /// reference available, house `ListDialog`/`Rewind` chrome).
    #[tokio::test]
    async fn snapshot_mcp_dialog_open() {
        let root = temporary();
        let engine = engine_dialling_a_missing_server(&root);
        engine.connect_mcp();
        let mut app = App::new(engine, None, Themes::builtin()).watching_mcp(1);
        wait_for_mcp_to_settle(&mut app).await;

        app.run_command(command::Action::Mcp).await;

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    // ---- D470/D471: the `/context` and `/usage` panels ----

    /// A breakdown with something in every category over a small round
    /// window, shared by the panel tests below.
    fn breakdown_fixture() -> ganja_core::engine::ContextBreakdown {
        ganja_core::engine::ContextBreakdown {
            model: "claude-sonnet-5".to_owned(),
            system_prompt: 3_000,
            instructions: 2_000,
            tools_builtin: 11_000,
            tools_mcp: 1_000,
            tools_builtin_count: 12,
            tools_mcp_count: 3,
            skills: 500,
            conversation_user: 4_000,
            conversation_assistant: 8_500,
            window: Some(100_000),
            reserve: Some(10_000),
        }
    }

    /// `/context` opens the panel from the command roster — computed on the
    /// spot, so a fresh session with zero turns still gets one — and Esc
    /// closes it.
    #[tokio::test]
    async fn slash_context_opens_the_panel_and_esc_closes_it() {
        let mut app = app();
        assert_eq!(
            command::lookup("context").map(|entry| entry.action),
            Some(command::Action::Context),
            "/context is on the roster"
        );

        app.run_command(command::Action::Context).await;
        assert!(app.context_dialog.is_some(), "/context opens the panel");

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.context_dialog.is_none(), "escape closes it");
    }

    /// `/usage` opens its panel from the roster, and Esc closes it.
    #[tokio::test]
    async fn slash_usage_opens_the_panel_and_esc_closes_it() {
        let mut app = app();
        assert_eq!(
            command::lookup("usage").map(|entry| entry.action),
            Some(command::Action::Usage),
            "/usage is on the roster"
        );

        app.run_command(command::Action::Usage).await;
        assert!(app.usage_dialog.is_some(), "/usage opens the panel");

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.usage_dialog.is_none(), "escape closes it");
    }

    /// AC5: the `/usage` session line is the status bar's own `Totals`
    /// string — the same formatter, asserted against the same value the bar
    /// itself renders — never a second formatting of the same numbers.
    #[tokio::test]
    async fn the_usage_panel_shows_the_status_bars_own_totals_string() {
        let mut app = app();
        app.record(
            &MessageId::from("msg_1".to_owned()),
            &Usage {
                input_tokens: 1_200,
                output_tokens: 34,
                reasoning_tokens: 5,
                cache_read_tokens: 600,
                cache_write_tokens: 100,
            },
        );

        app.run_command(command::Action::Usage).await;
        let mut terminal = terminal(80, 30);
        app.draw(&mut terminal).expect("a frame draws");

        let segment = app.totals.segment();
        assert!(
            screen(&terminal).contains(&segment),
            "want the bar's own {segment:?} in:\n{}",
            screen(&terminal)
        );
    }

    /// The plan-limit meters Claude Code leads with ride a vendor usage API
    /// ganja does not speak: the panel carries the one honest line naming
    /// why, and no such meter is ever drawn (D471, plan Open question 2).
    #[tokio::test]
    async fn the_usage_panel_carries_no_plan_limit_meter_and_says_why() {
        let mut app = app();
        app.run_command(command::Action::Usage).await;
        let mut terminal = terminal(80, 30);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        assert!(
            screen.contains("plan limits unavailable on this credential (probed 2026-08-14):"),
            "got:\n{screen}"
        );
        // P17 (**D485**): the fake provider serves no plan header, so the
        // panel says which credentials are silent and why — and still draws
        // no meter over nothing.
        assert!(
            screen.contains("platform.claude.com/docs/en/manage-claude/usage-cost-api"),
            "the reason names the vendor's own page, whole:\n{screen}"
        );
        assert!(
            !screen.contains("Plan limits"),
            "no plan-limit meter may be drawn:\n{screen}"
        );
    }

    /// The `/context` grid over a sized fixture breakdown (screenshot:
    /// Claude Code's grid-and-legend panel; house dialog chrome). The
    /// breakdown is injected rather than computed so the cells and legend
    /// are stable whatever machine renders them.
    #[tokio::test]
    async fn snapshot_context_dialog_open() {
        let mut app = app();
        app.context_dialog = Some(component::context::Context::new(
            Some("Claude Sonnet 5".to_owned()),
            breakdown_fixture(),
        ));

        let mut terminal = terminal(80, 36);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// The same panel degraded: no window, so totals alone and the honest
    /// sentence — no invented denominator.
    #[tokio::test]
    async fn snapshot_context_dialog_degraded() {
        let mut app = app();
        app.context_dialog = Some(component::context::Context::new(
            None,
            ganja_core::engine::ContextBreakdown {
                model: fake::MODEL.to_owned(),
                window: None,
                reserve: None,
                ..breakdown_fixture()
            },
        ));

        let mut terminal = terminal(80, 36);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// The `/usage` panel over a session with one recorded turn (screenshot:
    /// Claude Code's sectioned panel; house dialog chrome, HUD meter shape).
    #[tokio::test]
    async fn snapshot_usage_dialog_open() {
        let mut app = app();
        app.record(
            &MessageId::from("msg_fixture".to_owned()),
            &Usage {
                input_tokens: 1_200,
                output_tokens: 34,
                reasoning_tokens: 5,
                cache_read_tokens: 600,
                cache_write_tokens: 100,
            },
        );

        app.run_command(command::Action::Usage).await;
        let mut terminal = terminal(80, 30);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    #[test]
    fn only_a_failed_server_earns_a_notice_and_it_is_one_line() {
        let cases = [
            (vec![("fs", ganja_core::McpStatus::Connected)], None),
            (vec![("fs", ganja_core::McpStatus::Disabled)], None),
            (
                vec![(
                    "fs",
                    ganja_core::McpStatus::Failed {
                        error: "no such file\n  while spawning".to_owned(),
                    },
                )],
                Some("mcp fs: no such file"),
            ),
            (
                vec![
                    ("fs", ganja_core::McpStatus::Connected),
                    (
                        "hub",
                        ganja_core::McpStatus::Failed {
                            error: "connection refused".to_owned(),
                        },
                    ),
                ],
                Some("mcp hub: connection refused"),
            ),
        ];

        for (status, expected) in cases {
            let status: std::collections::BTreeMap<String, ganja_core::McpStatus> = status
                .into_iter()
                .map(|(name, status)| (name.to_owned(), status))
                .collect();

            assert_eq!(super::mcp_notice(&status).as_deref(), expected);
        }
        assert_eq!(super::mcp_notice(&std::collections::BTreeMap::new()), None);
    }

    /// A conversation of two exchanges, and the id of the prompt an undo of
    /// the last one anchors on.
    fn two_exchanges(app: &mut App) -> ganja_protocol::MessageId {
        app.chat
            .start_message(Message::user("the question that stands"));
        let mut first = Message::assistant("canned");
        first.parts.push(Part::text("the answer that stands"));
        app.chat.start_message(first);

        let taken_back = Message::user("the question that is taken back");
        let anchor = taken_back.id.clone();
        app.chat.start_message(taken_back);
        let mut second = Message::assistant("canned");
        second
            .parts
            .push(Part::text("the answer that goes with it"));
        app.chat.start_message(second);

        anchor
    }

    /// The event the engine sends after an undo.
    fn reverted(anchor: &ganja_protocol::MessageId, prompt: Option<&str>) -> AppEvent {
        AppEvent::core(CoreEvent::RevertChanged {
            session_id: session(),
            revert: Some(ganja_protocol::RevertInfo {
                message_id: anchor.clone(),
                files: vec!["src/lib.rs".to_owned()],
            }),
            prompt: prompt.map(str::to_owned),
        })
    }

    /// The event the engine sends when a revert ends, whichever way it ended.
    fn unreverted() -> AppEvent {
        AppEvent::core(CoreEvent::RevertChanged {
            session_id: session(),
            revert: None,
            prompt: None,
        })
    }

    /// The transcript pane alone: everything above the composer's box.
    ///
    /// A revert puts the prompt it took back into the composer, so a whole
    /// screen holds that text whether or not the transcript still shows the
    /// message — which is exactly the thing under test.
    fn transcript_pane(terminal: &Terminal<TestBackend>) -> String {
        let whole = screen(terminal);

        whole
            .split_once("\u{250c} message")
            .map_or(whole.clone(), |(above, _)| above.to_owned())
    }

    /// **R10**, the whole of the TUI half in one pass: what an undo hid stops
    /// being drawn, one row says how much and which files moved, and the
    /// prompt it took back is offered again for editing.
    #[tokio::test]
    async fn an_undo_hides_what_it_took_back_shows_one_marker_row_and_refills_the_editor() {
        let mut app = app();
        let anchor = two_exchanges(&mut app);

        app.handle(reverted(&anchor, Some("the question that is taken back")))
            .await
            .expect("a revert is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let pane = transcript_pane(&terminal);

        assert!(pane.contains("the question that stands"), "{pane}");
        assert!(
            !pane.contains("the question that is taken back"),
            "the anchor is hidden with everything after it:\n{pane}"
        );
        assert!(!pane.contains("the answer that goes with it"), "{pane}");
        assert!(
            pane.contains("2 messages reverted \u{2014} /redo to restore"),
            "{pane}"
        );
        assert!(pane.contains("src/lib.rs"), "{pane}");
        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("the question that is taken back"),
            "undoing and retyping a prompt is editing it"
        );
    }

    /// The disambiguation the engine deliberately leaves to the frontend: the
    /// same `revert: None` means two different things, and which one is
    /// decided by the command this side last sent (**R10**).
    #[tokio::test]
    async fn a_redo_past_the_newest_reverted_prompt_puts_those_messages_back() {
        let mut app = app();
        let anchor = two_exchanges(&mut app);
        app.handle(reverted(&anchor, Some("the question that is taken back")))
            .await
            .expect("a revert is handled");

        app.run_command(command::Action::Redo).await;
        app.handle(unreverted())
            .await
            .expect("a cleared revert is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let pane = transcript_pane(&terminal);

        assert!(
            pane.contains("the question that is taken back"),
            "a redo past the newest one restores them:\n{pane}"
        );
        assert!(pane.contains("the answer that goes with it"), "{pane}");
        assert!(!pane.contains("reverted"), "{pane}");
    }

    /// The other reading of the same event: the engine has just deleted those
    /// messages from history and from storage, so nothing is coming back and
    /// leaving them on screen would be showing a conversation that no longer
    /// exists.
    #[tokio::test]
    async fn a_prompt_after_an_undo_drops_the_messages_it_hid_for_good() {
        let (mut app, _events) = wired().await;
        let anchor = two_exchanges(&mut app);
        app.handle(reverted(&anchor, Some("the question that is taken back")))
            .await
            .expect("a revert is handled");

        // Through the editor, so what marks the clear as permanent is the same
        // submit path a person takes.
        app.editor.set_text("a different question");
        app.submit().await;
        app.handle(unreverted())
            .await
            .expect("a cleared revert is handled");
        // A second `/redo` has nothing left to show.
        app.run_command(command::Action::Redo).await;
        app.handle(unreverted())
            .await
            .expect("a cleared revert is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let pane = transcript_pane(&terminal);

        assert!(pane.contains("the question that stands"), "{pane}");
        assert!(
            !pane.contains("the question that is taken back"),
            "a prompt after an undo makes it permanent:\n{pane}"
        );
    }

    /// A refused prompt truncated nothing, so it must not leave this side
    /// believing the next cleared revert is a deletion — which would drop
    /// messages the engine still holds, and no later event could put back.
    #[tokio::test]
    async fn a_refused_prompt_leaves_the_revert_still_undoable() {
        let (mut app, mut events) = wired().await;
        let anchor = two_exchanges(&mut app);
        app.handle(reverted(&anchor, None))
            .await
            .expect("a revert is handled");
        assert_eq!(app.cleared, Cleared::Unhide);

        // A turn already streaming, which is what refuses the prompt below —
        // and, being accepted itself, what makes this undo permanent.
        app.editor.set_text("the turn that is already running");
        app.submit().await;
        pump(&mut app, &mut events, 2).await;
        assert_eq!(
            app.cleared,
            Cleared::Drop,
            "an accepted prompt is the user keeping what the undo did"
        );

        // A redo puts the reading back...
        app.run_command(command::Action::Redo).await;
        assert_eq!(app.cleared, Cleared::Unhide);

        // ...and a prompt the engine refuses must not take it away again. The
        // refusal a *prompt* can still meet since steering landed is `Busy`:
        // the turn above is running while this side has not yet seen the event
        // that says so, which is exactly the race the fallback lane exists for
        // (**F4**).
        app.turn_running = false;
        app.editor.set_text("the prompt the engine will refuse");
        app.submit().await;

        assert!(
            app.editor.is_empty() && app.queue.depth() == 1,
            "the fixture only proves anything while the prompt really is refused"
        );
        assert_eq!(
            app.cleared,
            Cleared::Unhide,
            "a refusal must put the reading back"
        );
    }

    /// **Resume.** A frontend that has just started learns the hidden range
    /// from this event and from nowhere else — and learns it without anything
    /// arriving in the editor, because reopening a conversation is not the
    /// moment to put words in somebody's.
    #[tokio::test]
    async fn a_resumed_session_reconstructs_the_hidden_range_without_refilling_the_editor() {
        let mut app = app();
        let anchor = two_exchanges(&mut app);
        app.editor.set_text("what was already being typed");

        app.handle(reverted(&anchor, None))
            .await
            .expect("a seeded revert is handled");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let pane = transcript_pane(&terminal);

        assert!(
            pane.contains("2 messages reverted \u{2014} /redo to restore"),
            "the marker is reconstructed from the event alone:\n{pane}"
        );
        assert!(!pane.contains("the question that is taken back"), "{pane}");
        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("what was already being typed"),
            "a resume leaves the composer alone"
        );
    }

    /// Both halves are engine commands with no key of their own (**D4**), so
    /// the palette row *is* the way to them: this asserts the row reaches the
    /// engine, by the refusal only `Command::Undo` can earn here.
    #[tokio::test]
    async fn the_palette_rows_send_undo_and_redo_to_the_engine() {
        for typed in ["undo", "redo"] {
            let mut app = app();
            app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
                .await
                .expect("control-p is handled");
            for event in typing(typed) {
                app.handle(event).await.expect("typing is handled");
            }
            app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .expect("enter is handled");

            let mut terminal = terminal(80, 12);
            app.draw(&mut terminal).expect("a frame draws");

            assert!(
                screen(&terminal).contains("takes no snapshots"),
                "/{typed} should have reached an engine that cannot do it:\n{}",
                screen(&terminal)
            );
        }
    }

    /// The `/` menu is the second view of the same command set, and a person
    /// who types `/undo` and presses Enter has named the same row.
    #[tokio::test]
    async fn the_command_menu_reaches_undo_too() {
        let mut app = app();
        for event in typing("/undo") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let mut terminal = terminal(80, 12);
        app.draw(&mut terminal).expect("a frame draws");

        assert!(
            screen(&terminal).contains("takes no snapshots"),
            "{}",
            screen(&terminal)
        );
        assert!(
            app.editor.is_empty(),
            "a UI command runs rather than being typed"
        );
    }

    #[test]
    fn snapshot_reverted_transcript() {
        let mut app = app();
        let anchor = two_exchanges(&mut app);
        app.chat.revert(
            anchor,
            vec![
                "crates/ganja-tui/src/app.rs".to_owned(),
                "README.md".to_owned(),
            ],
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");

        insta::assert_snapshot!(screen(&terminal));
    }

    /// The W2 follow-up, at the size it was reported at: two command rows had
    /// already pushed the `keys` section off a stock terminal and `/undo` and
    /// `/redo` push it further, so the card scrolls (deviation:
    /// help-card-scrolls) and every row is reachable with the arrow keys.
    #[tokio::test]
    async fn the_help_card_reaches_all_of_itself_on_a_stock_terminal() {
        let mut app = app();
        app.run_command(command::Action::Help).await;

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let opening = screen(&terminal);
        assert!(
            opening.contains("[up/down] scroll"),
            "the card does not fit, so it must say how to see the rest:\n{opening}"
        );
        assert!(
            !opening.contains("agent_cycle"),
            "the fixture only proves anything while the tail really is off screen:\n{opening}"
        );

        let mut seen = opening;
        for _ in 0..20 {
            app.handle(key(KeyCode::Down, KeyModifiers::NONE))
                .await
                .expect("down is handled");
            app.draw(&mut terminal).expect("a frame draws");
            seen.push('\n');
            seen.push_str(&screen(&terminal));
        }

        assert!(app.help.is_some(), "scrolling must not close the card");
        for row in ["/undo", "/redo", "keys", "agent_cycle"] {
            assert!(seen.contains(row), "{row} should be reachable:\n{seen}");
        }
    }

    /// A modal owns the keyboard: the keys that scroll the card must not also
    /// reach the composer beneath it.
    #[tokio::test]
    async fn scrolling_the_help_card_does_not_type_into_the_editor() {
        let mut app = app();
        app.run_command(command::Action::Help).await;

        for code in [KeyCode::Down, KeyCode::Char('j'), KeyCode::Char('k')] {
            app.handle(key(code, KeyModifiers::NONE))
                .await
                .expect("a key is handled");
        }

        assert!(app.help.is_some());
        assert!(app.editor.is_empty(), "nothing should have been typed");
    }

    /// A window with room for the whole card is still a window with room for
    /// the whole card: the scrolling is what a clip costs, not a permanent
    /// change of shape.
    #[tokio::test]
    async fn a_tall_terminal_shows_the_whole_help_card_at_once() {
        let mut app = app();
        app.run_command(command::Action::Help).await;

        // One row taller than it was, because the roster this card lists gained
        // `/team` (**D504**) — the card grows with the commands, which is what
        // "the whole card" means.
        let mut terminal = terminal(90, 41);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        for row in ["/undo", "/redo", "keys", "agent_cycle"] {
            assert!(screen.contains(row), "{row} should be listed:\n{screen}");
        }
        assert!(!screen.contains("[up/down] scroll"), "{screen}");
    }

    // ---- F4: steering and the queue behind it -----------------------------

    /// Drives a real turn to a streaming state and hands back the stream, so a
    /// steering test acts while the engine really is busy rather than while a
    /// flag says it is.
    async fn streaming() -> (App, BoxStream<'static, CoreEvent>) {
        let (mut app, mut events) = wired().await;
        typed(&mut app, "the turn to steer").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        // Both envelopes: the second is the assistant's, which is what tells
        // this side a turn holds the engine.
        pump(&mut app, &mut events, 2).await;
        assert!(app.turn_running, "the fixture needs a turn in flight");

        (app, events)
    }

    /// Runs the rest of a turn's events through the app, so a test never
    /// leaves one streaming behind it.
    async fn finish(app: &mut App, events: &mut BoxStream<'static, CoreEvent>) {
        while let Some(event) = events.next().await {
            let finished = matches!(event, CoreEvent::MessageFinished { .. });
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
            if finished {
                return;
            }
        }
    }

    /// The strip exists to be emptied by the engine's own word, and by nothing
    /// else: `SteerConsumed` naming an id is what retires that entry.
    #[tokio::test]
    async fn a_queued_entry_leaves_the_strip_when_the_engine_says_it_consumed_it() {
        let (mut app, mut events) = streaming().await;
        typed(&mut app, "one more thing").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert_eq!(app.queue.depth(), 1);

        let id = app.queue.entries()[0].id.clone();
        app.handle(AppEvent::core(CoreEvent::SteerConsumed {
            session_id: app.engine.session_id(),
            id,
        }))
        .await
        .expect("the event is handled");

        assert!(
            app.queue.is_empty(),
            "the engine took it, so the strip lets go"
        );
        assert!(!status_line(&mut app).contains("queued"));

        finish(&mut app, &mut events).await;
    }

    /// The whole path with nothing scripted in the middle: a real engine takes
    /// the steer, drains it before the turn it would otherwise have ended,
    /// announces it, and the strip empties on the engine's own word.
    #[tokio::test]
    async fn a_real_turn_takes_the_steer_and_the_strip_empties_on_its_own() {
        let (mut app, mut events) = streaming().await;
        typed(&mut app, "and one more thing").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert_eq!(app.queue.depth(), 1);

        let mut consumed = false;
        loop {
            let event = events.next().await.expect("the turn keeps reporting");
            let finished = matches!(event, CoreEvent::MessageFinished { .. });
            consumed |= matches!(event, CoreEvent::SteerConsumed { .. });
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
            if finished {
                break;
            }
        }

        assert!(consumed, "the running turn took the message");
        assert!(app.queue.is_empty(), "so the strip let go of it");

        let mut terminal = terminal(80, 20);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(
            screen.contains("and one more thing"),
            "and it is in the transcript, not on the strip:\n{screen}"
        );
        assert!(
            !screen.contains("press up to edit queued messages"),
            "the strip is gone:\n{screen}"
        );
    }

    /// An id nothing answers to is the withdrawal race, and is not an error:
    /// the entry was already taken back and the message lands exactly once.
    #[tokio::test]
    async fn a_consumed_id_that_names_nothing_changes_nothing() {
        let (mut app, mut events) = streaming().await;
        typed(&mut app, "one more thing").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        app.handle(AppEvent::core(CoreEvent::SteerConsumed {
            session_id: app.engine.session_id(),
            id: "steer-nobody".to_owned(),
        }))
        .await
        .expect("the event is handled");

        assert_eq!(app.queue.depth(), 1, "an unrelated id retires nothing");

        finish(&mut app, &mut events).await;
    }

    /// **Acceptance 4, Up.** With something waiting and nothing typed, Up
    /// takes the newest queued message back into the composer — and takes it
    /// off the strip, which is the whole of "withdraw".
    #[tokio::test]
    async fn up_recalls_and_withdraws_the_newest_queued_message() {
        let (mut app, mut events) = streaming().await;
        for text in ["first correction", "second correction"] {
            typed(&mut app, text).await;
            app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .expect("enter is handled");
        }
        assert_eq!(app.queue.depth(), 2);

        app.handle(key(KeyCode::Up, KeyModifiers::NONE))
            .await
            .expect("up is handled");

        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("second correction"),
            "the newest entry comes back for editing"
        );
        assert_eq!(app.queue.depth(), 1, "and leaves the strip");

        finish(&mut app, &mut events).await;
    }

    /// The queue sits *in front of* the history: once the strip is empty the
    /// same key walks remembered prompts exactly as it always did.
    #[tokio::test]
    async fn up_walks_the_history_again_once_the_strip_is_empty() {
        let directory = temporary();
        let mut app = app_with_history(&directory, &["an older prompt"]);

        app.handle(key(KeyCode::Up, KeyModifiers::NONE))
            .await
            .expect("up is handled");

        assert_eq!(app.editor.prompt().as_deref(), Some("an older prompt"));
    }

    /// **Acceptance 4, cancel.** The engine drains nothing on a cancel, so
    /// every entry still on the strip becomes the fallback lane's — and the
    /// lane sends it once the engine is idle.
    #[tokio::test]
    async fn an_unconsumed_steer_survives_a_cancelled_turn_into_the_fallback_lane() {
        let (mut app, mut events) = streaming().await;
        typed(&mut app, "the correction nobody took").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert!(app.queue.entries()[0].is_steered());

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        finish(&mut app, &mut events).await;

        // The finish stranded it and the same handler replayed it, so the
        // engine has a turn of its own for the message now.
        assert!(
            app.queue.is_empty(),
            "the lane owns it and has sent it: {:?}",
            app.queue.entries()
        );
        let started = events
            .next()
            .await
            .expect("the replay starts a turn of its own");
        let CoreEvent::MessageStarted { message, .. } = started else {
            panic!("a turn opens with the user's message");
        };
        assert_eq!(
            message
                .parts
                .iter()
                .filter_map(ganja_protocol::Part::as_text)
                .collect::<String>(),
            "the correction nobody took"
        );

        finish(&mut app, &mut events).await;
    }

    /// **Acceptance 4, the revert pause.** A replayed prompt would commit the
    /// undo the user just made, so the lane holds while one is outstanding —
    /// and the entry is still there, on screen, rather than quietly gone.
    #[tokio::test]
    async fn the_fallback_lane_pauses_while_a_revert_is_outstanding() {
        let (mut app, mut events) = wired().await;
        typed(&mut app, "the first turn").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        finish(&mut app, &mut events).await;

        // Refused `NotStreaming`, so the entry is the fallback lane's — the
        // one kind a replay could send, and the kind this test holds back.
        app.turn_running = true;
        typed(&mut app, "the queued correction").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        app.turn_running = false;
        assert_eq!(app.queue.depth(), 1);

        app.handle(AppEvent::core(CoreEvent::RevertChanged {
            session_id: app.engine.session_id(),
            revert: Some(ganja_protocol::RevertInfo {
                message_id: ganja_protocol::MessageId::from("msg_1".to_owned()),
                files: Vec::new(),
            }),
            prompt: None,
        }))
        .await
        .expect("the revert is handled");
        assert!(app.revert_pending);

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert_eq!(
            app.queue.depth(),
            1,
            "the entry waits for the person to decide about the revert"
        );
        assert!(status_line(&mut app).contains("1 queued"));

        // And once the revert is over, the same entry goes.
        app.handle(AppEvent::core(CoreEvent::RevertChanged {
            session_id: app.engine.session_id(),
            revert: None,
            prompt: None,
        }))
        .await
        .expect("the cleared revert is handled");
        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert!(app.queue.is_empty(), "the pause was a pause, not a drop");
        finish(&mut app, &mut events).await;
    }

    /// **Acceptance 4, the race towards idle.** A steer that arrives after the
    /// turn ended is refused `NotStreaming`, joins the fallback lane, and is
    /// replayed exactly once — no loss, no duplicate.
    #[tokio::test]
    async fn a_steer_that_loses_the_turn_is_refused_and_replayed_exactly_once() {
        let (mut app, mut events) = wired().await;
        typed(&mut app, "the first turn").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        finish(&mut app, &mut events).await;

        // The turn is over; this side has not noticed, which is the race.
        app.turn_running = true;
        typed(&mut app, "the message that lost the race").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(app.queue.depth(), 1);
        assert!(
            !app.queue.entries()[0].is_steered(),
            "a refused steer belongs to the fallback lane"
        );

        // The lane sends it on the next tick, and sends it once.
        app.turn_running = false;
        app.handle(AppEvent::Tick).await.expect("a tick is handled");
        assert!(app.queue.is_empty());

        let mut prompts = 0;
        loop {
            let event = events.next().await.expect("the replay runs a turn");
            let finished = matches!(event, CoreEvent::MessageFinished { .. });
            if let CoreEvent::MessageStarted { message, .. } = &event
                && message.role == ganja_protocol::Role::User
            {
                prompts += 1;
                assert_eq!(
                    message
                        .parts
                        .iter()
                        .filter_map(ganja_protocol::Part::as_text)
                        .collect::<String>(),
                    "the message that lost the race"
                );
            }
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
            if finished {
                break;
            }
        }
        assert_eq!(prompts, 1, "exactly once");

        app.handle(AppEvent::Tick).await.expect("a tick is handled");
        assert!(
            app.queue.is_empty(),
            "and nothing is left to send a second time"
        );
    }

    /// **Acceptance 4, the Busy retry.** A replay that loses a race to a turn
    /// starting underneath it keeps its place at the front and is tried again.
    #[tokio::test]
    async fn a_replay_that_meets_busy_keeps_its_place_and_is_retried() {
        let (mut app, mut events) = streaming().await;

        // Queued while a turn really is running, but with this side believing
        // it is idle: the send below meets `Busy` and the entry is what
        // survives it.
        app.turn_running = false;
        typed(&mut app, "the message that met busy").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(app.queue.depth(), 1, "the refusal cost nothing");
        assert!(app.editor.is_empty());

        // Another tick while the turn is still running: still refused, still
        // first in the queue.
        app.handle(AppEvent::Tick).await.expect("a tick is handled");
        assert_eq!(app.queue.depth(), 1);

        // And once the turn ends, the same entry goes.
        finish(&mut app, &mut events).await;
        assert!(app.queue.is_empty(), "the retry took");

        finish(&mut app, &mut events).await;
    }

    /// **Acceptance 4, the slash split.** An engine command acts on the engine
    /// between turns, so it never steers: it waits for the end of the turn.
    #[tokio::test]
    async fn an_engine_command_typed_mid_turn_waits_for_the_end_of_the_turn() {
        let (mut app, mut events) = streaming().await;

        // With an argument, so the inline command menu is closed and Enter is
        // the submit rather than the menu's own selection.
        typed(&mut app, "/init focus on the tests").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(app.queue.depth(), 1);
        assert!(
            !app.queue.entries()[0].is_steered(),
            "a command is not a message the model reads"
        );
        assert!(app.editor.is_empty());

        finish(&mut app, &mut events).await;
        assert!(app.queue.is_empty(), "and it runs when the turn is over");

        finish(&mut app, &mut events).await;
    }

    /// Shell mode keeps the refusal it always had: upstream's server refuses a
    /// shell command while busy too, and steering does not change what a `!`
    /// line is.
    #[tokio::test]
    async fn a_shell_submission_mid_turn_is_still_refused_and_never_queued() {
        let (mut app, mut events) = streaming().await;

        typed(&mut app, "!").await;
        typed(&mut app, "echo hello").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert_eq!(app.editor.prompt().as_deref(), Some("echo hello"));
        assert_eq!(app.editor.mode(), Mode::Shell);
        assert!(app.queue.is_empty(), "nothing steers a shell line");

        app.set_shell(false);
        app.editor.clear();
        finish(&mut app, &mut events).await;
    }

    /// Esc is the cancel and nothing else: a person stopping a turn has not
    /// said anything about the messages they queued for it.
    #[tokio::test]
    async fn escape_does_not_clear_the_strip() {
        let (mut app, mut events) = streaming().await;
        typed(&mut app, "still wanted").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");

        assert_eq!(app.queue.depth(), 1, "Esc says nothing about the queue");

        finish(&mut app, &mut events).await;
    }

    /// The whole strip, as a person sees it: the waiting message, the hint
    /// line under it, and the depth on the bar.
    #[tokio::test]
    async fn snapshot_queued_messages_strip() {
        let (mut app, mut events) = streaming().await;
        for text in ["run the tests too", "and then commit"] {
            typed(&mut app, text).await;
            app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .expect("enter is handled");
        }

        let mut terminal = terminal(80, 16);
        app.draw(&mut terminal).expect("a frame draws");
        insta::assert_snapshot!(screen(&terminal));

        finish(&mut app, &mut events).await;
    }

    // ---- F7: the rewind picker --------------------------------------------

    /// An app whose transcript holds two exchanges, which is two checkpoints
    /// as far as the picker is concerned.
    fn with_checkpoints() -> App {
        let mut app = app();
        two_exchanges(&mut app);

        app
    }

    /// Presses Esc `times` in a row, fast enough that the gesture's window
    /// never closes between them.
    async fn escapes(app: &mut App, times: usize) {
        for _ in 0..times {
            app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
                .await
                .expect("escape is handled");
        }
    }

    /// **Acceptance 7, the list.** `/rewind` lists exactly the session's user
    /// messages plus the row for where it already stands.
    #[tokio::test]
    async fn slash_rewind_lists_the_prompts_the_transcript_holds_and_current() {
        let mut app = with_checkpoints();
        app.run_command(command::Action::Rewind).await;

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        assert!(screen.contains("(Current)"), "got:\n{screen}");
        assert!(
            screen.contains("the question that stands"),
            "got:\n{screen}"
        );
        assert!(
            screen.contains("the question that is taken back"),
            "got:\n{screen}"
        );
        assert!(
            !screen.contains("the answer that stands"),
            "a reply is not a checkpoint:\n{screen}"
        );
    }

    /// A prompt whose turn recorded patches says how many files it moved; one
    /// whose turn recorded none says there is no code to put back.
    #[tokio::test]
    async fn a_checkpoint_says_whether_its_turn_changed_any_code() {
        let mut app = app();
        app.chat.start_message(Message::user("touch two files"));
        let mut reply = Message::assistant("canned");
        reply.parts.push(Part {
            id: ganja_protocol::PartId::from("prt_patch".to_owned()),
            body: PartBody::Patch {
                hash: "4b825dc".to_owned(),
                files: vec!["src/lib.rs".to_owned(), "src/app.rs".to_owned()],
            },
        });
        app.chat.start_message(reply);
        app.chat
            .start_message(Message::user("just tell me about it"));
        app.chat.start_message(Message::assistant("canned"));

        app.run_command(command::Action::Rewind).await;
        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);

        assert!(screen.contains("2 files changed"), "got:\n{screen}");
        assert!(
            screen.contains("\u{26a0} No code restore"),
            "a span with no patches says so:\n{screen}"
        );
    }

    /// **The gesture (D467).** Two Escs at an idle composer enter the
    /// backtrack walk on the newest user message, and each further Esc steps
    /// one older, holding at the oldest rather than wrapping.
    #[tokio::test]
    async fn esc_esc_highlights_the_newest_prompt_and_each_esc_steps_one_older() {
        let mut app = with_checkpoints();
        let newest = app.chat.checkpoints()[0].message_id.clone();
        let oldest = app.chat.checkpoints()[1].message_id.clone();

        escapes(&mut app, 1).await;
        assert!(app.backtrack.is_none(), "one Esc is still just a cancel");

        escapes(&mut app, 1).await;
        assert_eq!(
            app.chat.backtrack_anchor(),
            Some(&newest),
            "the second lands on the newest prompt"
        );
        assert!(
            app.rewind.is_none(),
            "the picker is /rewind's, not the gesture's"
        );

        escapes(&mut app, 1).await;
        assert_eq!(app.chat.backtrack_anchor(), Some(&oldest), "one step older");

        escapes(&mut app, 1).await;
        assert_eq!(
            app.chat.backtrack_anchor(),
            Some(&oldest),
            "the walk holds at the oldest"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains(BACKTRACK_HINT),
            "the status bar says what the walk's keys do"
        );
    }

    /// **The gesture's guard.** While a turn streams Esc is the cancel and
    /// nothing else — and it forgets any first press, so a double-press racing
    /// a turn's end cancels and then does nothing.
    #[tokio::test]
    async fn esc_esc_while_a_turn_streams_cancels_and_never_backtracks() {
        let (mut app, mut events) = streaming().await;

        escapes(&mut app, 2).await;
        assert!(
            app.backtrack.is_none(),
            "no walk starts over a turn the user is watching"
        );

        finish(&mut app, &mut events).await;
        assert!(!app.turn_running, "and the Esc really did cancel it");

        // The turn is over, so the gesture is armed again — and the press that
        // happened while it was streaming does not count towards it.
        escapes(&mut app, 1).await;
        assert!(app.backtrack.is_none(), "the streaming press was forgotten");
        escapes(&mut app, 1).await;
        assert!(app.backtrack.is_some());
    }

    /// Two Escs far enough apart are two cancels, not a gesture.
    #[tokio::test]
    async fn a_second_esc_after_the_window_has_closed_opens_nothing() {
        let mut app = with_checkpoints();

        escapes(&mut app, 1).await;
        app.last_esc = Instant::now().checked_sub(ESC_CHORD * 2);
        assert!(app.last_esc.is_some(), "the fixture needs a stale press");

        escapes(&mut app, 1).await;
        assert!(app.backtrack.is_none(), "the window had closed");
    }

    /// The gesture is a sequence: anything typed between the two presses ends
    /// it, however fast the second one follows.
    #[tokio::test]
    async fn a_keystroke_between_the_two_escs_ends_the_gesture() {
        let mut app = with_checkpoints();

        escapes(&mut app, 1).await;
        typed(&mut app, "n").await;
        escapes(&mut app, 1).await;

        assert!(
            app.backtrack.is_none(),
            "that was a cancel, a letter, a cancel"
        );
    }

    /// A transcript with no user message gives the gesture nothing to walk.
    #[tokio::test]
    async fn esc_esc_over_an_empty_transcript_enters_nothing() {
        let mut app = app();

        escapes(&mut app, 2).await;

        assert!(app.backtrack.is_none(), "there is nothing to step through");
    }

    /// A key that is neither Esc nor Enter leaves the walk silently — nothing
    /// reverted, highlight and hint down — and then lands where it would have
    /// without it.
    #[tokio::test]
    async fn any_other_key_exits_the_walk_without_reverting_and_is_then_handled() {
        let mut app = with_checkpoints();
        escapes(&mut app, 2).await;
        assert!(app.backtrack.is_some(), "the fixture needs a live walk");

        typed(&mut app, "n").await;

        assert!(app.backtrack.is_none(), "the walk exits silently");
        assert!(
            app.chat.backtrack_anchor().is_none(),
            "the highlight is down"
        );
        assert!(!app.chat.is_reverted(), "and nothing was reverted");
        assert_eq!(app.editor.text(), "n", "the key was then handled as typing");

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            !screen(&terminal).contains(BACKTRACK_HINT),
            "the hint left with the walk"
        );
    }

    /// **Acceptance 1, the whole gesture.** Enter reverts the conversation to
    /// before the highlighted prompt and the composer holds the *whole*
    /// multi-line prompt — a prefill fed from the checkpoint roster's
    /// render-clipped titles would drop the second line, which is the point.
    #[tokio::test]
    async fn backtrack_enter_reverts_and_hands_back_the_whole_multi_line_prompt() {
        let (mut app, mut events) = wired().await;
        typed(&mut app, "the first line").await;
        app.handle(key(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .await
            .expect("the newline chord is handled");
        typed(&mut app, "and the second").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        finish(&mut app, &mut events).await;

        escapes(&mut app, 2).await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        pump(&mut app, &mut events, 1).await;

        assert!(app.backtrack.is_none(), "confirming closes the walk");
        assert!(app.chat.is_reverted(), "the engine hid the checkpoint");
        assert_eq!(
            app.editor.text(),
            "the first line\nand the second",
            "the whole prompt comes back, not its clipped first line"
        );
        assert!(
            !app.code_only_rewind,
            "a backtrack is conversation-only and never reads as a code rewind"
        );
    }

    /// The engine's answer to a backtrack names the prompt it took back:
    /// `RevertChanged.prompt` is `Some`, which is what makes the existing
    /// composer-prefill path the walk's one prefill mechanism.
    #[tokio::test]
    async fn a_backtrack_revert_announces_the_prompt_it_took_back() {
        let (mut app, mut events) = wired().await;
        typed(&mut app, "the prompt to step back to").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        finish(&mut app, &mut events).await;

        escapes(&mut app, 2).await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        let event = events.next().await.expect("the engine answers the revert");
        let CoreEvent::RevertChanged { prompt, .. } = &event else {
            panic!("expected RevertChanged, got {event:?}");
        };
        assert_eq!(
            prompt.as_deref(),
            Some("the prompt to step back to"),
            "the event itself carries the prompt for the composer"
        );
        app.handle(AppEvent::core(event))
            .await
            .expect("the revert is handled");
    }

    /// `/rewind` is untouched by the gesture's retargeting: it still opens the
    /// Claude-style two-step picker, never the walk (**D467**'s split).
    #[tokio::test]
    async fn slash_rewind_still_opens_the_picker_and_never_the_walk() {
        let mut app = with_checkpoints();

        app.run_command(command::Action::Rewind).await;

        assert!(
            app.rewind.is_some(),
            "the two-step picker is /rewind's door"
        );
        assert!(app.backtrack.is_none(), "the walk is the gesture's alone");
    }

    /// A transcript already showing a revert offers the walk only what is
    /// still visible — the roster it steps is the checkpoint roster, which
    /// already leaves out what a standing revert hides.
    #[tokio::test]
    async fn a_walk_over_a_standing_revert_offers_only_visible_prompts() {
        let mut app = app();
        let anchor = two_exchanges(&mut app);
        app.handle(reverted(&anchor, None))
            .await
            .expect("a revert is handled");
        assert!(
            app.chat.is_reverted(),
            "the fixture needs a standing revert"
        );

        escapes(&mut app, 2).await;

        let backtrack = app.backtrack.as_ref().expect("the gesture still works");
        assert_eq!(
            backtrack.candidates.len(),
            1,
            "the hidden prompt is not on offer"
        );
        let visible = app.chat.checkpoints()[0].message_id.clone();
        assert_eq!(app.chat.backtrack_anchor(), Some(&visible));
    }

    /// **Acceptance 7, Esc.** Esc at either step changes nothing: no command
    /// reaches the engine and the transcript is where it was.
    #[tokio::test]
    async fn esc_closes_the_picker_from_either_step_and_changes_nothing() {
        let mut app = with_checkpoints();

        app.run_command(command::Action::Rewind).await;
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.rewind.is_none());
        assert!(!app.chat.is_reverted());

        app.run_command(command::Action::Rewind).await;
        app.handle(key(KeyCode::Down, KeyModifiers::NONE))
            .await
            .expect("down is handled");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert!(
            app.rewind.as_ref().is_some_and(Rewind::is_choosing_scope),
            "the scope step should be showing"
        );

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.rewind.is_none(), "Esc leaves the scope step too");
        assert!(!app.chat.is_reverted(), "and nothing was reverted");
    }

    /// Enter on `(Current)` is a person deciding not to rewind: the picker
    /// closes and nothing is sent.
    #[tokio::test]
    async fn enter_on_current_closes_the_picker_and_reverts_nothing() {
        let mut app = with_checkpoints();
        app.run_command(command::Action::Rewind).await;

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");

        assert!(app.rewind.is_none());
        assert!(!app.chat.is_reverted());
    }

    /// **Acceptance 7, the whole path.** A real turn, then the picker taken to
    /// *Conversation only*: the engine hides the message and hands the prompt
    /// back for editing, with the working tree never mentioned.
    #[tokio::test]
    async fn choosing_conversation_only_takes_the_prompt_back_through_the_engine() {
        let (mut app, mut events) = wired().await;
        typed(&mut app, "the prompt to rewind past").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        finish(&mut app, &mut events).await;

        app.run_command(command::Action::Rewind).await;
        // Off `(Current)`, onto the one checkpoint, into the scope step, then
        // down one row to "Conversation only".
        for code in [KeyCode::Down, KeyCode::Enter, KeyCode::Down, KeyCode::Enter] {
            app.handle(key(code, KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }
        pump(&mut app, &mut events, 1).await;

        assert!(app.rewind.is_none(), "choosing closes the picker");
        assert!(app.chat.is_reverted(), "the engine hid the checkpoint");
        assert_eq!(
            app.editor.prompt().as_deref(),
            Some("the prompt to rewind past"),
            "rewinding and retyping a prompt is editing it"
        );
        assert_eq!(
            app.cleared,
            Cleared::Unhide,
            "what this hid can still be stepped back through"
        );
    }

    /// **Acceptance 7, code only.** The engine announces the files it put back
    /// while recording no revert, so this side names them and hides nothing —
    /// and the fallback lane is not paused by a revert nobody is holding.
    #[tokio::test]
    async fn a_code_only_rewind_names_the_files_and_hides_nothing() {
        let mut app = with_checkpoints();
        let anchor = two_exchanges(&mut app);
        // What `App::rewind_to` sets when it sends `RevertScope::Files`; the
        // engine's answer is indistinguishable from any other revert's, and
        // this is the flag that tells them apart (**R10**).
        app.code_only_rewind = true;

        app.handle(reverted(&anchor, None))
            .await
            .expect("a revert is handled");

        assert!(
            !app.chat.is_reverted(),
            "a code-only rewind hides no message"
        );
        assert!(
            !app.revert_pending,
            "and holds nothing the fallback lane has to wait on"
        );
        assert!(
            app.editor.is_empty(),
            "nothing was taken back, so nothing is offered again"
        );

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("restored 1 file"), "got:\n{screen}");
        assert!(screen.contains("src/lib.rs"), "got:\n{screen}");
    }

    /// The flag is consumed by the one event it answers: a code-only rewind
    /// followed by an ordinary undo must not leave the undo unrendered.
    #[tokio::test]
    async fn the_code_only_reading_lasts_exactly_one_event() {
        let mut app = with_checkpoints();
        let anchor = two_exchanges(&mut app);
        app.code_only_rewind = true;
        app.handle(reverted(&anchor, None))
            .await
            .expect("the code-only answer is handled");

        app.handle(reverted(&anchor, None))
            .await
            .expect("an ordinary revert is handled");

        assert!(app.chat.is_reverted(), "the second one is an ordinary undo");
        assert!(app.revert_pending, "and the lane pauses for it");
    }

    /// A session that takes no snapshots cannot put files back, and says so
    /// rather than half-rewinding.
    #[tokio::test]
    async fn a_rewind_a_session_cannot_serve_says_so_in_the_status_bar() {
        let mut app = with_checkpoints();
        let anchor = two_exchanges(&mut app);

        app.rewind_to(anchor, RevertScope::Files).await;

        assert!(
            !app.code_only_rewind,
            "a refused rewind leaves no reading behind for an event that will not come"
        );

        let mut terminal = terminal(100, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("takes no snapshots"), "got:\n{screen}");
    }

    /// **Acceptance 7, the refusal.** An id that is not a checkpoint is named
    /// back rather than resolved to the nearest thing that is.
    #[tokio::test]
    async fn a_rewind_to_something_that_is_not_a_checkpoint_is_refused_by_name() {
        let (mut app, mut events) = wired().await;
        typed(&mut app, "the only prompt there is").await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        finish(&mut app, &mut events).await;

        app.rewind_to(
            MessageId::from("msg_nobody".to_owned()),
            RevertScope::Conversation,
        )
        .await;

        let mut terminal = terminal(100, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("no checkpoint named"), "got:\n{screen}");
        assert!(screen.contains("msg_nobody"), "got:\n{screen}");
        assert!(!app.chat.is_reverted(), "and nothing moved");
    }

    #[tokio::test]
    async fn snapshot_rewind_picker_open() {
        let mut app = with_checkpoints();
        app.run_command(command::Action::Rewind).await;

        let mut terminal = terminal(80, 20);
        app.draw(&mut terminal).expect("a frame draws");
        insta::assert_snapshot!(screen(&terminal));
    }

    #[tokio::test]
    async fn snapshot_rewind_scope_choice() {
        let mut app = with_checkpoints();
        app.run_command(command::Action::Rewind).await;
        for code in [KeyCode::Down, KeyCode::Enter] {
            app.handle(key(code, KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }

        let mut terminal = terminal(80, 20);
        app.draw(&mut terminal).expect("a frame draws");
        insta::assert_snapshot!(screen(&terminal));
    }

    // ---- D474: the `/plugin` dialog ----

    /// Writes `text` to `root/relative`, creating directories as needed.
    fn plant(root: &std::path::Path, relative: &str, text: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the fixture tree is creatable");
        }
        fs::write(path, text).expect("the fixture file is writable");
    }

    /// A store at its own temporary root, holding one installed plugin —
    /// `formatter` from `company-tools`, carrying one hook and a skills
    /// directory — built through the store's own doors, never by hand.
    fn plugin_store_fixture(directory: &TempDir) -> ganja_core::plugin::Store {
        let market = directory.path().join("market");
        plant(
            &market,
            ".claude-plugin/marketplace.json",
            r#"{
              "name": "company-tools",
              "owner": { "name": "DevTools" },
              "plugins": [{ "name": "formatter", "source": "./plugins/formatter" }]
            }"#,
        );
        plant(
            &market,
            "plugins/formatter/hooks/hooks.json",
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "true"}]}]}}"#,
        );
        plant(&market, "plugins/formatter/skills/fmt/SKILL.md", "# fmt\n");

        let store = ganja_core::plugin::Store::at(directory.path().join("plugin-store"));
        store
            .add_marketplace(market.to_str().expect("the fixture path is unicode"))
            .expect("the fixture marketplace adds");
        store
            .install("formatter", "company-tools")
            .expect("the fixture plugin installs");

        store
    }

    /// Ticks until the `/plugin` store action running off the loop has been
    /// reaped and its outcome is on the dialog.
    ///
    /// What the real loop does on its own — [`App::wants_wakeup`] keeps it
    /// ticking while the slot is full — driven by hand, because a test owns
    /// its own clock. Every store action is one tick later than the keypress
    /// that asked for it now (`zus`), which is the whole point: the loop
    /// draws while a `git clone` runs.
    async fn settle_plugin(app: &mut App) {
        for _ in 0..600 {
            if app.plugin_task.is_none() {
                return;
            }
            app.handle(AppEvent::Tick).await.expect("a tick is handled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        panic!("the store action never finished");
    }

    /// A blocking task in the shape of a slow clone: it parks for `millis`
    /// and answers the way [`super::run_store_effect`] would.
    ///
    /// A real `git clone` finishes when the network says so, and a test that
    /// raced one would pin timing rather than behavior. This is a real
    /// blocking task in the real slot, which is what the loop's arrangement
    /// is actually about.
    fn parked_add(millis: u64) -> JoinHandle<String> {
        tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(millis));
            "added marketplace slow-market from /slow".to_owned()
        })
    }

    /// `/plugin` opens from the roster and Esc walks back out — from the
    /// list, and from the per-plugin action step, the `/mcp` dialog's own
    /// two-step Esc.
    #[tokio::test]
    async fn slash_plugin_opens_the_dialog_and_esc_walks_back_out() {
        let directory = temporary();
        let store = plugin_store_fixture(&directory);
        let mut app = app().with_plugin_store(store);
        assert_eq!(
            command::lookup("plugin").map(|entry| entry.action),
            Some(command::Action::Plugin),
            "/plugin is on the roster"
        );

        app.run_command(command::Action::Plugin).await;
        assert!(app.plugin_dialog.is_some(), "/plugin opens the dialog");

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.plugin_dialog.is_none(), "escape closes the list step");

        app.run_command(command::Action::Plugin).await;
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        assert!(
            app.plugin_dialog
                .as_ref()
                .is_some_and(component::plugin::Plugin::is_choosing_action),
            "enter on a plugin row opens its actions"
        );
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("escape is handled");
        assert!(app.plugin_dialog.is_none(), "escape closes the action step");
    }

    /// **AC6's agreement half**: the dialog's rows are the store's own
    /// listing — the same `Store::list` the `ganja plugin` CLI prints —
    /// field for field, summary included.
    #[tokio::test]
    async fn the_plugin_dialog_lists_what_the_store_holds_and_agrees_with_the_collector() {
        let directory = temporary();
        let store = plugin_store_fixture(&directory);
        let listings = store.list().expect("the fixture store lists");
        let mut app = app().with_plugin_store(store);

        let (rows, complaint) = app.plugin_rows();
        assert_eq!(complaint, None);
        assert_eq!(rows.len(), listings.len());
        for (row, listing) in rows.iter().zip(&listings) {
            assert_eq!(row.name, listing.name);
            assert_eq!(row.enabled, listing.enabled);
            assert_eq!(row.marketplace, listing.marketplace);
            assert_eq!(
                row.summary,
                component::plugin::summarize(&listing.components),
                "the summary is computed from the collector's own component lines"
            );
        }

        app.run_command(command::Action::Plugin).await;
        let mut terminal = terminal(90, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("formatter"), "got:\n{screen}");
        assert!(screen.contains("Enabled"), "got:\n{screen}");
        assert!(screen.contains("company-tools"), "got:\n{screen}");
        assert!(
            screen.contains("1 hook \u{b7} skills"),
            "the hook and the skills directory both count:\n{screen}"
        );
    }

    /// Disable, enable and remove round-trip through the dialog: each lands
    /// in `plugins.json`, and the rows repaint from the store rather than
    /// from a tally of their own. A disabled plugin's row shows Disabled.
    #[tokio::test]
    async fn enable_disable_and_remove_round_trip_through_the_dialog() {
        let directory = temporary();
        let store = plugin_store_fixture(&directory);
        let mut app = app().with_plugin_store(store.clone());
        app.run_command(command::Action::Plugin).await;

        // Enter opens the row's actions; Enter again runs the toggle, which
        // reads Disable on an enabled row.
        for _ in 0..2 {
            app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }
        settle_plugin(&mut app).await;
        assert!(
            !store.state().expect("the state reads").plugins["formatter"].enabled,
            "the dialog's Disable landed in plugins.json"
        );
        let mut terminal = terminal(90, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("Disabled"),
            "the row repaints disabled:\n{}",
            screen(&terminal)
        );

        // The same toggle now reads Enable.
        for _ in 0..2 {
            app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }
        settle_plugin(&mut app).await;
        assert!(
            store.state().expect("the state reads").plugins["formatter"].enabled,
            "the dialog's Enable landed too"
        );

        // Remove is the action after the toggle.
        for code in [KeyCode::Enter, KeyCode::Down, KeyCode::Enter] {
            app.handle(key(code, KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }
        settle_plugin(&mut app).await;
        assert!(
            store.state().expect("the state reads").plugins.is_empty(),
            "remove deletes the state entry"
        );
        assert!(
            !store.plugin_root("formatter").exists(),
            "and the installed directory with it"
        );
    }

    /// The free-text step is the frontend's own: Enter submits to the store,
    /// Esc cancels the edit, and the engine hears nothing either way — no
    /// event, no question, nothing in the composer.
    #[tokio::test]
    async fn the_add_input_submits_on_enter_and_cancels_on_esc_without_an_engine_command() {
        let directory = temporary();
        let market = directory.path().join("market");
        plant(
            &market,
            ".claude-plugin/marketplace.json",
            r#"{"name": "m", "owner": {"name": "o"}, "plugins": []}"#,
        );
        let store = ganja_core::plugin::Store::at(directory.path().join("plugin-store"));
        let mut app = app().with_plugin_store(store.clone());
        let mut events = app.engine.subscribe().await.expect("the first subscriber");

        app.run_command(command::Action::Plugin).await;
        // An empty store starts the cursor on "Add marketplace".
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        assert!(
            app.plugin_dialog
                .as_ref()
                .is_some_and(component::plugin::Plugin::is_typing),
            "add opens the free-text step"
        );

        // Esc cancels the edit and keeps the dialog open.
        app.handle(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        assert!(
            app.plugin_dialog
                .as_ref()
                .is_some_and(|dialog| !dialog.is_typing()),
            "esc cancels the edit without closing the dialog"
        );

        // Enter with a real path submits to the store.
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        for character in market.to_str().expect("unicode").chars() {
            app.handle(key(KeyCode::Char(character), KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        settle_plugin(&mut app).await;
        assert!(
            store
                .state()
                .expect("the state reads")
                .marketplaces
                .contains_key("m"),
            "the typed marketplace was added"
        );

        assert!(
            app.editor.is_empty(),
            "nothing typed at the dialog reaches the composer"
        );
        assert!(app.question.is_none(), "and no question round trip exists");
        assert!(
            events.next().now_or_never().is_none(),
            "the engine heard nothing from any of it"
        );
    }

    /// A marketplace add that fails surfaces the refusal in the dialog —
    /// including a clone's, whose message carries git's own captured stderr.
    #[tokio::test]
    async fn a_failed_marketplace_add_surfaces_the_captured_error_in_the_dialog() {
        let directory = temporary();
        let store = ganja_core::plugin::Store::at(directory.path().join("plugin-store"));
        let mut app = app().with_plugin_store(store);
        app.run_command(command::Action::Plugin).await;

        // `.git` routes through a real `git clone`, which fails and is
        // captured; the notice is the error's own Display, stderr included.
        let missing = directory.path().join("nowhere.git");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        for character in missing.to_str().expect("unicode").chars() {
            app.handle(key(KeyCode::Char(character), KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        settle_plugin(&mut app).await;

        assert!(app.plugin_dialog.is_some(), "the dialog stays open");
        let mut terminal = terminal(100, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("git clone failed"),
            "the captured failure is on the notice line:\n{}",
            screen(&terminal)
        );
    }

    /// **`zus`**: the event loop answers keys and draws frames *while* a
    /// marketplace add runs, rather than freezing for the clone's duration.
    ///
    /// The proof is the parked task itself: the keys and the frame are
    /// handled, and only then is the task asked whether it has finished. If
    /// the loop had waited for it — the shape this test replaced — it could
    /// not have answered them before the task was done.
    #[tokio::test]
    async fn the_loop_answers_keys_while_a_marketplace_add_is_running() {
        let directory = temporary();
        let store = plugin_store_fixture(&directory);
        let mut app = app().with_plugin_store(store);
        app.run_command(command::Action::Plugin).await;

        app.plugin_task = Some(parked_add(500));
        app.plugin_dialog
            .as_mut()
            .expect("the dialog is open")
            .set_busy(true);

        for code in [KeyCode::Down, KeyCode::Up, KeyCode::Down] {
            app.handle(key(code, KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }
        let mut terminal = terminal(90, 24);
        app.draw(&mut terminal).expect("a frame draws");

        assert!(
            !app.plugin_task
                .as_ref()
                .expect("the action still owns the lane")
                .is_finished(),
            "the keys and the frame were answered while the add ran, not after it"
        );
        assert!(
            app.plugin_dialog
                .as_ref()
                .is_some_and(|dialog| dialog.selected_plugin().is_none()),
            "and the keys really moved the cursor: off the one plugin row"
        );

        settle_plugin(&mut app).await;
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("added marketplace slow-market"),
            "and the outcome lands on the same line one tick later:\n{}",
            screen(&terminal)
        );
        assert!(
            app.plugin_dialog
                .as_ref()
                .is_some_and(|dialog| !dialog.is_busy()),
            "the reap clears the running flag"
        );
    }

    /// One lane, so a second store action while one runs is refused rather
    /// than queued or raced — two writers over the same `plugins.json` is
    /// the outcome nobody asked for. The dialog refuses its own two
    /// store-writing actions before they open; the app catches the rest.
    #[tokio::test]
    async fn a_second_store_action_during_a_clone_is_refused_with_a_notice() {
        let directory = temporary();
        let store = plugin_store_fixture(&directory);
        let mut app = app().with_plugin_store(store.clone());
        app.run_command(command::Action::Plugin).await;

        app.plugin_task = Some(parked_add(400));
        app.plugin_dialog
            .as_mut()
            .expect("the dialog is open")
            .set_busy(true);

        // Down off the one plugin row lands on Add marketplace; Enter would
        // open its input step were an add not already running.
        for code in [KeyCode::Down, KeyCode::Enter] {
            app.handle(key(code, KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }
        assert!(
            app.plugin_dialog
                .as_ref()
                .is_some_and(|dialog| !dialog.is_typing()),
            "the second add never opens its input"
        );
        let mut terminal = terminal(100, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("already running"),
            "and the refusal is on the notice line:\n{}",
            screen(&terminal)
        );

        // The app's own backstop, for what the dialog does not refuse itself:
        // a row's Remove chosen while the add runs.
        app.run_plugin_effect(component::plugin::Effect::Remove("formatter".to_owned()));
        assert!(
            store.plugin_root("formatter").exists(),
            "the refused remove ran nothing"
        );
        assert!(
            !app.plugin_task
                .as_ref()
                .expect("the first action still owns the lane")
                .is_finished(),
            "and nothing was spawned beside it"
        );

        // A dialog closed and reopened while the same action runs is told the
        // lane is still busy, rather than offering an add it would refuse.
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        app.run_command(command::Action::Plugin).await;
        assert!(
            app.plugin_dialog
                .as_ref()
                .is_some_and(component::plugin::Plugin::is_busy),
            "the reopened dialog inherits the running action"
        );

        settle_plugin(&mut app).await;
        assert!(
            app.plugin_dialog
                .as_ref()
                .is_some_and(|dialog| !dialog.is_busy()),
            "the lane frees when the first action lands"
        );
    }

    /// P21 pre-mortem 3: closing the dialog while a marketplace add runs
    /// leaves no panic and nothing half-added.
    ///
    /// The reap only happens on a tick, and the only ticks here come after
    /// the Esc — so the answer provably arrives with no dialog to put it on,
    /// which is the case the delivery had to survive. What keeps the store
    /// clean is its own stage-validate-move, unchanged: a failed clone leaves
    /// no marketplace, and no staging directory either.
    #[tokio::test]
    async fn closing_the_dialog_mid_clone_leaves_no_panic_and_no_half_add() {
        let directory = temporary();
        let root = directory.path().join("plugin-store");
        let store = ganja_core::plugin::Store::at(root.clone());
        let mut app = app().with_plugin_store(store.clone());
        app.run_command(command::Action::Plugin).await;

        // An empty store starts the cursor on "Add marketplace"; `.git`
        // routes through a real `git clone`, which is the slow one.
        let missing = directory.path().join("nowhere.git");
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        for character in missing.to_str().expect("unicode").chars() {
            app.handle(key(KeyCode::Char(character), KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        assert!(app.plugin_task.is_some(), "the clone runs off the loop");
        // Nothing has ticked yet, so this frame is the one drawn *during* the
        // clone: it says what is running rather than showing a stale list.
        let mut terminal = terminal(100, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("adding marketplace from"),
            "the dialog says what is running:\n{}",
            screen(&terminal)
        );

        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        assert!(
            app.plugin_dialog.is_none(),
            "esc closes the dialog with the clone still in flight"
        );

        settle_plugin(&mut app).await;
        assert!(
            app.plugin_dialog.is_none(),
            "the landed answer reopens nothing"
        );
        assert!(
            store
                .state()
                .expect("the state reads")
                .marketplaces
                .is_empty(),
            "the failed clone added no marketplace"
        );
        let leftovers: Vec<String> = fs::read_dir(&root)
            .map(|entries| {
                entries
                    .filter_map(|entry| {
                        let name = entry.ok()?.file_name().to_string_lossy().into_owned();
                        name.starts_with(".staging-").then_some(name)
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "and left no staging directory behind: {leftovers:?}"
        );
    }

    /// **D474 pinned**: the reload notice names exactly what rebuilt
    /// in-session and what needs a restart — the honest split, verbatim.
    #[test]
    fn the_reload_notice_pins_the_honest_split() {
        assert_eq!(
            super::RELOAD_SPLIT,
            "reloaded now: hooks, skills \u{b7} restart required: agents, mcp, lsp"
        );
    }

    /// The dialog's list step, over two fixed rows and the three store
    /// actions (screenshot: no reference available; house `/mcp` chrome).
    #[tokio::test]
    async fn snapshot_plugin_dialog_open() {
        let mut app = app();
        app.plugin_dialog = Some(component::plugin::Plugin::new(vec![
            component::plugin::Row {
                name: "formatter".to_owned(),
                enabled: true,
                marketplace: "company-tools".to_owned(),
                summary: "1 hook \u{b7} skills".to_owned(),
            },
            component::plugin::Row {
                name: "deployer".to_owned(),
                enabled: false,
                marketplace: "company-tools".to_owned(),
                summary: "1 mcp \u{b7} 1 agent".to_owned(),
            },
        ]));

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        insta::assert_snapshot!(screen(&terminal));
    }

    /// The same dialog one Enter later: the per-plugin action menu.
    #[tokio::test]
    async fn snapshot_plugin_action_menu() {
        let mut app = app();
        let mut dialog = component::plugin::Plugin::new(vec![component::plugin::Row {
            name: "formatter".to_owned(),
            enabled: true,
            marketplace: "company-tools".to_owned(),
            summary: "1 hook \u{b7} skills".to_owned(),
        }]);
        assert_eq!(dialog.submit(), None, "enter opens the action step");
        app.plugin_dialog = Some(dialog);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        insta::assert_snapshot!(screen(&terminal));
    }

    /// The lead side (**D503**): an app leading a real team over a store and a
    /// teams root under `directory`, plus the stream its own loop would read.
    ///
    /// The session id is §2.1's own example, so the team on disk is
    /// `session-224cbeab` and a reader can find it by hand.
    async fn leading(
        directory: &TempDir,
    ) -> (
        App,
        Arc<ganja_core::teammate::TeammateRegistry>,
        BoxStream<'static, CoreEvent>,
    ) {
        let registry = Arc::new(ganja_core::teammate::TeammateRegistry::for_session(
            directory.path(),
            "224cbeab-4e62-497c-aa8f-d05cc33ce7ba",
            directory.path(),
        ));
        let engine = Engine::persistent(
            Arc::new(FakeProvider::default()),
            fake::MODEL,
            Arc::new(ganja_tool::Registry::new(Vec::new())),
            ganja_permission::Permissions::default(),
            Storage::open(directory.path().join("storage")),
        )
        .with_teammates(Arc::clone(&registry));
        let events = engine.subscribe().await.expect("the test subscribes first");

        (App::new(engine, None, Themes::builtin()), registry, events)
    }

    /// Writes one plain message into the lead's own inbox, as a peer would.
    fn peer_writes(registry: &ganja_core::teammate::TeammateRegistry, from: &str, text: &str) {
        crate::member::write(&registry.lead_inbox(), from, text);
    }

    /// Everything the stream will answer right now, without waiting.
    fn drained(events: &mut BoxStream<'static, CoreEvent>) -> Vec<CoreEvent> {
        std::iter::from_fn(|| events.next().now_or_never().flatten()).collect()
    }

    /// Leaves `app` with a turn really streaming, so a delivery takes the
    /// steer lane rather than being refused as `NotStreaming`.
    async fn turn_in_flight(app: &mut App, events: &mut BoxStream<'static, CoreEvent>) {
        for event in typing("what is left") {
            app.handle(event).await.expect("typing is handled");
        }
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("enter is handled");
        pump(app, events, 2).await;
        assert!(app.turn_running, "the fixture needs a turn in flight");
    }

    /// What is left in the lead's inbox.
    fn still_owed(registry: &ganja_core::teammate::TeammateRegistry) -> usize {
        ganja_core::team::mailbox::read(&registry.lead_inbox())
            .expect("the inbox reads")
            .valid
            .len()
    }

    /// **AC-10's lead leg**, idle half: a teammate's message reaches the
    /// conversation on the very next tick, and only then leaves the mailbox.
    #[tokio::test]
    async fn a_teammates_message_reaches_an_idle_conversation_and_only_then_leaves_the_mailbox() {
        let directory = temporary();
        let (mut app, registry, mut events) = leading(&directory).await;
        peer_writes(&registry, "w1", "the parser is done");
        assert_eq!(still_owed(&registry), 1);

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        // A **peer part**, not text (**D495**): the words are attributed on the
        // user message the turn starts from, and the request assembly is what
        // wraps them in §5.3's envelope. `as_text` is deliberately blind to it,
        // which is what keeps a teammate's sentence out of `/copy` and out of a
        // checkpoint title — so a test looking for text here would find nothing
        // even when delivery worked.
        let mut attributed = None;
        for event in drained(&mut events) {
            if let CoreEvent::MessageStarted { message, .. } = event {
                for part in &message.parts {
                    if let PartBody::Peer { from, body, .. } = &part.body {
                        attributed = Some((from.clone(), body.clone()));
                    }
                    assert_eq!(
                        part.as_text(),
                        None,
                        "a peer's words are never this conversation's text"
                    );
                }
            }
        }
        assert_eq!(
            attributed,
            Some(("w1".to_owned(), "the parser is done".to_owned())),
            "the message became a turn of the lead's own, and says who wrote it"
        );
        assert_eq!(
            still_owed(&registry),
            0,
            "a delivered message does not remain"
        );
    }

    /// **AC-10's lead leg**, the other half of the same rule: a control frame
    /// is acted on and **never** queued, so nothing about it reaches the model
    /// or the strip.
    #[tokio::test]
    async fn a_control_frame_from_a_teammate_never_reaches_the_strip_or_the_model() {
        let directory = temporary();
        let (mut app, registry, mut events) = leading(&directory).await;
        let approved =
            ganja_protocol::team::Frame::ShutdownApproved(ganja_protocol::team::ShutdownApproved {
                request_id: "req-1".to_owned(),
                from: "w1".to_owned(),
                timestamp: ganja_core::team::record::now_iso8601(),
                pane_id: None,
                backend_type: None,
            });
        crate::member::write_frame(&registry.lead_inbox(), "w1", &approved);

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert_eq!(app.queue.depth(), 0, "a control frame is not a message");
        assert!(
            !drained(&mut events)
                .iter()
                .any(|event| matches!(event, CoreEvent::MessageStarted { .. })),
            "and nothing about it is put to the model"
        );
        assert_eq!(still_owed(&registry), 0, "it was acted on and pruned");
    }

    /// **D503's split.** An `Acknowledged` peer's message is rendered pending
    /// until the engine says it took it; a `FireAndForget` peer's is retired at
    /// write time, because the acknowledgement it would wait for never comes.
    #[tokio::test]
    async fn the_strip_holds_an_acknowledged_peers_message_and_never_a_fire_and_forget_one() {
        let directory = temporary();
        let (mut app, _registry, mut events) = leading(&directory).await;
        turn_in_flight(&mut app, &mut events).await;

        assert!(
            app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
                "w1",
                "2026-08-17T00:00:00.000Z",
                "have a look at the parser",
                ganja_core::teammate::Delivery::Acknowledged,
            )])
            .await,
            "the engine took the steer"
        );
        assert_eq!(app.queue.depth(), 1, "pending until the turn consumes it");
        assert!(app.queue.entries()[0].is_steered());

        assert!(
            app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
                "claude-peer",
                "2026-08-17T00:00:01.000Z",
                "and one from a pane",
                ganja_core::teammate::Delivery::FireAndForget,
            )])
            .await
        );
        assert_eq!(
            app.queue.depth(),
            1,
            "sent at write time, so nothing new is pending"
        );
    }

    /// **§7-5, and the door it closes.** A teammate's message waits on the
    /// same strip a typed one does, and Up must not lift it into the composer:
    /// Enter there resolves `@` mentions, loads `$` skills and runs `/`
    /// commands, and the person at this terminal consented to none of it. The
    /// body is exactly the three tokens that would be acted on.
    #[tokio::test]
    async fn a_peers_message_cannot_be_recalled_into_the_composer() {
        let directory = temporary();
        let (mut app, _registry, mut events) = leading(&directory).await;
        turn_in_flight(&mut app, &mut events).await;
        app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
            "w1",
            "2026-08-17T00:00:00.000Z",
            "@Cargo.toml /init $skill",
            ganja_core::teammate::Delivery::Acknowledged,
        )])
        .await;
        assert_eq!(app.queue.depth(), 1);

        app.handle(key(KeyCode::Up, KeyModifiers::NONE))
            .await
            .expect("the arrow is handled");

        assert_eq!(
            app.editor.text(),
            "what is left",
            "the strip holds nothing this person wrote, so Up falls through to \
             the history walk exactly as an empty strip does — and what it \
             finds there is their own last prompt"
        );
        assert_eq!(app.queue.depth(), 1, "the peer's row is still waiting");

        // The person's own entry is still theirs to take back, from under the
        // peer's row.
        app.editor.set_text("");
        app.queue
            .push_steered("steer-99".to_owned(), "mine".to_owned());
        app.handle(key(KeyCode::Up, KeyModifiers::NONE))
            .await
            .expect("the arrow is handled");

        assert_eq!(app.editor.text(), "mine");
        assert_eq!(app.queue.depth(), 1, "and the peer's row stayed put");
    }

    /// **Delivery is not idempotent; only pruning is.** An `Acknowledged`
    /// sender's message stays in the inbox until the turn provably took it, so
    /// every pass in between offers it again — and a second pass that
    /// *delivered* it again would put the same words to the model over and
    /// over for the whole length of a long step.
    ///
    /// The message is written into the mailbox and handed over by hand, since
    /// only a member this registry really started reports `Acknowledged`, and
    /// the identity §2.3 composes has to be the same one on both sides for the
    /// pass to recognise what it is already holding.
    #[tokio::test]
    async fn a_message_still_awaiting_its_consumption_is_not_delivered_twice() {
        let directory = temporary();
        let (mut app, registry, mut events) = leading(&directory).await;
        turn_in_flight(&mut app, &mut events).await;
        let when = "2026-08-17T00:00:00.000Z";
        ganja_core::team::mailbox::write(
            &registry.lead_inbox(),
            ganja_core::team::MailboxMessage::new("w1", "the parser is done", when),
        )
        .expect("the lead's inbox takes a message");
        assert!(
            app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
                "w1",
                when,
                "the parser is done",
                ganja_core::teammate::Delivery::Acknowledged,
            )])
            .await
        );
        assert_eq!(app.steers, 1);
        assert_eq!(app.queue.depth(), 1);
        assert_eq!(
            still_owed(&registry),
            1,
            "the mailbox keeps it until the turn says it took it, which is \
             exactly what makes the next pass offer it again"
        );

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert_eq!(app.steers, 1, "the same message is not steered twice");
        assert_eq!(app.queue.depth(), 1, "and the strip did not grow a copy");
    }

    /// **One command per pass.** The engine takes a batch of peers on one
    /// `Steer`, and sending them one at a time made the second refuse: `Busy`
    /// is what an engine answers between accepting a prompt and running the
    /// turn it becomes, so the rest of a pass waited out another cadence for
    /// nothing.
    #[tokio::test]
    async fn a_whole_pass_of_messages_crosses_as_one_command() {
        let directory = temporary();
        let (mut app, registry, mut events) = leading(&directory).await;
        turn_in_flight(&mut app, &mut events).await;
        peer_writes(&registry, "w1", "the parser is done");
        peer_writes(&registry, "w2", "and the lexer");
        peer_writes(&registry, "w1", "one more thing");

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert_eq!(app.steers, 1, "three messages, one command");
        assert_eq!(
            still_owed(&registry),
            0,
            "and one prune for the whole batch"
        );

        // The strip still renders *messages*, one row each, all standing for
        // the single steer that carried them — shown here with senders whose
        // backend acknowledges, since those are the rows that wait.
        app.deliver_peers(vec![
            ganja_core::teammate::lead_inbox::Delivered::new(
                "w1",
                "2026-08-17T00:00:00.000Z",
                "have a look at the parser",
                ganja_core::teammate::Delivery::Acknowledged,
            ),
            ganja_core::teammate::lead_inbox::Delivered::new(
                "w2",
                "2026-08-17T00:00:01.000Z",
                "and at the lexer",
                ganja_core::teammate::Delivery::Acknowledged,
            ),
        ])
        .await;

        assert_eq!(app.steers, 2, "one more command");
        assert_eq!(app.queue.depth(), 2, "and one row per message");
        assert_eq!(
            app.queue
                .entries()
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            1,
            "all under the one id that will retire them together"
        );
    }

    /// An idle lead is not a busy one: the only thing it is waiting for is a
    /// file at §6's own thousand-millisecond cadence, so that is how long it
    /// sleeps rather than sixty times a second forever.
    #[tokio::test]
    async fn an_idle_lead_sleeps_until_its_next_mailbox_pass_rather_than_at_frame_rate() {
        let directory = temporary();
        let (mut app, _registry, _events) = leading(&directory).await;

        app.handle(AppEvent::Tick).await.expect("a tick is handled");
        app.dirty = false;

        assert!(app.wants_wakeup(), "a lead still has to keep waking");
        assert!(
            app.until_next_wakeup() > FRAME,
            "but for the mailbox rather than for a frame: {:?}",
            app.until_next_wakeup()
        );

        // Anything that is really about to be drawn takes the frame clock back.
        app.open_team();
        assert!(app.until_next_wakeup() <= FRAME);
    }

    /// A steer the turn never took is **not** handed to the replay lane: that
    /// lane resolves mentions, loads skills and matches command names, and a
    /// peer's words consent to none of it (§7-5). It goes back to the mailbox
    /// it was never pruned from.
    #[tokio::test]
    async fn a_peers_unconsumed_message_leaves_the_strip_rather_than_becoming_a_prompt() {
        let directory = temporary();
        let (mut app, _registry, mut events) = leading(&directory).await;
        turn_in_flight(&mut app, &mut events).await;
        app.deliver_peers(vec![ganja_core::teammate::lead_inbox::Delivered::new(
            "w1",
            "2026-08-17T00:00:00.000Z",
            "@Cargo.toml /init $skill",
            ganja_core::teammate::Delivery::Acknowledged,
        )])
        .await;
        assert_eq!(app.queue.depth(), 1);

        app.strand_peers();

        assert_eq!(app.queue.depth(), 0, "the strip entry is given back");
        assert!(
            !app.queue.has_fallback(),
            "and it is emphatically not the replay lane's to interpret"
        );
    }

    /// **D-5**: a teammate's dialog is shown through the machinery the
    /// engine's own goes through, and answered back down the channel it
    /// arrived on rather than as a command to an engine that holds nothing by
    /// that id.
    #[tokio::test]
    async fn a_teammates_permission_dialog_is_shown_and_answered_on_its_own_channel() {
        let mut app = app();
        let (reply, answered) = tokio::sync::oneshot::channel();
        let id = PermissionId::ascending();
        app.raise_teammate_dialog(ganja_core::teammate::posture::Forwarded {
            teammate: "w1".to_owned(),
            request: CoreEvent::PermissionRequested {
                session_id: session(),
                id: id.clone(),
                call_id: "call-1".to_owned(),
                tool: "bash".to_owned(),
                title: "rm -rf build".to_owned(),
                args: serde_json::json!({"command": "rm -rf build"}),
                directories: Vec::new(),
            },
            reply,
        });

        let dialog = app.permission.as_ref().expect("the dialog is on screen");
        assert_eq!(dialog.id(), &id);
        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(
            screen.contains("w1"),
            "whose call it is, is the thing this dialog has that the engine's own has not:\n{screen}"
        );

        app.handle(AppEvent::Term(TermEvent::Key(KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
        ))))
        .await
        .expect("the key is handled");

        assert_eq!(
            answered.await,
            Ok(PermissionReply::Once),
            "the answer goes back to the engine that is waiting on it"
        );
        assert!(app.permission.is_none(), "and the dialog is retired");
    }

    /// A yolo session stands in for the person on a teammate's dialog exactly
    /// as it does for its own — and answers `Once`, never `Always`, because a
    /// teammate's stored rules are not the lead's to write (**D479**).
    #[tokio::test]
    async fn a_yolo_lead_answers_its_teammates_dialogs_without_drawing_one() {
        let mut app = app().with_yolo(true);
        let (reply, answered) = tokio::sync::oneshot::channel();
        app.raise_teammate_dialog(ganja_core::teammate::posture::Forwarded {
            teammate: "w1".to_owned(),
            request: CoreEvent::PermissionRequested {
                session_id: session(),
                id: PermissionId::ascending(),
                call_id: "call-1".to_owned(),
                tool: "bash".to_owned(),
                title: "cargo build".to_owned(),
                args: serde_json::json!({"command": "cargo build"}),
                directories: Vec::new(),
            },
            reply,
        });

        assert!(app.permission.is_none(), "nobody was going to be asked");
        assert_eq!(answered.await, Ok(PermissionReply::Once));
    }

    /// A session leading no team does none of this and pays nothing for it,
    /// which is what keeps every other test in this file unchanged.
    #[tokio::test]
    async fn a_session_leading_no_team_polls_nothing_and_counts_nobody() {
        let mut app = app();

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert!(app.lead_inbox.is_none());
        assert!(app.teammate_dialogs.is_none());
        assert_eq!(app.teammates, 0);
        assert!(
            app.team_polled.is_none(),
            "there is no mailbox to have polled"
        );
        assert!(
            app.engine.teammates().is_none(),
            "leading no team is a different answer from leading an empty one"
        );
    }

    /// The frontend's own claim about the roster (**D504**): `open_team`
    /// renders the lead as the dialog's first row, with an empty ring, and a
    /// session with no teammates yet still has a team to show. The registry
    /// invariant underneath — lead first, the only `is_lead` row — is core's
    /// own to pin.
    #[tokio::test]
    async fn the_roster_a_team_dialog_renders_starts_as_the_lead_and_nobody_else() {
        let directory = temporary();
        let (mut app, _registry, _events) = leading(&directory).await;

        app.open_team();

        let dialog = app.team_dialog.as_ref().expect("the dialog opened");
        let lead = dialog.selected_member().expect("the cursor opens on a row");
        assert_eq!(lead.name, "team-lead");
        assert!(lead.is_lead);
        assert!(lead.recent.is_empty());
    }

    /// **Asking for the roster is what raises the dialog**, and nothing else
    /// is. The palette's data-free action asks for it and gets it; a line that
    /// asked for something else is answered where the person who typed it is
    /// looking, with no overlay in front of the composer they are still at.
    #[tokio::test]
    async fn only_asking_for_the_roster_raises_the_team_dialog() {
        let directory = temporary();
        let (mut app, _registry, _events) = leading(&directory).await;

        app.run_command(command::Action::Team).await;
        assert!(
            app.team_dialog.is_some(),
            "the palette's door asks for the roster"
        );
        app.team_dialog = None;

        app.editor.set_text("/team wat");
        app.submit().await;

        assert!(
            app.team_dialog.is_none(),
            "a line that did not ask for the roster does not raise it"
        );
        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(
            screen.contains("wat"),
            "and the refusal is still said, on the bar instead:\n{screen}"
        );
        assert!(
            app.editor.prompt().is_none(),
            "and the line it came from is out of the composer"
        );

        // `/team list` is the typed spelling of the very same ask, so it
        // raises the dialog exactly as the palette's row does.
        app.editor.set_text("/team list");
        app.submit().await;
        assert!(app.team_dialog.is_some(), "the typed door onto the roster");
    }

    /// A session with nowhere to keep a team says so once, rather than opening
    /// a dialog about nothing.
    #[tokio::test]
    async fn team_on_a_session_leading_none_refuses_readably_instead_of_opening() {
        let mut app = app();

        app.run_command(command::Action::Team).await;

        assert!(app.team_dialog.is_none());
        let mut terminal = terminal(100, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("leads no team"), "{screen}");
    }

    /// Every `/team` line with arguments joins the prompt history — accepted
    /// or refused by the grammar — because the words leave the composer either
    /// way and the history is where Up finds them again. A bare `/team` is the
    /// palette's own door, like `/help`, and is remembered no more than those
    /// are.
    #[tokio::test]
    async fn a_team_line_is_remembered_whatever_it_turned_out_to_mean() {
        let directory = temporary();
        let mut app = app_with_history(&directory, &[]);

        for line in [
            "/team spawn w1 --backend in-process explain this crate",
            "/team wat",
            "/team",
        ] {
            app.editor.set_text(line);
            app.submit().await;
            app.team_dialog = None;
            assert!(
                app.editor.prompt().is_none(),
                "{line}: the line left the composer"
            );
        }

        let remembered: Vec<String> = app
            .history
            .entries()
            .into_iter()
            .map(|recalled| recalled.prompt.input)
            .collect();
        assert_eq!(
            remembered,
            [
                "/team wat",
                "/team spawn w1 --backend in-process explain this crate",
            ],
            "newest first, every argument-bearing line as typed, the bare ask not"
        );
        assert_eq!(
            app.history
                .step(history::Direction::Older, "")
                .map(|recalled| recalled.input)
                .as_deref(),
            Some("/team wat"),
            "and Up brings the newest one back to fix"
        );
    }

    /// A spawn decided in the `/team` dialog is remembered as the composer
    /// line it is equivalent to, so the two doors leave the same thing behind.
    #[tokio::test]
    async fn a_spawn_from_the_team_dialog_is_remembered_as_its_team_spawn_line() {
        let directory = temporary();
        let mut app = app_with_history(&directory, &[]);
        let Some(command::Team::Spawn(line)) =
            command::team("/team spawn w2 --backend in-process hold the fort")
        else {
            panic!("a /team spawn line parses");
        };

        app.run_team_effect(component::team::Effect::Spawn {
            request: component::team::spawn_request(&line),
            typed: "w2 --backend in-process hold the fort".to_owned(),
        })
        .await;

        assert_eq!(
            app.history
                .entries()
                .first()
                .map(|recalled| recalled.prompt.input.as_str()),
            Some("/team spawn w2 --backend in-process hold the fort")
        );
    }

    /// One `/team` dialog row, as the fixture below hand-builds them.
    fn team_row(
        name: &str,
        backend: ganja_protocol::MemberBackend,
        is_lead: bool,
        recent: &[&str],
    ) -> component::team::Row {
        component::team::Row {
            name: name.to_owned(),
            backend,
            is_lead,
            color: None,
            recent: recent.iter().map(|call| (*call).to_owned()).collect(),
        }
    }

    /// The `/team` dialog the tests below open: a lead and two members, one
    /// with a ring — hand-built, the way the plugin snapshots build theirs, so
    /// what is pinned is layout and key routing rather than a registry's
    /// timing.
    fn team_dialog() -> component::team::Team {
        component::team::Team::new(vec![
            team_row(
                "team-lead",
                ganja_protocol::MemberBackend::InProcess,
                true,
                &[],
            ),
            team_row(
                "w1",
                ganja_protocol::MemberBackend::InProcess,
                false,
                &["read(src/lib.rs)", "grep(fn spawn)"],
            ),
            team_row("w2", ganja_protocol::MemberBackend::Claude, false, &[]),
        ])
    }

    /// The `/team` dialog's members step: every member with its backend and
    /// its ring, the lead marked, the Spawn row after them.
    #[tokio::test]
    async fn snapshot_team_dialog_open() {
        let mut app = app();
        app.team_dialog = Some(team_dialog());

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        insta::assert_snapshot!(screen(&terminal));
    }

    /// The `/team` dialog's per-member action step: whose actions these are,
    /// then Message and Shutdown.
    #[tokio::test]
    async fn snapshot_team_action_menu() {
        let mut app = app();
        let mut dialog = team_dialog();
        dialog.move_selection(1);
        assert_eq!(dialog.submit(), None, "enter opens the action step");
        app.team_dialog = Some(dialog);

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        insta::assert_snapshot!(screen(&terminal));
    }

    /// Esc walks the `/team` dialog back out from every step: the free-text
    /// step consumes the first press as "cancel the edit", and the other two
    /// close the dialog — the `/mcp` and `/plugin` dialogs' own Esc.
    #[tokio::test]
    async fn esc_closes_the_team_dialog_from_either_step() {
        let mut app = app();

        app.team_dialog = Some(team_dialog());
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        assert!(app.team_dialog.is_none(), "Esc on the members step closes");

        let mut dialog = team_dialog();
        dialog.move_selection(1);
        dialog.submit();
        assert!(dialog.is_choosing_action());
        app.team_dialog = Some(dialog);
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        assert!(app.team_dialog.is_none(), "Esc on the action step closes");

        let mut dialog = team_dialog();
        dialog.move_selection(9);
        dialog.submit();
        assert!(dialog.is_typing());
        app.team_dialog = Some(dialog);
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        let open = app.team_dialog.as_ref().expect("the dialog stays open");
        assert!(!open.is_typing(), "the first Esc only abandons the edit");
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        assert!(app.team_dialog.is_none(), "and the second closes");
    }

    /// While the `/team` dialog is open it owns every key: a list-step press
    /// moves the cursor or is swallowed, and the free-text step's characters
    /// land in the dialog's own buffer — none of it reaches the composer.
    #[tokio::test]
    async fn keys_while_the_team_dialog_is_open_do_not_reach_the_editor() {
        let mut app = app();
        app.team_dialog = Some(team_dialog());

        for code in [
            KeyCode::Char('x'),
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('q'),
        ] {
            app.handle(key(code, KeyModifiers::NONE))
                .await
                .expect("the key is handled");
        }
        assert_eq!(app.editor.text(), "", "nothing leaked past the dialog");
        assert!(
            app.team_dialog.is_some(),
            "and none of it closed the dialog"
        );

        let dialog = app.team_dialog.as_mut().expect("the dialog is open");
        dialog.move_selection(9);
        dialog.submit();
        for event in typing("w3") {
            app.handle(event).await.expect("typing is handled");
        }
        assert_eq!(
            app.team_dialog.as_ref().and_then(|dialog| dialog.input()),
            Some("w3"),
            "the characters landed in the dialog's buffer"
        );
        assert_eq!(app.editor.text(), "", "the composer never saw a character");
    }

    /// The free-text step's Enter runs the dialog's effect: a message typed at
    /// a member goes through `run_team_effect`, and what the mailbox answered
    /// lands on the dialog's own notice line.
    #[tokio::test]
    async fn enter_on_the_team_dialogs_input_step_runs_the_effect() {
        let directory = temporary();
        let (mut app, _registry, _events) = leading(&directory).await;
        // w1 is a row of the hand-built dialog and nobody in the registry's
        // roster, so the delivery below is refused by the roster — the answer
        // that proves the effect really ran.
        let mut dialog = team_dialog();
        dialog.move_selection(1);
        dialog.submit();
        app.team_dialog = Some(dialog);
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");
        assert!(
            app.team_dialog
                .as_ref()
                .is_some_and(component::team::Team::is_typing),
            "Message opens the free-text step"
        );
        for event in typing("status?") {
            app.handle(event).await.expect("typing is handled");
        }

        app.handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the key is handled");

        let mut terminal = terminal(90, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(
            screen.contains("nobody on this team answers to that name"),
            "{screen}"
        );
    }

    /// A spawn's own dialog is raised by the tick and answered on the channel
    /// the asker is waiting on — the same machinery a teammate's call dialog
    /// uses, on the other question (**D-5**).
    #[tokio::test]
    async fn a_spawns_own_dialog_is_raised_on_the_tick_and_answered_back_to_the_asker() {
        let directory = temporary();
        let (mut app, _registry, _events) = leading(&directory).await;
        let (reply, answered) = tokio::sync::oneshot::channel();
        app.spawn_asker
            .send((
                ganja_core::SpawnAsk {
                    title: "start teammate w1 on the in-process backend".to_owned(),
                    args: serde_json::json!({"name": "w1"}),
                    directories: Vec::new(),
                },
                reply,
            ))
            .await
            .expect("the queue takes the question");

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert!(app.permission.is_some(), "somebody is being asked");
        app.handle(AppEvent::Term(TermEvent::Key(KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
        ))))
        .await
        .expect("the key is handled");

        assert_eq!(answered.await, Ok(PermissionReply::Once));
        assert!(app.permission.is_none());
    }

    /// Starts `w1` through the typed door and waits, off the loop, for the
    /// registry to hold it — without ticking the app, so a test can watch what
    /// the next tick does with the moved roster.
    ///
    /// **The backend is named**, and naming it is Dv-1's own audit clause: an
    /// absent `--backend` means `ganja` since that deviation, so this line
    /// without one splits a real pane in whatever tmux session the developer
    /// happens to be sitting in. What these tests mean is in-process semantics
    /// — a member the registry holds, so the tick and the dialog have a roster
    /// that moved — and that is now said rather than inherited from a default.
    async fn registry_holds_w1(app: &mut App, registry: &ganja_core::teammate::TeammateRegistry) {
        app.run_team_line(
            command::team("/team spawn w1 --backend in-process").expect("a /team line"),
        )
        .await;
        assert!(app.team_spawn.is_some(), "the spawn runs off the loop");
        for _ in 0..500 {
            if registry.view().members.len() == 2 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the registry never recorded the spawn");
    }

    /// An open `/team` dialog repaints when the roster really moved, and
    /// leaves the frame alone when it did not — `poll_team_dialog`'s
    /// changed-only rule.
    #[tokio::test]
    async fn an_open_team_dialog_repaints_only_when_the_roster_moved() {
        let directory = temporary();
        let (mut app, registry, _events) = leading(&directory).await;
        app.open_team();

        app.dirty = false;
        app.poll_team_dialog();
        assert!(!app.dirty, "a roster nobody touched is no reason to redraw");

        registry_holds_w1(&mut app, &registry).await;

        app.dirty = false;
        app.poll_team_dialog();
        assert!(app.dirty, "a roster that grew is");
        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        assert!(
            screen(&terminal).contains("w1"),
            "and the refreshed dialog shows the new row"
        );

        app.dirty = false;
        app.poll_team_dialog();
        assert!(!app.dirty, "a dialog already caught up stays quiet");
    }

    /// The in-process reap (**D504**): the tick reaps the finished spawn, the
    /// outcome is said with Resolution 4's cleartext-path sentence, and the
    /// bar counts the teammate.
    ///
    /// Driven through the **typed** door, which raises no dialog, so what this
    /// pins is the half of that change that had to hold: the sentence is the
    /// same sentence, and the status bar carries the path the dialog used to.
    #[tokio::test]
    async fn a_team_spawn_is_reaped_by_the_tick_and_says_where_the_prompt_landed() {
        let directory = temporary();
        let (mut app, registry, _events) = leading(&directory).await;
        registry_holds_w1(&mut app, &registry).await;

        for _ in 0..500 {
            if app.team_spawn.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            app.handle(AppEvent::Tick).await.expect("a tick is handled");
        }

        assert!(app.team_spawn.is_none(), "the tick reaped the spawn");
        let mut terminal = terminal(90, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("w1 started"), "{screen}");
        assert!(
            screen.contains("prompt persisted in cleartext at"),
            "{screen}"
        );
        assert_eq!(app.teammates, 1, "the bar counts the started teammate");
    }

    /// A teammate that stopped waiting — its turn cancelled, its process gone
    /// — does not wedge the queue: the answer is dropped, the next dialog is
    /// asked, and nothing panics.
    #[tokio::test]
    async fn answering_a_dialog_whose_teammate_stopped_waiting_still_advances_the_queue() {
        let mut app = app();
        let asked = |name: &str, id: &PermissionId| ganja_core::teammate::posture::Forwarded {
            teammate: name.to_owned(),
            request: CoreEvent::PermissionRequested {
                session_id: session(),
                id: id.clone(),
                call_id: "call-1".to_owned(),
                tool: "bash".to_owned(),
                title: "rm -rf build".to_owned(),
                args: serde_json::json!({"command": "rm -rf build"}),
                directories: Vec::new(),
            },
            reply: tokio::sync::oneshot::channel().0,
        };
        let first = PermissionId::ascending();
        let second = PermissionId::ascending();
        app.raise_teammate_dialog(asked("w1", &first));
        app.raise_teammate_dialog(asked("w2", &second));
        assert_eq!(app.queued_permissions.len(), 1, "the second is queued");

        assert!(
            app.answer_teammate_dialog(&first, PermissionReply::Once),
            "the id was known even though nobody is listening"
        );

        let on_screen = app.permission.as_ref().expect("the queue advanced");
        assert_eq!(on_screen.id(), &second);
        assert!(app.answer_teammate_dialog(&second, PermissionReply::Once));
        assert!(app.permission.is_none());
    }

    /// `/team shutdown` with nobody named is the whole team, and a team that is
    /// only its lead is told so rather than silently doing nothing.
    #[tokio::test]
    async fn shutting_down_a_team_of_nobody_says_so_rather_than_writing_to_the_lead() {
        let directory = temporary();
        let (mut app, registry, _events) = leading(&directory).await;
        app.open_team();

        app.ask_whole_team_to_stop().await;

        let mut terminal = terminal(80, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("no teammates to stop"), "{screen}");
        assert_eq!(
            still_owed(&registry),
            0,
            "and the lead did not write a shutdown request to itself"
        );
    }

    /// A message typed at a member goes through the lead's own postbox, so an
    /// unknown name is refused by the roster rather than written anywhere.
    #[tokio::test]
    async fn a_message_to_a_name_nobody_answers_to_is_refused_on_the_dialog() {
        let directory = temporary();
        let (mut app, _registry, _events) = leading(&directory).await;
        app.open_team();

        app.run_team_effect(component::team::Effect::Message {
            to: "nobody".to_owned(),
            text: "hello".to_owned(),
        })
        .await;

        let mut terminal = terminal(90, 24);
        app.draw(&mut terminal).expect("a frame draws");
        let screen = screen(&terminal);
        assert!(screen.contains("nobody"), "{screen}");
    }

    /// The member side (§10.3): an app running **as** a pane teammate `w1` of
    /// the team a lead of §2.1's example session would lead, over a teams root
    /// under `directory`, plus the stream its own loop would read.
    ///
    /// No registry is installed and no team is led — a pane teammate is a
    /// member of the one that launched it — so what this app has is exactly a
    /// bare engine plus the member's inbox.
    async fn membered(directory: &TempDir) -> (App, BoxStream<'static, CoreEvent>) {
        let membership = crate::member::membership(directory.path(), Some("%5"));
        let engine = Engine::persistent(
            Arc::new(FakeProvider::default()),
            fake::MODEL,
            Arc::new(ganja_tool::Registry::new(Vec::new())),
            ganja_permission::Permissions::default(),
            Storage::open(directory.path().join("storage")),
        );
        let events = engine.subscribe().await.expect("the test subscribes first");
        let app = App::new(engine, None, Themes::builtin())
            .with_member(crate::member::Inbox::new(membership));

        (app, events)
    }

    /// The member's own inbox and the lead's, as the app resolved them.
    fn member_paths(app: &App) -> (std::path::PathBuf, std::path::PathBuf) {
        let membership = app
            .member
            .as_ref()
            .expect("the app is a member")
            .membership();

        (membership.inbox(), membership.lead_inbox())
    }

    /// Writes one plain message into the member's inbox, as the lead would —
    /// which is also exactly what the spawn's inbox seed is.
    fn lead_writes(inbox: &std::path::Path, text: &str) {
        crate::member::write(inbox, "team-lead", text);
    }

    /// What is left in a member-side inbox.
    fn owed(inbox: &std::path::Path) -> usize {
        crate::member::held(inbox).len()
    }

    /// Every frame the lead's inbox holds, by kind, oldest first.
    fn lead_heard(lead_inbox: &std::path::Path) -> Vec<ganja_protocol::team::Frame> {
        ganja_core::team::mailbox::read(lead_inbox)
            .expect("the lead's inbox reads")
            .valid
            .iter()
            .filter_map(ganja_core::team::MailboxMessage::frame)
            .collect()
    }

    /// The Peer part the turn's `MessageStarted` carried, if it carried one.
    fn attributed_start(seen: &[CoreEvent]) -> Option<(String, String)> {
        seen.iter().find_map(|event| match event {
            CoreEvent::MessageStarted { message, .. } => {
                message.parts.iter().find_map(|part| match &part.body {
                    PartBody::Peer { from, body, .. } => Some((from.clone(), body.clone())),
                    _ => None,
                })
            }
            _ => None,
        })
    }

    /// **AC-10's pane leg, the drain and idle seams** (§10.3-1, -2, -3): the
    /// prompt the lead seeded is the first turn, delivered as the lead's own
    /// attributed words; the turn's end reaches the lead as
    /// `idle_notification{available}` stamped with this member's name; and the
    /// seed leaves the inbox only once the turn provably took it.
    #[tokio::test]
    async fn a_pane_teammates_seeded_prompt_is_its_first_turn_and_its_end_tells_the_lead() {
        let directory = temporary();
        let (mut app, mut events) = membered(&directory).await;
        let (inbox, lead_inbox) = member_paths(&app);
        lead_writes(&inbox, "start on the parser");

        // The first tick is due at once — nothing has polled yet.
        app.dirty = false;
        assert_eq!(app.until_next_wakeup(), Duration::ZERO);
        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert_eq!(
            owed(&inbox),
            0,
            "an accepted prompt is the turn, so the seed is delivered and gone"
        );
        assert!(
            lead_heard(&lead_inbox).is_empty(),
            "nothing said until the turn ends"
        );

        let seen = pump_turn(&mut app, &mut events).await;

        assert_eq!(
            attributed_start(&seen),
            Some(("team-lead".to_owned(), "start on the parser".to_owned())),
            "the seed became a turn of the member's own, attributed to the lead"
        );
        assert!(!app.turn_running);
        let heard = lead_heard(&lead_inbox);
        assert_eq!(heard.len(), 1, "one turn, one idle notification: {heard:?}");
        match &heard[0] {
            ganja_protocol::team::Frame::IdleNotification(idle) => {
                assert_eq!(idle.from, "w1");
                assert_eq!(
                    idle.idle_reason,
                    Some(ganja_protocol::team::IdleReason::Available)
                );
            }
            other => panic!("an idle_notification was expected, got {other:?}"),
        }
    }

    /// The same lane mid-turn (D-3, §10.3-1): a message the lead writes while
    /// the member is working steers the running turn, waits pending on the
    /// strip until the engine says it took it, and only then leaves the inbox.
    #[tokio::test]
    async fn a_leads_message_steers_a_pane_teammates_running_turn() {
        let directory = temporary();
        let (mut app, mut events) = membered(&directory).await;
        let (inbox, _lead_inbox) = member_paths(&app);
        turn_in_flight(&mut app, &mut events).await;
        lead_writes(&inbox, "and the lexer too");

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert_eq!(app.queue.depth(), 1, "pending until the turn consumes it");
        assert!(app.queue.entries()[0].is_steered());
        assert_eq!(owed(&inbox), 1, "durable until consumed");
        let id = app
            .peer_steers
            .keys()
            .next()
            .cloned()
            .expect("one batch in flight");

        app.handle(AppEvent::core(CoreEvent::SteerConsumed {
            session_id: session(),
            id,
        }))
        .await
        .expect("the event is handled");

        assert_eq!(app.queue.depth(), 0);
        assert_eq!(owed(&inbox), 0, "consumed means gone");
    }

    /// **AC-10's pane leg, the shutdown seam** (§10.3-4, §6.2): an idle member
    /// answers a `shutdown_request` on the tick that reads it, quoting the
    /// request id, and leaves through the loop's own exit. The pane and
    /// backend fields the approval carries are `member.rs`'s own pin.
    #[tokio::test]
    async fn an_idle_pane_teammate_answers_a_shutdown_request_and_quits() {
        let directory = temporary();
        let (mut app, _events) = membered(&directory).await;
        let (inbox, lead_inbox) = member_paths(&app);
        crate::member::write_frame(
            &inbox,
            "team-lead",
            &crate::member::shutdown_request("req-1"),
        );

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert!(app.quit, "the request is the last thing this member reads");
        let heard = lead_heard(&lead_inbox);
        match heard.as_slice() {
            [ganja_protocol::team::Frame::ShutdownApproved(approved)] => {
                assert_eq!(approved.request_id, "req-1");
            }
            other => panic!("one shutdown_approved was expected, got {other:?}"),
        }
    }

    /// A shutdown that arrives mid-turn waits for the turn's end rather than
    /// cutting it short (`Teammate::shutdown`'s courtesy), and then answers in
    /// the order the lead expects: the turn's idle notification, then the
    /// approval.
    #[tokio::test]
    async fn a_shutdown_request_during_a_turn_waits_for_the_turn_to_end() {
        let directory = temporary();
        let (mut app, mut events) = membered(&directory).await;
        let (inbox, lead_inbox) = member_paths(&app);
        turn_in_flight(&mut app, &mut events).await;
        crate::member::write_frame(
            &inbox,
            "team-lead",
            &crate::member::shutdown_request("req-2"),
        );

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        assert!(!app.quit, "the turn is still running");
        assert!(app.member_shutdown.is_some(), "and the request is held");
        assert!(lead_heard(&lead_inbox).is_empty(), "nothing answered yet");
        assert!(
            app.wants_frame(),
            "a held shutdown keeps the loop ticking so its bound is read"
        );

        pump_turn(&mut app, &mut events).await;

        assert!(
            app.quit,
            "the turn ended, so the request is answered and the app leaves"
        );
        let kinds: Vec<&str> = lead_heard(&lead_inbox)
            .iter()
            .map(ganja_protocol::team::Frame::kind)
            .collect();
        assert_eq!(kinds, ["idle_notification", "shutdown_approved"]);
    }

    /// The lead's `mode_set_request` reaches the engine as
    /// `Command::SetPermissionMode` (D-15, AC-19's pane half): the engine
    /// answers with `PermissionModeChanged`, and the frame is gone.
    #[tokio::test]
    async fn a_leads_mode_set_request_reaches_the_pane_teammates_engine() {
        let directory = temporary();
        let (mut app, mut events) = membered(&directory).await;
        let (inbox, _lead_inbox) = member_paths(&app);
        crate::member::write_frame(
            &inbox,
            "team-lead",
            &ganja_protocol::team::Frame::ModeSetRequest(ganja_protocol::team::ModeSetRequest {
                mode: "bypassPermissions".to_owned(),
                from: "team-lead".to_owned(),
            }),
        );

        app.handle(AppEvent::Tick).await.expect("a tick is handled");

        let changed = drained(&mut events)
            .into_iter()
            .find_map(|event| match event {
                CoreEvent::PermissionModeChanged { mode, .. } => Some(mode),
                _ => None,
            });
        assert_eq!(changed, Some(ganja_protocol::PermissionMode::Bypass));
        assert_eq!(
            owed(&inbox),
            0,
            "a frame acted on leaves the inbox in the same pass"
        );
    }

    /// A member wakes at its own cadence, which is the teammate's rather than
    /// the lead's, and a session that is nobody's teammate wakes for neither.
    #[test]
    fn a_member_wakes_at_the_teammates_cadence() {
        let membership = crate::member::membership(std::path::Path::new("/nowhere"), None);
        let mut member = app().with_member(crate::member::Inbox::new(membership));
        // A fresh app wants its first frame; what is under test is the clock
        // an idle member falls back to once nothing is left to draw.
        member.dirty = false;

        assert!(member.wants_wakeup());
        assert_eq!(
            member.until_next_wakeup(),
            Duration::ZERO,
            "the first pass is due at once"
        );
        member.member_polled = Some(Instant::now());
        assert!(
            member.until_next_wakeup() > FRAME && member.until_next_wakeup() <= crate::member::POLL,
            "then the member's own clock, not the frame's: {:?}",
            member.until_next_wakeup()
        );
        let mut plain = app();
        plain.dirty = false;
        assert!(!plain.wants_wakeup(), "a plain session sleeps");
    }

    /// **AC-8's member half** (D-5): a pane teammate under `ForwardToLead`
    /// raises no dialog of its own. The ask its rules raise travels to the
    /// lead as §5's `permission_request`, the lead's `permission_response`
    /// comes back through the member's inbox as `ReplyPermission::Once`, and
    /// the call the ask was about actually runs.
    ///
    /// A real engine and a real tool, for `shelling`'s reason: what has to be
    /// shown is a turn that ran to its end with nobody at this terminal
    /// answering anything.
    #[tokio::test]
    async fn a_pane_teammates_ask_travels_to_the_lead_and_the_answer_lets_the_call_run() {
        let directory = temporary();
        let script = directory.path().join("shell.json");
        fs::write(
            &script,
            format!(
                r#"{{
                    "cadence_ms": 0,
                    "turns": [
                        {{"tool_calls": [{{
                            "name": "bash",
                            "args": {{"command": "echo {ECHOED}"}}
                        }}]}},
                        {{"text": "{CLOSING}"}}
                    ]
                }}"#
            ),
        )
        .expect("the fake-provider script writes");
        let membership = crate::member::membership(directory.path(), None);
        let engine = Engine::new(
            Arc::new(FakeProvider::new("", Duration::ZERO).with_script(&script)),
            fake::MODEL,
            Arc::new(ganja_tool::Registry::new(vec![Arc::new(
                ganja_tool::shell::ShellTool::new(),
            )])),
            ganja_permission::Permissions::default(),
        );
        let mut events = engine.subscribe().await.expect("the test subscribes first");
        let mut app = App::new(engine, None, Themes::builtin())
            .with_member(crate::member::Inbox::new(membership));
        let (inbox, lead_inbox) = member_paths(&app);
        app.engine
            .send(ganja_protocol::Command::SendPrompt {
                text: "run it".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts the prompt");

        let mut seen = Vec::new();
        let mut answered = false;
        for _ in 0..128 {
            let event = next_event(&mut events).await;
            let finished = matches!(event, CoreEvent::MessageFinished { .. });
            seen.push(event.clone());
            app.handle(AppEvent::core(event))
                .await
                .expect("an engine event is handled");
            assert!(
                app.permission.is_none() && app.queued_permissions.is_empty(),
                "a forwarding pane draws no dialog of its own"
            );
            if !answered
                && app
                    .member
                    .as_ref()
                    .is_some_and(|inbox| inbox.asks().waiting() > 0)
            {
                // The ask reached the lead as a frame naming the engine's own
                // request id, and this is the lead answering it.
                let request = lead_heard(&lead_inbox)
                    .into_iter()
                    .find_map(|frame| match frame {
                        ganja_protocol::team::Frame::PermissionRequest(request) => Some(request),
                        _ => None,
                    })
                    .expect("the ask reached the lead");
                assert_eq!(request.tool_name, "bash");
                assert_eq!(request.agent_id, "w1@session-224cbeab");
                assert_eq!(
                    request.request_id,
                    seen.iter()
                        .find_map(|event| match event {
                            CoreEvent::PermissionRequested { id, .. } =>
                                Some(id.as_str().to_owned()),
                            _ => None,
                        })
                        .expect("the engine asked"),
                    "the frame names the engine's own id"
                );
                // The lead's own encoder rather than a `PermissionResponse`
                // built here: a hand-built success body exercises the `Once`
                // spelling only, where `response_of` is the function a real
                // lead calls for all three replies and whose round trip with
                // `reply_of` core already pins.
                crate::member::write_frame(
                    &inbox,
                    "team-lead",
                    &ganja_protocol::team::Frame::PermissionResponse(
                        ganja_core::teammate::member::response_of(
                            &request.request_id,
                            &request.tool_name,
                            &request.input,
                            ganja_protocol::PermissionReply::Once,
                        ),
                    ),
                );
                app.handle(AppEvent::Tick).await.expect("a tick is handled");
                assert_eq!(
                    app.member.as_ref().map(|inbox| inbox.asks().waiting()),
                    Some(0),
                    "the answer closed the ask"
                );
                answered = true;
            }
            if finished {
                break;
            }
        }

        assert!(answered, "the turn never asked: {seen:#?}");
        let replies: Vec<_> = seen
            .iter()
            .filter_map(|event| match event {
                CoreEvent::PermissionReplied { reply, .. } => Some(*reply),
                _ => None,
            })
            .collect();
        assert_eq!(
            replies,
            vec![PermissionReply::Once],
            "the lead's success is Once, never Always"
        );
        assert!(
            completed_shell(&seen).is_some_and(|output| output.contains(ECHOED)),
            "the call the ask was about actually ran: {seen:#?}"
        );
    }
}
