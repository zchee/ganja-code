//! A teammate that is a headless `grok` child (**D508**, **D509**, **D510**).
//!
//! Spec: none. Upstream opencode has no teammates and Claude Code does not run
//! another vendor's agent as one, so every sentence here is ganja's own, and
//! the vendor surface it is written against is the binary itself — `grok 1.0.6
//! (24c70bc7ffdd) [alpha]`, probed on this machine rather than read out of a
//! source clone that is one version behind what a person has installed.
//!
//! This module is the **words**, and only the words: which flags go on a
//! command line and what a finished child's stdout meant. When a turn starts,
//! how long it may run, what its failure becomes and where its answer goes are
//! all [`crate::teammate::shim`]'s, shared with the other CLIs.
//!
//! # Why one child per message
//!
//! `--prompt-file` is a **single-turn** door — the vendor's own help says so —
//! so [`Shape::PerMessage`](crate::teammate::shim::Shape::PerMessage) is not a
//! choice between two doors but the only one that surface has. Continuity rides
//! `--resume <id>`, and the id is not captured from the stream the way codex's
//! is: this side **chooses** it.
//!
//! # The session id is minted here, not observed
//!
//! `--session-id` is documented as *"Use a specific session UUID for a **new**
//! conversation (must be a valid UUID and must not already exist under the
//! target session directory)… Does not resume existing sessions"*, which is
//! exactly the mint-then-resume shape: a first turn names the conversation it
//! is creating and every later turn resumes that name. The id is
//! [`ganja_protocol::uuidv7`]'s — the mint this tree already has (**D493**) —
//! and being UUID-shaped is load-bearing rather than tidy: `--resume` matches
//! *titles* as well as ids, and the vendor's own help pins the tie-break,
//! *"UUID-shaped values always mean IDs"*. A minted v7 can therefore never
//! resume somebody's session because it happened to share a title.
//!
//! **[`Driver::argv`](crate::teammate::shim::Driver::argv) mints, and that is this file's one departure from that
//! method's documented purity.** It is deliberate and it is bounded to the
//! first turn of a member: `argv` is called exactly once per turn, so one call
//! is one conversation, and two grok teammates spawned together get two ids
//! (**AC-19**). What makes it safe rather than merely convenient is that the
//! mint is **written back through the ordinary session seam**: the id is read
//! off the child's own `system`/`init` record — grok stating which session it
//! is running — and returned as [`Reply::session`](crate::teammate::shim::Reply::session), which
//! [`crate::teammate::shim::ShimRunner`] stores exactly as it stores codex's
//! observed thread id. So the runner still owns the per-member state and the
//! driver is still stateless; what differs is only who *proposes* the id.
//!
//! A turn that does not reach that record records nothing, and the next turn
//! mints again. That is the conservative direction: an id recorded for a
//! conversation grok never created would make every later turn fail, because a
//! `--resume` of an unknown id is a hard error — measured, not assumed:
//! resuming an invented UUID exits 1 with *"Failed to restore session from
//! remote: … 404 Not Found"* rather than starting a fresh conversation.
//!
//! **The clause is wider than "before the record arrives", and the doc says so
//! rather than flattering the code:** the shim reads a child's stdout only on a
//! **zero** exit, so a turn that printed its `init` record and *then* exited
//! non-zero — a mid-turn crash, a signal, a vendor error path that ends in a
//! status — loses the id too, and its next message starts a second
//! conversation rather than resuming the one that already exists. Salvaging the
//! id off a non-zero exit's stdout is a behavioural change to shared shim code
//! and is filed rather than smuggled in here; what this file owes today is an
//! honest sentence about it.
//!
//! # The posture, and why the flags are in this order
//!
//! `--sandbox read-only` is the bound and `--permission-mode dontAsk` is
//! defence-in-depth beside it, so the launch line reads the way the posture
//! works. Neither is decoration:
//!
//! - **The sandbox is applied at process entry**, before the interactive
//!   branch is even computed, so it holds on a headless turn. It is composed on
//!   the resume line too, and there it both pins and passes: that vendor
//!   refuses a resume requesting a *different* profile rather than silently
//!   applying it, so repeating the same one is the only spelling that does
//!   both.
//! - **`--permission-mode dontAsk` is not an approval axis at this version**
//!   and it is not inert either — see [`GROK_MODE_LINE`](crate::teammate::shim::GROK_MODE_LINE),
//!   which is the sentence
//!   the ring carries. What it does is select neither `yolo` nor `auto` for
//!   this launch, which suppresses a config-level always-approve. That is
//!   measurable rather than argued, and it was measured: this machine's own
//!   `~/.grok/config.toml` sets `[ui] yolo = true` and
//!   `permission_mode = "always-approve"`, and the composed launch still took
//!   the cancel arm.
//!
//! **The `read-only` spelling is pinned as a literal byte string** and a unit
//! test pins it, because `--sandbox` is unvalidated at clap and an unrecognized
//! value becomes a *custom* profile rather than an error: `read_only` would be
//! looked up as somebody's own profile, fail to load, and hard-exit the child.
//! Measured here too — `--sandbox read_only` refuses naming `'read_only'`,
//! where `--sandbox readonly` normalizes to the built-in and refuses naming
//! `'read-only'`.
//!
//! # What that refusal must arrive as
//!
//! Both of that vendor's startup refusals — the profile conflict and the
//! could-not-apply — are a hard `exit(1)` with an `error:` line on stderr. The
//! shim classifies a non-zero exit as [`Failure::Exit`](crate::teammate::shim::Failure::Exit)
//! before this file's parser is
//! ever reached, so the lead is told *"grok exited with status 1: <the
//! vendor's own sentence>"* rather than being told the stream was unreadable.
//! That ordering is what **AC-8**'s fourth arm asks for, and it is a property
//! of the shim core rather than of anything here — pinned by a test in this
//! file's suite so it cannot quietly stop being true.
//!
//! It is not hypothetical. On the machine this was written on, `~/.grok` was
//! a symlink until 2026-08-20, and the `read-only` profile refuses to start
//! under one: *"hook write-deny ensure failed: symlinked GROK_HOME is not
//! allowed under sandbox write-deny"*. A grok teammate there refused every
//! turn with that sentence, which is the honest outcome — the alternative
//! would be starting with the protections missing. The user un-symlinked the
//! directory and the same launch line then ran real turns; the `GROK_HOME`
//! carve-out that would have spared the symlink stays filed as bead
//! `ganja-code-q98` for a machine that keeps one.
//!
//! # The parser, and the door it did not take (**D510**)
//!
//! `--output-format streaming-messages-json` is *"NDJSON in the Anthropic
//! Messages API wire format"*, and `--include-partial-messages` adds the
//! incremental `stream_event` lines beside the whole messages. The alternative
//! door was `streaming-json`, that vendor's own ACP session updates; the
//! Messages wire won because it is a shape this repository already speaks and
//! the parse is one bounded serde module, where ACP would be a new protocol
//! surface adopted for one CLI.
//!
//! The duplication against [`crate::provider`]'s Messages decoder is
//! **deliberate and bounded**: that decoder is shaped around SSE framing and a
//! provider's `ChatRequest`, and what is read here is six record kinds off a
//! pipe with no request behind them. Sharing it would mean bending a wire
//! nobody else on this path speaks.
//!
//! # What `--include-partial-messages` is actually for here
//!
//! A [`Shape::PerMessage`](crate::teammate::shim::Shape::PerMessage) child's
//! stdout is read to the end, so the deltas
//! do not reach a reader any earlier than the whole messages do — the
//! mid-turn granularity the flag exists for is not something this shape can
//! show yet. It is composed anyway, and it earns its place on a narrower fact:
//! a turn that **dies mid-call** may never emit a whole `assistant` record at
//! all, and the partial `content_block_start` is then the only place the tool
//! it was running is named. Naming that tool is what turns a silent dead turn
//! into a sentence a lead can act on.
//!
//! # What travels in the environment
//!
//! Nothing beyond [`crate::teammate::shim::CARRIED`], and the emptiness is the
//! design rather than an omission: every flag this CLI needs is on the command
//! line, and **no `GROK_*` variable may ever be carried** — that vendor has at
//! least three environment doors onto the very posture pinned above,
//! `GROK_SANDBOX` (named in `--sandbox`'s own help as its env source) among
//! them. The rule is a class rule in [`crate::teammate::shim::admits`], enforced
//! by the enumeration and asserted at [`crate::teammate::shim::prepare`], so
//! adding one here is a caught mistake rather than a silent posture change.
//!
//! # No auth pre-check
//!
//! codex is the one of these CLIs that answers the question cheaply; this one
//! does not. `grok models` prints *"You are not authenticated."* and exits
//! **zero**, so its status says nothing, and parsing that sentence would be
//! pinning a display string as a protocol. A grok teammate therefore reports an
//! authentication failure on its first turn, through the same structured mail
//! every other turn failure takes — measured: an unauthenticated turn ends with
//! a `result` record carrying `is_error` and the vendor's own 401 text, which
//! this file's parser turns into exactly that mail.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use ganja_protocol::team::MemberBackend;
use ganja_team::ShimCli;
use serde::Deserialize;

