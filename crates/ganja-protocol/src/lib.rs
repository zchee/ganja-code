//! The wire protocol frontends speak, version 1.
//!
//! Its own crate because it is the one thing every side of the app needs and
//! the only thing some of them need: rendering a transcript, asserting on an
//! event, or later driving a session from the far end of a socket takes none of
//! the engine. The dependency list is that boundary made visible, and it is
//! `serde`, the value type a tool call's arguments arrive as, and the one
//! crate that mints an id ([`uuidv7`]).
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

pub mod team;

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
/// The [`team`] types that appear in signatures outside the frame vocabulary:
/// a trust boundary ([`LeadFrame`]), the classifier every
/// messaging path asks ([`Frame`]), the projection a frontend renders
/// ([`TeamView`] and its two halves), and the two [`Event::PeerReceipt`]
/// carries — [`PeerMessageId`] and [`PeerReceiptStatus`] — which live beside
/// the rest of the peer surface in `team.rs` rather than here, for the same
/// reason `Frame` does.
///
/// Everything else — the fifteen frame payloads, the two reserved-set consts,
/// the display cap — stays behind `team::`, because a caller naming one of
/// those is already inside that vocabulary and the qualification says so.
pub use team::{
    Frame, LeadFrame, MemberBackend, MemberView, PeerMessageId, PeerReceiptStatus, TeamView,
};
use uuid::Uuid;

/// Milliseconds since the Unix epoch, saturating rather than failing when the
/// clock is set before 1970.
///
/// Public so that the engine stamps a message it minted itself with the same
/// clock reading the types here carry, rather than a second one of its own.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// Mints an identifier that sorts after every identifier minted before it: a
/// standard lowercase hyphenated UUIDv7, and nothing else — no prefix
/// (**D493**).
///
/// What this replaced was upstream's layout, `<prefix>_<millis hex><counter
/// hex>` off a **process-local** counter starting at zero. Inside one process
/// it was sound; across two it was not, and not merely by bad luck: the first
/// id every process mints ends in six zeroes, so two `ganja` processes reaching
/// engine construction in the same millisecond were *guaranteed* to mint the
/// same session id. A team is exactly that — several processes started
/// together — so the layout had to go rather than be defended
/// (`ganja-code-76w`). UUIDv7 fills what the counter left deterministic with
/// random bits, so a collision now needs the same millisecond *and* the same
/// 41-bit seed *and* the same trailing entropy.
///
/// The prefixes went with it. `ses_`/`msg_`/`prt_`/`perm_`/`que_` are never
/// minted again, because a session id is written into files whose format is
/// somebody else's, where a `ses_` would be a foreign body.
///
/// *Ordering survives the change.* The four hyphens sit at fixed positions in
/// every UUID, so they never discriminate between two of them, and `'0'..='9'`
/// precedes `'a'..='f'` in ASCII exactly as their values order. A lexicographic
/// sort of these strings is therefore still a sort by creation time, and
/// storage's `ORDER BY id` still means what it always meant.
///
/// *Within one millisecond it is not luck either.* Plain UUIDv7's 74 random
/// bits are unordered, and a streaming turn mints many part ids per
/// millisecond — an unordered sequence would scramble a transcript on
/// reassembly. So the mint takes RFC 9562's monotonic-counter method through
/// [`Uuid::now_v7`], whose process-global `ContextV7` reseeds a 42-bit counter
/// on each new millisecond, increments it within one, and carries the timestamp
/// forward when it wraps. That context lives behind a mutex in the crate's own
/// static, which is the synchronization this needs and the reason there is no
/// second one here.
///
/// Public so that a stored session's ids are minted here too: two
/// implementations of "sorts after everything before it" is one too many.
#[must_use]
pub fn uuidv7() -> String {
    Uuid::now_v7().hyphenated().to_string()
}

/// Whether a string is spelled the way [`uuidv7`] spells what it mints.
///
/// Strict, and the strictness is the point: the text must parse as a UUID,
/// carry version 7, and be the thirty-six-character lowercase hyphenated form.
/// The same UUID written braced, as a URN, unhyphenated or in uppercase is
/// refused, because the question this answers is "did a build of this tree mint
/// this id" and not "is this text a UUID somewhere".
///
/// Public because two callers outside this crate ask it: the store, deciding
/// whether the rows it just opened predate this mint and must be set aside, and
/// the tests that pin the mint itself.
#[must_use]
pub fn is_uuidv7(id: &str) -> bool {
    let mut hyphenated = [0_u8; uuid::fmt::Hyphenated::LENGTH];

    Uuid::try_parse(id).is_ok_and(|parsed| {
        parsed.get_version_num() == 7 && parsed.hyphenated().encode_lower(&mut hyphenated) == id
    })
}

/// Identifies a [`Message`] within a session.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(String);

impl MessageId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(uuidv7())
    }

    /// The id as it travels the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for MessageId {
    /// Adopts a stored id verbatim. Being a UUIDv7 is what [`uuidv7`] mints,
    /// never something this door enforces: a transcript read back from disk —
    /// or a test's pinned fixture — keeps exactly what it was written with.
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
        Self(uuidv7())
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
        Self(uuidv7())
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
        Self(uuidv7())
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

/// Identifies one held inbound peer message ([`Event::PeerHeld`]), so a
/// settlement can name what it decides (**D524**).
///
/// A type of its own rather than a reused [`PermissionId`], for
/// [`QuestionId`]'s reason and one more that is load-bearing: a frontend's
/// dialog auto-answer machinery answers `PermissionId`s, so a hold it cannot
/// even name is a hold it can never silently release.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HeldId(String);

