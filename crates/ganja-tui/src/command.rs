//! The commands the palette and the `/` dropdown offer, and how a typed
//! fragment narrows them.
//!
//! Spec: upstream `packages/tui/src/component/command-palette.tsx` and
//! `packages/tui/src/component/prompt/autocomplete.tsx`. Upstream has two
//! disjoint command populations — UI actions that dispatch immediately, and
//! engine commands that insert text and run on Enter — and both live here:
//! [`Entry`] is the first, [`EngineCommand`] the second, and [`Choice`] is
//! what the `/` dropdown offers once they are merged. Only the first reaches
//! the palette, because the palette has no way to take the arguments an
//! engine command expects.
//!
//! The names follow upstream except the documented `/effort` deviation,
//! **plurals and aliases included**: `/models` not `/model`, `mo` because
//! upstream added it to bias a half-typed `/mo` toward the model list. Getting
//! those wrong would mean muscle memory transferring from opencode and landing
//! on nothing.
//!
//! Ranking is [`nucleo_matcher`]'s, not upstream's `fuzzysort`'s. Two
//! libraries scoring the same fragment identically is not something either of
//! them promises, and pinning it would mean reimplementing one of them —
//! **D10** says parity here is not a goal. What *is* pinned is that the order
//! is total and deterministic: ties break on the command's own name, so a
//! fragment always produces the same list.

use std::time::{Duration, SystemTime};

use ganja_core::teammate::{BACKENDS, DEFAULT_BACKEND, backend_name};
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::keybind;

/// What choosing a command does.
///
/// A UI action, always: everything here is something the frontend performs
/// itself, which is what makes the palette able to dispatch on selection
/// rather than typing text into the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Open the stored-session picker.
    Sessions,
    /// Start a fresh session, and empty the screen the old one filled.
    New,
    /// Summarize the conversation so far and carry on from the summary.
    Compact,
    /// Compose the prompt in `$EDITOR`.
    Editor,
    /// Open the model list for the provider this session runs on.
    Models,
    /// Open the flat picker over the active model's catalog efforts.
    Effort,
    /// Open the list of agents the session may switch to.
    Agents,
    /// Open the theme picker.
    Themes,
    /// Open the `/mcp` dialog: every configured server's status and tool
    /// count, with Reconnect on a failed one.
    Mcp,
    /// Open the `/skills` list: every skill a `$` invocation can load, with
    /// Enter inserting `$name ` into the composer (**D491**).
    Skills,
    /// Open the `/context` panel: what fills the model's context window,
    /// estimated per category (**D470**).
    Context,
    /// Open the `/usage` panel: what this session has spent (**D471**).
    Usage,
    /// Open the `/plugin` dialog: every installed plugin's state and
    /// components, with the store's own actions beside them (**D474**).
    Plugin,
    /// Open the `/teammate` dialog: every member of this session's team, what it
    /// has been doing, and the doors onto starting, messaging and shutting
    /// one down (**D503**, **D504**).
    Team,
    /// Open the `/held` dialog: every inbound cross-session message the
    /// admission gate is holding for review, with Release and Deny on each
    /// row (**D524**).
    Held,
    /// Open the key and command reference.
    Help,
    /// Leave.
    Exit,
    /// Put the whole conversation on the clipboard.
    Copy,
    /// Put the model's last reply on the clipboard.
    CopyMessage,
    /// Take back the last prompt, and the file changes its turn made.
    Undo,
    /// Put back what an undo took, one prompt at a time.
    Redo,
    /// Open the picker that takes the session back to a checkpoint.
    Rewind,
    /// Give this session a name (**D527**). Bare `/rename` names nothing to
    /// rename to; [`rename`] is the door that reads the argument off the
    /// buffer, the same shape [`team`] reads `/teammate spawn`'s off.
    Rename,
    /// Tell the model how long this sitting has (**D557**). Bare `/deadline`
    /// says what is set; [`deadline`] is the door that reads the argument off
    /// the buffer, the third and last builtin shaped that way.
    Deadline,
}

impl Action {
    /// The binding whose key is shown on this command's row, where one action
    /// has a key of its own.
    ///
    /// Not every command does: reaching `/models` costs a palette open, which
    /// is exactly what the palette is for.
    #[must_use]
    pub fn keybind(self) -> Option<keybind::Action> {
        match self {
            Self::Sessions => Some(keybind::Action::SessionsOpen),
            Self::Themes => Some(keybind::Action::ThemesOpen),
            Self::Exit => Some(keybind::Action::AppExit),
            // `/undo` and `/redo` are upstream's `<leader>u` and `<leader>r`,
            // and ganja has no leader (**D4**) — so both are reached by name,
            // from the palette or the `/` menu, and by nothing else. `/rewind`
            // has no chord of its own either: its second door is the Esc Esc
            // gesture, which the binding table cannot express (**D452**, at the
            // Esc arm in `app.rs`).
            Self::New
            | Self::Compact
            | Self::Editor
            | Self::Models
            | Self::Effort
            | Self::Agents
            | Self::Mcp
            | Self::Skills
            | Self::Context
            | Self::Usage
            | Self::Plugin
            | Self::Team
            | Self::Held
            | Self::Help
            | Self::Copy
            | Self::CopyMessage
            | Self::Undo
            | Self::Redo
            | Self::Rewind
            | Self::Rename
            | Self::Deadline => None,
        }
    }
}

/// The group a command is listed under in the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// Things done to the conversation.
    Session,
    /// Things done to who is answering it.
    Agent,
    /// Everything else.
    System,
}

impl Category {
    /// The heading the palette prints above the group.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }
}

/// One command, as both surfaces list it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// What it does.
    pub action: Action,
    /// What it is called, without the leading slash.
    pub name: &'static str,
    /// Other spellings that reach it, upstream's set exactly.
    pub aliases: &'static [&'static str],
    /// The short label the palette shows.
    pub title: &'static str,
    /// The longer line the dropdown shows beside the name, and which the
    /// dropdown also matches against — upstream matches descriptions in slash
    /// mode and deliberately does not in `@` mode.
    pub description: &'static str,
    /// The palette group.
    pub category: Category,
    /// Whether the palette pins it at the top while nothing has been typed.
    pub suggested: bool,
}

impl Entry {
    /// How the command is typed: the leading slash included, because that is
    /// what both surfaces print and what the dropdown matches.
    #[must_use]
    pub fn slash(&self) -> String {
        format!("/{}", self.name)
    }
}