use crate::teammate::{
    readback,
    shim::{Door, Driver, Reply, Shape, Turn},
};

/// The executable a spawn looks for on `PATH`.
pub const BINARY: &str = "grok";

/// The `--sandbox` value D508(a) pins, as a **literal byte string**.
///
/// Not a spelling to be normalized on the way past: `--sandbox` is unvalidated
/// at clap and an unrecognized value becomes a custom profile that fails to
/// load and hard-exits the child, so `read_only` is a broken teammate rather
/// than a typo. A unit test pins these exact bytes.
pub const SANDBOX_VALUE: &str = "read-only";

/// The `--permission-mode` value composed on every turn.
///
/// One of that flag's six documented values, and the reason it is this one
/// rather than `plan` is recorded in D508(a): as an approval axis the two are
/// the same non-event at this version, while `dontAsk`'s own vendor doc states
/// the intent it will carry the day the mode is wired — *silently deny
/// non-pre-approved tools* — so composing it now means the posture tightens
/// rather than needing a code change.
pub const PERMISSION_MODE: &str = "dontAsk";

/// The wire this side reads: *"NDJSON in the Anthropic Messages API wire
/// format"*.
pub const OUTPUT_FORMAT: &str = "streaming-messages-json";

/// The floors, and **only** the floors, for a pane running this CLI's own TUI
/// (**D512**): the bound, then the mode beside it, in the first turn's order.
///
/// Nothing else. No `--prompt-file` — an interactive grok takes its prompt
/// from its composer, and the words travel a tmux paste buffer rather than
/// argv or a `0600` file — no `--session-id` and no `--resume`, because a TUI
/// holds its own conversation, and neither `--output-format` nor
/// `--include-partial-messages`, which are the headless wire's and which a
/// composer never prints. The absence of every prompt door is also what makes
/// that vendor *start* a TUI: it computes "interactive" as no command, no `-p`,
/// no `--prompt-json`, no `--prompt-file`. No identity flag, because that CLI
/// has none to give.
///
/// Measured twice on 2026-08-20, and the recording
/// (`tests/fixtures/grok-tui-probe.txt`) carries both: against `grok 1.0.6`
/// with `~/.grok` a symlink the flags parse and then the read-only profile
/// refuses to apply (bead `ganja-code-q98`), with the same sentence the
/// headless child exits on — the dead-pane case a pane exists to keep on
/// screen, kept verbatim; and against `grok 1.0.7` with `~/.grok` a real
/// directory the same argv reaches the composer under `sandbox:read-only`.
/// The test compares this table against the recorded launch line rather than
/// against a second literal.
pub const TUI_ARGV: [&str; 4] = [
    "--sandbox",
    SANDBOX_VALUE,
    "--permission-mode",
    PERMISSION_MODE,
];

/// What a readiness poll looks for in a captured pane: the composer's prompt
/// glyph, **measured** (`tests/fixtures/grok-tui-probe.txt`, the 1.0.7
/// recording).
///
/// grok's composer draws no placeholder text — codex's `Ask Codex to do
/// anything` has no counterpart here — so the empty composer line is the box
/// border and this glyph and nothing else, and the glyph is what a poll can
/// look for. The test beside this reads the recording's `composer capture`
/// line, strips the box border, and byte-compares what is left against this
/// constant, the way codex's and agy's markers are pinned. Until the user
/// un-symlinked this machine's `~/.grok` (bead `ganja-code-q98`) the marker
/// was the vendor's welcome-banner string read out of its own source; the
/// banner still co-renders with the composer in the first frame, but a
/// captured composer outranks a string nobody observed.
///
/// The glyph also opens every user row grok's transcript draws after a
/// submit, which is no hazard to a readiness poll: it only ever asks whether
/// the composer has been drawn at all, and the settle and the liveness
/// re-listing in `shim_tui` bound what a too-early sighting can cost.
pub const READY_MARKER: &str = "❯";

