//! Held-settlement receipts (**D534**): both halves of the wire's remaining
//! promise, unblocked by D532's `reply_to`. **Held settlements only** ride
//! back to a reply-capable sender over one new socket route, matched
//! against an outstanding-id registry this module owns and trusted for
//! nothing.
//!
//! # What a receipt is, and what it is not
//!
//! Ganja's `held` receipt already shipped, **synchronously**, since D523:
//! `SocketDelivered.held` (`crate::subagent::SocketDelivered`) carries a
//! hold's cause on the very POST that was held. v2 delivers that fact over
//! the socket; ganja delivers it in the answer to the request itself — the
//! same content, a different transport, and the divergence is recorded
//! here rather than left for a reader to reconstruct. What this module adds
//! is what comes **after**: the three statuses that settle a hold once it
//! is already parked (v2 §"Explicit outcomes (`P8a`)", evidence
//! 620644-620683 — accept merely lets the message continue; refuse drops it
//! without enqueueing; only a hold parks it and answers `held`). An id is
//! therefore registered **only when the synchronous answer already said
//! held** — everything else was resolved at send time and needs no entry —
//! so silence on this route means exactly one thing: **still held**.
//!
//! # The trust paragraph
//!
//! **What the sender may trust about a receipt: nothing.** Admission is
//! three tests and no more:
//!
//! 1. **The id must be outstanding.** This session's registry holds at most
//!    200 ids (v2's own number, §"Receipts and sender UX"), evicted
//!    oldest-first, cleared by `NewSession`. The id is the whole
//!    capability: a v7 [`PeerMessageId`](ganja_protocol::PeerMessageId) minted by this sender and posted
//!    to exactly one address, so a process that knows it either is that
//!    address or was forwarded to by it. A receipt for an unknown id is
//!    dropped without an answer.
//! 2. **Any of the three statuses removes the entry** — the reference's own
//!    lifecycle, minus the `held` transition ganja performs synchronously
//!    at registration time (an id is only ever registered *because* the
//!    answer said held, so the entry is born in v2's awaiting-terminal
//!    state). A second receipt for a settled id is a receipt for an
//!    unknown id, and is dropped the same way.
//! 3. **The route answers identically either way**, admitted or dropped,
//!    for the reason the message route already gives: a distinct answer
//!    would let any same-uid process probe which ids a session is holding.
//!
//! What a forged receipt can therefore do is lie about one known message's
//! fate to the session that sent it. It cannot inject text (every word the
//! model reads is ganja's own rendering, and the attacker supplies one enum
//! value), cannot enqueue a turn, cannot touch permissions, and cannot
//! reach an id it does not know.
//!
//! # The reflector, and the bound the guard hoist makes true
//!
//! `reply_to` is sender-asserted and vetted only for *shape*, never for
//! *being the sender* — so a message A sends B can name C's socket as the
//! reply address, and B, on settling it, will connect to C. The payload
//! impact is nil (C answers an unknown id, byte-identically to an admitted
//! one), but the primitive is real and is named rather than discovered.
//! Four things bound it:
//!
//! - **At most one connect attempt per held settlement, and never a
//!   retry**, under `RECEIPT_DEADLINE` — a dead or wedged third party
//!   costs one timeout, never a hang.
//! - **Only a held entry's *settlement* emits one, and only by a person's
//!   decision or the `dialog_expiry` clock.** This is the bound D534's own
//!   guard hoist (`inbound.rs`'s N1/D1) makes true rather than assumed:
//!   with the guard now running ahead of the hold arm for the parity
//!   causes, hold *generation* is rate-limited and deduplicated exactly
//!   like an accept, so this side cannot be flooded into existence at
//!   machine rate either. A capacity eviction and the shutdown drain each
//!   settle their victim without ever calling
//!   [`Receipts::settle_and_post`](crate::teammate::receipts::Receipts::settle_and_post)
//!   at all — `hold()` and `shutdown_settle()` in `inbound.rs` stay
//!   byte-unchanged, and the distinction lives entirely on this side: the
//!   only callers this module's own API is meant to have are a person's
//!   approve, a person's deny, and the `dialog_expiry` timer.
//! - [`crate::tool::socket::vet_address`] confines the target to a `.sock`
//!   in this uid's own `0700` directory; nothing outside it is reachable.
//! - The third session sees an ordinary unknown-id drop, answered
//!   identically to an admitted receipt, so the reflection is not even a
//!   probe.
//!
//! # The two halves, and where each lives
//!
//! **Sender side.** [`Receipts::register`](crate::teammate::receipts::Receipts::register) is called (by the engine, once
//! wired) after a send's synchronous answer: an entry is kept only when
//! that answer's `held` field was present *and* this session emitted a
//! `reply_to` — an unbound sender, an accepted send and a refused or
//! guard-dropped send all register nothing. [`Receipts::settle_sent`](crate::teammate::receipts::Receipts::settle_sent)
//! applies an inbound `POST /peer/receipt`; [`Receipts::clear_sent`](crate::teammate::receipts::Receipts::clear_sent) is
//! `NewSession`'s own door.
//!
//! **Receiver side (N3).** [`Receipts::associate`](crate::teammate::receipts::Receipts::associate) pairs the [`HeldId`](ganja_protocol::HeldId) a
//! socket-door hold now returns
//! (`crate::teammate::inbound::SocketAdmission::Held`) with the message's
//! own id and vetted `reply_to`, at admission time. The association lives
//! in **this module's own map**, never on
//! `crate::teammate::inbound::HeldMessage` — that record is one of D534's
//! byte-untouched promises, and the pairing is receipt business rather than
//! gate business. [`Receipts::settle_and_post`](crate::teammate::receipts::Receipts::settle_and_post) consumes one association
//! and posts best-effort; an id with no association — a restart, a sender
//! with no `reply_to`, an entry this map's own cap already evicted — posts
//! nothing, the same silence every other unreachable case here produces.
//!
//! # What reaches the model
//!
//! A settled receipt becomes an `Event::PeerReceipt` (the frontend's
//! notice) and, batched at the next prompt intake, one `<peer_receipt>`-
//! tagged [`Part::text`](crate::protocol::Part::text) from [`rendered`](crate::teammate::receipts::rendered) —
//! D529's own vehicle (`session.rs`'s `user_message` seam), reused rather
//! than reinvented. Unlike a peer's own words this is ganja's own sentence
//! about ganja's own send, so nothing here is display-only: the model must
//! be able to act on it (retry, route elsewhere), which is why it rides
//! ordinary conversation text rather than a display-only part. The
//! model-facing half is **ganja-inferred**: v2 places the settlement notice
//! on the sender-side UI alone and does not say it reaches the model (v2
//! §"Receipts and sender UX", evidence 1228101-1228199), and this landing's
//! argument for going further is D529's own precedent rather than anything
//! borrowed from the reference.
//!
//! # The client
//!
//! `post` builds its own `reqwest::Client` bound to `reply_to`, exactly
//! the way `crate::subagent::Socket::open` builds one for the ordinary
//! socket crossing — per-send rather than cached (`subagent.rs`'s own doc
//! says why: nothing here is long-lived enough to be worth pooling). No new
//! dependency: `reqwest`'s `unix_socket` builder method is already this
//! crate's, and the crates-registry standing rule is satisfied by that
//! reuse rather than a fresh search.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use ganja_protocol::{HeldId, HeldOutcome, PeerMessageId, PeerReceiptStatus};