/// Every UI command both surfaces offer.
///
/// The engine's own commands — `/init` and whatever a config file adds — are
/// deliberately not here: they take arguments, so choosing one types its name
/// into the buffer instead of running it, and a palette has nowhere to type.
/// They reach the dropdown as [`EngineCommand`]s instead.
pub const COMMANDS: &[Entry] = &[
    Entry {
        action: Action::Sessions,
        name: "sessions",
        aliases: &["resume", "continue"],
        title: "Switch session",
        description: "Reopen a conversation this project has stored",
        category: Category::Session,
        suggested: true,
    },
    Entry {
        action: Action::New,
        name: "new",
        aliases: &["clear"],
        title: "New session",
        description: "Leave this conversation and start an empty one",
        category: Category::Session,
        suggested: false,
    },
    Entry {
        action: Action::Compact,
        name: "compact",
        aliases: &["summarize"],
        title: "Compact session",
        description: "Summarize the conversation so far and carry on from it",
        category: Category::Session,
        suggested: false,
    },
    Entry {
        action: Action::Editor,
        name: "editor",
        aliases: &[],
        title: "Open editor",
        description: "Write the prompt in $EDITOR instead of the composer",
        category: Category::Session,
        suggested: false,
    },
    Entry {
        action: Action::Models,
        name: "models",
        // Upstream's own comment: `mo` exists so that a half-typed `/mo`
        // reaches the model list rather than one of its neighbours.
        aliases: &["mo"],
        title: "Switch model",
        description: "Ask the rest of this session of a different model",
        category: Category::Agent,
        suggested: true,
    },
    Entry {
        action: Action::Effort,
        name: "effort",
        aliases: &[],
        // Upstream's `variant.list` command, filed under its Agent category. Deviation
        // `effort-not-variants`: upstream's slash name is "variants" and its title
        // "Switch model variant"; ganja surfaces the same catalog mechanism as
        // "effort" by owner decision. The catalog field keeps upstream's schema name.
        title: "Switch model effort",
        description: "Run the model under one of its catalog efforts",
        category: Category::Agent,
        suggested: false,
    },
    Entry {
        action: Action::Agents,
        name: "agents",
        aliases: &[],
        title: "Switch agent",
        description: "Run the rest of this session as a different agent",
        category: Category::Agent,
        suggested: false,
    },
    Entry {
        action: Action::Themes,
        name: "themes",
        aliases: &[],
        title: "Switch theme",
        description: "Repaint the screen in another palette",
        category: Category::System,
        suggested: false,
    },
    Entry {
        action: Action::Mcp,
        name: "mcp",
        aliases: &[],
        title: "MCP servers",
        description: "See what every configured server lends, and reconnect a failed one",
        category: Category::System,
        suggested: false,
    },
    Entry {
        action: Action::Skills,
        name: "skills",
        aliases: &[],
        title: "Skills",
        description: "List the skills a $ invocation can load, and insert one",
        category: Category::System,
        suggested: false,
    },
    // Claude Code's `/context` and `/usage`, with no upstream opencode
    // counterpart at all (**D470**, **D471**). Filed under `System` for the
    // copy commands' reason: both panels look at the conversation and do
    // nothing to it.
    Entry {
        action: Action::Context,
        name: "context",
        aliases: &[],
        title: "Context usage",
        description: "See what fills the model's context window, estimated per category",
        category: Category::System,
        suggested: false,
    },
    Entry {
        action: Action::Usage,
        name: "usage",
        aliases: &[],
        title: "Session usage",
        description: "See what this session has spent, and its cache hit rate",
        category: Category::System,
        suggested: false,
    },
    // Claude Code's `/plugin`, over ganja's own install store — no upstream
    // opencode counterpart, like the whole plugin system (**D472**; the
    // dialog itself is **D474**). `System` because the store is this build's,
    // not the conversation's.
    Entry {
        action: Action::Plugin,
        name: "plugin",
        aliases: &[],
        title: "Plugins",
        description: "See the installed plugins; add, install, enable, disable or reload one",
        category: Category::System,
        suggested: false,
    },
    // Filed with `/mcp` and `/plugin` rather than under `Agent`, for the
    // reason that group's own doc gives: `Agent` is *who is answering this
    // conversation*, and a teammate answers its own. A team is a facility
    // running beside this session, which is what `System` is for.
    Entry {
        action: Action::Team,
        name: "teammate",
        aliases: &["teammates"],
        title: "Teammates",
        description: "See this session's team; start, message or shut down a member",
        category: Category::System,
        suggested: false,
    },
    // The admission gate's listing dialog (**D524**), and the *only* review
    // surface an explicit or mode-unknown hold has — those raise no approval
    // modal. `System` for `/teammate`'s reason: the hold buffer is a facility
    // running beside this session, not the conversation itself.
    Entry {
        action: Action::Held,
        name: "held",
        aliases: &[],
        title: "Held messages",
        description: "Review the inbound peer messages held by the admission gate",
        category: Category::System,
        suggested: false,
    },
    Entry {
        action: Action::Help,
        name: "help",
        aliases: &[],
        title: "Help",
        description: "List the commands and the keys that reach them",
        category: Category::System,
        suggested: false,
    },
    Entry {
        action: Action::Exit,
        name: "exit",
        aliases: &["quit", "q"],
        title: "Exit the app",
        description: "Leave ganja",
        category: Category::System,
        suggested: false,
    },
    // Upstream files both of these under its `Session` category; here they are
    // `System`, because ganja's `Session` is documented as *things done to the
    // conversation* and taking a copy does nothing to it (deviation:
    // copy-commands-categorised-system). Upstream's own titles and slash names
    // are kept exactly.
    Entry {
        action: Action::Copy,
        name: "copy",
        aliases: &[],
        title: "Copy session transcript",
        description: "Put the whole conversation on the clipboard, as markdown",
        category: Category::System,
        suggested: false,
    },
    Entry {
        action: Action::CopyMessage,
        name: "copy-message",
        aliases: &[],
        title: "Copy message",
        description: "Put the model's last reply on the clipboard",
        category: Category::System,
        suggested: false,
    },
    // Upstream's titles and slash names, in upstream's own `Session` category:
    // taking a prompt back is the plainest thing there is to do *to* the
    // conversation. Both sit at the end of the table on purpose — the palette
    // lists it in order, and the rows a stock terminal can show are the ones
    // above the clip.
    Entry {
        action: Action::Undo,
        name: "undo",
        aliases: &[],
        title: "Undo previous message",
        description: "Take back the last prompt and the file changes its turn made",
        category: Category::Session,
        suggested: false,
    },
    Entry {
        action: Action::Redo,
        name: "redo",
        aliases: &[],
        title: "Redo",
        description: "Put back what an undo took, one prompt at a time",
        category: Category::Session,
        suggested: false,
    },
    // Upstream reaches its message-revert dialog from the transcript rather
    // than by name, so the slash name is Claude Code's (**D451**, at
    // `RevertScope`). It sits with `/undo` and `/redo` because it is the same
    // thing done to the conversation, with the checkpoint and the scope chosen
    // rather than assumed.
    Entry {
        action: Action::Rewind,
        name: "rewind",
        aliases: &[],
        title: "Rewind to a checkpoint",
        description: "Restore the code and/or conversation to an earlier prompt",
        category: Category::Session,
        suggested: false,
    },
    // `System` for `/teammate`'s reason: naming this session is a facility
    // beside the conversation, not something done to it. The one other
    // builtin besides `/teammate` that reads an argument off the buffer — see
    // [`rename`].
    Entry {
        action: Action::Rename,
        name: "rename",
        aliases: &[],
        title: "Rename this session",
        description: "Give this session a name other sessions can address it by",
        category: Category::System,
        suggested: false,
    },
    // `Session`, where `/rename` beside it is `System`, and the difference is
    // this group's own definition rather than a coin toss: a name is how
    // somebody else reaches this process, where a time budget changes what
    // the model does with the conversation — which is a thing done *to* the
    // conversation, the same shelf `/compact` and `/undo` sit on.
    Entry {
        action: Action::Deadline,
        name: "deadline",
        aliases: &[],
        title: "Set a time budget",
        description: "Tell the model how long this sitting has, so every request says what is left",
        category: Category::Session,
        suggested: false,
    },
];