impl HeldId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(uuidv7())
    }

    /// The id as it travels the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for HeldId {
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
        Self(uuidv7())
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
    /// The **stored** part carries a **reference and nothing else**: the
    /// content is read when a request is built, never when the mention is made,
    /// so a file the user edits between attaching it and sending reaches the
    /// model as it is now rather than as it was. That is upstream's shape — its
    /// file part carries a `file://` URL the server resolves at send time — and
    /// it is also why a mention is not a read: nothing here records the file in
    /// `ganja-tool`'s `FileTimes`, so `edit` still refuses a file the model
    /// itself has not opened.
    ///
    /// [`start`](Self::File::start) and [`end`](Self::File::end) carry an
    /// `@path#12-40` line range, upstream's `?start=&end=` URL params
    /// (`autocomplete.tsx:254`). They are lines, 1-indexed and inclusive, and
    /// the send-time read slices to them; absent, the whole file is read. `end`
    /// is kept only when `start < end`, upstream's rule (`autocomplete.tsx:47`),
    /// so a `#20-10` becomes `start: Some(20), end: None`.
    ///
    /// [`content`](Self::File::content) is the one field the **stored** part
    /// never carries and the request's own copy sometimes does: an image or a
    /// PDF the send-time read turned into base64, so the wire that carries
    /// binary content has bytes to encode rather than a path it cannot follow.
    /// It stays out of every transcript — `skip_serializing_if` keeps a stored
    /// or resumed part byte-identical to the reference it was — because a
    /// mention is a reference and inlining the payload at attach time is the
    /// staleness the read-at-send ruling exists to avoid.
    File {
        /// Where the file is, relative to the project root.
        path: String,
        /// What kind of file it is, upstream's `mime`, derived from the path's
        /// extension: `image/png`, `application/pdf`, `image/svg+xml`, or
        /// `text/plain` for everything the attachment allowlist does not name.
        mime: String,
        /// First line of an attached range, 1-indexed and inclusive. Absent
        /// reads the whole file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<u32>,
        /// Last line of an attached range, 1-indexed and inclusive. Absent —
        /// even with `start` set — reads from `start` to the end, and is what a
        /// reversed `#20-10` collapses to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end: Option<u32>,
        /// The base64 payload of a binary attachment, present only on a
        /// request's own copy after the send-time read and never on the wire or
        /// on disk. See the variant's own documentation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    /// The turn began another model request. Tool results make a turn span
    /// several requests, and each one opens with this marker.
    StepStart,
    /// A model request finished, and what it spent.
    StepFinish {
        /// What this request cost, as the provider reported it.
        usage: Usage,
    },
    /// The model's own thinking, in words a person can read.
    ///
    /// Spec: upstream `session/message-v2.ts:362-376`, where a `reasoning` part
    /// carries `text` beside the provider metadata that signs it. **Ganja
    /// splits what upstream fuses**: the readable half is this variant, the
    /// sealed half is [`PartBody::Reasoning`]. The split is what makes the next
    /// rule expressible.
    ///
    /// # Display-only, and why the split buys that
    ///
    /// This part is **never request-affecting**. Upstream replays its reasoning
    /// part into the next request; ganja replays the sealed blob instead, which
    /// is the half a provider actually asked to have handed back. So every wire
    /// drops one of these when it encodes a request, and losing one costs a
    /// reader some lines on a screen and costs the model nothing — the ordinary
    /// shape of loss that every variant but [`PartBody::Reasoning`] has.
    ///
    /// It is deliberately **outside [`Part::as_text`]**, which means the reply
    /// text and only the reply text: that accessor titles a rewind checkpoint
    /// and answers `/copy --message`, and thinking doing either would be the
    /// model's scratch paper standing in for its answer. A caller that wants
    /// thinking matches this variant, which is what every reader of it does.
    ///
    /// # What an older build does with one
    ///
    /// The tag is `reasoning_text`, which keeps the [`REASONING_TAG`] prefix
    /// the variant below makes a contract of. A build too old to decode it
    /// therefore takes that contract's reader arm — `ganja-core`'s storage puts
    /// a stateless [`PartBody::Reasoning`] in its place and warns — which is
    /// that contract working as designed rather than a fault: the marker is
    /// never sent, so nothing downstream is harmed. The one imprecision is that
    /// the old build's warning says the next request lost reasoning for that
    /// step, where in truth only a rendered line was lost. Nothing in a newer
    /// build can reword a message an older one prints; it is recorded here
    /// because the row that provokes it is defined here.
    ReasoningText {
        /// Everything accumulated so far, grown by the deltas that stream it.
        text: String,
    },
    /// A tool the **provider** ran on its own side, recorded so a person can
    /// see what happened (**D489**).
    ///
    /// No upstream counterpart: opencode has no gateway server tools at all.
    /// The one vendor that serves them today is OpenRouter, whose `tools` array
    /// takes `{"type": "openrouter:<name>"}` rows the model may call and *that
    /// vendor* executes, returning the result to the model before the reply
    /// continues (`docs/guides/features/server-tools`, read 2026-08-14).
    ///
    /// # Display-only, and why that is the whole design
    ///
    /// Three things this part is deliberately not, each of them a bug it exists
    /// to prevent:
    ///
    /// - **Not a call to execute.** The work is already done by the time the
    ///   item arrives; a wire that reported one as a tool-call *event* would
    ///   wedge the turn on a tool no registry has.
    /// - **Not a call to gate.** A permission dialog asks whether *this* machine
    ///   may do something. Nothing here runs on it, and a dialog whose only
    ///   honest answer is "it already happened" is worse than none.
    /// - **Not request-affecting.** It is never replayed — the vendor keeps the
    ///   record of its own tools, and sending one back as a `function_call`
    ///   naming a function the roster never advertised is the guess this build
    ///   does not make. Every wire drops it when it encodes a request, exactly
    ///   as it drops [`PartBody::ReasoningText`], and losing one costs a reader
    ///   some lines and costs the model nothing.
    ///
    /// A frontend renders it in the ordinary tool grammar (`● tool(args)` and
    /// its `⎿` result), because what a person wants to know — *something
    /// searched the web, and this is what it found* — is the same question a
    /// local call's row answers.
    ServerTool {
        /// What the provider called it, verbatim: the item's own type, which
        /// for this vendor is `openrouter:<name>`.
        ///
        /// Deliberately un-prefixed and un-tidied. The namespace is the one
        /// thing that says this row is not a local call, and a registry name it
        /// could be confused with is exactly what it must not look like.
        tool: String,
        /// The arguments the model called it with, as the item carried them.
        ///
        /// [`Value::Null`](serde_json::Value::Null) where the item named none —
        /// the vendor documents a different argument shape per tool, so what is
        /// recorded is what arrived rather than a shape assumed for it.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        input: serde_json::Value,
        /// What the provider reported it produced, already rendered as the text
        /// a row previews. Empty where the item reported nothing.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        output: String,
    },
    /// Something another agent said, carried in this conversation (**D495**).
    ///
    /// No upstream counterpart: opencode has no teams and no second agent to
    /// hear from. The specification is Claude Code's §5.3, whose
    /// `formatTeammateMessage` renders exactly this into the request that
    /// follows:
    ///
    /// ```text
    /// <teammate-message teammate_id="w1" color="blue" summary="picked up W2">
    /// starting on the protocol surface
    /// </teammate-message>
    /// ```
    ///
    /// The envelope itself is built where a request is assembled, not here —
    /// this is the record it is built from, and the record is what a
    /// transcript keeps.
    ///
    /// # Data, never authority
    ///
    /// This is the one part whose content was written by something that is
    /// neither this session's model nor the person at the terminal, and §7-5
    /// is what follows from that: a peer's words are information the model
    /// reads, never an instruction it is bound by and never consent for
    /// anything. [`team::PeerPayload`] states that rule as a type; this
    /// variant states it on the wire, by staying **outside [`Part::as_text`]**
    /// — that accessor titles a rewind checkpoint and answers the copy
    /// surfaces, and a teammate's sentence standing in for what *this*
    /// conversation said is a misattribution rather than a truncation.
    ///
    /// # Display-only is the wrong word for this one
    ///
    /// [`PartBody::ReasoningText`] and [`PartBody::ServerTool`] are drawn and
    /// never sent. This part is drawn **and** sent — as the envelope above,
    /// rendered into the user turn, never as a message of its own under a role
    /// no vendor has. So a message whose only part is one of these does have
    /// content ([`Message::has_content`]), where a message holding only
    /// thinking does not: the model was told this, and a later request carries
    /// it.
    ///
    /// # What an older build does with one
    ///
    /// Drops the part and keeps the rest of the message. The tag takes no
    /// [`REASONING_TAG`]-shaped forward contract, because that contract exists
    /// for state the *next request* must hand back and this is not that — but
    /// the loss is real all the same, a message the model was told going
    /// missing, which is why the tag is minted once here and never renamed.
    Peer {
        /// Which teammate wrote it: the bare member name that is also its
        /// mailbox address.
        ///
        /// Never `main`. That word names the sender's own parent conversation
        /// (§5.5.1) and is a member of nothing, so a part carrying it would
        /// name a teammate that cannot exist.
        from: String,
        /// A one-line summary for the envelope's `summary` attribute, where
        /// the sender wrote one.
        ///
        /// Capped at [`team::DISPLAY_FIELD_CAP`] characters **by whoever
        /// builds the part**: [`team::PeerPayload::into_part`] is where that
        /// happens on the path a message actually travels, and
        /// [`team::cap_for_display`] is the function to call anywhere else.
        /// This type deliberately does not re-cap — a stored part must read
        /// back as the bytes it was written as, and a constructor that
        /// silently shortened a decoded field would make that false.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// The member's assigned color, for a frontend to draw it in and for
        /// the envelope's `color` attribute — which §5.3 writes only when it
        /// validates, so absent here means "draw it plainly" rather than
        /// "unknown".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        /// What the peer said, verbatim.
        body: String,
    },
    /// The model's own thinking, as the provider sealed it, kept so the next
    /// request can hand it back.
    ///
    /// **Nothing here reads it, and nothing may.** A reasoning model that is
    /// asked to keep no state of its own (`store: false`) is handed its
    /// previous thinking as an opaque blob the client returns verbatim; the
    /// blob is the provider's, and this part is the envelope it travels in.
    /// Nothing renders it: a frontend meeting one has nothing to draw, because
    /// what is *in* it is bytes only the sealing wire can open. Thinking a
    /// person can read travels beside it as [`PartBody::ReasoningText`], and
    /// the two are independent — a wire may send either, both or neither.
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
    /// The call has not started: its arguments are still streaming, or —
    /// once they ride here — complete while the call waits its turn behind
    /// the step's earlier calls (2026-08-15).
    Pending {
        /// The parsed arguments, present from the moment the provider marked
        /// them complete and absent while they still stream. Optional and
        /// left off the wire when absent, so every `{"status":"pending"}`
        /// written before this field existed still reads back.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
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
        Self { id: PartId::ascending(), body: PartBody::Text { text: text.into() } }
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
                state: ToolState::Pending { input: None },
            },
        }
    }

    /// Builds a file part with a fresh id, for a file the user mentioned with
    /// no line range: the whole file, read at send time.
    #[must_use]
    pub fn file(path: impl Into<String>, mime: impl Into<String>) -> Self {
        Self::file_range(path, mime, None, None)
    }

    /// Builds a file part with a fresh id, carrying the line range the mention
    /// named. `start`/`end` are lines, 1-indexed and inclusive; the send-time
    /// read slices to them.
    #[must_use]
    pub fn file_range(
        path: impl Into<String>,
        mime: impl Into<String>,
        start: Option<u32>,
        end: Option<u32>,
    ) -> Self {
        Self {
            id: PartId::ascending(),
            body: PartBody::File {
                path: path.into(),
                mime: mime.into(),
                start,
                end,
                content: None,
            },
        }
    }

    /// Builds a readable-thinking part with a fresh id.
    ///
    /// Opened empty and grown by the deltas that stream it, the way a reply's
    /// own text part is.
    #[must_use]
    pub fn reasoning_text(text: impl Into<String>) -> Self {
        Self { id: PartId::ascending(), body: PartBody::ReasoningText { text: text.into() } }
    }

    /// Builds a provider-run tool row with a fresh id (**D489**).
    ///
    /// Complete when it is minted, unlike a local call's part: the work was
    /// done on the provider's side before the item that reports it arrived, so
    /// there is no pending state for this one to pass through.
    #[must_use]
    pub fn server_tool(
        tool: impl Into<String>,
        input: serde_json::Value,
        output: impl Into<String>,
    ) -> Self {
        Self {
            id: PartId::ascending(),
            body: PartBody::ServerTool { tool: tool.into(), input, output: output.into() },
        }
    }

    /// Builds a peer's message part with a fresh id (**D495**).
    ///
    /// Complete when it is minted, like a provider-run tool row and unlike a
    /// reply's own text: a peer's words arrive whole through the mailbox, so
    /// there is nothing here for a delta to grow.
    ///
    /// The arguments are in the field order, because two of them are
    /// `Option<String>` and adjacent. `summary` is expected already capped —
    /// [`team::PeerPayload::into_part`] caps on its way through here, and any
    /// other caller applies [`team::cap_for_display`] itself.
    #[must_use]
    pub fn peer(
        from: impl Into<String>,
        summary: Option<String>,
        color: Option<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: PartId::ascending(),
            body: PartBody::Peer { from: from.into(), summary, color, body: body.into() },
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
            body: PartBody::Reasoning { provider: provider.into(), item: item.into(), encrypted },
        }
    }

    /// The **reply** text this part carries, or [`None`] when it carries
    /// something else.
    ///
    /// Deliberately not thinking: this is what titles a rewind checkpoint and
    /// what the copy surfaces read, and the model's scratch paper is not its
    /// answer. Thinking is [`PartBody::ReasoningText`], matched by name
    /// wherever it is wanted.
    ///
    /// Deliberately not a peer's words either, for the same accessor's sake
    /// reached from the other side: [`PartBody::Peer`] is what somebody
    /// *else's* agent said, and letting it answer here would title this
    /// session's checkpoint with a sentence this session never uttered.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match &self.body {
            PartBody::Text { text } => Some(text),
            PartBody::Tool { .. }
            | PartBody::File { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
            | PartBody::ReasoningText { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Peer { .. }
            | PartBody::Reasoning { .. } => None,
        }
    }

    /// The reply text this part carries, for accumulating streamed fragments.
    pub fn as_text_mut(&mut self) -> Option<&mut String> {
        match &mut self.body {
            PartBody::Text { text } => Some(text),
            PartBody::Tool { .. }
            | PartBody::File { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
            | PartBody::ReasoningText { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Peer { .. }
            | PartBody::Reasoning { .. } => None,
        }
    }

    /// The text an [`Event::PartDelta`] for this part appends to, whichever
    /// kind of text it is.
    ///
    /// The one accessor that spans both, because the one caller that needs it —
    /// a frontend applying an event stream — is told a part's id and a fragment
    /// and is not told which of the two it is growing. Everything that *does*
    /// know reaches for the accessor that names what it means.
    ///
    /// A [`PartBody::Peer`] is neither, and not because of what it says: no
    /// provider streams one. It arrives whole out of a mailbox, so there is no
    /// event that would ever name its id and a fragment.
    pub fn streamed_mut(&mut self) -> Option<&mut String> {
        match &mut self.body {
            PartBody::Text { text } | PartBody::ReasoningText { text } => Some(text),
            PartBody::Tool { .. }
            | PartBody::File { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Peer { .. }
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
            time: MessageTime { created, completed: Some(created) },
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
            time: MessageTime { created: now(), completed: None },
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
    ///
    /// [`PartBody::ReasoningText`] is out for the same reason arrived at from
    /// the other side: no wire sends it, so a message holding nothing else is
    /// one every later request would carry as an assistant turn that said
    /// nothing at all. That it is readable on screen does not make it
    /// something the model was told.
    ///
    /// A [`PartBody::Peer`] is **in**, and it is the one display-shaped part
    /// that is: the request assembly renders it into the user turn as §5.3's
    /// envelope, so a message carrying nothing but a teammate's words is a
    /// message the model was told and a later request carries. Dropping it as
    /// empty would lose the message and leave whatever the model did about it
    /// standing there unexplained.
    #[must_use]
    pub fn has_content(&self) -> bool {
        self.parts.iter().any(|part| match &part.body {
            PartBody::Text { text } => !text.is_empty(),
            PartBody::Tool { .. } | PartBody::File { .. } | PartBody::Peer { .. } => true,
            PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
            | PartBody::ReasoningText { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Reasoning { .. } => false,
        })
    }
}

/// One file the user attached to a prompt, by `@`-mentioning it.
///
/// A path, and — when the mention named an `@path#12-40` line range — the
/// lines it named: what the file *says* is read when the request is built, not
/// when the mention is made. See [`PartBody::File`], whose `start`/`end` these
/// become.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    /// Where the file is, relative to the project root.
    pub path: String,
    /// First line of an attached range, 1-indexed and inclusive. Absent — and
    /// absent from the wire — for a whole-file mention, which keeps a mention
    /// written before ranges existed byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u32>,
    /// Last line of an attached range, 1-indexed and inclusive. Absent reads
    /// from `start` to the end of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u32>,
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

/// How much of a checkpoint a [`Command::RevertTo`] puts back.
///
/// **D451** (`rewind-scope-choice`). Upstream reverts one thing: its
/// `RevertInput` (`session/revert.ts:13-17`) names a session and a message and
/// takes the checkout *and* the conversation back together, which is what
/// [`Command::Undo`] already is here. The split is Claude Code's — its rewind
/// picker asks "Restore the code and/or conversation" before it does anything
/// — and it is a divergence rather than a port because upstream has no such
/// question and no wire field to answer it with. What is *not* ported from
/// upstream is the other half of its input: `partID`, which reverts to a point
/// inside a turn. Ganja's checkpoints are whole user messages (recorded as a
/// follow-up, not as a deviation: nothing here contradicts upstream, it is
/// simply narrower).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevertScope {
    /// The checkout and the conversation, which is what [`Command::Undo`]
    /// does to the prompt before it.
    Both,
    /// The conversation alone: the messages from the checkpoint on are hidden
    /// and the working tree is left exactly as it is.
    Conversation,
    /// The checkout alone: the files those turns changed come back and the
    /// transcript is untouched, so nothing is hidden and there is nothing to
    /// redo.
    Files,
}

impl RevertScope {
    /// Whether this scope puts files back, which is what decides whether the
    /// session needs snapshots to serve it at all.
    #[must_use]
    pub fn touches_files(self) -> bool {
        matches!(self, Self::Both | Self::Files)
    }

    /// Whether this scope hides messages, which is what decides whether the
    /// engine remembers the revert and a redo has anything to step through.
    #[must_use]
    pub fn touches_conversation(self) -> bool {
        matches!(self, Self::Both | Self::Conversation)
    }
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
        /// Skills the user explicitly invoked with `$name` tokens — the
        /// OpenAI Codex CLI's grammar — still present in `text`. Names, not
        /// bodies: the engine loads each one at the same seam mentions
        /// resolve at, so what the model reads is decided where the roots
        /// live, not by whichever frontend sent this. Absent from the wire
        /// when there are none, same as `mentions`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skills: Vec<String>,
        /// Teammates and live sessions the user's `@`-mention tokens named
        /// (**D529**) — still present in `text`, exactly as `skills` names
        /// are. Names, not resolutions: the engine resolves each one at the
        /// seam skills expand at, so which session a name means is decided
        /// where the roster and the identity index live, not by whichever
        /// frontend sent this. Absent from the wire when there are none,
        /// same as the fields beside it, so a prompt naming no session is
        /// byte-for-byte the prompt this protocol always sent.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        session_mentions: Vec<String>,
        /// Messages teammates wrote, which this prompt hands to the model as
        /// [`PartBody::Peer`] parts rather than as text (**D495**).
        ///
        /// **A list, not one.** The lead's inbox is polled on a tick and can
        /// hold several messages by the time it is read; a single-message
        /// field would force the second and third into commands of their own,
        /// which the one-turn-at-a-time rule then refuses. §5.3 has the same
        /// shape for the same reason — its `formatTeammateMessages` is plural.
        ///
        /// The text beside them may be empty, and usually is: a delivery turn
        /// is a turn whose content *is* what the teammates said. Absent from
        /// the wire when there are none, like the two fields above it, so a
        /// prompt that has nothing to do with a team is byte-for-byte the
        /// prompt this protocol always sent.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        peers: Vec<team::PeerPayload>,
    },
    /// Hands a message to the turn that is **already** streaming, which takes
    /// it on at its next step boundary rather than starting a turn of its own.
    ///
    /// The turn stays singular: this is not a second prompt and it does not
    /// repeal the one-turn-at-a-time rule that refuses one. What it adds is an
    /// explicit mailbox the running loop drains between steps, so a correction
    /// typed while the model works reaches *that* model request instead of
    /// waiting for the next turn.
    ///
    /// **D450** (`steer-is-an-explicit-command`). Upstream v1.18.22 has the
    /// same *observable* behavior and no contract for it: `session/prompt.ts`
    /// persists a message mid-turn and `effect/runner.ts`'s loop re-reads the
    /// conversation each iteration, so whether a prompt starts a turn or
    /// silently joins one is decided by a race nobody named. Porting that
    /// shape would have meant repealing the refusal a frontend depends on and
    /// putting nothing in its place. This is the same outcome reached through
    /// a command of its own — with a correlation id a frontend can render
    /// against and a typed refusal when it loses the race — which is the shape
    /// Codex's `session/input_queue.rs` and `session/inject.rs` also arrived
    /// at.
    ///
    /// The payload is [`Command::SendPrompt`]'s plus [`id`](Self::Steer::id).
    /// A steer that arrives with nothing streaming is refused rather than
    /// promoted to a prompt: which of the two a message is belongs to whoever
    /// typed it, not to how the timing fell.
    Steer {
        /// The frontend's own correlation id, echoed back by
        /// [`Event::SteerConsumed`]. It exists so a rendered queue entry can
        /// be retired exactly when the engine provably took the message, and
        /// never on a guess: no id, and a frontend showing what is still
        /// waiting would have to infer it from the transcript.
        id: String,
        /// What the user typed.
        text: String,
        /// Files the user attached to it, read when the request that carries
        /// this message is built — the same read-at-send rule a prompt's
        /// mentions follow. Absent from the wire when there are none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mentions: Vec<Mention>,
        /// Skills the message's `$name` tokens invoke, resolved when the
        /// carrying request is built — [`Command::SendPrompt::skills`]'
        /// read-at-send rule, at the steer seam. Absent from the wire when
        /// there are none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skills: Vec<String>,
        /// Teammates and live sessions the message's `@` tokens named, on
        /// [`Command::SendPrompt::session_mentions`]' terms — names, not
        /// resolutions, resolved when the carrying request is built. Absent
        /// from the wire when there are none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        session_mentions: Vec<String>,
        /// Messages teammates wrote, on [`Command::SendPrompt::peers`]' terms.
        ///
        /// A teammate that answers while a turn is running is answering *this*
        /// turn, so the same reason steering exists at all applies to it:
        /// waiting for the next turn would deliver the answer to a question
        /// the model has already stopped asking.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        peers: Vec<team::PeerPayload>,
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
    /// Runs the rest of the session under one of the active model's catalog
    /// efforts — a named bundle of provider options its wire splices into
    /// every request — or back under none. Takes effect at the next turn, and
    /// is refused while one is streaming.
    ///
    /// The names are the catalog's (upstream `provider.ts:1049`), so an engine
    /// refuses a name the active model's row does not carry, and refuses any
    /// name at all on a provider the catalog has no rows for — the same
    /// no-catalog posture that already denies such a session sizing and
    /// pricing.
    SwitchEffort {
        /// The effort's name, or [`None`] for upstream's "Default" — no
        /// effort at all. Absent from the wire when [`None`], so the clearing
        /// command's bytes carry nothing but its type.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// Runs the rest of the session under a different permission posture
    /// (**D-15**, **D496**).
    ///
    /// **Accepted while a turn streams**, unlike the three switches above it,
    /// and applied at the **next** turn's start. The asymmetry is deliberate
    /// and it is D474's discipline rather than [`Command::SwitchAgent`]'s
    /// refusal: what sends this may be a team's lead answering a teammate's
    /// `mode_set_request` ([`team::ModeSetRequest`]) mid-turn, and a refusal
    /// there would drop a decision nobody would think to re-send. A switch of
    /// agent, model or effort is typed by the person watching the turn, who
    /// can wait for it to end.
    ///
    /// [`Event::PermissionModeChanged`] announces the acceptance, which is
    /// earlier than the effect; that event's own documentation says when the
    /// change bites.
    SetPermissionMode {
        /// The posture the next turn runs under.
        mode: PermissionMode,
    },
    /// Settles one held inbound peer message ([`Event::PeerHeld`]) with a
    /// person's decision (**D524**).
    ///
    /// A release is a request, never an override: the engine re-checks the
    /// admission policy first, so an approval cannot deliver past a policy
    /// that has since become refuse — the entry settles denied instead (v2
    /// §"Reevaluation and manual decision", evidence 620847-620877). A settle
    /// naming an id nobody holds is ignored, the same rule a permission reply
    /// racing a cancel already lives under: the hold may have expired or been
    /// re-evaluated while the dialog was up.
    ///
    /// [`Event::PeerHoldSettled`] announces how the hold actually ended,
    /// which the re-check means is not always what `decision` asked.
    SettleHeld {
        /// The held message being settled.
        id: HeldId,
        /// What the person decided.
        decision: HeldDecision,
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
    /// Takes the session back to a checkpoint the user picked, restoring
    /// whatever [`scope`](Self::RevertTo::scope) names.
    ///
    /// A **superset** of [`Command::Undo`], never a replacement for it: `/undo`
    /// still means "one prompt back, files and conversation together", and this
    /// is the same machinery reached with an anchor and a scope of the user's
    /// own choosing. Upstream's `session.revert({sessionID, messageID})`
    /// (`session/revert.ts:13-23`, called from `dialog-message.tsx:22-52`) is
    /// the spec for the semantics; the scope is Claude Code's — see
    /// [`RevertScope`].
    ///
    /// Refused while a turn is streaming, for [`Command::Undo`]'s reason
    /// (**D119**), and refused by name when `message_id` is not a user message
    /// still in the live window: a rewind names a checkpoint, and the engine
    /// says so rather than reverting to the nearest thing it can find.
    ///
    /// Announced by [`Event::RevertChanged`], which already carries every
    /// shape this can produce — including the files-only one, where the event
    /// names the files that came back while nothing is hidden.
    RevertTo {
        /// The user message to stop at. It, and everything after it, is what
        /// the revert takes back.
        message_id: MessageId,
        /// How much of that checkpoint to put back.
        scope: RevertScope,
    },
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

/// How much a session asks before it acts (**D496**).
///
/// **Two values, and they are ganja's own.** This build has no runtime
/// permission-mode switch at all — the bypass trio
/// (`--auto`/`--yolo`/`--dangerously-skip-permissions`, D479) resolves once at
/// startup and everything after that is the rule engine's business. A team's
/// lead may nonetheless send a `mode_set_request` ([`team::ModeSetRequest`])
/// mid-session, so there has to be something the engine can be set *to*, and
/// two postures is what ganja actually has to offer.
///
/// [`PermissionMode::from_claude_name`] is where Claude Code's four names
/// become these two. A frame carries its mode as **text** precisely so that
/// mapping — which has a refusal in it — happens at the applier, where a
/// refusal can be reported to whoever asked, rather than at the decoder, where
/// it would drop the frame before anything could name what was refused.
///
/// The rename attribute is this crate's `snake_case` rule, not a choice
/// between spellings: two one-word variants are written identically by
/// `snake_case` and `kebab-case`, so the rule that governs every other enum
/// here governs this one too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// The rules decide, and a call they do not allow raises a dialog. What
    /// every session runs as unless it was started otherwise.
    Ask,
    /// A raised dialog is answered "allow once" without asking anyone.
    ///
    /// **A deny rule still denies**: this is the startup trio's exact posture
    /// (D479) reached at a turn boundary instead of at launch, and that trio
    /// answers dialogs rather than repealing rules.
    Bypass,
}

impl PermissionMode {
    /// Reads Claude Code's mode vocabulary as one of ganja's two, or refuses
    /// by name.
    ///
    /// The four names are `default`, `acceptEdits`, `bypassPermissions` and
    /// `plan` (§10.3-4). Three of them arrive somewhere:
    ///
    /// - `bypassPermissions` → [`PermissionMode::Bypass`];
    /// - `default` and `acceptEdits` → [`PermissionMode::Ask`], because what
    ///   `acceptEdits` decides per mode ganja's rules already decide per tool.
    ///   Collapsing the two is honest; minting a third value that nothing in
    ///   this build enforces would not be.
    /// - `plan` is **refused**, and it is the interesting one: this build
    ///   already has that switch, as an agent
    ///   ([`Command::SwitchAgent`] with `plan`), and two spellings of one
    ///   thing is one too many.
    ///
    /// Anything else is refused as the name it was rather than falling back to
    /// [`PermissionMode::Ask`], so the log line — the only place the refusal
    /// lands today — names what was asked.
    ///
    /// # Errors
    ///
    /// [`UnknownPermissionMode`], carrying the sentence the caller reports.
    pub fn from_claude_name(name: &str) -> Result<Self, UnknownPermissionMode> {
        match name {
            "bypassPermissions" => Ok(Self::Bypass),
            "default" | "acceptEdits" => Ok(Self::Ask),
            "plan" => Err(UnknownPermissionMode::Plan),
            other => Err(UnknownPermissionMode::Unknown(other.to_owned())),
        }
    }
}

/// Why a mode name is not one of ganja's two
/// ([`PermissionMode::from_claude_name`]).
///
/// The sentences are ganja's own words about ganja's own vocabulary — nothing
/// here is Claude Code prose — and they are pinned by a test so the wording
/// cannot drift silently out from under whoever quotes it back to a sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnknownPermissionMode {
    /// `plan`: this build has it, and has it as an agent rather than a mode.
    Plan,
    /// A name outside the four the reference records.
    Unknown(String),
}

impl std::fmt::Display for UnknownPermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plan => f.write_str(
                "plan is an agent here, not a permission mode: switch to it with /agent plan",
            ),
            Self::Unknown(name) => write!(
                f,
                "{name} is not a permission mode this build knows: it takes \
                 bypassPermissions, default or acceptEdits"
            ),
        }
    }
}

impl std::error::Error for UnknownPermissionMode {}

/// Why an inbound cross-session message was held for review instead of being
/// delivered or refused outright (**D523**).
///
/// Defined once, here: it crosses on [`Event::PeerHeld`] and is rendered by
/// the review surfaces, while the engine's admission resolver imports it and
/// keeps its resolver-only siblings — policy, receiver class, verdict — to
/// itself. Three surfaces render it — the approval modal's sentence, the
/// `/held` listing's short label, the sender's held note — each in its own
/// words, deliberately: no shared `Display` here, because a spelling sized
/// for a modal, a column and a wire note at once would serve none of those
/// readers. The parity causes are the reference's matrix (v2 §"The parity
/// matrix, and when it actually applies", evidence 620535-620617); which of
/// them installs an expiry timer is the `expires_in_ms` contract on
/// [`Event::PeerHeld`], not this enum's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HoldCause {
    /// A configured `cross_session_inbound: "hold"` decided, before any
    /// parity was consulted — an explicit value always wins over parity (v2
    /// §"Explicit values", evidence 680146-680160). Carries which tier said
    /// so, for a review surface to name.
    Explicit {
        /// The config tier the explicit policy came from.
        source: PolicySource,
    },
    /// Sender and receiver asserted different permission classes.
    ModeMismatch,
    /// The receiver runs bypass-classed and the sender asserted no class at
    /// all — the collapsed matrix's one hold, and the only parity cause
    /// ganja's wire can produce today, since no ganja sender asserts a mode.
    NoModeAsserted,
    /// The receiver's own mode could not be read, so the message is held
    /// fail-closed (v2 §"Receiver permission classes"). Structurally
    /// unreachable while the mode is plain engine state; the arm exists
    /// before its producer does, which is what fail-closed means.
    ModeUnknown,
}

/// The config tier an explicit inbound policy came from (**D523**).
///
/// Ganja's spelling of the reference's source chain (v2 §"Source precedence
/// and repository tightening (`MRf`)", evidence 620378-620481) over its own
/// tiers: there is no managed-policy tier to name, and the project tier may
/// only have tightened what the trusted tiers chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    /// The global config home.
    Global,
    /// The file `GANJA_CONFIG` names, which outranks the global tier.
    ExplicitFile,
    /// A project config file.
    Project,
}