use crate::subagent::{ReceiptStatus, SocketReceipt};

/// How many outstanding sends this session tracks at once — v2's own
/// number (v2 §"Receipts and sender UX"). Oldest evicted first, the hold
/// buffer's own shape reused for the same bounded-volatile reason (P4).
const OUTSTANDING_CAP: usize = 200;

/// How many held-entry associations the receiver side tracks at once.
/// Ganja-inferred: v2 records no number for this side because it has no
/// counterpart to it (ganja's association exists only because `hold()`
/// stays byte-unchanged rather than growing an id field). Mirrors
/// `crate::teammate::inbound`'s own `HELD_CAP` of 100 — an association is
/// only ever created alongside a held entry, so the hold buffer's own cap
/// already bounds how many can exist, and this constant just states that
/// bound on this side rather than reading the other module's.
const ASSOCIATION_CAP: usize = 100;

/// How long one attempt at a receipt POST may take. A local socket answers
/// in milliseconds or not at all, so this bounds a hang, not a budget a
/// healthy exchange approaches — shorter than `crate::subagent`'s own
/// ordinary crossing, because a receipt is best-effort and must never make
/// a settlement wait on a wedged third party.
const RECEIPT_DEADLINE: Duration = Duration::from_secs(2);

/// The route a settlement rides, on the **sender's** socket — the fourth
/// and only new entry in `ganja-serve`'s socket route table, named in that
/// table's own doc as the deliberate edit its contract requires.
const RECEIPT_ROUTE: &str = "/peer/receipt";