/// The words that quit when they are the whole prompt.
///
/// Upstream checks the trimmed buffer before anything else on submit, so
/// typing `exit` and pressing Enter leaves rather than asking the model what
/// it thinks about the word (`component/prompt/index.tsx:962-966`).
const BARE_EXITS: [&str; 3] = ["exit", "quit", ":q"];

/// Whether `text`, submitted on its own, means "leave".
#[must_use]
pub fn is_bare_exit(text: &str) -> bool {
    BARE_EXITS.contains(&text.trim())
}

/// The command `name` reaches, by name or by alias.
///
/// The leading slash is optional so that the same lookup serves the dropdown's
/// `/models` and a bare `models`.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static Entry> {
    let wanted = name.strip_prefix('/').unwrap_or(name);

    COMMANDS.iter().find(|entry| entry.name == wanted || entry.aliases.contains(&wanted))
}

/// The UI command a submitted buffer names on its own, or nothing when the
/// buffer is prose.
///
/// The dropdown cannot be the only door to these: its menu closes the moment
/// a space follows the name (`dropdown::triggered`), and Tab completion
/// deliberately leaves `/exit ` in the buffer (**D446**) — so the Enter that
/// follows has to read the text itself, which is how Claude Code and Codex
/// both dispatch a slash command. Only a leading `/name` (or alias) with
/// nothing but whitespace after it qualifies: these commands take no
/// arguments — `/teammate` is the one exception, whose argument-carrying lines
/// [`team`] reads off the same buffer — so `/models gpt` stays text, under
/// the same ruling that keeps an unknown slash command out of the UI's hands.
#[must_use]
pub fn submitted(text: &str) -> Option<&'static Entry> {
    let name = text.strip_prefix('/')?.trim_end();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }

    lookup(name)
}

/// What a submitted `/teammate` line asked for.
///
/// The second door onto what [`Action::Team`] opens, and the only one that
/// can carry arguments: the palette has nowhere to type them and the dropdown
/// closes the moment a space follows the name, so a line with a subcommand
/// has to be read off the buffer on submit exactly as [`submitted`] reads an
/// argument-less one. That is what lets `/teammate spawn w1 --backend ganja` — the
/// spec's own spelling — reach the same spawn sequence the `task` tool's
/// teammate door reaches (**D504**).
///
/// [`Team::List`] and [`Action::Team`] mean the same thing, so a bare `/teammate`
/// opens the dialog whichever of the two doors the app reads first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Team {
    /// Show the roster: a bare `/teammate`, and `/teammate list`.
    List,
    /// Start a teammate.
    Spawn(TeamSpawn),
    /// Ask a member to shut down, or — with no name — every one of them.
    Shutdown {
        /// The member named, or [`None`] for the whole team.
        member: Option<String>,
    },
    /// The line said `/teammate` and then something this grammar has not got.
    ///
    /// One sentence rather than a kind, for [`crate::component::team::Team`]'s
    /// notice line to show: nothing downstream branches on which mistake it
    /// was, and a refusal a person cannot read is one they cannot act on.
    Refused(String),
}

/// A `/teammate spawn` line, parsed.
///
/// Deliberately strings and [`Option`]s: which surfaces exist, which agent
/// kinds are spawnable, and which names a team will take are every one of
/// them the engine's to answer, and a second list here would be a second
/// place for them to drift. What this grammar decides is only whether a value
/// was *given*.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamSpawn {
    /// The name asked for, unchecked.
    pub name: String,
    /// What `--backend` named, unchecked. [`None`] is the far side's default
    /// rather than a value chosen here, exactly as it is on the `task` door.
    pub backend: Option<String>,
    /// What `--agent` named. [`None`] means the kind
    /// [`crate::component::team::spawn_request`] fills in.
    pub agent_type: Option<String>,
    /// What the teammate is being asked to do, verbatim from the first word
    /// that is not a flag. Empty is allowed — AC-11's own spelling
    /// (`/teammate spawn w1 --backend ganja`) carries no prompt, and a teammate
    /// started to be messaged afterwards is a real thing to want.
    pub prompt: String,
}

/// `/teammate spawn`'s grammar, spelled once: the refusal a nameless spawn reads
/// names it, and the `/teammate` dialog's own input step shows it, because
/// [`team_spawn`] is the one parser both doors feed.
pub const SPAWN_GRAMMAR: &str = "<name> [--backend <surface>] [--agent <kind>] [what it should do]";

/// The inline hint a builtin command shows once its name is typed (**D518**).
///
/// `/teammate`, `/rename` and `/deadline` are the only builtins that read
/// arguments off the buffer; everything else answers [`None`] and shows
/// nothing. Display-only, like a command file's `argument-hint` — the grammar
/// that actually decides is [`team`]'s, [`rename`]'s and [`deadline`]'s
/// respectively.
fn builtin_hint(name: &str) -> Option<&'static str> {
    match name {
        "teammate" => {
            Some("list | spawn <name> [--backend] [--agent] [prompt] | shutdown [member]")
        }
        "rename" => Some("<name>"),
        "deadline" => Some(DEADLINE_GRAMMAR),
        _ => None,
    }
}

