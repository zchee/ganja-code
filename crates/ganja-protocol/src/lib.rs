//! The wire protocol frontends speak, version 1.
//!
//! Its own crate because it is the one thing every side of the app needs and
//! the only thing some of them need: rendering a transcript, asserting on an
//! event, or later driving a session from the far end of a socket takes none of
//! the engine. The dependency list is that boundary made visible, and it is
//! `serde` and the value type a tool call's arguments arrive as.
//!
//! Every type here is serde-serializable so that the same values can later
//! cross a socket unchanged, and so that a stored session is these values
//! written out verbatim. The model follows upstream's `session/message-v2.ts`:
//! messages carry ordered parts, parts carry a type tag beside their id, and
//! ids sort in creation order.
//!
//! Text parts came first; tool and step parts arrived later as new
//! [`PartBody`] variants, changing nothing already on the wire — which is the
//! pattern every further variant is expected to follow.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// Prefix upstream gives message ids, kept so transcripts read the same.
const MESSAGE_PREFIX: &str = "msg";

/// Prefix upstream gives part ids.
const PART_PREFIX: &str = "prt";

/// Prefix for permission request ids.
const PERMISSION_PREFIX: &str = "perm";

/// Prefix for question request ids, matching upstream's `que_`
/// (`packages/schema/src/v1/question.ts`).
const QUESTION_PREFIX: &str = "que";

/// Prefix session ids carry, matching upstream's `ses_` ids.
const SESSION_PREFIX: &str = "ses";

/// Milliseconds since the Unix epoch, saturating rather than failing when the
/// clock is set before 1970.
///
/// Public so that the engine stamps a message it minted itself with the same
/// clock reading the types here carry, rather than a second one of its own.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Mints an identifier that sorts after every identifier minted before it.
///
/// The layout mirrors upstream's ascending ids: a millisecond timestamp
/// followed by a per-process counter, both fixed-width hex, so ids sort
/// lexicographically by creation time and cannot collide inside one process.
/// Ordering across processes is only as good as the clock, which is the same
/// guarantee upstream makes.
///
/// Public so that a stored session's ids are minted by this counter too: two
/// implementations of "sorts after everything before it" is one too many.
pub fn ascending(prefix: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let millis = now();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed) & 0xff_ffff;

    format!("{prefix}_{millis:011x}{sequence:06x}")
}

/// Identifies a [`Message`] within a session.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(String);

impl MessageId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(ascending(MESSAGE_PREFIX))
    }

    /// The id as it travels the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for MessageId {
    /// Adopts a stored id. The prefix is a convention rather than an
    /// invariant: a transcript read back from disk keeps whatever it was
    /// written with.
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Identifies a [`Part`] within a message.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PartId(String);

impl PartId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(ascending(PART_PREFIX))
    }

    /// The id as it travels the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for PartId {
    /// Adopts a stored id; see [`MessageId::from`].
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Identifies one permission request, so a reply can name what it answers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionId(String);

impl PermissionId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(ascending(PERMISSION_PREFIX))
    }

    /// The id as it travels the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for PermissionId {
    /// Adopts a stored id; see [`MessageId::from`].
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Identifies one question request, so a reply or a rejection can name what it
/// answers.
///
/// A type of its own rather than a reused [`PermissionId`], because the two
/// requests are answered by different commands and a frontend holding both
/// dialogs must not be able to send one's id where the other belongs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QuestionId(String);

impl QuestionId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(ascending(QUESTION_PREFIX))
    }

    /// The id as it travels the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for QuestionId {
    /// Adopts a stored id; see [`MessageId::from`].
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Identifies a session: the conversation an [`Event`] belongs to and a
/// stored record belongs under.
///
/// It began life beside the store and moved here when events started naming
/// their session — a wire type has to live with the wire. Behavior is the
/// one it always had: minted ascending like every other id here, transparent
/// on the wire, adopted verbatim from storage.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(ascending(SESSION_PREFIX))
    }

    /// The id as it travels the wire, and as it appears in rows and listings.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SessionId {
    /// Adopts a stored id, whatever it was written with.
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Who produced a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The person at the terminal.
    User,
    /// The model.
    Assistant,
}

/// What a turn spent, as the provider reported it.
///
/// Cost stays out until there is a model table to price tokens against; the
/// counts are what every provider reports and what the engine accumulates per
/// session.
///
/// # The three input counters are disjoint
///
/// [`input_tokens`](Self::input_tokens), [`cache_read_tokens`](Self::cache_read_tokens)
/// and [`cache_write_tokens`](Self::cache_write_tokens) never count the same
/// token twice: what a prompt cost on the way in is their **sum**, and each is
/// billed at its own rate, a cache read costing a fraction of fresh input.
/// [`reasoning_tokens`](Self::reasoning_tokens) is the one exception and the
/// only nesting here — it is a *subset* of [`output_tokens`](Self::output_tokens),
/// which both providers already bill whole, so pricing it again would
/// double-charge the thinking. `catalog::cost` and the frontend's session
/// totals both read the counters that way.
///
/// **Normalizing to that shape is the provider's job**, because the vendors do
/// not agree. Anthropic's Messages API reports its cache counts beside
/// `input_tokens` and outside it, so its mapping is a copy. OpenAI's
/// `prompt_tokens` is the whole prompt *including* the cached part, so its
/// mapping subtracts `prompt_tokens_details.cached_tokens` before filling
/// `input_tokens` in. A provider that hands its raw numbers straight through
/// makes every consumer of this type over-report — silently, and worst on the
/// heavily cached sessions where the counts matter most.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Fresh input tokens: what the request cost that the cache did not serve.
    ///
    /// Disjoint from both cache counters — see the type's own documentation.
    pub input_tokens: u64,
    /// Tokens the reply cost, thinking included.
    pub output_tokens: u64,
    /// Output tokens the model spent thinking, where it reports them apart.
    ///
    /// A subset of [`output_tokens`](Self::output_tokens) rather than a count
    /// beside it, which is why nothing prices it separately.
    pub reasoning_tokens: u64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written into the provider's prompt cache.
    pub cache_write_tokens: u64,
}

/// The `type` prefix reserved for [`PartBody::Reasoning`] and every later
/// variant of it.
///
/// Public because the contract needs one owner and a reader is on the other
/// side of a crate boundary: a decoder that meets a part it cannot understand
/// asks this whether the record it is holding was request-affecting state, and
/// a literal spelled a second time in that decoder is the contract drifting.
pub const REASONING_TAG: &str = "reasoning";