/// The scheme and host every socket request is spelled under — the same
/// label `crate::subagent`'s own crossing uses. `reqwest` resolves nothing
/// when a client is bound to a socket, so this is a label, never an
/// address.
const RECEIPT_URL: &str = "http://ganja";

/// The most code points of peer-authored text (`to`, echoing what a far
/// session's answer named) [`rendered`] will show —
/// `crate::teammate::identity`'s own bound, applied again here because this
/// is a second model-facing surface reading the same self-asserted bytes.
const SHOWN_CAP: usize = 256;

/// One send this session is still waiting to hear the fate of — registered
/// only when the synchronous answer said held and this session emitted a
/// `reply_to` (**AC-27**, **AC-32**).
#[derive(Debug)]
struct Outstanding {
    message_id: PeerMessageId,
    to: String,
}

/// One held entry's receiver-side association: the message it holds, and
/// where to answer (**N3**).
#[derive(Debug)]
struct Association {
    message_id: PeerMessageId,
    reply_to: PathBuf,
}

/// One receipt this session learned about a send it made, ready to fold
/// into an [`Event::PeerReceipt`](ganja_protocol::Event::PeerReceipt) and
/// into [`rendered`]'s batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    /// The sender's own id for the settled message.
    pub id: PeerMessageId,
    /// Who it was sent to — the same identity `Sent.to`'s far-side answer
    /// already carries.
    pub to: String,
    /// How it settled.
    pub status: PeerReceiptStatus,
}

#[derive(Debug, Default)]
struct SenderState {
    /// Oldest first — the eviction order at [`OUTSTANDING_CAP`].
    outstanding: VecDeque<Outstanding>,
}

#[derive(Debug, Default)]
struct ReceiverState {
    /// Oldest first — the eviction order at [`ASSOCIATION_CAP`]. A
    /// `VecDeque` of pairs rather than a map, the same linear-scan idiom
    /// `crate::teammate::inbound`'s own hold buffer uses (`position_of`),
    /// sized for the same reason: caps this small make a scan cheaper than
    /// the bookkeeping a map's invariants would cost.
    associations: VecDeque<(HeldId, Association)>,
}

/// Both halves of D534's receipt state: the sender-side outstanding-id
/// registry, and the receiver-side `HeldId` association — engine-owned,
/// one instance per session, every method `&self` over its own lock. No
/// method holds a guard across an `await`: [`Receipts::settle_and_post`]
/// takes the association's fields under the lock, drops the guard, and
/// only then awaits the POST.
#[derive(Debug, Default)]
pub struct Receipts {
    sender: Mutex<SenderState>,
    receiver: Mutex<ReceiverState>,
}

impl Receipts {
    /// An empty registry: nothing outstanding, nothing associated.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // ---------------------------------------------------------------
    // Sender side
    // ---------------------------------------------------------------

    /// Registers one outstanding send — **only** when the synchronous
    /// answer said held and this session emitted a `reply_to`
    /// (**AC-27**, **AC-32**): an unbound sender, an accepted send, and a
    /// refused or guard-dropped send all register nothing. Evicts the
    /// oldest entry first at `OUTSTANDING_CAP`, the hold buffer's own
    /// shape.
    pub fn register(
        &self,
        message_id: PeerMessageId,
        to: String,
        held: bool,
        reply_to: Option<&Path>,
    ) {
        if !held || reply_to.is_none() {
            return;
        }
        let mut state = self.lock_sender();
        if state.outstanding.len() >= OUTSTANDING_CAP {
            state.outstanding.pop_front();
        }
        state.outstanding.push_back(Outstanding { message_id, to });
    }