/// The dim hint the composer draws after a typed command name (**D518**),
/// Claude Code's own presentation: the full name alone shows what the
/// arguments would be, and words typed after it consume the hint front to
/// back — the slot still being typed into included — so what remains stays
/// standing beside the cursor until the grammar runs out, or a word it has
/// not got arrives.
///
/// `/teammate` refines per subcommand: a `spawn` line's hint is [`SPAWN_GRAMMAR`]
/// — the same one spelling the refusal and the `/teammate` dialog's input step
/// already use — consumed the same way.
#[must_use]
pub fn inline_hint(text: &str, engine: &[EngineCommand]) -> Option<String> {
    if text.contains('\n') {
        return None;
    }
    let rest = text.strip_prefix('/')?;
    let (name, tail) = match rest.split_once(char::is_whitespace) {
        Some((name, tail)) => (name, tail),
        None => (rest, ""),
    };
    if name.is_empty() {
        return None;
    }
    // Through [`lookup`] rather than against the typed spelling, for
    // [`team`]'s own reason: an alias reaches the hint its command shows
    // rather than showing nothing beside the cursor. Only the builtin half
    // is resolved — an engine command is still asked for by the name that
    // was typed, since it is a different roster and knows nothing of these.
    let builtin = lookup(name).map_or(name, |entry| entry.name);
    if builtin == "teammate" {
        return team_hint(tail);
    }
    let hint = builtin_hint(builtin).map(str::to_owned).or_else(|| {
        engine.iter().find(|command| command.name == name).and_then(|command| command.hint.clone())
    })?;
    remaining(&hint, tail)
}

/// The `/teammate` line's own hint: the overview while nothing follows the name,
/// then the chosen subcommand's remaining grammar as its arguments fill in.
fn team_hint(tail: &str) -> Option<String> {
    let trimmed = tail.trim_start();
    if trimmed.is_empty() {
        return builtin_hint("teammate").map(str::to_owned);
    }
    let (first, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((first, rest)) => (first, rest),
        None => (trimmed, ""),
    };
    match first {
        "spawn" => remaining(SPAWN_GRAMMAR, rest),
        "shutdown" => remaining("[member]", rest),
        _ => None,
    }
}

/// One slot of a hint's grammar — `<name>`, `[--backend <surface>]`,
/// `[what it should do]` — as [`hint_slots`] cut them.
struct HintSlot {
    /// The slot verbatim, for what remains of the hint.
    text: String,
    /// The flag that fills this slot (`--backend`), or [`None`] for a
    /// positional one the next bare word fills.
    flag: Option<String>,
    /// Whether the flag takes a value token of its own.
    takes_value: bool,
}