/// The kinds of content a [`Part`] can carry.
///
/// The tag travels as a `type` field beside the part's id, which is the shape
/// upstream's parts have, so a stored transcript can gain variants without
/// moving anything already on the wire.
///
/// `Eq` stops at [`PartBody::Tool`]: tool arguments are arbitrary JSON, and
/// `serde_json::Value` holds floats, so everything containing a part compares
/// with `PartialEq` only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PartBody {
    /// Plain text, streamed in fragments.
    Text {
        /// Everything accumulated so far.
        text: String,
    },
    /// One tool call, from the model asking for it through its result.
    Tool {
        /// Correlates the provider's call with its result on the next request.
        call_id: String,
        /// Tool being called, by registry id.
        tool: String,
        /// Where the call currently stands.
        state: ToolState,
    },
    /// A file the user attached to their message with an `@` mention.
    ///
    /// The part carries a **reference and nothing else**: the content is read
    /// when a request is built, never when the mention is made, so a file the
    /// user edits between attaching it and sending reaches the model as it is
    /// now rather than as it was. That is upstream's shape — its file part
    /// carries a `file://` URL the server resolves at send time — and it is
    /// also why a mention is not a read: nothing here records the file in
    /// `ganja-tool`'s `FileTimes`, so `edit` still refuses a file the model
    /// itself has not opened.
    File {
        /// Where the file is, relative to the project root.
        path: String,
        /// What kind of file it is, upstream's `mime`. `text/plain` for
        /// everything this build attaches.
        mime: String,
    },
    /// The turn began another model request. Tool results make a turn span
    /// several requests, and each one opens with this marker.
    StepStart,
    /// A model request finished, and what it spent.
    StepFinish {
        /// What this request cost, as the provider reported it.
        usage: Usage,
    },
    /// The model's own thinking, as the provider sealed it, kept so the next
    /// request can hand it back.
    ///
    /// **Nothing here reads it, and nothing may.** A reasoning model that is
    /// asked to keep no state of its own (`store: false`) is handed its
    /// previous thinking as an opaque blob the client returns verbatim; the
    /// blob is the provider's, and this part is the envelope it travels in.
    /// Nothing renders it — there is no reasoning *text* part in this build —
    /// so a frontend meeting one has nothing to draw.
    ///
    /// # The opacity contract, and what a build that cannot read one must do
    ///
    /// This is the first part whose absence changes **what the next request
    /// carries** rather than what a transcript looks like. Every other variant
    /// is renderable content or bookkeeping: losing one costs a line on a
    /// screen. Losing this one silently costs the model the record of its own
    /// reasoning while the calls that reasoning produced stay in the request —
    /// a conversation that looks whole and is not.
    ///
    /// So the tag is part of the contract: **a later variant of this part
    /// keeps the `reasoning` prefix in its `type`**, because that prefix is the
    /// one thing a build too old to decode the record can still recognize. A
    /// reader that cannot decode a `reasoning*` part is required to keep the
    /// rest of the message and put a stateless one of these in its place
    /// (`encrypted: None`), so the loss is recorded where the next request is
    /// built instead of vanishing. `ganja-core`'s storage is that reader.
    Reasoning {
        /// Which provider minted it, spelled as that provider's own id.
        ///
        /// Sealed state means only the wire that sealed it can open it, so the
        /// blob is handed back to that wire and to no other. A session that
        /// changes vendors mid-conversation is the case this exists for.
        provider: String,
        /// The provider's own identifier for the reasoning item.
        ///
        /// Named `item` rather than `id` because [`Part`] flattens this body
        /// beside its own `id`, and two `id` keys in one object is a record
        /// that does not round-trip.
        item: String,
        /// The sealed state itself, absent when this build does not hold it.
        ///
        /// Two situations produce [`None`], and they mean the same thing to
        /// whoever builds the next request — *there was reasoning here and it
        /// cannot be replayed*: an item the provider streamed without state,
        /// and a stored record a reader could not decode. Neither may be
        /// reconstructed, and neither may be sent: a reasoning item without
        /// state is what the provider rejects.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
    },
    /// What one step of the turn changed on disk, and the snapshot it changed
    /// it from. Appended after the step's [`PartBody::StepFinish`] when the
    /// tools it ran moved any file, and absent entirely from a step that
    /// changed nothing.
    ///
    /// This is the whole of what `/undo` consumes: checking `files` out of
    /// `hash` is undoing the step. Nothing renders it — it is bookkeeping the
    /// transcript carries so that a session reopened tomorrow can still be
    /// undone.
    Patch {
        /// The snapshot taken **before** the step, which the files are
        /// restored from.
        hash: String,
        /// What the step changed, relative to the project root.
        ///
        /// Upstream stores these absolute; a stored transcript that named
        /// somebody's home directory would stop working the moment the
        /// checkout moved (deviation: patch-files-are-project-relative), and
        /// every other path on this wire is already relative to the root.
        files: Vec<String>,
    },
}

/// Where a tool call stands, mirroring upstream's `pending → running →
/// completed | error` lifecycle.
///
/// Timestamps are milliseconds since the Unix epoch, like every other time on
/// the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolState {
    /// The model is still streaming the call's arguments.
    Pending,
    /// The call is executing.
    Running {
        /// The parsed arguments it runs with.
        input: serde_json::Value,
        /// What the call has produced so far, for a frontend to render while
        /// it runs: a shell command's output as it arrives, a subagent's
        /// progress as it works. Upstream republishes the same field on a
        /// running part (`session/prompt.ts`, `shellImpl`).
        ///
        /// Null — and absent from the wire — for a call that reports nothing
        /// until it finishes, which is every builtin tool the model calls, so
        /// the bytes of an ordinary running part are what they always were.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        metadata: serde_json::Value,
        /// When execution began.
        started: u64,
    },
    /// The call finished and its output is what the model sees next.
    Completed {
        /// The arguments it ran with.
        input: serde_json::Value,
        /// What the tool returned, as the model sees it.
        output: String,
        /// One-line description of what ran, for rendering.
        title: String,
        /// Structured extras a frontend may render richer than text.
        metadata: serde_json::Value,
        /// When execution began.
        started: u64,
        /// When execution finished.
        completed: u64,
    },
    /// The call failed, or was refused, and the message is what the model
    /// sees next.
    Error {
        /// The arguments it was asked to run with.
        input: serde_json::Value,
        /// What went wrong, as the model sees it.
        error: String,
        /// When execution began.
        started: u64,
        /// When it failed.
        completed: u64,
    },
}

/// One piece of a message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Part {
    /// Identifies the part so that [`Event::PartDelta`] and
    /// [`Event::PartUpdated`] can address it.
    pub id: PartId,
    /// What the part carries.
    #[serde(flatten)]
    pub body: PartBody,
}

impl Part {
    /// Builds a text part with a fresh id.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            id: PartId::ascending(),
            body: PartBody::Text { text: text.into() },
        }
    }

    /// Builds a tool part with a fresh id, opening in
    /// [`ToolState::Pending`].
    #[must_use]
    pub fn tool(call_id: impl Into<String>, tool: impl Into<String>) -> Self {
        Self {
            id: PartId::ascending(),
            body: PartBody::Tool {
                call_id: call_id.into(),
                tool: tool.into(),
                state: ToolState::Pending,
            },
        }
    }

    /// Builds a file part with a fresh id, for a file the user mentioned.
    #[must_use]
    pub fn file(path: impl Into<String>, mime: impl Into<String>) -> Self {
        Self {
            id: PartId::ascending(),
            body: PartBody::File {
                path: path.into(),
                mime: mime.into(),
            },
        }
    }

    /// Builds a reasoning part with a fresh id, carrying the state `provider`
    /// sealed under its own `item` id.
    ///
    /// Takes the state rather than defaulting it: the one caller that has none
    /// to give is a reader recording a loss, and spelling `None` there is the
    /// point.
    #[must_use]
    pub fn reasoning(
        provider: impl Into<String>,
        item: impl Into<String>,
        encrypted: Option<String>,
    ) -> Self {
        Self {
            id: PartId::ascending(),
            body: PartBody::Reasoning {
                provider: provider.into(),
                item: item.into(),
                encrypted,
            },
        }
    }

    /// The text this part carries, or [`None`] when it carries something else.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match &self.body {
            PartBody::Text { text } => Some(text),
            PartBody::Tool { .. }
            | PartBody::File { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
            | PartBody::Reasoning { .. } => None,
        }
    }

    /// The text this part carries, for accumulating streamed fragments.
    pub fn as_text_mut(&mut self) -> Option<&mut String> {
        match &mut self.body {
            PartBody::Text { text } => Some(text),
            PartBody::Tool { .. }
            | PartBody::File { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
            | PartBody::Reasoning { .. } => None,
        }
    }
}

/// When a message happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTime {
    /// Milliseconds since the Unix epoch.
    pub created: u64,
    /// Set once nothing more will be added to the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
}

