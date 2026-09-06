//! `ChatRequest.messages` → cursor's conversation state (**D553**).
//!
//! Spec: the behaviour of the `opencode-cursor` plugin's proxy at
//! `a37a6ba9a6d6d8d176bb68248f59240271f46767` (MIT, see
//! `THIRD_PARTY_NOTICES.md`) — `proxy.ts:636-673`, the walk from messages to
//! history entries and the action; `:720-803`, the blobs (the root entries
//! the server builds its prompt from, the turns beside them, and sha256 as
//! the id of each); `:840-856`, the action's two arms; and `:1341-1351`, the
//! deterministic id layout. **Behaviour only: no code is copied**, and every
//! shape below is written against `cursor.proto`'s messages rather than
//! translated from TypeScript.
//!
//! **Why blobs, and why every request rebuilds them.** On cursor's wire a
//! conversation's past does not ride the run request; the request *names* it
//! as content-addressed blob ids and the server *fetches* each over the kv
//! half of the stream. The server builds the model prompt from
//! `root_prompt_messages_json` — one JSON entry per system prompt and per
//! history message — and not from `turns[]` (`proxy.ts:744-750`), which is
//! why both are composed here and the root is what a divergence in the
//! model's memory would trace to. Nothing is held across turns: the same
//! bytes hash to the same id, so a rebuild on every request is what keeps
//! whatever the server caches by id warm, and it is the reference's own
//! ground — server-echoed checkpoints blank out historical user entries, so
//! the checkpoint's own root cannot be reused (`:747-750`). Every fresh Run's
//! `Duplex` is seeded with [`Composed::blobs`] before it opens, and
//! `request::kv_answer` serves the server's gets from it.
//!
//! **The cut between `entries` and `blobs`** is where two encodings
//! share a shape. The walk — which message contributes what, where history
//! ends and the action begins, which parts a wire never sends — is the part
//! that is tested against the transcript's shapes, and it produces a
//! `History` any encoder could read. This module's encoder is the
//! reference's blob composition; a later inline encoder over the same
//! `History` (the bundle's `conversation_history = 7`, unmeasured today) is
//! a second `blobs`, not a second walk.
//!
//! **What diverges from the reference, each on purpose.** The assistant
//! entry carries the tool calls it made as clamped `[Tool Call]` text, where
//! the reference keeps assistant text and tool *results* and loses the call
//! (`:657-661`, `:793-800`): on a recovered turn a model shown a result it
//! never asked for is the model most likely to issue the call again and run
//! its side effect twice, and the call beside the result is the mitigation.
//! History user-blob ids are [`derived`] from each message's own
//! `MessageId`, where the reference seeds them from turn index and text
//! (`:786`): the same v4 shape on the wire, from a seed that survives
//! compaction, which an index shift would re-mint. The **action's** id stays
//! random, exactly as the reference mints it (`:849`). And the system head
//! appears only beside history: a request with nothing before its newest
//! run sends the empty state it always sent, where the reference always
//! sends a head — each request is a measured shape on its own.
//!
//! **Nothing here reaches a log but counts, id prefixes and sizes.** The
//! blobs are conversation state; the two `debug!` lines at the end of
//! `blobs` are what a probe correlates the server's gets against, and they
//! carry no text.

use std::collections::HashMap;
use std::fmt::{self, Write as _};

use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::{ID, proto, request};
use crate::protocol::{Message, MessageId, PartBody, Role, ToolState};
use crate::provider::{ChatRequest, NO_RESULT};

/// The bound on one rendered `[Tool Call]` input, in bytes: 8 KiB.
///
/// An argument larger than this is file content — `write`'s `content`,
/// `edit`'s strings — which is on disk and one `read` away, and on a wire
/// where the request grows with the transcript and nothing compacts it, what
/// compounds is worth bounding. Tool *outputs* are already clamped in the
/// transcript at the tool layer and are rendered whole. The cut is a plain
/// in-memory one, deliberately not `truncate::clamp_bytes`, which spills its
/// overflow to a file and would write one on every request.
pub const CALL_INPUT_LIMIT: usize = 8 * 1024;