/// Cuts a hint at top-level spaces, so a bracketed phrase stays one slot.
fn hint_slots(hint: &str) -> Vec<HintSlot> {
    let mut slots = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for character in hint.chars() {
        match character {
            '[' | '<' => {
                depth += 1;
                current.push(character);
            }
            ']' | '>' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ' ' if depth == 0 => {
                if !current.is_empty() {
                    slots.push(slot(std::mem::take(&mut current)));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        slots.push(slot(current));
    }
    slots
}

/// Reads one cut piece into a [`HintSlot`].
fn slot(text: String) -> HintSlot {
    let (flag, takes_value) = {
        let inner = text.strip_prefix('[').unwrap_or(text.as_str());
        if let Some(rest) = inner.strip_prefix("--") {
            let name_end = rest
                .find(|character: char| character.is_whitespace() || character == ']')
                .unwrap_or(rest.len());
            let flag = format!("--{}", &rest[..name_end]);
            let value = rest[name_end..].trim_end_matches(']');
            (Some(flag), !value.trim().is_empty())
        } else {
            (None, false)
        }
    };
    HintSlot { text, flag, takes_value }
}

/// What is left of `hint` once the words already typed have consumed their
/// slots, front to back: a bare word fills the first positional slot, a flag
/// fills its own named slot (and its value, where it takes one, fills
/// nothing), and the slot a word is still being typed into counts as filled —
/// which is what keeps the rest of the hint standing beside the cursor.
///
/// [`None`] the moment the words outrun the grammar — every slot filled, or a
/// flag the hint never named — because a hint that cannot say what comes next
/// honestly says nothing.
fn remaining(hint: &str, typed: &str) -> Option<String> {
    let mut slots = hint_slots(hint);
    let tokens: Vec<&str> = typed.split_whitespace().collect();
    let last_in_progress = !typed.is_empty() && !typed.ends_with(char::is_whitespace);
    let mut expect_value = false;
    for (index, token) in tokens.iter().enumerate() {
        if expect_value {
            expect_value = false;
            continue;
        }
        let complete = index + 1 < tokens.len() || !last_in_progress;
        if token.starts_with("--") {
            let matched = slots.iter().position(|slot| {
                slot.flag.as_deref().is_some_and(|flag| {
                    if complete { flag == *token } else { flag.starts_with(token) }
                })
            })?;
            let slot = slots.remove(matched);
            expect_value = complete && slot.takes_value;
        } else {
            let position = slots.iter().position(|slot| slot.flag.is_none())?;
            slots.remove(position);
        }
    }
    if slots.is_empty() {
        return None;
    }
    Some(slots.iter().map(|slot| slot.text.as_str()).collect::<Vec<_>>().join(" "))
}

/// What `--backend` reads when the line ends before its value. Which surfaces
/// there are is not repeated here on purpose: the far side refuses an unknown
/// one by name, and two lists would be two places for them to drift.
const BACKEND_NEEDS_A_VALUE: &str =
    "`--backend` names the surface a teammate runs on, and this line ends before naming one";

/// What `--agent` reads when the line ends before its value.
const AGENT_NEEDS_A_VALUE: &str =
    "`--agent` names the kind of agent a teammate runs as, and this line ends before naming one";

/// The `/teammate` command a submitted buffer names, or [`None`] when the buffer
/// is not a `/teammate` line at all — in which case it is prose, and nothing here
/// has an opinion about it.
///
/// A refused subcommand or flag comes back as [`Team::Refused`] rather than as
/// [`None`]: a line that plainly says `/teammate` and then gets something wrong
/// should be told so, not sent to the model as a question about itself.
#[must_use]
pub fn team(text: &str) -> Option<Team> {
    let (name, rest) = split_word(text.strip_prefix('/')?);
    // Through [`lookup`] rather than against a second spelling of the name,
    // so an alias the roster grows one day reaches this door for free.
    if lookup(name).is_none_or(|entry| entry.action != Action::Team) {
        return None;
    }

    let (subcommand, rest) = split_word(rest);
    // Trailing whitespace is nobody's argument, and leaving it on would put it
    // inside the quotes of a refusal that names what it could not take.
    let rest = rest.trim_end();
    Some(match subcommand {
        "" => Team::List,
        "list" => match rest {
            "" => Team::List,
            extra => Team::Refused(format!(
                "`/teammate list` takes nothing after it, and this line adds {extra:?}"
            )),
        },
        "spawn" => match team_spawn(rest) {
            Ok(spawn) => Team::Spawn(spawn),
            Err(refusal) => Team::Refused(refusal),
        },
        "shutdown" => {
            let (member, extra) = split_word(rest);
            if extra.is_empty() {
                Team::Shutdown {
                    // No name is the whole team, which is what the caller fans
                    // out over: one request per member is what the far side
                    // takes, so "everybody" is expanded where the roster is
                    // known rather than invented as a second kind of request.
                    member: (!member.is_empty()).then(|| member.to_owned()),
                }
            } else {
                Team::Refused(format!(
                    "`/teammate shutdown` names one member, or nobody for the whole team, and this line adds {extra:?}"
                ))
            }
        }
        other => Team::Refused(format!(
            "`/teammate` has no {other:?} subcommand: it lists the team, and takes `spawn`, `list` and `shutdown`"
        )),
    })
}

/// What a submitted `/rename` line asked for (**D527**).
///
/// [`Action::Rename`]'s second door, and the only one that can carry an
/// argument — the palette has nowhere to type one and the dropdown closes
/// the moment a space follows the name — so a line naming a target has to be
/// read off the buffer on submit exactly as [`team`] reads `/teammate spawn`'s.
/// Unlike `/teammate`'s grammar this one takes no subcommands: everything after
/// the name, trimmed, is the name asked for — a name may carry no whitespace
/// (`registry::vet_name`'s own clause), so there is nothing here to split on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rename {
    /// A name was given, unvalidated: `registry::vet_name` is the grammar
    /// that actually decides whether it may be used.
    To(String),
    /// `/rename` alone: nothing to rename to.
    Missing,
}

/// The `/rename` command a submitted buffer names, or [`None`] when the
/// buffer is not a `/rename` line at all — in which case it is prose, and
/// nothing here has an opinion about it.
#[must_use]
pub fn rename(text: &str) -> Option<Rename> {
    let (name, rest) = split_word(text.strip_prefix('/')?);
    if lookup(name).is_none_or(|entry| entry.action != Action::Rename) {
        return None;
    }

    let rest = rest.trim();
    Some(if rest.is_empty() { Rename::Missing } else { Rename::To(rest.to_owned()) })
}

/// `/deadline`'s grammar, spelled once: the composer's hint shows it and every
/// refusal below ends with it, so a person who mistyped reads the same words
/// beside the cursor and in the answer.
pub const DEADLINE_GRAMMAR: &str = "<duration> | HH:MM | off";

/// What a submitted `/deadline` line asked for (**D557**).
///
/// [`Action::Deadline`]'s second door, and the only one that can carry an
/// argument — the palette has nowhere to type one — so a line naming a budget
/// is read off the buffer on submit exactly as [`rename`] reads `/rename`'s.
/// [`Deadline::Show`] and [`Action::Deadline`] mean the same thing, so a bare
/// `/deadline` says what is set whichever of the two doors the app reads
/// first, which is [`Team::List`]'s arrangement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deadline {
    /// The instant the budget runs out, already resolved.
    ///
    /// An instant rather than the span that was typed, even for `/deadline 5m`:
    /// resolving here means one clock reading decides the deadline, where a
    /// span carried onward would be added to a *second* reading taken when the
    /// command is sent, and the two differ by however long the frontend took.
    /// It also leaves one shape for both spellings, since `10:00` was never a
    /// span in the first place.
    Set(SystemTime),
    /// `/deadline off`: whatever was set is cleared.
    Off,
    /// `/deadline` alone: say what is set, and change nothing.
    Show,
    /// The line said `/deadline` and then something this grammar has not got.
    ///
    /// One sentence rather than a kind, [`Team::Refused`]'s reason exactly:
    /// nothing downstream branches on which mistake it was, and a refusal a
    /// person cannot read is one they cannot act on.
    Refused(String),
}

/// The `/deadline` command a submitted buffer names, or [`None`] when the
/// buffer is not a `/deadline` line at all — in which case it is prose, and
/// nothing here has an opinion about it.
///
/// `now` is handed in rather than read, and that is what makes the wall-clock
/// arm testable: `/deadline 10:00` means ten o'clock **today, local**, so it
/// needs the current date and the current time — the first to build the
/// instant and the second to refuse one that has already gone by. A parser
/// that read the real clock could only be tested at whatever time the suite
/// happened to run.
#[must_use]
pub fn deadline(text: &str, now: SystemTime) -> Option<Deadline> {
    let (name, rest) = split_word(text.strip_prefix('/')?);
    if lookup(name).is_none_or(|entry| entry.action != Action::Deadline) {
        return None;
    }

    let argument = rest.trim();
    Some(match argument {
        "" => Deadline::Show,
        "off" => Deadline::Off,
        _ => match resolve_deadline(argument, now) {
            Ok(until) => Deadline::Set(until),
            Err(reason) => Deadline::Refused(refused_deadline(reason)),
        },
    })
}

/// A budget in `/deadline`'s grammar, resolved against `now` to the instant it
/// names: a span (`90s`, `5m`, `1h30m`) that far from `now`, or a clock time
/// (`10:00`) today, local.
///
/// **The one parser of that grammar, with two doors** (**D557**): [`deadline`]
/// reads a typed `/deadline` line, and `ganja run --deadline` reads a flag, so
/// the two cannot come to disagree about what `1h30m` means. Only an instant is
/// resolved here. `off` and the bare line are the slash command's own words
/// rather than budgets, so [`deadline`] answers them before it asks, and a
/// flag handed `off` is refused — a fresh headless run has nothing to clear.
///
/// # Errors
///
/// The reason as one sentence, without the usage that follows it: each door
/// ends it with its own, [`deadline`] with `/deadline`'s grammar. Refused are a
/// span of no time at all, a unit or shape the grammar has not got, a clock
/// time out of range or already behind, and an instant further off than
/// [`ganja_protocol::Command::SetDeadline`]'s milliseconds can name.
pub fn resolve_deadline(argument: &str, now: SystemTime) -> Result<SystemTime, String> {
    // The clock form is tried by its own shape rather than after the span
    // parser has failed, because the two grammars cannot overlap — a span
    // has no colon in it — and because the refusals differ: a clock time
    // may be well-formed and still be refused for being behind, which is
    // a sentence the span parser has no way to produce.
    let until = if argument.contains(':') {
        clock_deadline(argument, now)?
    } else {
        let span = parse_span(argument)
            .ok_or_else(|| format!("`/deadline` did not understand {argument:?}"))?;
        now.checked_add(span).ok_or_else(|| further_off(argument))?
    };

    // The wire spells the instant in `u64` milliseconds since the epoch, which
    // runs out long before this machine's clock does. Without this check a
    // span past it resolved, reached the frontend's conversion as nothing, and
    // went out as `until: None` — clearing the deadline somebody had just set
    // while the notice said it was set. One sentence for both ceilings,
    // because to whoever typed the number they are the same fact.
    let carried = until
        .duration_since(std::time::UNIX_EPOCH)
        .is_ok_and(|since| u64::try_from(since.as_millis()).is_ok());
    if carried { Ok(until) } else { Err(further_off(argument)) }
}

/// A span so large no deadline can name its end. Refused rather than clamped:
/// somebody who typed it meant a number, and silently substituting a different
/// one is worse than saying no.
fn further_off(argument: &str) -> String {
    format!("{argument:?} is further off than this machine's clock reaches")
}

/// `HH:MM` resolved against `now`: today's local wall clock.
///
/// Refused when it is already behind, and deliberately **not** rolled forward
/// to tomorrow. Somebody typing `/deadline 09:00` at ten in the morning has
/// almost certainly mistyped or misread the time; giving them a deadline
/// twenty-three hours out would be a budget nobody chose, and the whole point
/// of this feature is that the person chose it.
fn clock_deadline(argument: &str, now: SystemTime) -> Result<SystemTime, String> {
    let Some((hours, minutes)) = argument.split_once(':') else {
        return Err(format!("`/deadline` did not understand {argument:?}"));
    };
    // The shapes first, on the text: one or two digits for the hour and
    // exactly two for the minute, so `10:0` and `10:000` are refused rather
    // than read as ten o'clock. `9:30` is allowed because people write it.
    let shaped = matches!(hours.len(), 1 | 2)
        && minutes.len() == 2
        && hours.bytes().chain(minutes.bytes()).all(|byte| byte.is_ascii_digit());
    // Then the ranges, on the numbers. `24:00` and `10:60` get their own
    // sentence: they *are* clock times in shape, and what is wrong with them
    // is worth saying rather than folding into "did not understand".
    let (Ok(hour), Ok(minute)) = (hours.parse::<i8>(), minutes.parse::<i8>()) else {
        return Err(format!("`/deadline` did not understand {argument:?}"));
    };
    if !shaped {
        return Err(format!("`/deadline` did not understand {argument:?}"));
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return Err(format!(
            "{argument:?} is not a clock time: the hours run 00 to 23 and the minutes 00 to 59"
        ));
    }

    let Ok(stamp) = jiff::Timestamp::try_from(now) else {
        return Err("this machine's clock is outside the range ganja can read".to_owned());
    };
    let Ok(zoned) = stamp
        .to_zoned(jiff::tz::TimeZone::system())
        .with()
        .hour(hour)
        .minute(minute)
        .second(0)
        .subsec_nanosecond(0)
        .build()
    else {
        // A local time that does not exist today — the hour a spring-forward
        // transition skips. Naming the gap is more use than a generic
        // refusal, because the fix is to pick another minute.
        return Err(format!("{argument:?} does not happen today in this time zone"));
    };

    let until = SystemTime::from(zoned.timestamp());
    if until <= now {
        return Err(format!(
            "{argument:?} is already behind: a clock time names a moment later today"
        ));
    }

    Ok(until)
}

/// `<n>h<n>m<n>s` in any subset — `90s`, `5m`, `1h30m`, `2h`, `1h30m15s` — or
/// [`None`] when the text is not that.
///
/// Units may appear at most once and must run largest to smallest, so `5m5m`
/// and `30s5m` are refused rather than quietly summed: both are far more
/// likely to be a typo than an intention, and a parser that accepted them
/// would have to decide what `1h1h` meant.
///
/// A total of zero is [`None`] too. `/deadline 0s` parses as a number and a
/// unit, but no time at all is not a budget — and the one thing somebody
/// might mean by it, "clear this", already has its own word.
fn parse_span(text: &str) -> Option<Duration> {
    let mut rest = text;
    let mut seconds = 0_u64;
    // Consumed in order, so a unit already passed cannot come round again.
    let mut units = [('h', 3600_u64), ('m', 60), ('s', 1)].into_iter().peekable();
    while !rest.is_empty() {
        let digits = rest.find(|character: char| !character.is_ascii_digit())?;
        let value: u64 = rest[..digits].parse().ok()?;
        let unit = rest[digits..].chars().next()?;
        let scale = loop {
            let (name, scale) = units.next()?;
            if name == unit {
                break scale;
            }
        };
        seconds = seconds.checked_add(value.checked_mul(scale)?)?;
        rest = &rest[digits + unit.len_utf8()..];
    }

    (seconds > 0).then(|| Duration::from_secs(seconds))
}

/// One refusal sentence: the reason, then the grammar that would have worked.
///
/// The grammar is appended here rather than written into each reason so that
/// every refusal ends the same way — somebody who has just been told no is
/// reading for what to type instead, and it should be in the same place every
/// time.
fn refused_deadline(reason: String) -> String {
    format!("{reason}. /deadline {DEADLINE_GRAMMAR}")
}

/// The arguments after `/teammate spawn`, parsed — the same grammar the dialog's
/// own free-text step takes, so there is one spelling of a spawn rather than
/// two that could drift.
///
/// Flags come before the prompt, and the first word that is not a flag begins
/// it: from there the rest of the line is the prompt verbatim, dashes
/// included, because a prompt is prose and prose has dashes in it. A word
/// starting with `--` *before* that point and outside the two this grammar
/// has is refused by name rather than swallowed, on the same reasoning the
/// `task` tool refuses a `backend` with no `name`: the likeliest way to send
/// one is a flag that was meant to work. (`--bypass` was the third until
/// 2026-08-22 — **D513** retired it with the axis beneath it, so the grammar
/// now asks for exactly what the `task` door's arguments ask for.)
///
/// # Errors
///
/// One sentence, for the notice line: a missing name, a flag whose value the
/// line ends before, or a flag this grammar has not got.
pub fn team_spawn(text: &str) -> Result<TeamSpawn, String> {
    let (name, mut rest) = split_word(text);
    if name.is_empty() || name.starts_with('-') {
        return Err(format!(
            "`/teammate spawn` starts a teammate under a name: /teammate spawn {SPAWN_GRAMMAR}"
        ));
    }

    let mut spawn =
        TeamSpawn { name: name.to_owned(), backend: None, agent_type: None, prompt: String::new() };
    loop {
        let (word, after) = split_word(rest);
        match word {
            "" => break,
            "--backend" | "--agent" => {
                let (value, tail) = split_word(after);
                if value.is_empty() || value.starts_with('-') {
                    return Err(if word == "--backend" {
                        BACKEND_NEEDS_A_VALUE.to_owned()
                    } else {
                        AGENT_NEEDS_A_VALUE.to_owned()
                    });
                }
                if word == "--backend" {
                    spawn.backend = Some(value.to_owned());
                } else {
                    spawn.agent_type = Some(value.to_owned());
                }
                rest = tail;
            }
            unknown if unknown.starts_with("--") => {
                return Err(format!(
                    "`/teammate spawn` has no {unknown:?} flag: it takes `--backend` and `--agent`"
                ));
            }
            // Not a flag, so the prompt starts here and runs to the end.
            _ => {
                spawn.prompt = rest.trim_end().to_owned();
                break;
            }
        }
    }

    Ok(spawn)
}

/// The first whitespace-delimited word of `text`, and everything after it with
/// the whitespace between them dropped — so the remainder of a line is always
/// ready to be either parsed further or taken whole as a prompt.
fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    match text.find(char::is_whitespace) {
        Some(end) => (&text[..end], text[end..].trim_start()),
        None => (text, ""),
    }
}