/// Everything this CLI's argv may **never** carry, in every spelling that
/// binary has.
///
/// The single source for the test that asserts it rather than a list the test
/// repeats, and the spellings were read off `grok --help` at 1.0.6 rather than
/// guessed — that surface carries compat aliases (`--allowedTools` for
/// `--allow`, `--system-prompt` for `--system-prompt-override`) and short
/// aliases (`-c` for `--continue`, `-w` for `--worktree`) that a long-flag
/// grep would walk straight past.
///
/// Six entries are **values** rather than flags, for the reason codex's list
/// carries two: a posture is escaped as easily by a value as by a flag. Three
/// of them are real `--permission-mode` values that would select an approval
/// posture this grant does not include — `acceptEdits`, `auto` and
/// `bypassPermissions`, out of the six that flag documents (`default`,
/// `acceptEdits`, `auto`, `dontAsk`, `bypassPermissions`, `plan`). `plan` and
/// `default` are absent because neither widens anything.
///
/// **`always-approve` is on the list and is *not* one of those six**, which is
/// worth stating rather than leaving as an apparent typo: it is the
/// **config-level** spelling, the other value that vendor's own
/// `resolve_effective_yolo` matches beside `bypassPermissions`. It is banned
/// here so that no future edit reaches for it as though the flag took it.
///
/// The last two values are `--sandbox` profiles. `strict` is on the list even
/// though it reads stricter, and D508(a) records why: it buys a narrower read
/// by making **the workspace itself writable**, which is the wrong trade under
/// a v1 that grants read and not write.
///
/// `-s` is deliberately **absent**. On this vendor's surface it is the short
/// alias of `--session-id`, which this file composes — unlike codex, where the
/// same two letters are the sandbox flag.
pub const NEVER_COMPOSED: [&str; 26] = [
    // Approval escapes: the flag that approves everything, the three
    // `--permission-mode` values that would, and the config-level spelling of
    // the first of them.
    "--always-approve",
    "bypassPermissions",
    "acceptEdits",
    "auto",
    "always-approve",
    // Rule and tool surface: `--allow` widens what needs no approval,
    // `--tools` changes which built-ins exist at all.
    "--allow",
    "--allowedTools",
    "--tools",
    // The agent's own instructions, in all three of that surface's spellings:
    // `--rules` *appends* to the system prompt where `--system-prompt-override`
    // replaces it, and appending to the instructions a foreign agent runs under
    // is the same class of act as replacing them.
    "--system-prompt-override",
    "--system-prompt",
    "--rules",
    // Which agent takes the turn at all. Named because D508(a)'s own tracing
    // ends here: `--permission-mode` travels to an `AgentDefinition` field
    // rather than to that vendor's permission engine, so an agent definition is
    // the thing that carries the mode — and composing one would hand a foreign
    // definition the axis this build declines to grant.
    "--agent",
    "--agents",
    // Session identity: `--continue` resumes *the most recent* conversation
    // for this directory, which may be another teammate's or the person's own;
    // `--fork-session` would silently mint a second conversation off one this
    // side believes it is resuming.
    "--continue",
    "-c",
    "--fork-session",
    // `--restore-code` would check a repository snapshot out over the person's
    // working tree — the one composed flag that could destroy work.
    "--restore-code",
    // Where the agent stands: both move its root away from the cwd the spawn
    // dialog gated, which is codex's `-C/--cd` under two other names.
    "--cwd",
    "--worktree",
    "-w",
    // The other three prompt doors, and every one of them is an **AC-21**
    // hazard rather than a posture one: `-p/--single` and `--prompt-json` take
    // the prompt as a flag *value*, so composing either would put a peer's
    // words — a documented place for a credential to land in cleartext — into
    // an argv the whole machine can read through `ps`. `--prompt-file` is the
    // one door that names a path instead.
    "-p",
    "--single",
    "--prompt-json",
    // The wire itself: `--json-schema` "Implies --output-format json", so it
    // would silently replace the Messages NDJSON this file's parser is written
    // against with a shape it cannot read — a turn lost to a flag nobody
    // reading the launch line would connect to the output format.
    "--json-schema",
    // The two sandbox profiles that are not the floor, as values.
    "strict",
    "off",
];

/// The record kind that opens a turn and names the session grok is running.
const SYSTEM: &str = "system";

/// Its `subtype`.
const INIT: &str = "init";

/// The record kind carrying one whole assistant message.
const ASSISTANT: &str = "assistant";

/// The record kind that ends a turn.
const RESULT: &str = "result";

/// The record kind wrapping one incremental Messages event.
const STREAM_EVENT: &str = "stream_event";

/// One NDJSON record, in the minimum shape this side reads.
///
/// Every field is optional and unknown fields are ignored, which is the same
/// forward-compatibility posture codex's parser takes: a vendor printing one
/// more kind, or one more field, must not cost a turn that otherwise
/// succeeded.
#[derive(Debug, Default, Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: Option<String>,
    /// `system`'s discriminator — `init` is the only one this side reads.
    subtype: Option<String>,
    /// The session grok says it is running, on the `system`/`init` record.
    session_id: Option<String>,
    /// The whole message, on an `assistant` record.
    message: Option<Message>,
    /// Whether a `result` ended the turn badly.
    is_error: Option<bool>,
    /// A `result`'s own final text.
    result: Option<String>,
    /// Why the turn stopped, on a `result`.
    stop_reason: Option<String>,
    /// What went wrong, in the vendor's own words.
    #[serde(default)]
    errors: Vec<String>,
    /// One incremental Messages event, on a `stream_event` record.
    event: Option<Event>,
}

/// One assistant message.
#[derive(Debug, Default, Deserialize)]
struct Message {
    #[serde(default)]
    content: Vec<Block>,
}

/// One content block of one message.
#[derive(Debug, Default, Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: Option<String>,
    /// A `text` block's words.
    text: Option<String>,
    /// A `tool_use` block's tool, which is where the reducer puts the name.
    name: Option<String>,
}

/// One incremental event, as `--include-partial-messages` emits it.
#[derive(Debug, Default, Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: Option<String>,
    /// What a `content_block_start` is starting — the only place a tool call
    /// that never completed is named.
    content_block: Option<Block>,
    /// A `content_block_delta`'s or `message_delta`'s payload.
    delta: Option<Delta>,
}

/// One delta.
#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(rename = "type")]
    kind: Option<String>,
    /// A `text_delta`'s words.
    text: Option<String>,
    /// A `message_delta`'s reason for stopping.
    stop_reason: Option<String>,
}

/// The `text` content-block kind.
const TEXT: &str = "text";

/// The `tool_use` content-block kind, whose `name` is the tool.
const TOOL_USE: &str = "tool_use";

/// The delta kind carrying assistant words.
const TEXT_DELTA: &str = "text_delta";

/// The event kind that opens a content block.
const CONTENT_BLOCK_START: &str = "content_block_start";

/// The event kind carrying a delta.
const CONTENT_BLOCK_DELTA: &str = "content_block_delta";

/// The event kind carrying the message's own stop reason.
const MESSAGE_DELTA: &str = "message_delta";

/// What a turn the CLI stopped says it stopped for, in both places it says it.
const CANCELLED: &str = "cancelled";

/// A teammate driven through a headless `grok`.
///
/// Stateless: the conversation id lives in the shim runner, which is what lets
/// one driver serve every member on this CLI. What this file *proposes* on a
/// first turn is a fresh mint; what is *remembered* is what the child said back.
#[derive(Clone, Copy, Debug, Default)]
pub struct Grok;