/// One turn's worth of content from one side of the conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Identifies the message; ids sort in creation order.
    pub id: MessageId,
    /// Who produced it.
    pub role: Role,
    /// Its content, in the order it arrived.
    pub parts: Vec<Part>,
    /// When it started and, once known, finished.
    pub time: MessageTime,
    /// Model that produced it, absent on user messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What the turn spent, absent until the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl Message {
    /// Builds the complete user message carrying `text`.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        let created = now();

        Self {
            id: MessageId::ascending(),
            role: Role::User,
            parts: vec![Part::text(text)],
            time: MessageTime {
                created,
                completed: Some(created),
            },
            model: None,
            usage: None,
        }
    }

    /// Opens an assistant message that `model` streams parts into.
    #[must_use]
    pub fn assistant(model: impl Into<String>) -> Self {
        Self {
            id: MessageId::ascending(),
            role: Role::Assistant,
            parts: Vec::new(),
            time: MessageTime {
                created: now(),
                completed: None,
            },
            model: Some(model.into()),
            usage: None,
        }
    }

    /// Marks the message complete, returning when that happened so the caller
    /// can report the same instant it recorded.
    pub fn complete(&mut self) -> u64 {
        let completed = now();
        self.time.completed = Some(completed);

        completed
    }

    /// Whether the message carries anything worth keeping. An assistant turn
    /// that failed before its first fragment does not, and neither do bare
    /// step markers: what counts is text the model said or a tool it called.
    ///
    /// A [`PartBody::Patch`] does not count either, for the step markers'
    /// reason: it records what a tool did rather than being something the
    /// model said, and a message holding one always holds the tool call that
    /// earned it.
    ///
    /// Neither does a [`PartBody::Reasoning`], and that one is load-bearing: a
    /// turn that died after sealing its thinking and before saying anything
    /// would otherwise enter the history as a message whose only content is
    /// state the model cannot be shown, and every later request would carry
    /// it.
    #[must_use]
    pub fn has_content(&self) -> bool {
        self.parts.iter().any(|part| match &part.body {
            PartBody::Text { text } => !text.is_empty(),
            PartBody::Tool { .. } | PartBody::File { .. } => true,
            PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
            | PartBody::Reasoning { .. } => false,
        })
    }
}

/// One file the user attached to a prompt, by `@`-mentioning it.
///
/// A path and nothing more: what the file *says* is read when the request is
/// built, not when the mention is made. See [`PartBody::File`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    /// Where the file is, relative to the project root.
    pub path: String,
}

/// How much of a session is currently reverted, as a frontend sees it.
///
/// The engine keeps more than this — the snapshot a redo restores from — but
/// a frontend's whole job here is to hide a range and say which files moved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertInfo {
    /// The user message the revert stopped at. It, and everything after it,
    /// is hidden: still in the transcript, still restorable, and no longer
    /// part of what the next request will carry.
    pub message_id: MessageId,
    /// Files the revert put back, relative to the project root. Empty — and
    /// absent from the wire — for a turn that changed none, which is a revert
    /// of the conversation and not of the checkout.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

/// A request from a frontend to the engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Starts a turn that answers `text`.
    SendPrompt {
        /// What the user typed.
        text: String,
        /// Files the user attached to it. Absent from the wire when there are
        /// none, so a frontend that knows nothing about mentions sends — and a
        /// stored command replays — exactly the bytes it always did.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mentions: Vec<Mention>,
    },
    /// Stops the turn that is streaming; a no-op when the engine is idle.
    /// When the turn is waiting on a permission, cancelling also refuses it.
    CancelTurn,
    /// Answers an [`Event::PermissionRequested`]. Ignored when nothing with
    /// this id is waiting, which is what a reply racing a cancel becomes.
    ReplyPermission {
        /// The request being answered.
        id: PermissionId,
        /// The user's decision.
        reply: PermissionReply,
    },
    /// Answers an [`Event::QuestionAsked`]. Ignored when nothing with this id
    /// is waiting, which is what a reply racing a cancel becomes.
    ///
    /// Named for the event it answers, as [`Command::ReplyPermission`] is.
    ReplyQuestion {
        /// The request being answered.
        id: QuestionId,
        /// One answer per question, in the order they were asked. A question
        /// the person skipped is answered with an empty list rather than
        /// omitted: the model reads the answers positionally.
        answers: Vec<QuestionAnswer>,
    },
    /// Dismisses an [`Event::QuestionAsked`] without answering it. Ignored
    /// when nothing with this id is waiting.
    ///
    /// A command of its own rather than a `ReplyQuestion` carrying a refusing
    /// value, because upstream's rejection is its own event with its own
    /// payload and the tool call fails rather than completing — see
    /// [`Event::QuestionRejected`].
    RejectQuestion {
        /// The request being dismissed.
        id: QuestionId,
    },
    /// Runs the rest of the session as a different agent: its prompt, its
    /// rules, and the model it prefers. Takes effect at the **next** turn —
    /// upstream re-resolves the agent per prompt and so does this — and is
    /// refused while one is streaming.
    SwitchAgent {
        /// The agent's name, which must be one the engine's registry holds
        /// and must not be a subagent.
        name: String,
    },
    /// Asks the rest of the session's requests of a different model. Same
    /// provider only: the provider instance is fixed when the engine is built.
    /// Takes effect at the next turn, and is refused while one is streaming.
    SwitchModel {
        /// The model's id, as the provider spells it.
        model: String,
    },
    /// Runs `command` in the shell on the user's behalf and puts both the
    /// command and its output in the transcript, where the next model request
    /// will read them. Upstream's `!` passthrough.
    ///
    /// **Not gated by permissions.** This is the person at the terminal typing
    /// a command, not the model asking to run one, and upstream runs it without
    /// a dialog for exactly that reason.
    RunShell {
        /// What to run, verbatim.
        command: String,
    },
    /// Expands the named command's template and starts a turn with the result,
    /// which is an ordinary prompt by the time the model sees it.
    RunCommand {
        /// The command's name, without the leading slash.
        name: String,
        /// Everything the user typed after the name, unparsed: the template
        /// decides what `$1` and `$ARGUMENTS` make of it.
        args: String,
    },
    /// Summarizes the conversation so far and continues from the summary. The
    /// manual half of the compaction that otherwise happens on its own when a
    /// session fills its model's context window.
    Compact,
    /// Forgets the session the engine is on, so the next prompt starts a fresh
    /// one. Nothing stored is touched: the old session is still there to
    /// resume.
    NewSession,
    /// Puts the files back to what they were before the last prompt, and hides
    /// that prompt and everything after it. Sending it again walks one prompt
    /// further back.
    ///
    /// No payload: the engine owns the transcript, so it is the engine that
    /// works out which message to stop at. Upstream's client computes the same
    /// message and names it in the request; collapsing that into the engine
    /// changes nothing observable and keeps a frontend from having to hold the
    /// history to undo.
    ///
    /// Refused while a turn is streaming. Upstream aborts the turn and then
    /// reverts; here the person at the terminal cancels first, so an undo is
    /// never something that stopped work they were watching (**D119**).
    ///
    /// **Nothing is deleted.** The hidden messages stay in the transcript, and
    /// stay restorable by [`Command::Redo`], until the next prompt or shell
    /// command makes the choice permanent.
    Undo,
    /// Steps one prompt forward through what [`Command::Undo`] hid, restoring
    /// the files that prompt's turn changed. Past the newest one, the whole
    /// working tree goes back to what it was before the first undo.
    Redo,
}

/// What the user decided about one permission request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionReply {
    /// Run this one call.
    Once,
    /// Run it, and stop asking about calls like it in this project.
    Always,
    /// Refuse the call. The model is told, and decides what to do next.
    Reject,
}

/// One choice a question offers.
///
/// Spec: upstream `packages/schema/src/v1/question.ts`, `Option`. The two
/// descriptions are the model's own words, written for the person who has to
/// pick, which is why both are required rather than the label alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    /// Display text (1-5 words, concise).
    pub label: String,
    /// Explanation of choice.
    pub description: String,
}