/// One command the **engine** offers, as the dropdown lists it.
///
/// Owned rather than borrowed because this half of the roster is resolved at
/// runtime: `/init` is compiled in, but everything beside it comes out of the
/// user's config file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineCommand {
    /// What the user types after the slash.
    pub name: String,
    /// The one line the engine had to say about it, where it said anything.
    pub description: Option<String>,
    /// The file's own `argument-hint`, for the composer's inline hint
    /// (**D518**); the description above already carries it folded, which is
    /// what the dropdown row keeps showing.
    pub hint: Option<String>,
}

impl EngineCommand {
    /// Every command `registry` holds, in the order it lists them.
    #[must_use]
    pub fn roster(registry: &ganja_core::command::Registry) -> Vec<Self> {
        registry
            .commands()
            .iter()
            .map(|definition| Self {
                name: definition.name.clone(),
                description: definition.description.clone(),
                hint: definition.argument_hint.clone(),
            })
            .collect()
    }
}

/// One row the `/` dropdown offers, from either population.
///
/// The two halves differ in what choosing them does, which is the whole reason
/// upstream keeps them apart: a UI command runs, an engine command is typed
/// into the buffer and waits for the arguments its template expects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// A UI command, which runs the moment it is chosen.
    Ui(&'static Entry),
    /// An engine command, which is inserted as `/name ` and runs on Enter.
    Engine(EngineCommand),
    /// A value for a `/teammate` argument slot, which replaces the partial word
    /// under the cursor (**D519**).
    Value(Completion),
}