impl Grok {
    /// The driver.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// [`TUI_ARGV`] as the owned words a pane's launch line is composed from.
    ///
    /// Takes no [`Turn`] on purpose: there is no prompt file, no session and
    /// no deadline to read, so nothing a peer said can reach this argv by
    /// construction rather than by a test — and no id is minted, because a TUI
    /// holds its own conversation.
    #[must_use]
    pub fn tui_argv(&self) -> Vec<OsString> {
        TUI_ARGV.iter().map(OsString::from).collect()
    }
}

/// What one turn's stdout added up to.
#[derive(Debug, Default)]
struct Seen {
    /// The session grok said it was running.
    session: Option<String>,
    /// The last whole assistant message's text.
    message: Option<String>,
    /// The `result` record's own final text.
    result: Option<String>,
    /// Assistant words as deltas, for a turn whose whole message never came.
    streamed: String,
    /// Whether a `result` said the turn ended badly.
    failed: bool,
    /// What the vendor said went wrong.
    errors: Vec<String>,
    /// Why the turn stopped, from whichever record named it last.
    stop: Option<String>,
    /// Every tool this turn named, in call order.
    tools: Vec<String>,
    /// Whether any record this build reads was seen at all.
    read_anything: bool,
}

impl Seen {
    /// The turn's answer, preferring the vendor's own final text.
    ///
    /// Three sources in falling order of authority: the `result` record's
    /// `result` field, which is that vendor's own statement of what the turn
    /// answered; the last whole `assistant` message; and the text deltas, which
    /// are all that a turn cut off before its message completed leaves behind.
    fn answer(&self) -> Option<&str> {
        [
            self.result.as_deref(),
            self.message.as_deref(),
            Some(self.streamed.as_str()),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|text| !text.is_empty())
    }

    /// Whether the CLI itself ended this turn on an unapproved tool request.
    ///
    /// Two spellings of the one fact because the probed version prints both,
    /// and either alone would be a narrower guard than the vendor gives: the
    /// terminal `result` carries `stop_reason: "cancelled"` **and** a
    /// one-word `errors: ["cancelled"]`.
    fn cancelled(&self) -> bool {
        self.stop.as_deref() == Some(CANCELLED) || self.errors.iter().any(|said| said == CANCELLED)
    }

    /// `after asking to run X`, for a sentence about a turn that stopped.
    ///
    /// The **last** tool named rather than all of them: a turn that ran five
    /// tools and died on the sixth is a turn whose sixth is the news.
    fn on_tool(&self) -> String {
        match self.tools.last() {
            Some(tool) => format!(" The last tool it asked to run was `{tool}`."),
            None => String::new(),
        }
    }
}

#[async_trait]
impl Driver for Grok {
    fn cli(&self) -> ShimCli {
        ShimCli::Grok
    }

    fn backend(&self) -> MemberBackend {
        MemberBackend::Grok
    }

    fn binary(&self) -> &str {
        BINARY
    }

    fn shape(&self) -> Shape {
        Shape::PerMessage
    }

    fn door(&self) -> Door {
        // The prompt travels in a `0600` file whose path the argv names, never
        // in argv itself — and the flag is what makes the child non-interactive
        // in the first place: that vendor computes "is this a TUI" as *no
        // command, no `-p`, no `--prompt-json`, no `--prompt-file`*.
        Door::File
    }

    fn argv(&self, turn: &Turn<'_>) -> Vec<OsString> {
        let mut argv = Vec::with_capacity(12);
        // A resume names the conversation first, because the id is this line's
        // subject; a first turn names the file first, because there is no
        // conversation yet to name.
        if let Some(session) = turn.session {
            argv.push(OsString::from("--resume"));
            argv.push(OsString::from(session));
        }
        argv.push(OsString::from("--prompt-file"));
        // An absent path composes an empty one rather than dropping the flag,
        // and the difference is a wedge: without any prompt door this vendor
        // starts its interactive TUI on a piped stdio and answers nothing until
        // the deadline, where an unreadable file exits at once with a sentence.
        // `Door::File` means the runner always writes one, so this is the arm
        // nothing takes.
        argv.push(turn.prompt.map_or_else(OsString::new, OsString::from));
        if turn.session.is_none() {
            // The mint. See this module's header: one `argv` call is one turn,
            // so one call is one conversation, and it is written back through
            // `Reply::session` off the child's own `init` record.
            argv.push(OsString::from("--session-id"));
            argv.push(OsString::from(ganja_protocol::uuidv7()));
        }
        // The posture, on **both** lines, and it is the **repetition** that is
        // load-bearing rather than anything about the order: a resume asking
        // for a *different* profile is refused by that vendor rather than
        // silently applied, so naming the same one again is the only spelling
        // that both pins and passes. The two lines happen to differ in the
        // order of the pair because the plan wrote them that way — clap does
        // not care, and neither does anything here.
        if turn.session.is_none() {
            argv.push(OsString::from("--sandbox"));
            argv.push(OsString::from(SANDBOX_VALUE));
            argv.push(OsString::from("--permission-mode"));
            argv.push(OsString::from(PERMISSION_MODE));
        } else {
            argv.push(OsString::from("--permission-mode"));
            argv.push(OsString::from(PERMISSION_MODE));
            argv.push(OsString::from("--sandbox"));
            argv.push(OsString::from(SANDBOX_VALUE));
        }
        argv.push(OsString::from("--output-format"));
        argv.push(OsString::from(OUTPUT_FORMAT));
        argv.push(OsString::from("--include-partial-messages"));

        argv
    }

    fn reply(&self, stdout: &str) -> Result<Reply, String> {
        let seen = read(stdout);

        if !seen.read_anything {
            return Err(format!(
                "grok printed no record this build reads; `--output-format {OUTPUT_FORMAT}` \
                 prints one JSON object per line"
            ));
        }
        // Whatever it managed to say before stopping is still owed to the lead,
        // so the words and the reason travel together rather than one replacing
        // the other.
        let messages: Vec<String> = seen.answer().map(str::to_owned).into_iter().collect();
        let refused = if seen.cancelled() {
            // **The measured shape.** A probed 1.0.6 ends a turn whose tool ask
            // nothing approved with `stop_reason: "cancelled"` and
            // `errors: ["cancelled"]` — one word, which on its own tells a lead
            // nothing. This is the sentence that does, and it names the tool
            // because "your teammate stopped" is not something anybody can act
            // on where "it stopped asking to run `write`" is.
            Some(format!(
                "grok cancelled this turn on an unapproved tool request.{} This build composes no \
                 approval flag, so that CLI's headless client answers every permission request \
                 `Cancelled`, which ends the turn rather than denying the call and letting the \
                 turn continue. Reading takes no approval; anything that does ends the turn here.",
                seen.on_tool()
            ))
        } else if seen.failed {
            let said = seen
                .errors
                .first()
                .map_or("the vendor named no reason", String::as_str);

            Some(format!(
                "grok ended the turn as failed: {said}{}",
                seen.on_tool()
            ))
        } else if messages.is_empty() {
            // Neither an answer nor a reason: the turn ran and said nothing
            // either way. Reported rather than passed off as an empty answer,
            // because a teammate that goes quiet is the one outcome Principle 4
            // exists to rule out.
            let why = seen
                .stop
                .as_deref()
                .map_or(String::new(), |stop| format!(" (stop reason: {stop})"));

            Some(format!(
                "grok ended the turn without an answer and without naming a reason{why}.{}",
                seen.on_tool()
            ))
        } else {
            None
        };

        Ok(Reply {
            messages,
            session: seen.session,
            refused,
        })
    }
}

