//! What a `send_message` call reaches another teammate through.
//!
//! Spec: Claude Code's teammate messaging — §5.1's two reserved sets, §5.2's
//! validation ladder, §5.5's recipient kinds and §5.6's address forms of the
//! teammates reference this port works from. Upstream opencode has no
//! teammates and no counterpart to any of it, so nothing here is ported from
//! its TypeScript.
//!
//! # What is here and what is not
//!
//! Delivering a message is not something a tool knows how to do: a team, a
//! mailbox on disk, a peer's queued turn and a socket are all the engine's
//! vocabulary. So the delivering is somebody else's, reached through
//! [`Postbox`] — an [`Address`] and a [`Body`] in, a [`Sent`] or an
//! [`Undelivered`] out — and what stays here is the tool's own half: the
//! ladder that decides what may be sent at all, the sentences a refusal is
//! read in, and the roster the model is offered.
//!
//! The seam is drawn where it is for a second reason the compiler holds:
//! this crate's internal dependency list is asserted to be exactly
//! `ganja-permission`, so it may not name `ganja-protocol`, where the frame
//! vocabulary lives. [`Reserved`] is therefore the one thing that crosses
//! which is not a string or opaque JSON — *which* reserved set a text falls
//! in **and which frame it is**, because §5.2's last rung has to know whether
//! it is holding a `shutdown_approved`. On the engine's side that answer is
//! one `Frame` parse; nothing in here parses a frame, and no list of frame
//! names is duplicated here.

use std::path::PathBuf;

use async_trait::async_trait;

/// What a text is, as far as the frame vocabulary is concerned.
///
/// `kind` is the frame's own `type` discriminator, reported so the last rung
/// of the ladder can act on the frame it is actually holding — set membership
/// alone cannot answer "is this the shutdown answer?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reserved {
    /// Not a frame: ordinary prose, or JSON that means nothing to the
    /// protocol.
    No,
    /// One of §5.1's ten — frames an agent may legitimately originate, and
    /// which therefore have a structured door in [`Body::Frame`].
    AgentSendable {
        /// The frame's `type`.
        kind: &'static str,
    },
    /// One of §5.1's five — frames only the harness originates, which have no
    /// door at all, structured or otherwise.
    HarnessOnly {
        /// The frame's `type`.
        kind: &'static str,
    },
}

/// Where a message is addressed, as §5.2's rungs 2 to 4 read the `to`
/// argument.
///
/// Two forms, because this build carries two transports and refuses the rest
/// by name rather than letting an unrecognized scheme fall through to a
/// teammate lookup for somebody called `did:…`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    /// A member of this session's team, named by its bare name. There is one
    /// team per session, so the name is the whole address — which is what
    /// makes rung 4's refusal of a scoped `name@somewhere` meaningful rather
    /// than fussy.
    Local(String),
    /// Another session, reached at its socket: `uds:<path>`, or the bare
    /// leading `/` that §5.6's own parser reads as the same thing.
    Uds {
        /// The socket to reach, exactly as the model wrote it.
        path: PathBuf,
    },
}

/// What a message carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Prose, which is what a message between teammates almost always is.
    Text {
        /// What the recipient reads.
        text: String,
        /// One line naming what it is about, rendered beside the message
        /// where a frontend has the room. Already capped when it arrives
        /// here, and [`None`] rather than empty when the model said nothing.
        summary: Option<String>,
    },
    /// A protocol frame the model composed, carried as the JSON object it
    /// wrote. Opaque here by construction: the types are `ganja-protocol`'s
    /// and this crate may not name them, so what crosses is the document and
    /// the [`Reserved`] verdict somebody else formed about it.
    Frame(serde_json::Value),
}

/// A message that reached its recipient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    /// Who it reached, as the postbox resolved it — reported back rather than
    /// echoed from the arguments, so a resolution that differed from what the
    /// model typed is visible in the transcript instead of assumed.
    pub to: String,
    /// What became of it, in the terms the model reads next: queued into a
    /// peer's next turn, written to a teammate's inbox. Which of those it was
    /// is only the deliverer's to know, so the sentence is the deliverer's.
    pub note: String,
}

/// Why a message did not reach anybody.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Undelivered {
    /// Nobody this session may address goes by that name. Reported rather
    /// than asked about beforehand, so that the sentence the model reads and
    /// retries on is written in exactly one place — `send_message`'s own
    /// constants.
    Unknown,
    /// There is a recipient, and this build has no way to reach it. Since
    /// D505 landed the cross-session socket this is the pane member's answer
    /// to a `uds:` address — its postbox holds no socket transport — and it
    /// carries its own sentence because *which* transport is missing stays a
    /// fact about the deliverer, not one for a tool to guess at.
    NoTransport {
        /// What is missing, in the terms the model reads next.
        reason: String,
    },
    /// Delivery was attempted and failed: a mailbox that would not open, a
    /// socket that refused the connection. The message is what the model
    /// reads, so it says what went wrong in terms it can act on.
    Failed {
        /// What went wrong.
        reason: String,
    },
}

/// One teammate as the model is offered it.
///
/// The roster is assembled where the team is — who this caller may address is
/// the team's business, not a tool's — and arrives here as the little the
/// description and the last rung need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// What a `to` argument carries.
    pub name: String,
    /// The one line it is listed under, when there is anything to say about
    /// it.
    pub description: Option<String>,
    /// Whether this row is the team's lead — the one address a structured
    /// `shutdown_approved` may carry (§5.2's rung 8). A flag on the row
    /// rather than a method on [`Postbox`], because the roster is already the
    /// answer to "who may I address", and who leads is part of that answer.
    /// At most one row in a roster may set it: rung 8 names the lead by taking
    /// the first that does, so a second would be a lead the refusal never
    /// names, and keeping there to be only one is the team registry's
    /// invariant to hold rather than something a tool re-checks per call.
    pub lead: bool,
}

/// Delivers one teammate message on a `send_message` call's behalf.
///
/// Deliberately says nothing about *how*: a mailbox, a queued turn and a
/// socket are the engine's vocabulary, and a tool that named them would be a
/// tool the engine cannot be assembled without. What crosses is an address, a
/// body, and an answer.
///
/// # The sender is bound at construction, never passed
///
/// No method here takes a `from`, and that is a mechanism rather than a
/// preference. An implementation carries the sender's name as a field, set
/// once when the postbox is built for a particular engine, so a teammate's
/// postbox can only ever write that teammate's name on a message. A `from`
/// parameter would instead be a fact about *what the caller typed* — and the
/// caller is a model, one whose arguments could say `"from": "team-lead"` and
/// stamp the lead's name on its own message, which every sibling would then
/// believe. Bound at construction, the identity is a fact about *who is
/// calling*, which is the only thing a recipient can safely act on.
///
/// [`std::fmt::Debug`] is required because [`ToolCtx`] derives it, and an
/// implementation is expected to render which team and which sender it speaks
/// for rather than the machinery behind them.
///
/// [`ToolCtx`]: crate::ToolCtx
#[async_trait]
pub trait Postbox: std::fmt::Debug + Send + Sync {
    /// Whether `text` parses to a reserved frame, which of §5.1's two sets it
    /// is in, and which frame it is.
    fn classify(&self, text: &str) -> Reserved;

    /// Delivers `body` to `to`, or says why it could not.
    async fn deliver(&self, to: Address, body: Body) -> Result<Sent, Undelivered>;

    /// The team as this caller may address it: what the description lists,
    /// and where the last rung reads the lead's name from.
    fn roster(&self) -> Vec<Peer>;
}