impl Choice {
    /// How the row is spelled — the leading slash included for a command,
    /// and the bare value for a slot.
    #[must_use]
    pub fn slash(&self) -> String {
        match self {
            Self::Ui(entry) => entry.slash(),
            Self::Engine(command) => format!("/{}", command.name),
            Self::Value(completion) => completion.text.clone(),
        }
    }

    /// The line shown beside the name.
    #[must_use]
    pub fn description(&self) -> &str {
        match self {
            Self::Ui(entry) => entry.description,
            Self::Engine(command) => command.description.as_deref().unwrap_or_default(),
            Self::Value(completion) => &completion.detail,
        }
    }

    /// The name a row sorts under, which is the name without its slash.
    fn name(&self) -> &str {
        match self {
            Self::Ui(entry) => entry.name,
            Self::Engine(command) => &command.name,
            Self::Value(completion) => &completion.text,
        }
    }
}

/// One value a `/teammate` argument slot completes to (**D519**).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// What replaces the partial word under the cursor.
    pub text: String,
    /// The line shown beside it.
    pub detail: String,
}

/// A `/teammate` slot the cursor is standing in, and what could fill it
/// (**D519**).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    /// What the menu is titled.
    pub title: &'static str,
    /// Where the partial word starts on the first line, in characters.
    pub start: usize,
    /// The partial word so far — what a chosen value replaces.
    pub partial: String,
    /// Everything that could stand there, in roster order.
    pub candidates: Vec<Completion>,
}

/// The `/teammate` slot the cursor is in, if it is in one the composer can fill
/// (**D519**): a subcommand after `/teammate`, a flag after `spawn`, the surface
/// after `--backend`, the kind after `--agent`.
///
/// The surfaces are [`BACKENDS`] — the one spelling the spawn door's refusal
/// lists, imported rather than repeated, so a seventh surface reaches this
/// menu the day it reaches the parser. The agent kinds are `agents`, which
/// the app reads off the engine's registry for the same reason. Nothing here
/// validates: a value typed past the menu is still the far side's to refuse
/// by name, exactly as before.
///
/// The word under the cursor runs from the last whitespace before it to the
/// cursor; text after the cursor is left alone, so completing mid-line
/// replaces only the word being typed. A word that already **is** one of the
/// candidates raises nothing: there is nothing left to complete, and a menu
/// still up there would take the Enter that means "send this line" — which
/// is exactly what `/teammate spawn w1 --backend ganja` + Enter means, and what
/// the pane drills type.
#[must_use]
pub fn team_completion(text: &str, cursor: (usize, usize), agents: &[Completion]) -> Option<Slot> {
    let (row, column) = cursor;
    if row != 0 {
        return None;
    }
    let line: Vec<char> = text.lines().next().unwrap_or_default().chars().collect();
    let column = column.min(line.len());
    let start = line[..column]
        .iter()
        .rposition(|character| character.is_whitespace())
        .map_or(0, |index| index + 1);
    let partial: String = line[start..column].iter().collect();
    let before: String = line[..start].iter().collect();
    let mut words = before.split_whitespace();
    // Through [`lookup`] for [`team`]'s reason: the alias completes exactly
    // as the name does, rather than a second spelling of it living here.
    let named = words.next().and_then(|word| word.strip_prefix('/')).and_then(lookup);
    if named.is_none_or(|entry| entry.action != Action::Team) {
        return None;
    }
    let words: Vec<&str> = words.collect();

    let slot = |title, candidates: Vec<Completion>| {
        if candidates.iter().any(|candidate| candidate.text == partial) {
            return None;
        }
        Some(Slot { title, start, partial: partial.clone(), candidates })
    };
    match words.as_slice() {
        [] => slot(" teammate ", subcommands()),
        ["spawn", rest @ ..] => match rest.last() {
            Some(&"--backend") => slot(" backends ", backends()),
            Some(&"--agent") => slot(" agents ", agents.to_vec()),
            _ if partial.starts_with('-') => slot(" flags ", flags(rest)),
            _ => None,
        },
        _ => None,
    }
}