/// Everything one turn's stdout said, in one pass.
fn read(stdout: &str) -> Seen {
    let mut seen = Seen::default();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A line this side cannot parse is the vendor's to explain, not this
        // build's to fail a turn over.
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        let Some(kind) = record.kind.as_deref() else {
            continue;
        };
        seen.read_anything = true;
        match kind {
            SYSTEM if record.subtype.as_deref() == Some(INIT) => {
                // grok's own statement of which conversation this turn is, and
                // therefore the id a later turn resumes. Read rather than
                // assumed even though this side proposed it: a value echoed by
                // the child is a value the child accepted.
                //
                // Blank is refused rather than stored, and the asymmetry is why:
                // a stored id is **sticky** — nothing ever clears it — so one
                // empty string here would compose `--resume ""` on every later
                // turn of that member, and `--resume` takes an *optional* value
                // ("or the most recent if omitted"). The failure would not be a
                // dead teammate but a live one silently continuing somebody
                // else's conversation.
                seen.session = record.session_id.filter(|id| !id.trim().is_empty());
            }
            ASSISTANT => {
                if let Some(message) = record.message {
                    let mut text = String::new();
                    for block in message.content {
                        match block.kind.as_deref() {
                            Some(TEXT) => {
                                if let Some(said) = block.text {
                                    text.push_str(&said);
                                }
                            }
                            Some(TOOL_USE) => {
                                if let Some(tool) = block.name {
                                    remember(&mut seen.tools, tool);
                                }
                            }
                            // `thinking` is the model thinking, which is not a
                            // teammate talking and never becomes mail.
                            _ => {}
                        }
                    }
                    if !text.trim().is_empty() {
                        // The **last** whole message rather than all of them: a
                        // turn that ran tools says several things on its way to
                        // an answer, and mailing each would flood a lead with
                        // narration. What the plan asks for is the final one.
                        seen.message = Some(text);
                    }
                }
            }
            RESULT => {
                seen.failed |= record.is_error.unwrap_or_default();
                if let Some(text) = record.result {
                    seen.result = Some(text);
                }
                if let Some(stop) = record.stop_reason {
                    seen.stop = Some(stop);
                }
                seen.errors.extend(record.errors);
            }
            STREAM_EVENT => {
                if let Some(event) = record.event {
                    partial(&mut seen, event);
                }
            }
            _ => {}
        }
    }

    seen
}

/// One incremental event's contribution.
fn partial(seen: &mut Seen, event: Event) {
    match event.kind.as_deref() {
        Some(CONTENT_BLOCK_START) => {
            // The one thing the partial stream gives that the whole messages do
            // not: a tool named by a turn that died before its message
            // completed, and therefore before any `assistant` record carried
            // that call.
            if let Some(block) = event.content_block
                && block.kind.as_deref() == Some(TOOL_USE)
                && let Some(tool) = block.name
            {
                remember(&mut seen.tools, tool);
            }
        }
        Some(CONTENT_BLOCK_DELTA) => {
            if let Some(delta) = event.delta
                && delta.kind.as_deref() == Some(TEXT_DELTA)
                && let Some(text) = delta.text
            {
                seen.streamed.push_str(&text);
            }
        }
        Some(MESSAGE_DELTA) => {
            if let Some(stop) = event.delta.and_then(|delta| delta.stop_reason) {
                seen.stop = Some(stop);
            }
        }
        _ => {}
    }
}

/// Records a tool call, without recording the same one twice.
///
/// The partial stream names a call and the whole message names it again, so
/// without this every tool would be counted at least twice — and the sentence
/// a lead reads is about *which* tool, never about how many records mentioned
/// it.
fn remember(tools: &mut Vec<String>, tool: String) {
    if tools.last() == Some(&tool) {
        return;
    }
    tools.push(tool);
}

/// Where this CLI keeps its sessions, as **this** process reads it.
///
/// Not a contradiction of the class rule that bans every `GROK_*` name from a
/// child's environment (**D508**): that rule is about what ganja *hands* the
/// CLI, because a `GROK_SANDBOX` travelling in would move the posture a
/// person consented to. Reading this one here moves nothing — it only says
/// where to look for what the pane already wrote. A tmux server started with
/// a different `GROK_HOME` than the lead's own is the case this cannot see,
/// and it shows up as a session that is never found rather than as somebody
/// else's conversation.
const HOME_ENV: &str = "GROK_HOME";

/// grok's own record of a conversation, as this side reads it (**D515**).
///
/// `<grok home>/sessions/<percent-encoded cwd>/<session id>/updates.jsonl`,
/// which is why this reader is the one that uses the pane's directory: the
/// vendor shards by it, and encoding the same path this side opened the pane
/// in narrows the search to one directory before a byte is read.
///
/// Re-read whole and counted, [`readback::Cursor::answers`]: an answer here
/// is *the last message of a finished turn*, so a scan that began in the
/// middle of one would have to carry the turn's own state between polls —
/// and these files are tens of kilobytes, where codex's are megabytes.
///
/// One answer per `turn_completed`, which is what grok's headless driver
/// mails too: that vendor narrates as it works, and its own module says why
/// mailing each line would flood a lead with commentary.
#[derive(Debug)]
pub struct Transcript;

/// The reader [`crate::teammate::readback::of`] hands out for this CLI.
pub static TRANSCRIPT: Transcript = Transcript;

impl Transcript {
    /// Where this CLI keeps its sessions: `GROK_HOME`, else `~/.grok`.
    fn sessions() -> Option<PathBuf> {
        let home = match std::env::var_os(HOME_ENV) {
            Some(home) if !home.is_empty() => PathBuf::from(home),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".grok"),
        };

        Some(home.join("sessions"))
    }

    /// The vendor's own spelling of a directory as one path segment: every
    /// byte outside the unreserved set percent-encoded, uppercase hex.
    ///
    /// Hand-written rather than taken from a crate because it is three lines
    /// and because what has to match is *this vendor's* encoding of one path
    /// — a general-purpose escaper with a different unreserved set would
    /// name a directory that exists on nobody's disk.
    fn encoded(cwd: &Path) -> String {
        // **Resolved first.** grok records the path it resolved, not the one
        // it was handed: on a machine where `/tmp` is a symlink for
        // `/private/tmp`, every session of a pane opened under `/tmp` lands
        // in a `%2Fprivate%2Ftmp…` directory (measured: 20 such directories
        // on this machine, and none under `%2Ftmp`). Encoding what this side
        // was handed would look in a directory that exists on nobody's disk,
        // and the pane would silently never be heard from.
        std::fs::canonicalize(cwd)
            .unwrap_or_else(|_| cwd.to_path_buf())
            .to_string_lossy()
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (byte as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect()
    }
}