/// What [`derived`] hashes ahead of a message id, so a history blob's id is
/// a function of this build's own seed and never collides with an id the
/// reference would mint from the same transcript.
const DERIVATION_SEED: &str = "ganja-cursor-message:";

/// What the run request asks the agent to do, decided by the request's own
/// shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// The newest user turn, inline: the request's last message is the
    /// user's.
    User {
        /// The turn's text — every user message of the run, joined.
        text: String,
    },
    /// Continue from the composed state without a new user message: the
    /// request's last message is the assistant's, which on this wire only a
    /// bridged step can leave (`proxy.ts:664-673`).
    Resume,
}

/// One entry of the history the server builds its prompt from, in the order
/// the transcript holds them.
#[derive(Debug, PartialEq)]
pub(super) enum Entry<'a> {
    /// The system prompt, at the head — present only beside history.
    System(&'a str),
    /// A user message: its text parts, joined.
    User {
        /// The transcript's own id, which the user blob's `message_id` is
        /// derived from.
        id: &'a MessageId,
        text: String,
    },
    /// An assistant reply: its text, and the calls it made.
    Assistant { text: String, calls: Vec<Call<'a>> },
    /// What one call answered, following the assistant entry that made it.
    Result { text: &'a str, is_error: bool },
}

/// One tool call an assistant entry made, as the entry renders it.
#[derive(Debug, PartialEq)]
pub(super) struct Call<'a> {
    pub(super) tool: &'a str,
    /// The arguments, or [`None`] for a call the model never finished
    /// streaming — rendered as the empty object, the honest spelling of "the
    /// model was still saying".
    pub(super) input: Option<&'a serde_json::Value>,
}

/// The walk's result: the history entries and the action, cut at the
/// boundary [`entries`] describes.
#[derive(Debug, PartialEq)]
pub(super) struct History<'a> {
    pub(super) entries: Vec<Entry<'a>>,
    pub(super) action: Action,
}

/// The composed state: what the run request names, and the bytes it names.
pub struct Composed {
    /// `root_prompt_messages_json`: the blob ids of the root entries, in
    /// order.
    pub root: Vec<Vec<u8>>,
    /// `turns`: the blob ids of the [`proto::ConversationTurn`]s, in order.
    pub turns: Vec<Vec<u8>>,
    /// Every blob the two lists name, and every blob those blobs name — a
    /// turn's user message and steps — keyed by id, ready to seed a Run's
    /// store.
    pub blobs: HashMap<Vec<u8>, Vec<u8>>,
    pub action: Action,
    /// [`derived`] from the first message's id; absent on a request with no
    /// messages, which nothing keys to.
    pub conversation_id: Option<String>,
    /// How many `[Tool Call]` inputs were cut at [`CALL_INPUT_LIMIT`].
    pub clamped_calls: usize,
}

impl fmt::Debug for Composed {
    /// Counts and the action only: the blobs are conversation state, and a
    /// `Debug` that rendered them would put a transcript on whatever line
    /// printed it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Composed")
            .field("root", &self.root.len())
            .field("turns", &self.turns.len())
            .field("blobs", &self.blobs.len())
            .field("action", &self.action.spelled())
            .field("conversation_id", &self.conversation_id)
            .field("clamped_calls", &self.clamped_calls)
            .finish()
    }
}

impl Action {
    /// The one word a log line names the action by.
    fn spelled(&self) -> &'static str {
        match self {
            Self::User { .. } => "user",
            Self::Resume => "resume",
        }
    }
}

/// The state `request` composes to: `entries`, then `blobs`.
#[must_use]
pub fn compose(request: &ChatRequest) -> Composed {
    blobs(&entries(request), request)
}