    /// Settles one outstanding send against an inbound [`SocketReceipt`]
    /// (**AC-26**): an id this session actually registered removes the
    /// entry and answers `Some`; an unknown id, a settled id, or a second
    /// terminal for the same id no-ops and answers `None` — the same
    /// answer in all three cases, so the caller's own route can stay
    /// byte-identical regardless of which applies.
    #[must_use]
    pub fn settle_sent(
        &self,
        message_id: &PeerMessageId,
        status: ReceiptStatus,
    ) -> Option<Settled> {
        let mut state = self.lock_sender();
        let position = state
            .outstanding
            .iter()
            .position(|outstanding| &outstanding.message_id == message_id)?;
        let outstanding = state.outstanding.remove(position)?;
        Some(Settled {
            id: outstanding.message_id,
            to: outstanding.to,
            status: peer_status_of(status),
        })
    }

    /// Forgets every outstanding send — `NewSession`'s own door, the pin
    /// map's and the inbound chain's own precedent (**AC-27**).
    pub fn clear_sent(&self) {
        self.lock_sender().outstanding.clear();
    }

    // ---------------------------------------------------------------
    // Receiver side
    // ---------------------------------------------------------------

    /// Associates one held entry with the message it holds and where to
    /// answer, at admission time (**N3**). Additive over
    /// [`SocketAdmission::Held`](crate::teammate::inbound::SocketAdmission::Held)'s
    /// own `id`, never on `HeldMessage` itself (`inbound.rs`'s own record,
    /// private by design). Evicts the oldest association first at
    /// `ASSOCIATION_CAP`.
    pub fn associate(&self, held_id: HeldId, message_id: PeerMessageId, reply_to: PathBuf) {
        let mut state = self.lock_receiver();
        if state.associations.len() >= ASSOCIATION_CAP {
            state.associations.pop_front();
        }
        state.associations.push_back((
            held_id,
            Association {
                message_id,
                reply_to,
            },
        ));
    }

    /// Settles one held entry's receipt, best-effort. Meant to be called
    /// from exactly three places — a person's approve, a person's deny,
    /// and the `dialog_expiry` timer's fire — and from nowhere else: a
    /// capacity eviction and the shutdown drain settle their victim
    /// entirely inside `crate::teammate::inbound`, with no call here at
    /// all, which is what keeps them silent (N1, D3; see the module doc's
    /// reflector paragraph). A `held_id` with no association — a restart,
    /// a sender with no `reply_to`, an entry this map's own cap already
    /// evicted — posts nothing, exactly like every other unreachable case
    /// this route produces.
    pub async fn settle_and_post(&self, held_id: &HeldId, outcome: HeldOutcome) {
        let association = {
            let mut state = self.lock_receiver();
            let position = state.associations.iter().position(|(id, _)| id == held_id);
            position
                .and_then(|position| state.associations.remove(position))
                .map(|(_, association)| association)
        };
        let Some(association) = association else {
            return;
        };
        post(
            &association.reply_to,
            association.message_id,
            wire_status_of(outcome),
        )
        .await;
    }

    fn lock_sender(&self) -> MutexGuard<'_, SenderState> {
        self.sender
            .lock()
            .expect("the receipts registry's sender lock is never poisoned")
    }

    fn lock_receiver(&self) -> MutexGuard<'_, ReceiverState> {
        self.receiver
            .lock()
            .expect("the receipts registry's receiver lock is never poisoned")
    }
}

/// [`HeldOutcome`]'s three settlement values, exactly as they cross the
/// wire.
fn wire_status_of(outcome: HeldOutcome) -> ReceiptStatus {
    match outcome {
        HeldOutcome::Delivered => ReceiptStatus::Delivered,
        HeldOutcome::Denied => ReceiptStatus::Denied,
        HeldOutcome::Expired => ReceiptStatus::Expired,
    }
}

/// The wire's [`ReceiptStatus`] translated into the display vocabulary
/// [`Event::PeerReceipt`](ganja_protocol::Event::PeerReceipt) and
/// [`rendered`] read — a plain match rather than a `From` impl, since
/// orphan rules forbid implementing a foreign trait (`From`) for a foreign
/// type (`PeerReceiptStatus`) from this crate.
fn peer_status_of(status: ReceiptStatus) -> PeerReceiptStatus {
    match status {
        ReceiptStatus::Delivered => PeerReceiptStatus::Delivered,
        ReceiptStatus::Denied => PeerReceiptStatus::Denied,
        ReceiptStatus::Expired => PeerReceiptStatus::Expired,
    }
}