impl readback::Transcript for Transcript {
    fn find(&self, mark: &str, cwd: &Path, since: std::time::SystemTime) -> Option<PathBuf> {
        let sessions = Self::sessions()?;
        let directory = sessions.join(Self::encoded(cwd));
        // The encoded directory first, and every directory under `sessions`
        // as the fallback: the encoding is this vendor's own and a version
        // that changed it would otherwise take the pane's voice away
        // silently. The fingerprint and the spawn time are what decide in
        // either case, so the wider search costs candidates and not
        // correctness.
        let mut roots = vec![directory.clone()];
        if !directory.is_dir() {
            roots = readback::listing(&sessions, |path| path.is_dir());
        }
        let candidates = roots
            .iter()
            .flat_map(|root| readback::listing(root, |path| path.is_dir()))
            .map(|session| session.join("updates.jsonl"))
            .filter(|path| path.is_file())
            .collect();

        readback::matching(candidates, mark, self, since)
    }

    fn user_said(&self, record: &serde_json::Value, mark: &str) -> bool {
        let update = &record["params"]["update"];
        update["sessionUpdate"] == "user_message_chunk"
            && update["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains(mark))
    }

    fn answers(&self, path: &Path, cursor: &mut readback::Cursor) -> Vec<String> {
        let mut finished = Vec::new();
        let mut latest: Option<String> = None;
        for line in readback::whole(path) {
            let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let update = &record["params"]["update"];
            match update["sessionUpdate"].as_str() {
                // A new turn began: whatever the last one said, said or not,
                // is not this turn's answer. Without this an interrupted turn
                // — one that never reached `turn_completed`, which this
                // machine has recordings of — would have its last message
                // carried later, attributed to the turn after it.
                Some("user_message_chunk") => latest = None,
                Some("agent_message_chunk") => {
                    if let Some(text) = update["content"]["text"].as_str()
                        && !text.trim().is_empty()
                    {
                        latest = Some(text.to_owned());
                    }
                }
                // The turn ended: whatever it last said is its answer, and
                // anything it said before that was narration on the way.
                Some("turn_completed") => finished.extend(latest.take()),
                _ => {}
            }
        }

        readback::beyond(finished, cursor)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use ganja_team::{MemberName, TeamName, TeamsRoot};

    use super::*;
    use crate::teammate::{SpawnSpec, shim};

    /// A spawn to compose against. Nothing in an argv reads any of it — which
    /// is itself the point of **AC-21**, and is why this can be one value.
    fn spec() -> SpawnSpec {
        SpawnSpec {
            name: MemberName::parse("w1").expect("a member name"),
            team: TeamName::default_team(),
            lead: MemberName::lead(),
            root: TeamsRoot::new(PathBuf::from("/nonexistent/teams")),
            backend: MemberBackend::Grok,
            agent_type: "general".to_owned(),
            model: "whatever-the-person-configured".to_owned(),
            color: "blue".to_owned(),
            prompt: "the spawn prompt, which travels through the mailbox".to_owned(),
            cwd: PathBuf::from("/nonexistent/work"),
            plan_mode_required: false,
            parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
            shell: crate::teammate::pane::PaneShell::default(),
        }
    }

    /// The argv for a turn that has, or has not, a conversation to resume.
    fn argv(session: Option<&str>) -> Vec<String> {
        let spec = spec();
        Grok.argv(&Turn {
            spec: &spec,
            text: "a teammate's words, which never reach a command line",
            prompt: Some(Path::new("/tmp/ganja-shim-xyz/prompt.txt")),
            session,
            deadline: shim::GROK_TURN_TIMEOUT,
        })
        .iter()
        .map(|token| token.to_string_lossy().into_owned())
        .collect()
    }

    /// The token after `flag`, if the argv carries one.
    fn value(argv: &[String], flag: &str) -> Option<String> {
        argv.iter()
            .position(|token| token == flag)
            .and_then(|at| argv.get(at + 1))
            .cloned()
    }

    #[test]
    fn a_first_turn_mints_the_conversation_it_is_creating() {
        let argv = argv(None);

        assert_eq!(
            argv.iter()
                .map(String::as_str)
                .filter(|token| *token != value(&argv, "--session-id").unwrap_or_default())
                .collect::<Vec<_>>(),
            vec![
                "--prompt-file",
                "/tmp/ganja-shim-xyz/prompt.txt",
                "--session-id",
                "--sandbox",
                "read-only",
                "--permission-mode",
                "dontAsk",
                "--output-format",
                "streaming-messages-json",
                "--include-partial-messages",
            ]
        );
        let minted = value(&argv, "--session-id").expect("a first turn names its own session");
        assert!(
            ganja_protocol::is_uuidv7(&minted),
            "a UUID-shaped id is what makes `--resume` mean an id rather than a title: {minted}"
        );
        assert!(
            !argv.iter().any(|token| token == "--resume"),
            "a first turn resumes nothing: {argv:?}"
        );
    }

    #[test]
    fn two_first_turns_never_propose_one_conversation() {
        // **AC-19** at the composition level: `argv` is called once per turn,
        // so two members' first turns are two calls and must be two ids. The
        // end-to-end half of this is in `teammate_shim_grok.rs`.
        let first = value(&argv(None), "--session-id").expect("an id");
        let second = value(&argv(None), "--session-id").expect("an id");

        assert_ne!(first, second);
    }

    #[test]
    fn a_resume_turn_names_the_conversation_and_repeats_the_posture() {
        let id = "01998ad0-0000-7000-8000-000000000000";

        assert_eq!(
            argv(Some(id)),
            vec![
                "--resume",
                id,
                "--prompt-file",
                "/tmp/ganja-shim-xyz/prompt.txt",
                "--permission-mode",
                "dontAsk",
                "--sandbox",
                "read-only",
                "--output-format",
                "streaming-messages-json",
                "--include-partial-messages",
            ]
        );
        assert!(
            !argv(Some(id)).iter().any(|token| token == "--session-id"),
            "`--session-id` is for a new conversation and does not resume"
        );
    }

    #[test]
    fn the_sandbox_value_is_the_exact_byte_string_the_builtin_answers_to() {
        // `--sandbox` is unvalidated at clap, so an unrecognized value becomes
        // a *custom* profile that fails to load and hard-exits the child.
        // Measured on 1.0.6: `read_only` refuses naming `'read_only'`, where
        // `readonly` normalizes and refuses naming `'read-only'`. This is the
        // spelling that neither.
        assert_eq!(SANDBOX_VALUE, "read-only");
        for session in [None, Some("01998ad0-0000-7000-8000-000000000000")] {
            let argv = argv(session);
            assert_eq!(
                value(&argv, "--sandbox").as_deref(),
                Some("read-only"),
                "the bound is pinned on every turn: {argv:?}"
            );
            assert_eq!(
                value(&argv, "--permission-mode").as_deref(),
                Some("dontAsk"),
                "and the mode beside it: {argv:?}"
            );
        }
    }

    #[test]
    fn no_never_composed_spelling_reaches_either_argv() {
        // Iterated rather than re-listed: [`NEVER_COMPOSED`] is the single
        // source, so a flag added to it is a flag this assertion picks up.
        for session in [None, Some("01998ad0-0000-7000-8000-000000000000")] {
            let argv = argv(session);
            for refused in NEVER_COMPOSED {
                assert!(
                    !argv.iter().any(|token| token == refused),
                    "{refused} must never be composed, and is in {argv:?}"
                );
            }
        }
    }

    #[test]
    fn no_prompt_text_is_ever_on_a_command_line() {
        let spec = spec();
        let secret = "the words a peer said, which argv is world-readable through ps";
        let argv = Grok.argv(&Turn {
            spec: &spec,
            text: secret,
            prompt: Some(Path::new("/tmp/ganja-shim-xyz/prompt.txt")),
            session: None,
            deadline: shim::GROK_TURN_TIMEOUT,
        });

        assert!(
            !argv
                .iter()
                .any(|token| token.to_string_lossy().contains(secret)),
            "argv is for flags; `--prompt-file` is what says where the prompt is"
        );
        assert_eq!(Grok.door(), Door::File);
    }

    #[test]
    fn the_environment_carries_no_door_onto_the_posture() {
        // Empty rather than short: every flag this CLI needs is on the command
        // line, and `GROK_SANDBOX` is `--sandbox`'s own documented environment
        // source — carrying any `GROK_*` name would hand a person's exported
        // variable the posture they consented to at spawn.
        assert!(Grok.additions().is_empty());
        assert!(
            !Grok
                .additions()
                .iter()
                .any(|name| name.starts_with("GROK_"))
        );
    }

    /// The shapes a probed `grok 1.0.6` actually printed, for a turn that
    /// answered.
    fn answered() -> String {
        [
            r#"{"type":"system","subtype":"init","session_id":"1c2f16a6-c5ed-4d60-9167-f34374890a6f","apiKeySource":"oauth","model":"grok-4.6","permissionMode":"dontAsk"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_0","role":"assistant","content":[]}},"session_id":"1c2f16a6-c5ed-4d60-9167-f34374890a6f"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"The"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"HELLO"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":41}}}"#,
            r#"{"type":"assistant","message":{"id":"msg_0","role":"assistant","content":[{"type":"thinking","thinking":"not mail","signature":"x"},{"type":"text","text":"HELLO"}],"stop_reason":"end_turn"}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":3186,"num_turns":1,"result":"HELLO","stop_reason":"end_turn"}"#,
        ]
        .join("\n")
    }

    #[test]
    fn a_turn_that_answered_is_one_mail_and_the_session_it_ran_in() {
        let reply = Grok.reply(&answered()).expect("a turn that answered");

        assert_eq!(reply.messages, vec!["HELLO"]);
        assert_eq!(
            reply.session.as_deref(),
            Some("1c2f16a6-c5ed-4d60-9167-f34374890a6f"),
            "the id a later turn resumes is the one the child said it was running"
        );
    }

    #[test]
    fn thinking_is_not_a_teammate_talking() {
        let reply = Grok.reply(&answered()).expect("a turn that answered");

        assert!(
            !reply.messages.iter().any(|text| text.contains("not mail")),
            "{:?}",
            reply.messages
        );
    }

    #[test]
    fn only_the_final_message_becomes_mail() {
        // A turn that runs tools says several things on its way to an answer.
        // The lead is owed the answer, not the narration — and the vendor's own
        // `result` is the strongest statement of which one that is.
        let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"let me look"},{"type":"tool_use","id":"t1","name":"hashline_read","input":{}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"it says four things"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"it says four things","stop_reason":"end_turn"}"#,
        ]
        .join("\n");

        let reply = Grok.reply(&stdout).expect("a turn that answered");

        assert_eq!(reply.messages, vec!["it says four things"]);
    }