/// Walks `request.messages` into the history entries and the action.
///
/// **The boundary.** The newest user turn is [`request::newest_user_run`]'s
/// slice — the run of user messages back to the reply before it, clamped at
/// `turn_start`. When the request's last message is the user's, that run is
/// the action and everything before it is history; otherwise — the last
/// message is the assistant's, which on this wire only a bridged step can
/// leave — the action is [`Action::Resume`] and the whole list is history
/// (`proxy.ts:664-673`). A request with no messages at all, which the engine
/// never builds, is the empty user action it always was.
///
/// **Per message.** A user message is one entry of its text parts joined; an
/// assistant message is one entry of its text and its calls, followed by one
/// result entry per call — `Completed` as the output, `Error` as the error,
/// `Pending | Running` as [`NO_RESULT`] with `is_error`, the rule every
/// other wire renders a dead turn's call by. A message that contributes no
/// text and no call contributes no entry, so a message carrying only parts a
/// wire never sends (below) leaves no trace in the state.
///
/// **The system head** is pushed first when at least one message entry
/// follows it — never on its own, so a request whose history contributes
/// nothing composes the empty state.
///
/// **Parts a wire never sends**, named exhaustively in [`pieces`] with no
/// wildcard: the anthropic encoder's exclusions — display-only thinking,
/// server-tool parts, another wire's sealed reasoning, peer envelopes, step
/// markers, patches — plus `File`, both arms. Anthropic sends an attachment
/// as an image or document block; a root entry has no measured JSON shape
/// for one, so the arm is a recorded limitation rather than a guess.
pub(super) fn entries(request: &ChatRequest) -> History<'_> {
    let messages = request.messages.as_slice();

    let (history, action) = match request::newest_user_run(request) {
        // The newest user message is the last message: the run is the action.
        Some(run) if *run.end() + 1 == messages.len() => {
            (&messages[..*run.start()], Action::User { text: request::newest_user_text(request) })
        }
        // A user message exists but the assistant's follows it: a resume.
        Some(_) => (messages, Action::Resume),
        // No user message at all — nothing, which the engine never builds,
        // or a list of the assistant's alone.
        None if messages.is_empty() => (messages, Action::User { text: String::new() }),
        None => (messages, Action::Resume),
    };

    let mut entries = Vec::new();
    for message in history {
        let mut texts = Vec::new();
        let mut calls = Vec::new();
        for piece in pieces(message) {
            match piece {
                Piece::Text(text) => texts.push(text),
                Piece::Call { tool, state } => calls.push((tool, state)),
            }
        }
        let text = texts.join("\n\n");

        match message.role {
            // A user message carries text; a tool part on one is not a shape
            // the engine builds, and the calls are the assistant's to render.
            Role::User => {
                if !text.is_empty() {
                    entries.push(Entry::User { id: &message.id, text });
                }
            }
            Role::Assistant => {
                let made: Vec<Call<'_>> =
                    calls.iter().map(|(tool, state)| Call { tool, input: input(state) }).collect();
                if !text.is_empty() || !made.is_empty() {
                    entries.push(Entry::Assistant { text, calls: made });
                }
                for (_, state) in calls {
                    let (text, is_error) = result(state);
                    entries.push(Entry::Result { text, is_error });
                }
            }
        }
    }

    if !entries.is_empty()
        && let Some(system) = request.system.as_deref().filter(|system| !system.is_empty())
    {
        entries.insert(0, Entry::System(system));
    }

    History { entries, action }
}

/// What one part contributes to the walk.
pub(super) enum Piece<'a> {
    Text(&'a str),
    Call {
        /// The tool's registry name, and the state the call is in.
        tool: &'a str,
        state: &'a ToolState,
    },
}