/// What a person decided about one held message ([`Command::SettleHeld`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldDecision {
    /// Deliver it — subject to the policy re-check the command's own
    /// documentation owns.
    Release,
    /// Drop it. The sender is not told: the held answer it already got was
    /// the last word.
    Deny,
}

/// How a hold ended (**D524**).
///
/// Exactly the reference's settlement statuses minus `held`, which is the
/// initial answer rather than an ending (v2 §"Receipts and sender UX",
/// evidence 220977-221015, 886033-886075).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldOutcome {
    /// Released into the ordinary delivery path — by a person, or by a mode
    /// change whose re-evaluation now accepts it.
    Delivered,
    /// Dropped — by a person's deny, or by a release re-checked against a
    /// policy that now refuses.
    Denied,
    /// Dropped undecided: the deadline passed, capacity evicted it, or
    /// shutdown settled it — never delivered, because nobody said so.
    Expired,
}

/// Somebody's words, rendered in `Debug` as their size and never their text
/// (**D524**).
///
/// The `summary` and `preview` on [`Event::PeerHeld`] are a foreign sender's
/// own prose, and a derived `Debug` would carry them into any traced event —
/// the admission gate's observability is typed reasons and identities, never
/// bodies. Serde is transparent, so the wire and the store carry the text for
/// the dialogs that exist to show it; only the `{:?}` rendering is redacted.
/// The spelling (`<N bytes>`) matches the rule `ganja-team`'s
/// `MailboxMessage` debug states, so one grep finds every place it is
/// applied.
///
/// Deliberately no `Display`: showing the text is a decision a caller makes
/// by writing [`as_str`](Self::as_str), never one a format string makes for
/// them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RedactedText(String);