/// What follows `/teammate`, in the order [`team`] reads them.
fn subcommands() -> Vec<Completion> {
    [
        ("spawn", "start a teammate"),
        ("shutdown", "ask a member — or, unnamed, every member — to shut down"),
        ("list", "show the roster"),
    ]
    .into_iter()
    .map(|(text, detail)| Completion { text: text.to_owned(), detail: detail.to_owned() })
    .collect()
}

/// The six surfaces, the default marked as such.
fn backends() -> Vec<Completion> {
    BACKENDS
        .iter()
        .map(|name| Completion {
            text: (*name).to_owned(),
            detail: if *name == backend_name(DEFAULT_BACKEND) {
                "the default when none is named".to_owned()
            } else {
                String::new()
            },
        })
        .collect()
}

/// `spawn`'s flags, minus the ones the line already carries.
fn flags(given: &[&str]) -> Vec<Completion> {
    [("--backend", "the surface the teammate runs on"), ("--agent", "the kind of agent it runs as")]
        .into_iter()
        .filter(|(flag, _)| !given.contains(flag))
        .map(|(text, detail)| Completion { text: text.to_owned(), detail: detail.to_owned() })
        .collect()
}

/// The rows `partial` narrows `candidates` to, best match first — every row in
/// roster order when nothing has been typed yet.
#[must_use]
pub fn value_matches(partial: &str, candidates: &[Completion]) -> Vec<Choice> {
    if partial.is_empty() {
        return candidates.iter().cloned().map(Choice::Value).collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let atom = fragment(partial);
    let mut buffer = Vec::new();
    let mut scored: Vec<(u32, Completion)> = candidates
        .iter()
        .filter_map(|candidate| {
            atom.score(Utf32Str::new(&candidate.text, &mut buffer), &mut matcher)
                .map(|score| (u32::from(score), candidate.clone()))
        })
        .collect();
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.text.cmp(&right.1.text)));

    scored.into_iter().map(|(_, choice)| Choice::Value(choice)).collect()
}

/// The rows `query` narrows to across both populations, best match first.
///
/// An empty query is every row: the UI commands in table order, then the
/// engine's in the order the registry lists them. The dropdown re-sorts that
/// case by name, because with nothing typed there is no ranking to show.
#[must_use]
pub fn dropdown_matches(query: &str, engine: &[EngineCommand]) -> Vec<Choice> {
    let needle = query.trim().trim_start_matches('/');
    if needle.is_empty() {
        return COMMANDS
            .iter()
            .map(Choice::Ui)
            .chain(engine.iter().cloned().map(Choice::Engine))
            .collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let atom = fragment(needle);

    let mut scored: Vec<(u32, Choice)> = COMMANDS
        .iter()
        .filter_map(|entry| {
            score(&atom, &mut matcher, entry, Surface::Dropdown)
                .map(|score| (score, Choice::Ui(entry)))
        })
        .collect();
    scored.extend(engine.iter().filter_map(|command| {
        score_engine(&atom, &mut matcher, command)
            .map(|score| (score, Choice::Engine(command.clone())))
    }));
    scored.sort_by(|left, right| {
        right.0.cmp(&left.0).then_with(|| left.1.name().cmp(right.1.name()))
    });

    scored.into_iter().map(|(_, choice)| choice).collect()
}

/// The best score `atom` gets against an engine command's name or its
/// description — the same two fields, and the same weights, the dropdown
/// scores a UI command by.
fn score_engine(atom: &Atom, matcher: &mut Matcher, command: &EngineCommand) -> Option<u32> {
    let mut buffer = Vec::new();
    let mut best = None;
    let mut consider = |text: &str, weight: u32| {
        if let Some(score) = atom.score(Utf32Str::new(text, &mut buffer), matcher) {
            let scaled = u32::from(score) * weight;
            best = Some(best.map_or(scaled, |current: u32| current.max(scaled)));
        }
    };

    consider(&command.name, 2);
    if let Some(description) = &command.description {
        consider(description, 1);
    }

    best
}

/// Which surface is matching, and therefore what a fragment is compared
/// against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// The palette: the name, its aliases and the title.
    Palette,
    /// The dropdown, which also reads descriptions — upstream's slash mode
    /// adds `description` to its keys where `@` mode leaves it out.
    Dropdown,
}

/// The commands `query` narrows to, best match first.
///
/// An empty query is every command in table order, which is the order the
/// table is written in rather than anything computed: the palette groups them
/// itself and the dropdown sorts by name.
#[must_use]
pub fn matches(query: &str, surface: Surface) -> Vec<&'static Entry> {
    let needle = query.trim().trim_start_matches('/');
    if needle.is_empty() {
        return COMMANDS.iter().collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let atom = fragment(needle);

    let mut scored: Vec<(u32, &'static Entry)> = COMMANDS
        .iter()
        .filter_map(|entry| score(&atom, &mut matcher, entry, surface).map(|score| (score, entry)))
        .collect();
    // Descending by score, then by name so that two commands scoring the same
    // always come out in the same order — the part of the ranking that is a
    // promise, where the score itself is the matcher's business (**D10**).
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.name.cmp(right.1.name)));

    scored.into_iter().map(|(_, entry)| entry).collect()
}

/// The needle a typed fragment becomes.
///
/// `Atom::new` rather than `Atom::parse`: a fragment the user typed is a
/// fragment, not a query language, so a leading `^` or a trailing `$` should
/// narrow by those characters instead of changing the match mode.
fn fragment(needle: &str) -> Atom {
    Atom::new(needle, CaseMatching::Ignore, Normalization::Smart, AtomKind::Fuzzy, false)
}

/// The best score `atom` gets against any of `entry`'s matchable fields.
///
/// The maximum rather than the sum: a fragment that is the whole of an alias
/// should rank as well as one that is the whole of a name, and adding the
/// fields would instead reward commands for having more of them.
fn score(atom: &Atom, matcher: &mut Matcher, entry: &Entry, surface: Surface) -> Option<u32> {
    let mut buffer = Vec::new();
    let mut best = None;

    // The name is weighted above everything else, as upstream weights its
    // title above its category: typing `mo` is a guess at a name.
    let mut consider = |text: &str, weight: u32| {
        if let Some(score) = atom.score(Utf32Str::new(text, &mut buffer), matcher) {
            let scaled = u32::from(score) * weight;
            best = Some(best.map_or(scaled, |current: u32| current.max(scaled)));
        }
    };

    consider(entry.name, 2);
    for alias in entry.aliases {
        consider(alias, 2);
    }
    consider(entry.title, 1);
    if surface == Surface::Dropdown {
        consider(entry.description, 1);
    }

    best
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