/// The parts of `message` this wire sends, in order.
///
/// Every variant is named and there is no wildcard, on purpose: this is the
/// one place on this wire where a new `PartBody` would otherwise compile
/// silently into "not sent", and a part the model ought to read is not
/// something to discover from a bug report. The set excluded is
/// `anthropic.rs`'s, plus `File` — the module doc says why for each.
pub(super) fn pieces(message: &Message) -> impl Iterator<Item = Piece<'_>> {
    message.parts.iter().filter_map(|part| match &part.body {
        PartBody::Text { text } => Some(Piece::Text(text)),
        PartBody::Tool { tool, state, .. } => Some(Piece::Call { tool, state }),
        // `File` with content is an attachment anthropic sends as a block; one
        // without is a reference resolved into text before a request is
        // built. Neither has a measured shape in a root entry, so both arms
        // contribute nothing here.
        PartBody::File { .. }
        // `StepStart` is the boundary a step was cut at and `StepFinish` is
        // its bill; a `Patch` is a working-tree note; neither is content.
        | PartBody::StepStart
        | PartBody::StepFinish { .. }
        | PartBody::Patch { .. }
        // Readable thinking is rendered, never replayed; a `Reasoning` part
        // is another wire's sealed state; a `ServerTool` ran on another
        // vendor's side and is display-only.
        | PartBody::ReasoningText { .. }
        | PartBody::Reasoning { .. }
        | PartBody::ServerTool { .. }
        // A peer's words are rendered into the user turn at request assembly
        // (D495); a wire never encodes one as a message of its own.
        | PartBody::Peer { .. } => None,
    })
}

/// The text parts of `message`, in order — what a user turn is made of.
pub(super) fn texts(message: &Message) -> impl Iterator<Item = &str> {
    pieces(message).filter_map(|piece| match piece {
        Piece::Text(text) => Some(text),
        Piece::Call { .. } => None,
    })
}

/// The arguments a call ran with, or [`None`] when it never got that far.
fn input(state: &ToolState) -> Option<&serde_json::Value> {
    match state {
        ToolState::Pending { input } => input.as_ref(),
        ToolState::Running { input, .. }
        | ToolState::Completed { input, .. }
        | ToolState::Error { input, .. } => Some(input),
    }
}

/// What a call produced and whether that counts as a failure — the rule
/// `anthropic.rs` renders the same states by.
fn result(state: &ToolState) -> (&str, bool) {
    match state {
        ToolState::Completed { output, .. } => (output, false),
        ToolState::Error { error, .. } => (error, true),
        // See [`NO_RESULT`]: the turn that made this call died before the tool
        // answered, and the model reads the hole as a failed call rather than
        // a call that vanished.
        ToolState::Pending { .. } | ToolState::Running { .. } => (NO_RESULT, true),
    }
}

/// A root entry for a system prompt: the reference's `{role, content}` with
/// the prompt as a bare string (`proxy.ts:742`).
#[derive(Serialize)]
struct SystemEntry<'a> {
    role: &'static str,
    content: &'a str,
}

/// A root entry for a user or assistant message: the reference's `{role,
/// content: [{type: "text", text}]}` (`proxy.ts:753-761`).
///
/// Field order is declaration order, and it is load-bearing: the blob's id
/// is the hash of these bytes, and two spellings of one entry would be two
/// blobs.
#[derive(Serialize)]
struct TextEntry<'a> {
    role: &'static str,
    content: [TextBlock<'a>; 1],
}

#[derive(Serialize)]
struct TextBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

impl<'a> TextEntry<'a> {
    fn new(role: &'static str, text: &'a str) -> Self {
        Self { role, content: [TextBlock { kind: "text", text }] }
    }
}

/// The blobs under composition: the store a Run is seeded with, and the
/// byte count the log line reports.
#[derive(Default)]
struct Store {
    blobs: HashMap<Vec<u8>, Vec<u8>>,
    bytes: usize,
}

impl Store {
    /// Stores `bytes` under their sha256 and returns the id — the raw
    /// 32-byte digest, which is what the wire carries (`proxy.ts:726-733`).
    /// The same bytes stored twice are one blob.
    fn put(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        let id = Sha256::digest(&bytes).to_vec();
        if !self.blobs.contains_key(&id) {
            self.bytes += bytes.len();
            self.blobs.insert(id.clone(), bytes);
        }

        id
    }