/// One question as a frontend is asked to put it.
///
/// Spec: upstream's `Info` — its `Prompt` plus [`custom`](Self::custom). The
/// model sends a `Prompt`; the engine is what turns each one into an `Info`,
/// so the extra field is the engine's to fill and never the model's to claim.
///
/// **This shape is declared twice**, here and as `ganja-tool`'s `question`
/// argument struct, because `ganja-tool` may not depend on this crate. The two
/// are held together by a round-trip pin in `ganja-core` — the one crate that
/// sees both — which destructures exhaustively in both directions and compares
/// serde representations, so a field, a rename or a default attribute that
/// moves on one side reddens rather than drifting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionInfo {
    /// Complete question.
    pub question: String,
    /// Very short label (max 30 chars).
    pub header: String,
    /// Available choices.
    pub options: Vec<QuestionOption>,
    /// Allow selecting more than one choice. Absent from the wire when unset,
    /// which is upstream's optional field and reads as "one choice".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    /// Allow typing a custom answer (default: true). Absent from the wire when
    /// unset — and unset is what the model's own `Prompt` always produces,
    /// since upstream's `Prompt` does not carry this field at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

/// Where in the transcript a question was asked from.
///
/// Spec: upstream's `QuestionTool`. [`Option`]al on the request for the same
/// reason it is optional upstream: asking is a service, and a caller that is
/// not a tool call has no part to name. Every question this build asks comes
/// from the `question` tool, so today it is always present — carried anyway,
/// because a frontend that correlates the dialog with the call's part should
/// not have to learn that fact from the absence of an alternative.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionSource {
    /// The assistant message holding the call's part.
    pub message_id: MessageId,
    /// The provider's id for the call that asked.
    pub call_id: String,
}

/// One question's answer: the labels the person picked, or typed.
///
/// Upstream's `Answer` is an array of strings even for a single-choice
/// question — the same shape answers both, and `multiple` is about what the
/// dialog permits rather than about what an answer looks like.
pub type QuestionAnswer = Vec<String>;

/// Something the engine observed, delivered to every subscriber in the order
/// it happened, under the policy each subscriber chose: a lossless subscriber
/// is waited for and misses nothing, and a droppable one is evicted whole —
/// its stream ends with an error value — rather than shown a silent gap.
///
/// The stream is the whole truth: a frontend that applies every event in order
/// holds the same transcript the engine does, which is what lets a session be
/// rebuilt from disk and later served over a socket. Names follow upstream's
/// bus —
/// [`Event::PartDelta`] is `message.part.delta`, [`Event::PartStarted`] and
/// [`Event::PartUpdated`] are `message.part.updated`.
///
/// Every variant names the session it happened in, so a consumer fed more
/// than one conversation can attribute each event instead of guessing. The
/// field is spelled `session_id` where upstream writes `sessionID`: this
/// protocol has one casing rule, and a camel-case island would be the only
/// one on the wire (deviation: session-id-is-snake-case).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A message entered the transcript. A user message arrives complete; an
    /// assistant message arrives empty and grows through the events below.
    MessageStarted {
        /// Session this happened in.
        session_id: SessionId,
        /// The message as it stands.
        message: Message,
    },
    /// A part was appended to a message that is still streaming. It arrives
    /// before any delta addresses it, so a frontend always knows a part's id
    /// and kind before its content.
    PartStarted {
        /// Session this happened in.
        session_id: SessionId,
        /// Message the part belongs to.
        message_id: MessageId,
        /// The part, empty of content.
        part: Part,
    },
    /// Content was appended to a part.
    PartDelta {
        /// Session this happened in.
        session_id: SessionId,
        /// Message the part belongs to.
        message_id: MessageId,
        /// Part to append to.
        part_id: PartId,
        /// What to append.
        delta: String,
    },
    /// A part changed as a whole — a tool call moved through its lifecycle —
    /// and this is its new value, replacing the part with the same id.
    PartUpdated {
        /// Session this happened in.
        session_id: SessionId,
        /// Message the part belongs to.
        message_id: MessageId,
        /// The part as it now stands.
        part: Part,
    },
    /// A tool call wants to run and the engine is waiting on the answer. The
    /// turn holds until [`Command::ReplyPermission`] names this id, or the
    /// turn is cancelled.
    PermissionRequested {
        /// Session this happened in. A dialog that crossed from a subagent
        /// carries the delegating session's id, because that is the
        /// conversation whose turn is waiting on the answer.
        session_id: SessionId,
        /// Names this request, for the reply.
        id: PermissionId,
        /// The tool call waiting on the decision.
        call_id: String,
        /// Tool asking to run, by registry id.
        tool: String,
        /// One line saying what would run, fit for a dialog.
        title: String,
        /// The arguments it would run with.
        args: serde_json::Value,
        /// Directories outside the project this call would work in, which an
        /// "always" answer would also remember. Empty — and absent from the
        /// wire — for a call that stays inside the checkout, which is what
        /// keeps the common case's bytes what they always were.
        ///
        /// A dialog that showed the command and not these would be asking
        /// about something narrower than what the answer covers.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        directories: Vec<String>,
    },
    /// A permission request was answered — by the user, or by a cancel
    /// refusing it — so a frontend can retire the dialog.
    PermissionReplied {
        /// Session this happened in, addressed as its request was.
        session_id: SessionId,
        /// The request that was answered.
        id: PermissionId,
        /// What was decided.
        reply: PermissionReply,
    },
    /// The model asked the person something and the turn is waiting on the
    /// answer. It holds until [`Command::ReplyQuestion`] or
    /// [`Command::RejectQuestion`] names this id, or the turn is cancelled.
    ///
    /// Spec: upstream's `question.asked` (`packages/schema/src/v1/question.ts`),
    /// whose payload is the whole request.
    ///
    /// Unlike a permission, this is not a gate on a call that would otherwise
    /// run: the call *is* the asking. That is why the request carries no
    /// tool arguments and why refusing it has an event of its own below.
    QuestionAsked {
        /// Session this happened in. A question that crossed from a subagent
        /// carries the delegating session's id, because that is the
        /// conversation whose turn is waiting on the answer.
        session_id: SessionId,
        /// Names this request, for the reply.
        id: QuestionId,
        /// What is being asked, in order. Every one of them is answered, or
        /// none is: a reply carries an answer per question.
        questions: Vec<QuestionInfo>,
        /// Where in the transcript the question was asked from. Absent from
        /// the wire when nothing named a call — see [`QuestionSource`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<QuestionSource>,
    },
    /// A question was answered, so a frontend can retire the dialog.
    ///
    /// Spec: upstream's `question.replied`, whose payload is
    /// `{sessionID, requestID, answers}`.
    QuestionReplied {
        /// Session this happened in, addressed as its request was.
        session_id: SessionId,
        /// The request that was answered.
        id: QuestionId,
        /// One answer per question, in the order they were asked.
        answers: Vec<QuestionAnswer>,
    },
    /// A question was dismissed — by the user, or by a cancel refusing it —
    /// so a frontend can retire the dialog.
    ///
    /// Spec: upstream's `question.rejected`, which carries **its own payload**
    /// (`{sessionID, requestID}`) rather than being a `replied` with a
    /// refusing value. That is the shape ported: a dismissal is not an answer,
    /// and a consumer that treats it as one would have to invent answers
    /// nobody gave.
    QuestionRejected {
        /// Session this happened in, addressed as its request was.
        session_id: SessionId,
        /// The request that was dismissed.
        id: QuestionId,
    },
    /// How much of the transcript is currently reverted, and what the editor
    /// should hold.
    ///
    /// Sent when [`Command::Undo`] or [`Command::Redo`] moves the anchor, when
    /// a revert is cleared, and when a resumed session turns out to have been
    /// left in one — a frontend that starts fresh learns the hidden range from
    /// this event and from nowhere else.
    ///
    /// A `revert` of [`None`] means the hidden range is over. It arrives in
    /// exactly two situations, and a frontend tells them apart by what it
    /// asked for: a [`Command::Redo`] that stepped past the newest reverted
    /// message — where those messages are still in the transcript and come
    /// back — and the prompt or shell command that follows an undo, where they
    /// have just been deleted and the frontend drops them. The engine draws no
    /// distinction because the frontend's own command already did.
    RevertChanged {
        /// Session this happened in.
        session_id: SessionId,
        /// Where the revert stands, or [`None`] when there is none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revert: Option<RevertInfo>,
        /// The prompt the revert took back, for the editor to offer again.
        /// [`None`] when there is nothing to offer: a cleared revert, and a
        /// resumed session — reopening a conversation is not the moment to put
        /// words in somebody's editor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// The engine adopted an agent after construction, whether at the
    /// plan-approval turn boundary or through [`Command::SwitchAgent`].
    /// Emitting from both paths keeps a frontend's indicator independent of
    /// whether that frontend issued the command itself. The model travels with
    /// the agent because adoption may also adopt its preferred model; the build
    /// agent has no preference, so the boundary emission reports the exact
    /// model that remains active.
    AgentChanged {
        /// Session this happened in.
        session_id: SessionId,
        /// The agent now active for this session, by name (e.g. "build", "plan").
        agent: String,
        /// The model now active for this session.
        model: String,
    },
    /// The turn ended and the engine is idle again. It is the last event of a
    /// turn, whatever went wrong during it, save for the one
    /// [`Event::AgentChanged`] a plan approval may announce immediately after
    /// it.
    MessageFinished {
        /// Session this happened in.
        session_id: SessionId,
        /// The assistant message that just closed.
        message_id: MessageId,
        /// Why it ended.
        reason: FinishReason,
        /// What the turn spent, when the provider reported it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// What went wrong, set exactly when `reason` is
        /// [`FinishReason::Failed`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Milliseconds since the Unix epoch, matching the message's
        /// [`MessageTime::completed`].
        completed: u64,
    },
}