/// One attempt, best-effort, never a retry: `reply_to` is vetted through
/// [`crate::tool::socket::vet_address`] before anything opens, and any
/// failure past that — a dead socket, a timeout, an unreadable answer — is
/// a trace line naming the id and the status only (**AC-10**'s rule: no
/// bodies, no `reply_to` paths), never a reason the delivery it describes
/// should reverse (**AC-30**).
async fn post(reply_to: &Path, message_id: PeerMessageId, status: ReceiptStatus) {
    if let Err(refusal) = crate::tool::socket::vet_address(reply_to) {
        tracing::debug!(
            id = message_id.as_str(),
            ?status,
            %refusal,
            "a receipt's reply_to failed vetting; not opened"
        );
        return;
    }

    let client = match reqwest::Client::builder()
        .unix_socket(reply_to)
        .timeout(RECEIPT_DEADLINE)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!(
                id = message_id.as_str(),
                ?status,
                %error,
                "a receipt client could not be built"
            );
            return;
        }
    };

    let id = message_id.as_str().to_owned();
    let body = SocketReceipt { message_id, status };
    if let Err(error) = client
        .post(format!("{RECEIPT_URL}{RECEIPT_ROUTE}"))
        .json(&body)
        .send()
        .await
    {
        tracing::debug!(
            id,
            ?status,
            %error,
            "a receipt post failed; the delivery it describes stands regardless"
        );
    }
}

/// The tag a batch of receipts is wrapped in — D529's own vehicle
/// (`session.rs`'s `user_message` seam), so a settlement reaches the model
/// exactly the way an `@`-mention reminder does.
pub const TAG: &str = "peer_receipt";

/// One `<peer_receipt>`-tagged block naming every receipt this session
/// learned since its last prompt intake — the batch's own rendering,
/// byte-pinned so a test compares against this function rather than a
/// second copy of its words (**AC-29**).
///
/// Ordinary conversation text: unlike a peer's own words, a receipt is
/// ganja's own sentence about ganja's own send, so nothing here is
/// display-only or exempt from `Part::as_text`.
#[must_use]
pub fn rendered(batch: &[Settled]) -> String {
    let mut lines = Vec::with_capacity(batch.len() + 2);
    lines.push(format!("<{TAG}>"));
    for settled in batch {
        lines.push(format!(
            "- message {} to {:?}: {}",
            short_id(&settled.id),
            neutralized(&settled.to),
            sentence(settled.status)
        ));
    }
    lines.push(format!("</{TAG}>"));
    lines.join("\n")
}

/// The first eight characters of a message id — "the ids are rendered
/// short" (D534) — long enough to tell a small batch's rows apart without
/// repeating a full id the model has no use for. Cut on a character
/// boundary, not a byte count: this session mints ASCII v7 ids, but
/// [`PeerMessageId`] wraps any string a wire hands it, and a rendering must
/// not be the thing that panics on one.
fn short_id(id: &PeerMessageId) -> &str {
    let id = id.as_str();
    match id.char_indices().nth(8) {
        Some((cut, _)) => &id[..cut],
        None => id,
    }
}

/// One settled status, in the sentence [`rendered`] shows it with.
fn sentence(status: PeerReceiptStatus) -> &'static str {
    match status {
        PeerReceiptStatus::Delivered => "delivered",
        PeerReceiptStatus::Denied => "denied",
        PeerReceiptStatus::Expired => "the review window ran out before anyone decided",
    }
}

/// Strips control characters and this rendering's own frame delimiters
/// before framing peer-authored text (`to`, echoed off a far session's
/// answer) — the same move `crate::teammate::identity`'s mention reminder
/// makes, for the same reason: content that could end its own frame is
/// neutralized *before* it is framed, never after.
fn neutralized(value: &str) -> String {
    let admits = |point: &char| !point.is_control() && *point != '<' && *point != '>';
    let kept: String = value.chars().filter(admits).take(SHOWN_CAP).collect();
    if value.chars().filter(admits).count() > SHOWN_CAP {
        format!("{kept}…")
    } else {
        kept
    }
}

#[cfg(test)]
#[path = "receipts_tests.rs"]
mod tests;