    fn json<T: Serialize>(&mut self, entry: &T) -> Vec<u8> {
        self.put(serde_json::to_vec(entry).expect("a root entry is strings in structs"))
    }

    fn proto<M: buffa::Message>(&mut self, message: &M) -> Vec<u8> {
        self.put(message.encode_to_vec())
    }
}

/// A turn under composition: the user blob's id, and its steps' so far.
struct Turn {
    user: Vec<u8>,
    steps: Vec<Vec<u8>>,
}

impl Turn {
    fn into_proto(self) -> proto::ConversationTurn {
        proto::ConversationTurn {
            agent_conversation_turn: buffa::MessageField::some(proto::AgentConversationTurn {
                user_message: Some(self.user),
                steps: self.steps,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

/// Encodes `history` into the state the run request names.
///
/// **`root_prompt_messages_json`**, one JSON blob per entry: the system head
/// as `{role: "system", content}`; a user entry as a `user` text entry; an
/// assistant entry as an `assistant` text entry whose text is the reply's
/// text followed by one `[Tool Call] <tool> <input>` paragraph per call, the
/// input as compact JSON cut at [`CALL_INPUT_LIMIT`]; a result entry as a
/// `user` text entry reading `[Tool Result]\n<output>` or
/// `[Tool Result (error)]\n<error>`.
///
/// **`turns`**, one [`proto::ConversationTurn`] per user entry: its
/// `user_message` is the blob of a [`proto::UserMessage`] carrying the text
/// and a [`derived`] id, and its `steps` are one
/// [`proto::ConversationStep`] per following assistant or result entry, each
/// an `assistant_message` carrying the same text the root entry got. An
/// assistant entry before any user entry — a compaction summary at
/// `messages[0]` — is carried in the root and dropped from the turns, the
/// reference's rule, because the server reads the root.
///
/// Every blob's id is the sha256 of its bytes; the root entries are
/// `serde_json`'s compact spelling of a struct with a fixed field order and
/// the rest are `buffa`'s encoding, so a rebuild of the same request yields
/// the same ids. `conversation_id` is [`derived`] from the first message's
/// id: stable within an uncompacted session, re-rooted by compaction (the
/// summary becomes `messages[0]`) and per-invocation on the title and
/// summary one-shots — harmless every time, since nothing is read back under
/// it and every request carries a complete state.
pub(super) fn blobs(history: &History<'_>, request: &ChatRequest) -> Composed {
    let mut store = Store::default();
    let mut root = Vec::with_capacity(history.entries.len());
    let mut turns = Vec::new();
    let mut current: Option<Turn> = None;
    let mut clamped_calls = 0;

    for entry in &history.entries {
        match entry {
            Entry::System(content) => {
                root.push(store.json(&SystemEntry { role: "system", content }));
            }
            Entry::User { id, text } => {
                root.push(store.json(&TextEntry::new("user", text)));
                if let Some(turn) = current.take() {
                    turns.push(store.proto(&turn.into_proto()));
                }
                let user = store.proto(
                    &proto::UserMessage::default().with_text(text).with_message_id(derived(id)),
                );
                current = Some(Turn { user, steps: Vec::new() });
            }
            Entry::Assistant { text, calls } => {
                let text = assistant_text(text, calls, &mut clamped_calls);
                root.push(store.json(&TextEntry::new("assistant", &text)));
                if let Some(turn) = &mut current {
                    turn.steps.push(store.proto(&step(&text)));
                }
            }
            Entry::Result { text, is_error } => {
                let text = result_text(text, *is_error);
                root.push(store.json(&TextEntry::new("user", &text)));
                if let Some(turn) = &mut current {
                    turn.steps.push(store.proto(&step(&text)));
                }
            }
        }
    }
    if let Some(turn) = current {
        turns.push(store.proto(&turn.into_proto()));
    }

    let conversation_id = request.messages.first().map(|message| derived(&message.id));

    tracing::debug!(
        provider = ID,
        entries = history.entries.len(),
        turns = turns.len(),
        blobs = store.blobs.len(),
        bytes = store.bytes,
        clamped_calls,
        action = history.action.spelled(),
        "composed the conversation state"
    );
    if tracing::enabled!(tracing::Level::DEBUG) {
        // Prefixes and sizes, sorted so two logs of one composition read the
        // same; never a byte of what an id names.
        let mut ids: Vec<String> = store
            .blobs
            .iter()
            .map(|(id, bytes)| format!("{}={}", request::blob_key(id), bytes.len()))
            .collect();
        ids.sort_unstable();
        tracing::debug!(provider = ID, ids = ?ids, "the composed blob ids");
    }

    Composed {
        root,
        turns,
        blobs: store.blobs,
        action: history.action.clone(),
        conversation_id,
        clamped_calls,
    }
}

/// An assistant entry's text: the reply's text, then one paragraph per call,
/// its input clamped and counted.
fn assistant_text(text: &str, calls: &[Call<'_>], clamped_calls: &mut usize) -> String {
    let mut paragraphs = Vec::with_capacity(1 + calls.len());
    if !text.is_empty() {
        paragraphs.push(text.to_owned());
    }
    for call in calls {
        let input = match call.input {
            Some(input) => {
                serde_json::to_string(input).expect("a serde_json::Value always serializes")
            }
            None => "{}".to_owned(),
        };
        let (input, cut) = clamp(input);
        if cut {
            *clamped_calls += 1;
        }
        paragraphs.push(format!("[Tool Call] {} {input}", call.tool));
    }

    paragraphs.join("\n\n")
}

/// `input` cut at [`CALL_INPUT_LIMIT`] on a char boundary, with an elision
/// naming exactly how many bytes were omitted; and whether it was cut.
fn clamp(input: String) -> (String, bool) {
    if input.len() <= CALL_INPUT_LIMIT {
        return (input, false);
    }

    let cut = input.floor_char_boundary(CALL_INPUT_LIMIT);
    let omitted = input.len() - cut;
    let mut clamped = input[..cut].to_owned();
    write!(clamped, "… [+{omitted} bytes]").expect("writing into a String cannot fail");

    (clamped, true)
}

/// A result entry's text, the reference's `[Tool Result]` prefix with the
/// failure marked.
fn result_text(text: &str, is_error: bool) -> String {
    let marker = if is_error { "[Tool Result (error)]" } else { "[Tool Result]" };

    format!("{marker}\n{text}")
}

/// One step of a turn: the text as an assistant message, the only step kind
/// the reference writes.
fn step(text: &str) -> proto::ConversationStep {
    proto::ConversationStep {
        assistant_message: buffa::MessageField::some(
            proto::AssistantMessage::default().with_text(text),
        ),
        ..Default::default()
    }
}

/// A history blob's `message_id`, derived from the transcript's own `id`:
/// the first sixteen bytes of `sha256(seed ‖ id)` rendered as a v4-shaped
/// UUID — the reference's `deterministicUuid` layout (`proxy.ts:1341-1351`)
/// from a different seed.
///
/// Derived rather than minted so the same message composes to the same blob
/// on every request, and from the transcript's id rather than the reference's
/// turn index and text so the id survives compaction, where an index shift
/// re-mints every history id the reference has. Two consumers: each history
/// user blob's `message_id`, and the run request's `conversation_id`, derived
/// from the first message's. The action's own id is never derived — it is
/// `request::fresh_id`'s random one, the reference's shape.
#[must_use]
pub fn derived(id: &MessageId) -> String {
    let digest = Sha256::new().chain_update(DERIVATION_SEED).chain_update(id.as_str()).finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);

    request::render_v4(bytes)
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