impl Event {
    /// The session this event belongs to, whatever its variant.
    ///
    /// The field lives on every variant rather than on a wrapper, so the wire
    /// shape stays flat; this is the one place that knows every spelling of
    /// that fact, so a consumer that filters or groups by session does not
    /// have to write the whole match itself.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        match self {
            Event::MessageStarted { session_id, .. }
            | Event::PartStarted { session_id, .. }
            | Event::PartDelta { session_id, .. }
            | Event::PartUpdated { session_id, .. }
            | Event::PermissionRequested { session_id, .. }
            | Event::PermissionReplied { session_id, .. }
            | Event::QuestionAsked { session_id, .. }
            | Event::QuestionReplied { session_id, .. }
            | Event::QuestionRejected { session_id, .. }
            | Event::RevertChanged { session_id, .. }
            | Event::AgentChanged { session_id, .. }
            | Event::MessageFinished { session_id, .. } => session_id,
        }
    }
}

/// Why a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The provider ran out of text.
    Completed,
    /// A [`Command::CancelTurn`] arrived before the provider finished.
    Cancelled,
    /// The provider could not answer.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::{
        Command, Event, FinishReason, Mention, Message, MessageId, MessageTime, Part, PartBody,
        PartId, PermissionId, PermissionReply, QuestionId, QuestionInfo, QuestionOption,
        QuestionSource, REASONING_TAG, RevertInfo, Role, SessionId, ToolState, Usage,
    };

    /// The session every pinned event happens in.
    fn pinned_session() -> SessionId {
        SessionId::from("ses_1".to_owned())
    }

    /// Builds a completed tool part with pinned ids and times, the richest
    /// shape a part takes on the wire.
    fn pinned_tool_part() -> Part {
        Part {
            id: PartId::from("prt_1".to_owned()),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "read".to_owned(),
                state: ToolState::Completed {
                    input: serde_json::json!({"path": "a.rs"}),
                    output: "fn main() {}".to_owned(),
                    title: "a.rs".to_owned(),
                    metadata: serde_json::json!({}),
                    started: 7,
                    completed: 9,
                },
            },
        }
    }

    /// Builds a message with pinned ids and times so a test can assert on the
    /// exact bytes that reach the wire.
    fn pinned_message() -> Message {
        Message {
            id: MessageId::from("msg_1".to_owned()),
            role: Role::User,
            parts: vec![Part {
                id: PartId::from("prt_1".to_owned()),
                body: PartBody::Text {
                    text: "hi".to_owned(),
                },
            }],
            time: MessageTime {
                created: 7,
                completed: Some(7),
            },
            model: None,
            usage: None,
        }
    }

    #[test]
    fn ids_sort_in_creation_order_and_carry_their_prefix() {
        let ids: Vec<MessageId> = (0..64).map(|_| MessageId::ascending()).collect();

        assert!(
            ids.iter().all(|id| id.as_str().starts_with("msg_")),
            "ids should carry the upstream prefix: {ids:?}"
        );
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "ids should sort in creation order: {ids:?}"
        );

        let parts: Vec<PartId> = (0..64).map(|_| PartId::ascending()).collect();
        assert!(parts.iter().all(|id| id.as_str().starts_with("prt_")));
        assert!(parts.windows(2).all(|pair| pair[0] < pair[1]));

        let sessions: Vec<SessionId> = (0..64).map(|_| SessionId::ascending()).collect();
        assert!(sessions.iter().all(|id| id.as_str().starts_with("ses_")));
        assert!(sessions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_user_message_is_born_complete_and_a_reply_is_not() {
        let user = Message::user("hi");

        assert_eq!(user.role, Role::User);
        assert_eq!(user.time.completed, Some(user.time.created));
        assert_eq!(user.parts.first().and_then(Part::as_text), Some("hi"));
        assert!(user.model.is_none());

        let mut assistant = Message::assistant("canned");

        assert_eq!(assistant.role, Role::Assistant);
        assert!(assistant.parts.is_empty());
        assert!(assistant.time.completed.is_none());
        assert_eq!(assistant.model.as_deref(), Some("canned"));

        let completed = assistant.complete();
        assert_eq!(assistant.time.completed, Some(completed));
    }

    #[test]
    fn an_empty_part_is_not_content_but_a_filled_one_is() {
        let mut message = Message::assistant("canned");
        assert!(!message.has_content());

        message.parts.push(Part::text(""));
        assert!(!message.has_content());

        if let Some(text) = message.parts.last_mut().and_then(Part::as_text_mut) {
            text.push_str("hello");
        }
        assert!(message.has_content());
    }

    #[test]
    fn commands_round_trip_through_json() {
        let cases = [
            Command::SendPrompt {
                text: "hello".to_owned(),
                mentions: Vec::new(),
            },
            Command::SendPrompt {
                text: "what does this do".to_owned(),
                mentions: vec![Mention {
                    path: "src/main.rs".to_owned(),
                }],
            },
            Command::CancelTurn,
            Command::ReplyPermission {
                id: PermissionId::from("perm_1".to_owned()),
                reply: PermissionReply::Always,
            },
            Command::SwitchAgent {
                name: "plan".to_owned(),
            },
            Command::SwitchModel {
                model: "claude-haiku-4.5".to_owned(),
            },
            Command::RunShell {
                command: "git status".to_owned(),
            },
            Command::RunCommand {
                name: "init".to_owned(),
                args: "focus on the tests".to_owned(),
            },
            Command::Compact,
            Command::NewSession,
            Command::Undo,
            Command::Redo,
        ];

        for command in cases {
            let encoded = serde_json::to_string(&command).expect("a command serializes");
            let decoded: Command = serde_json::from_str(&encoded).expect("a command deserializes");
            assert_eq!(decoded, command, "round trip changed {encoded}");
        }
    }

    /// The cases cover every variant, so the loop's accessor assertion is
    /// also the proof that [`Event::session_id`] reads every one of them.
    #[test]
    fn events_round_trip_through_json() {
        let message = pinned_message();
        let cases = [
            Event::MessageStarted {
                session_id: pinned_session(),
                message: message.clone(),
            },
            Event::PartStarted {
                session_id: pinned_session(),
                message_id: message.id.clone(),
                part: Part::text(""),
            },
            Event::PartDelta {
                session_id: pinned_session(),
                message_id: message.id.clone(),
                part_id: PartId::from("prt_1".to_owned()),
                delta: "hi".to_owned(),
            },
            Event::MessageFinished {
                session_id: pinned_session(),
                message_id: message.id.clone(),
                reason: FinishReason::Failed,
                usage: Some(Usage {
                    input_tokens: 3,
                    output_tokens: 4,
                    reasoning_tokens: 5,
                    cache_read_tokens: 6,
                    cache_write_tokens: 7,
                }),
                error: Some("no credentials".to_owned()),
                completed: 9,
            },
            Event::PartUpdated {
                session_id: pinned_session(),
                message_id: message.id.clone(),
                part: pinned_tool_part(),
            },
            Event::PermissionRequested {
                session_id: pinned_session(),
                id: PermissionId::from("perm_1".to_owned()),
                call_id: "call_1".to_owned(),
                tool: "shell".to_owned(),
                title: "cargo test".to_owned(),
                args: serde_json::json!({"command": "cargo test"}),
                directories: vec!["/tmp/scratch".to_owned()],
            },
            Event::PermissionReplied {
                session_id: pinned_session(),
                id: PermissionId::from("perm_1".to_owned()),
                reply: PermissionReply::Reject,
            },
            Event::RevertChanged {
                session_id: pinned_session(),
                revert: Some(RevertInfo {
                    message_id: message.id.clone(),
                    files: vec!["src/main.rs".to_owned()],
                }),
                prompt: Some("rename the thing".to_owned()),
            },
            Event::RevertChanged {
                session_id: pinned_session(),
                revert: None,
                prompt: None,
            },
            Event::AgentChanged {
                session_id: pinned_session(),
                agent: "build".to_owned(),
                model: "claude-sonnet-4-5".to_owned(),
            },
        ];

        for event in cases {
            assert_eq!(
                event.session_id(),
                &pinned_session(),
                "the accessor reads the session off {event:?}"
            );

            let encoded = serde_json::to_string(&event).expect("an event serializes");
            let decoded: Event = serde_json::from_str(&encoded).expect("an event deserializes");
            assert_eq!(decoded, event, "round trip changed {encoded}");
        }
    }

    #[test]
    fn agent_changed_carries_the_session_the_agent_and_the_model() {
        let event = Event::AgentChanged {
            session_id: pinned_session(),
            agent: "build".to_owned(),
            model: "claude-sonnet-4-5".to_owned(),
        };

        assert_eq!(event.session_id(), &pinned_session());
        match event {
            Event::AgentChanged { agent, model, .. } => {
                assert_eq!(agent, "build");
                assert_eq!(model, "claude-sonnet-4-5");
            }
            other => panic!("expected an agent change, got {other:?}"),
        }
    }

    /// Pins the bytes of every variant. A change here is a protocol change: it
    /// invalidates stored sessions and anything speaking the protocol over a
    /// socket, so it has to be a deliberate edit rather than a side effect of
    /// renaming a field.
    #[test]
    fn the_wire_format_is_stable() {
        let cases = [
            // A prompt with nothing attached, whose bytes are exactly what
            // they were before mentions existed.
            (
                serde_json::to_string(&Command::SendPrompt {
                    text: "hi".to_owned(),
                    mentions: Vec::new(),
                }),
                r#"{"type":"send_prompt","text":"hi"}"#,
            ),
            (
                serde_json::to_string(&Command::SendPrompt {
                    text: "hi".to_owned(),
                    mentions: vec![
                        Mention {
                            path: "src/main.rs".to_owned(),
                        },
                        Mention {
                            path: "README.md".to_owned(),
                        },
                    ],
                }),
                r#"{"type":"send_prompt","text":"hi","mentions":[{"path":"src/main.rs"},{"path":"README.md"}]}"#,
            ),
            (
                serde_json::to_string(&Command::CancelTurn),
                r#"{"type":"cancel_turn"}"#,
            ),
            (
                serde_json::to_string(&Command::RunShell {
                    command: "git status".to_owned(),
                }),
                r#"{"type":"run_shell","command":"git status"}"#,
            ),
            (
                serde_json::to_string(&Command::RunCommand {
                    name: "init".to_owned(),
                    args: String::new(),
                }),
                r#"{"type":"run_command","name":"init","args":""}"#,
            ),
            (
                serde_json::to_string(&Command::Compact),
                r#"{"type":"compact"}"#,
            ),
            (
                serde_json::to_string(&Command::NewSession),
                r#"{"type":"new_session"}"#,
            ),
            (serde_json::to_string(&Command::Undo), r#"{"type":"undo"}"#),
            (serde_json::to_string(&Command::Redo), r#"{"type":"redo"}"#),
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::Patch {
                        hash: "4b825dc".to_owned(),
                        files: vec!["src/main.rs".to_owned()],
                    },
                }),
                r#"{"id":"prt_1","type":"patch","hash":"4b825dc","files":["src/main.rs"]}"#,
            ),
            (
                serde_json::to_string(&Event::RevertChanged {
                    session_id: pinned_session(),
                    revert: Some(RevertInfo {
                        message_id: MessageId::from("msg_1".to_owned()),
                        files: vec!["src/main.rs".to_owned()],
                    }),
                    prompt: Some("rename it".to_owned()),
                }),
                r#"{"type":"revert_changed","session_id":"ses_1","revert":{"message_id":"msg_1","files":["src/main.rs"]},"prompt":"rename it"}"#,
            ),
            (
                serde_json::to_string(&Event::RevertChanged {
                    session_id: pinned_session(),
                    revert: None,
                    prompt: None,
                }),
                r#"{"type":"revert_changed","session_id":"ses_1"}"#,
            ),
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::File {
                        path: "src/main.rs".to_owned(),
                        mime: "text/plain".to_owned(),
                    },
                }),
                r#"{"id":"prt_1","type":"file","path":"src/main.rs","mime":"text/plain"}"#,
            ),
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::Reasoning {
                        provider: "openai".to_owned(),
                        item: "rs_1".to_owned(),
                        encrypted: Some("sealed".to_owned()),
                    },
                }),
                // The part's own id and the provider's item id are two keys,
                // which is why the second is not called `id`.
                r#"{"id":"prt_1","type":"reasoning","provider":"openai","item":"rs_1","encrypted":"sealed"}"#,
            ),
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::Reasoning {
                        provider: "openai".to_owned(),
                        item: "rs_1".to_owned(),
                        encrypted: None,
                    },
                }),
                // State this build does not hold is written as its absence
                // rather than as a null, so the record says "there is none"
                // in the one spelling a reader also accepts from a build that
                // never wrote the field.
                r#"{"id":"prt_1","type":"reasoning","provider":"openai","item":"rs_1"}"#,
            ),
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::Text {
                        text: "hi".to_owned(),
                    },
                }),
                r#"{"id":"prt_1","type":"text","text":"hi"}"#,
            ),
            (
                serde_json::to_string(&Event::MessageStarted {
                    session_id: pinned_session(),
                    message: pinned_message(),
                }),
                r#"{"type":"message_started","session_id":"ses_1","message":{"id":"msg_1","role":"user","parts":[{"id":"prt_1","type":"text","text":"hi"}],"time":{"created":7,"completed":7}}}"#,
            ),
            (
                serde_json::to_string(&Event::PartStarted {
                    session_id: pinned_session(),
                    message_id: MessageId::from("msg_1".to_owned()),
                    part: Part {
                        id: PartId::from("prt_1".to_owned()),
                        body: PartBody::Text {
                            text: String::new(),
                        },
                    },
                }),
                r#"{"type":"part_started","session_id":"ses_1","message_id":"msg_1","part":{"id":"prt_1","type":"text","text":""}}"#,
            ),
            (
                serde_json::to_string(&Event::PartDelta {
                    session_id: pinned_session(),
                    message_id: MessageId::from("msg_1".to_owned()),
                    part_id: PartId::from("prt_1".to_owned()),
                    delta: "hi".to_owned(),
                }),
                r#"{"type":"part_delta","session_id":"ses_1","message_id":"msg_1","part_id":"prt_1","delta":"hi"}"#,
            ),
            (
                serde_json::to_string(&Event::MessageFinished {
                    session_id: pinned_session(),
                    message_id: MessageId::from("msg_1".to_owned()),
                    reason: FinishReason::Completed,
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 2,
                        ..Usage::default()
                    }),
                    error: None,
                    completed: 9,
                }),
                r#"{"type":"message_finished","session_id":"ses_1","message_id":"msg_1","reason":"completed","usage":{"input_tokens":1,"output_tokens":2,"reasoning_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0},"completed":9}"#,
            ),
            (
                serde_json::to_string(&Event::MessageFinished {
                    session_id: pinned_session(),
                    message_id: MessageId::from("msg_1".to_owned()),
                    reason: FinishReason::Cancelled,
                    usage: None,
                    error: None,
                    completed: 9,
                }),
                r#"{"type":"message_finished","session_id":"ses_1","message_id":"msg_1","reason":"cancelled","completed":9}"#,
            ),
            (
                serde_json::to_string(&Event::MessageFinished {
                    session_id: pinned_session(),
                    message_id: MessageId::from("msg_1".to_owned()),
                    reason: FinishReason::Failed,
                    usage: None,
                    error: Some("no credentials".to_owned()),
                    completed: 9,
                }),
                r#"{"type":"message_finished","session_id":"ses_1","message_id":"msg_1","reason":"failed","error":"no credentials","completed":9}"#,
            ),
            (
                serde_json::to_string(&Command::ReplyPermission {
                    id: PermissionId::from("perm_1".to_owned()),
                    reply: PermissionReply::Once,
                }),
                r#"{"type":"reply_permission","id":"perm_1","reply":"once"}"#,
            ),
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::Tool {
                        call_id: "call_1".to_owned(),
                        tool: "read".to_owned(),
                        state: ToolState::Pending,
                    },
                }),
                r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"read","state":{"status":"pending"}}"#,
            ),
            (
                serde_json::to_string(&pinned_tool_part()),
                r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"read","state":{"status":"completed","input":{"path":"a.rs"},"output":"fn main() {}","title":"a.rs","metadata":{},"started":7,"completed":9}}"#,
            ),
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::Tool {
                        call_id: "call_1".to_owned(),
                        tool: "shell".to_owned(),
                        state: ToolState::Error {
                            input: serde_json::json!({"command": "rm -rf /"}),
                            error: "refused".to_owned(),
                            started: 7,
                            completed: 9,
                        },
                    },
                }),
                r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"shell","state":{"status":"error","input":{"command":"rm -rf /"},"error":"refused","started":7,"completed":9}}"#,
            ),
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::StepStart,
                }),
                r#"{"id":"prt_1","type":"step_start"}"#,
            ),
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::StepFinish {
                        usage: Usage {
                            input_tokens: 1,
                            output_tokens: 2,
                            ..Usage::default()
                        },
                    },
                }),
                r#"{"id":"prt_1","type":"step_finish","usage":{"input_tokens":1,"output_tokens":2,"reasoning_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0}}"#,
            ),
            (
                serde_json::to_string(&Event::PartUpdated {
                    session_id: pinned_session(),
                    message_id: MessageId::from("msg_1".to_owned()),
                    part: Part {
                        id: PartId::from("prt_1".to_owned()),
                        body: PartBody::Tool {
                            call_id: "call_1".to_owned(),
                            tool: "shell".to_owned(),
                            state: ToolState::Running {
                                input: serde_json::json!({"command": "ls"}),
                                metadata: serde_json::Value::Null,
                                started: 7,
                            },
                        },
                    },
                }),
                r#"{"type":"part_updated","session_id":"ses_1","message_id":"msg_1","part":{"id":"prt_1","type":"tool","call_id":"call_1","tool":"shell","state":{"status":"running","input":{"command":"ls"},"started":7}}}"#,
            ),
            // A call that reports progress while it runs — the `!` passthrough
            // streaming its output, or a task tool watching a subagent.
            (
                serde_json::to_string(&Part {
                    id: PartId::from("prt_1".to_owned()),
                    body: PartBody::Tool {
                        call_id: "call_1".to_owned(),
                        tool: "bash".to_owned(),
                        state: ToolState::Running {
                            input: serde_json::json!({"command": "ls"}),
                            metadata: serde_json::json!({"output": "a.rs\n"}),
                            started: 7,
                        },
                    },
                }),
                r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"bash","state":{"status":"running","input":{"command":"ls"},"metadata":{"output":"a.rs\n"},"started":7}}"#,
            ),
            // A call that stays inside the checkout, whose `directories` are
            // absent from the wire exactly as they were when the field
            // arrived.
            (
                serde_json::to_string(&Event::PermissionRequested {
                    session_id: pinned_session(),
                    id: PermissionId::from("perm_1".to_owned()),
                    call_id: "call_1".to_owned(),
                    tool: "shell".to_owned(),
                    title: "ls".to_owned(),
                    args: serde_json::json!({"command": "ls"}),
                    directories: Vec::new(),
                }),
                r#"{"type":"permission_requested","session_id":"ses_1","id":"perm_1","call_id":"call_1","tool":"shell","title":"ls","args":{"command":"ls"}}"#,
            ),
            (
                serde_json::to_string(&Event::PermissionRequested {
                    session_id: pinned_session(),
                    id: PermissionId::from("perm_1".to_owned()),
                    call_id: "call_1".to_owned(),
                    tool: "shell".to_owned(),
                    title: "ls /etc".to_owned(),
                    args: serde_json::json!({"command": "ls /etc"}),
                    directories: vec!["/etc".to_owned(), "/tmp/scratch".to_owned()],
                }),
                r#"{"type":"permission_requested","session_id":"ses_1","id":"perm_1","call_id":"call_1","tool":"shell","title":"ls /etc","args":{"command":"ls /etc"},"directories":["/etc","/tmp/scratch"]}"#,
            ),
            (
                serde_json::to_string(&Event::PermissionReplied {
                    session_id: pinned_session(),
                    id: PermissionId::from("perm_1".to_owned()),
                    reply: PermissionReply::Reject,
                }),
                r#"{"type":"permission_replied","session_id":"ses_1","id":"perm_1","reply":"reject"}"#,
            ),
            // A question with everything absent that may be absent: the two
            // optional flags the model did not send, and no call to name.
            (
                serde_json::to_string(&Event::QuestionAsked {
                    session_id: pinned_session(),
                    id: QuestionId::from("que_1".to_owned()),
                    questions: vec![QuestionInfo {
                        question: "Which database?".to_owned(),
                        header: "Database".to_owned(),
                        options: vec![QuestionOption {
                            label: "Postgres".to_owned(),
                            description: "Relational, what the rest of the fleet runs".to_owned(),
                        }],
                        multiple: None,
                        custom: None,
                    }],
                    source: None,
                }),
                r#"{"type":"question_asked","session_id":"ses_1","id":"que_1","questions":[{"question":"Which database?","header":"Database","options":[{"label":"Postgres","description":"Relational, what the rest of the fleet runs"}]}]}"#,
            ),
            // And the same question with every optional field carried.
            (
                serde_json::to_string(&Event::QuestionAsked {
                    session_id: pinned_session(),
                    id: QuestionId::from("que_1".to_owned()),
                    questions: vec![QuestionInfo {
                        question: "Which database?".to_owned(),
                        header: "Database".to_owned(),
                        options: Vec::new(),
                        multiple: Some(true),
                        custom: Some(false),
                    }],
                    source: Some(QuestionSource {
                        message_id: MessageId::from("msg_1".to_owned()),
                        call_id: "call_1".to_owned(),
                    }),
                }),
                r#"{"type":"question_asked","session_id":"ses_1","id":"que_1","questions":[{"question":"Which database?","header":"Database","options":[],"multiple":true,"custom":false}],"source":{"message_id":"msg_1","call_id":"call_1"}}"#,
            ),
            (
                serde_json::to_string(&Event::QuestionReplied {
                    session_id: pinned_session(),
                    id: QuestionId::from("que_1".to_owned()),
                    answers: vec![vec!["Postgres".to_owned()], Vec::new()],
                }),
                r#"{"type":"question_replied","session_id":"ses_1","id":"que_1","answers":[["Postgres"],[]]}"#,
            ),
            // Rejection carries its own payload — no `answers` field, because
            // a dismissal is not an answer.
            (
                serde_json::to_string(&Event::QuestionRejected {
                    session_id: pinned_session(),
                    id: QuestionId::from("que_1".to_owned()),
                }),
                r#"{"type":"question_rejected","session_id":"ses_1","id":"que_1"}"#,
            ),
            // Agent adoption announces both values because choosing an agent
            // may also choose its preferred model.
            (
                serde_json::to_string(&Event::AgentChanged {
                    session_id: pinned_session(),
                    agent: "build".to_owned(),
                    model: "claude-sonnet-4-5".to_owned(),
                }),
                r#"{"type":"agent_changed","session_id":"ses_1","agent":"build","model":"claude-sonnet-4-5"}"#,
            ),
            (
                serde_json::to_string(&Command::ReplyQuestion {
                    id: QuestionId::from("que_1".to_owned()),
                    answers: vec![vec!["Postgres".to_owned()]],
                }),
                r#"{"type":"reply_question","id":"que_1","answers":[["Postgres"]]}"#,
            ),
            (
                serde_json::to_string(&Command::RejectQuestion {
                    id: QuestionId::from("que_1".to_owned()),
                }),
                r#"{"type":"reject_question","id":"que_1"}"#,
            ),
            (
                serde_json::to_string(&Command::SwitchAgent {
                    name: "plan".to_owned(),
                }),
                r#"{"type":"switch_agent","name":"plan"}"#,
            ),
            (
                serde_json::to_string(&Command::SwitchModel {
                    model: "claude-haiku-4.5".to_owned(),
                }),
                r#"{"type":"switch_model","model":"claude-haiku-4.5"}"#,
            ),
        ];

        for (encoded, expected) in cases {
            assert_eq!(encoded.expect("the value serializes"), expected);
        }
    }

    /// The shape every frontend written before mentions existed sends. It has
    /// to keep parsing, and it has to keep meaning "no files attached" rather
    /// than failing on a field that is not there.
    #[test]
    fn a_prompt_written_before_mentions_existed_still_parses() {
        let decoded: Command = serde_json::from_str(r#"{"type":"send_prompt","text":"hi"}"#)
            .expect("the original shape parses");

        assert_eq!(
            decoded,
            Command::SendPrompt {
                text: "hi".to_owned(),
                mentions: Vec::new(),
            }
        );

        let decoded: Command = serde_json::from_str(
            r#"{"type":"send_prompt","text":"hi","mentions":[{"path":"a.rs"}]}"#,
        )
        .expect("the new shape parses");
        assert_eq!(
            decoded,
            Command::SendPrompt {
                text: "hi".to_owned(),
                mentions: vec![Mention {
                    path: "a.rs".to_owned()
                }],
            }
        );
    }

    /// A running part written before it could report progress still parses,
    /// and reads back as one that reports nothing.
    #[test]
    fn a_running_part_without_metadata_still_parses() {
        let decoded: Part = serde_json::from_str(
            r#"{"id":"prt_1","type":"tool","call_id":"call_1","tool":"bash","state":{"status":"running","input":{},"started":7}}"#,
        )
        .expect("the original shape parses");

        let PartBody::Tool {
            state: ToolState::Running { metadata, .. },
            ..
        } = decoded.body
        else {
            panic!("the fixture is a running tool part");
        };
        assert!(metadata.is_null());
    }

    /// A stored assistant message keeps its model and usage; a stored user
    /// message keeps neither, and reading one back does not invent them.
    #[test]
    fn an_assistant_message_round_trips_with_its_model_and_usage() {
        let mut message = Message::assistant("canned");
        message.parts.push(Part::text("hello"));
        message.usage = Some(Usage {
            input_tokens: 1,
            output_tokens: 2,
            ..Usage::default()
        });
        message.complete();

        let encoded = serde_json::to_string(&message).expect("a message serializes");
        let decoded: Message = serde_json::from_str(&encoded).expect("a message deserializes");

        assert_eq!(decoded, message, "round trip changed {encoded}");

        let user = Message::user("hi");
        let encoded = serde_json::to_string(&user).expect("a message serializes");
        assert!(
            !encoded.contains("model") && !encoded.contains("usage"),
            "a user message should carry neither: {encoded}"
        );
    }

    /// The tag is the whole of the downgrade contract: a reader too old to
    /// decode the record recognizes it by this prefix and nothing else, so a
    /// rename would silently turn every future reasoning record into a part
    /// that vanishes without a trace.
    #[test]
    fn a_reasoning_part_is_tagged_with_the_prefix_a_later_variant_must_keep() {
        let part = Part::reasoning("openai", "rs_1", Some("sealed".to_owned()));
        let encoded = serde_json::to_value(&part).expect("a part serializes");

        assert_eq!(encoded["type"], serde_json::json!(REASONING_TAG));
        assert!(
            encoded["type"]
                .as_str()
                .is_some_and(|tag| tag.starts_with(REASONING_TAG)),
            "the reserved prefix is what a decoder that cannot read the rest \
             still matches on: {encoded}"
        );
    }

    /// The two shapes a reader has to accept, and the one it must never
    /// invent: a record whose state field was never written reads back as
    /// state this build does not hold, not as an empty blob it could send.
    #[test]
    fn a_reasoning_record_without_state_reads_back_as_state_nobody_holds() {
        let decoded: Part = serde_json::from_str(
            r#"{"id":"prt_1","type":"reasoning","provider":"openai","item":"rs_1"}"#,
        )
        .expect("a record written without the field parses");

        assert_eq!(
            decoded.body,
            PartBody::Reasoning {
                provider: "openai".to_owned(),
                item: "rs_1".to_owned(),
                encrypted: None,
            }
        );

        let held: Part = serde_json::from_str(
            r#"{"id":"prt_1","type":"reasoning","provider":"openai","item":"rs_1","encrypted":"sealed"}"#,
        )
        .expect("a record written with the field parses");
        assert_eq!(
            held.body,
            PartBody::Reasoning {
                provider: "openai".to_owned(),
                item: "rs_1".to_owned(),
                encrypted: Some("sealed".to_owned()),
            }
        );
    }

    /// Sealed state is not something the model said. A turn that produced only
    /// this is a turn that produced nothing, and letting it into the history
    /// would carry an unreplayable blob into every later request.
    #[test]
    fn a_message_holding_only_sealed_reasoning_has_no_content() {
        let mut message = Message::assistant("gpt");
        message
            .parts
            .push(Part::reasoning("openai", "rs_1", Some("sealed".to_owned())));

        assert!(!message.has_content());
        assert!(
            message.parts[0].as_text().is_none(),
            "nothing renders sealed state, so nothing may read text out of it"
        );

        message.parts.push(Part::text("and here is the answer"));
        assert!(message.has_content());
    }
}