impl RedactedText {
    /// The text itself, for the surfaces whose purpose is to show it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for RedactedText {
    /// Adopts the text verbatim; capping it for display is the builder's job,
    /// exactly as it is for a peer part's fields.
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl std::fmt::Debug for RedactedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} bytes>", self.0.len())
    }
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
    /// A [`Command::Steer`]'s message was taken into the running turn.
    ///
    /// Emitted immediately before the [`Event::MessageStarted`] that carries
    /// it, so a frontend retires its queue entry in the same breath as the
    /// message appears in the transcript, and never earlier.
    ///
    /// There is no complementary "not consumed" event, and there is nothing
    /// for one to say: a steer is either taken (this), refused when it
    /// arrives (`EngineError::NotStreaming`, the engine's own answer to the
    /// command), or still unconsumed when [`Event::MessageFinished`] ends the
    /// turn — which is itself the signal, since a turn that has ended will
    /// never drain another one.
    SteerConsumed {
        /// Session this happened in.
        session_id: SessionId,
        /// The id the [`Command::Steer`] carried.
        id: String,
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
    /// Sent when [`Command::Undo`], [`Command::Redo`] or [`Command::RevertTo`]
    /// moves the anchor, when a revert is cleared, and when a resumed session
    /// turns out to have been left in one — a frontend that starts fresh
    /// learns the hidden range from this event and from nowhere else.
    ///
    /// One shape needs a word: a [`RevertScope::Files`] rewind announces
    /// `revert: Some(_)` naming the checkpoint and the files that came back,
    /// while the engine records no revert at all — nothing is hidden and there
    /// is nothing to redo. A frontend tells that one apart the same way it
    /// tells the two `None`s apart: by the command it sent.
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
    /// The engine adopted a model effort — or cleared one, which a switch to
    /// a model that lacks the current effort's name also does (upstream
    /// `prompt.ts:654`). Announced from every path that moves the selection,
    /// so a frontend's indicator does not depend on having issued
    /// [`Command::SwitchEffort`] itself.
    EffortChanged {
        /// Session this happened in.
        session_id: SessionId,
        /// The effort now active, or [`None`] for upstream's "Default".
        /// Absent from the wire when [`None`], matching the command that asks
        /// for it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// The engine took a [`Command::SetPermissionMode`] (**D-15**, **D496**).
    ///
    /// **Fired at acceptance, which is not when it bites.** The posture
    /// applies at the start of the next turn, so one of these arriving
    /// mid-stream says the engine is *holding* the change, never that the turn
    /// on screen has moved to it. A frontend drawing the posture therefore
    /// draws what the next turn will do — which is exactly what the sender of
    /// the command was told, so the two never disagree.
    PermissionModeChanged {
        /// Session this happened in.
        session_id: SessionId,
        /// The posture the next turn runs under.
        mode: PermissionMode,
    },
    /// An inbound cross-session message was held for review instead of being
    /// delivered (**D524**): it has **not** reached the model, and never will
    /// unless something settles it delivered. No turn waits on this —
    /// settlement rides [`Command::SettleHeld`], a mode change's
    /// re-evaluation, expiry, capacity eviction, or shutdown, and
    /// [`Event::PeerHoldSettled`] announces whichever came.
    ///
    /// A frontend branches on `cause`: the parity causes raise the
    /// per-message approval dialog, while an explicit or mode-unknown hold is
    /// reviewed in the listing dialog alone.
    PeerHeld {
        /// Session this happened in: the lead session holding the message.
        session_id: SessionId,
        /// Names this hold, for the settlement.
        id: HeldId,
        /// The sender's claimed identity, `<name>@<team>` — claimed, because
        /// the transport validates its shape and never who wrote it.
        from: String,
        /// Why it was held rather than delivered.
        cause: HoldCause,
        /// The sender's own one-line summary, where it wrote one. Absent from
        /// the wire when there is none, exactly as a peer part's is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<RedactedText>,
        /// The body's opening, capped by whoever built the event, for the
        /// approval dialog's expandable preview.
        preview: RedactedText,
        /// How long until this hold expires on its own, for a dialog to count
        /// down. Present exactly for the parity causes: an explicit or
        /// mode-unknown hold installs no timer and sits until mode change,
        /// capacity, settlement or shutdown (v2 §"Cross-pass reconciliation",
        /// the expiry re-check, evidence 1227895-1227987, 1265450-1265503).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_in_ms: Option<u64>,
    },
    /// A hold ended, so a frontend can retire its dialog and its listing row.
    /// What `outcome` says may differ from what a [`Command::SettleHeld`]
    /// asked, and the difference is the policy re-check doing its job.
    PeerHoldSettled {
        /// Session this happened in, addressed as its hold was.
        session_id: SessionId,
        /// The hold that ended.
        id: HeldId,
        /// How it ended.
        outcome: HeldOutcome,
    },
    /// A peer message this session **sent** was held on the far side and has
    /// now settled, for the frontend's notice line (**D534**).
    ///
    /// Never fired for a plain accept, a refuse, or a guard drop: the receipt
    /// route exists only for a message this session's own send held on
    /// arrival, and only that hold's settlement ever crosses it back.
    ///
    /// The model-facing half — one `<peer_receipt>`-tagged
    /// [`Part::text`](crate::Part::text) batched at the next prompt intake —
    /// is **ganja-inferred**: v2 places the settlement notice on the
    /// sender-side UI alone and does not say it reaches the model (v2
    /// §"Receipts and sender UX", evidence 1228101-1228199 — the UI batches
    /// status updates and injects a meta notice). Rendering it as
    /// conversation text besides is ganja's own call, argued from D529's own
    /// mention-reminder precedent rather than borrowed from the reference.
    PeerReceipt {
        /// Session this happened in: the one that sent the message.
        session_id: SessionId,
        /// The sender's own id for the message this receipt settles.
        id: PeerMessageId,
        /// How it settled.
        status: PeerReceiptStatus,
        /// Who it was sent to — `<name>@<team>`, the same identity a peer
        /// part or a hold's `from` already carries. A session leading nobody
        /// is no exception: its team is its own `session-<hex>` and it sends
        /// under that team's lead identity (**D543**).
        to: String,
    },
    /// A compaction is summarizing the window, and this is how far along it
    /// is: the summary streamed so far, estimated in tokens, against the
    /// output budget the engine's fit guard reserves for one.
    ///
    /// Emitted when the summarize request is about to stream (`tokens: 0`)
    /// and again whenever the estimate grows, for both triggers — the
    /// automatic one at a turn's start and the `/compact` a person typed.
    /// The [`Event::MessageStarted`] carrying the finished summary is what
    /// ends the run; there is no closing progress event, because that
    /// announcement already says everything one would.
    ///
    /// No upstream counterpart: opencode compacts silently and its frontend
    /// learns only from the summary's own announcement (user directive,
    /// 2026-08-25). The estimate is the engine's four-characters-per-token
    /// convention, so a consumer should read it as a gauge, never an
    /// invoice — the provider's own accounting arrives on the summary
    /// message, like every other request's.
    CompactionProgress {
        /// Session this happened in.
        session_id: SessionId,
        /// Estimated tokens of summary streamed so far.
        tokens: u64,
        /// The output budget a summary is expected to fit, carried so a
        /// frontend draws the fraction without hard-coding the engine's
        /// constant.
        budget: u64,
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
            | Event::SteerConsumed { session_id, .. }
            | Event::PermissionReplied { session_id, .. }
            | Event::QuestionAsked { session_id, .. }
            | Event::QuestionReplied { session_id, .. }
            | Event::QuestionRejected { session_id, .. }
            | Event::RevertChanged { session_id, .. }
            | Event::AgentChanged { session_id, .. }
            | Event::EffortChanged { session_id, .. }
            | Event::PermissionModeChanged { session_id, .. }
            | Event::PeerHeld { session_id, .. }
            | Event::PeerHoldSettled { session_id, .. }
            | Event::PeerReceipt { session_id, .. }
            | Event::CompactionProgress { session_id, .. }
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
#[path = "lib_tests.rs"]
mod tests;