    #[test]
    fn a_cancelled_turn_says_so_in_words_and_keeps_the_conversation() {
        // **The measured shape**, byte for byte what a probed 1.0.6 printed for
        // a turn whose `write` nothing approved: `stop_reason: "cancelled"` and
        // a one-word `errors: ["cancelled"]`, on a *zero* exit.
        //
        // Two things have to be true of the answer and neither is obvious. It
        // is not an `Err`, because this build read the stream perfectly and a
        // refusal that reads as unreadable output is a refusal nobody acts on;
        // and the session survives, because a cancelled turn is a live
        // conversation the next message should resume rather than a second one
        // to start.
        let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"write"}}}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["cancelled"],"stop_reason":"cancelled","num_turns":1}"#,
        ]
        .join("\n");

        let reply = Grok.reply(&stdout).expect("a cancelled turn is readable");
        let refused = reply.refused.expect("and it says why there is no answer");

        assert!(refused.contains("cancelled this turn"), "{refused}");
        assert!(refused.contains("`write`"), "and which tool: {refused}");
        assert!(
            refused.contains("Reading takes no approval"),
            "and what still works, which is the whole of what a grok teammate is \
             for: {refused}"
        );
        assert_eq!(reply.session.as_deref(), Some("s-1"));
        assert!(reply.messages.is_empty(), "{:?}", reply.messages);
    }

    #[test]
    fn a_tool_named_only_in_the_partial_stream_is_still_named() {
        // The one thing `--include-partial-messages` buys a shape that reads
        // its child's stdout to the end: a message cut off mid-call never
        // arrives as a whole `assistant` record, so the partial
        // `content_block_start` is the only place that call exists.
        let whole_message_only = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["cancelled"],"stop_reason":"cancelled"}"#,
        ]
        .join("\n");

        let unnamed = Grok
            .reply(&whole_message_only)
            .expect("still readable")
            .refused
            .expect("still a refusal");

        assert!(
            !unnamed.contains("last tool"),
            "with no partial there is nothing to name, and it says nothing rather than \
             guessing: {unnamed}"
        );
    }

    #[test]
    fn a_turn_that_said_something_before_stopping_still_delivers_those_words() {
        // The words and the reason travel together: a turn may answer half a
        // question and *then* ask for a tool nothing approves, and the half is
        // still owed to whoever asked.
        let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"it has three facts; let me write them down"},{"type":"tool_use","id":"t1","name":"write"}]}}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["cancelled"],"stop_reason":"cancelled"}"#,
        ]
        .join("\n");

        let reply = Grok.reply(&stdout).expect("a readable turn");

        assert_eq!(
            reply.messages,
            vec!["it has three facts; let me write them down"]
        );
        assert!(reply.refused.is_some(), "and the reason beside them");
    }

    #[test]
    fn a_turn_the_vendor_failed_carries_the_vendors_own_reason() {
        // The shape an unauthenticated turn actually printed, cut to the field
        // this side reads. Not a cancel, so it takes the other arm — and it is
        // still a reason rather than a parse failure, because this side read it.
        let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"stop_reason":null,"errors":["Internal error: Unauthorized (401) from https://cli-chat-proxy.grok.com/v1/responses"]}"#,
        ]
        .join("\n");

        let refused = Grok
            .reply(&stdout)
            .expect("a failed turn is still readable")
            .refused
            .expect("and it says what the vendor said");

        assert!(refused.contains("Unauthorized (401)"), "{refused}");
        assert!(
            !refused.contains("cancelled this turn"),
            "an authentication failure is not an unapproved tool ask: {refused}"
        );
    }

    #[test]
    fn a_line_this_build_cannot_read_does_not_cost_a_turn_that_otherwise_succeeded() {
        let stdout = [
            "a line that is not JSON at all",
            r#"{"type":"a.kind.this.build.has.never.heard.of","whatever":true}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"answered"}"#,
        ]
        .join("\n");

        let reply = Grok.reply(&stdout).expect("the readable half is readable");

        assert_eq!(reply.messages, vec!["answered"]);
    }

    #[test]
    fn output_carrying_no_record_at_all_is_refused_rather_than_read_as_silence() {
        let refusal = Grok
            .reply("this is not the shape any driver reads\n")
            .expect_err("garbage is refused");

        assert!(refusal.contains(OUTPUT_FORMAT), "{refusal}");
    }

    #[test]
    fn a_turn_cut_off_before_its_message_completed_still_answers_with_what_arrived() {
        // The deltas are the only place those words exist, which is the second
        // reason `--include-partial-messages` is composed.
        let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half an "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}}"#,
        ]
        .join("\n");

        let reply = Grok.reply(&stdout).expect("what arrived is what there is");

        assert_eq!(reply.messages, vec!["half an answer"]);
    }

    /// The pane-mode recording (**D512**), compared against rather than
    /// re-typed — the P27 posture-probe pattern: two literals agreeing proves
    /// only that somebody typed carefully.
    const TUI_PROBE: &str = include_str!("../../tests/fixtures/grok-tui-probe.txt");

    /// The launch line the recording says the pane ran, binary first.
    fn recorded_launch() -> Vec<&'static str> {
        TUI_PROBE
            .lines()
            .find_map(|line| line.strip_prefix("launch: "))
            .expect("the recording names the launch line it probed")
            .split_whitespace()
            .collect()
    }

    /// What the driver composes for a pane, as strings.
    fn tui() -> Vec<String> {
        Grok.tui_argv()
            .iter()
            .map(|token| token.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_tui_argv_is_the_launch_line_the_pane_probe_ran() {
        // Byte for byte against the recording, binary included — and the
        // `read-only` bytes matter here for the reason the headless test
        // states: `--sandbox` is unvalidated at clap, so a near-spelling is a
        // custom profile that fails to load.
        let recorded = recorded_launch();
        let (binary, floors) = recorded
            .split_first()
            .expect("a binary, then the floors it was launched with");

        assert_eq!(*binary, BINARY);
        assert_eq!(tui(), floors);
        // The flags parsed on both recordings: under a symlinked home what
        // happened next was the vendor's refusal, not a parse error — the
        // outcome a pane is meant to keep in front of a person — and under a
        // real one the composer.
        let outcomes: Vec<&str> = TUI_PROBE
            .lines()
            .filter(|line| line.trim_start().starts_with("outcome ("))
            .collect();
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        // Keyed on what each recording says about the home rather than on
        // position, so a third recording fails this loudly instead of
        // shifting which line answers which question.
        let symlinked = outcomes
            .iter()
            .find(|line| line.contains("a symlink"))
            .expect("the symlinked-home recording");
        let real = outcomes
            .iter()
            .find(|line| line.contains("a real directory"))
            .expect("the real-home recording");
        assert!(symlinked.contains("flags parse;"), "{symlinked}");
        assert!(real.contains("composer reached"), "{real}");
        let refusal = TUI_PROBE
            .lines()
            .find_map(|line| line.strip_prefix("error: "))
            .expect("the recording carries the vendor's own refusal verbatim");
        assert!(
            refusal.contains("could not apply the 'read-only' sandbox profile"),
            "{refusal}"
        );
    }

    #[test]
    fn the_ready_marker_is_the_composer_glyph_the_probe_captured() {
        // The line under `composer capture`, minus the box border it was
        // drawn inside: grok's composer carries no placeholder, so what the
        // recording shows with nothing typed is the border and the glyph, and
        // the glyph is the marker.
        let captured = TUI_PROBE
            .lines()
            .skip_while(|line| !line.trim_start().starts_with("composer capture"))
            .nth(1)
            .expect("the recording captured the empty composer");
        let glyph = captured
            .trim()
            .strip_prefix('│')
            .and_then(|inner| inner.strip_suffix('│'))
            .expect("the composer line is drawn inside a box")
            .trim();

        assert_eq!(READY_MARKER, glyph);
        // And nothing provisional is left: the recording no longer carries a
        // marker read out of somebody's source instead of off a screen.
        assert!(
            !TUI_PROBE
                .lines()
                .any(|line| line.trim_start().starts_with("provisional marker")),
            "the composer was captured; the provisional line has no reader left"
        );
    }

    #[test]
    fn the_tui_argv_carries_the_posture_and_none_of_the_headless_machinery() {
        let tui = tui();
        assert_eq!(value(&tui, "--sandbox").as_deref(), Some(SANDBOX_VALUE));
        assert_eq!(
            value(&tui, "--permission-mode").as_deref(),
            Some(PERMISSION_MODE)
        );
        // Every word here is a word of the headless first turn — one posture
        // rule, not a second one written for panes.
        let headless = argv(None);
        for token in &tui {
            assert!(headless.contains(token), "{token} is not a headless word");
        }
        // And none of the headless wire: no prompt door of any kind (their
        // absence is what makes that vendor start a TUI at all), no minted or
        // resumed id, no output flags.
        for headless_only in [
            "--prompt-file",
            "--session-id",
            "--resume",
            "--output-format",
            OUTPUT_FORMAT,
            "--include-partial-messages",
        ] {
            assert!(
                !tui.iter().any(|token| token == headless_only),
                "{headless_only} is the headless wire's, and is in {tui:?}"
            );
        }
    }

    #[test]
    fn no_never_composed_spelling_reaches_the_tui_argv() {
        // Iterated rather than re-listed, exactly as for the headless argvs.
        let tui = tui();
        for refused in NEVER_COMPOSED {
            assert!(
                !tui.iter().any(|token| token == refused),
                "{refused} must never be composed, and is in {tui:?}"
            );
        }
    }
}
