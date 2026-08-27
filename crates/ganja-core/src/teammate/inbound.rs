//! The receiver-side cross-session admission gate (**D523**, **D524**,
//! **D525**): what a session that leads a team does with a peer message
//! arriving from **outside** that team, before anything is delivered.
//!
//! The specification is Claude Code's cross-session admission behavior, cited
//! throughout as `v2 §"<section>", evidence <ranges>`; the plan is
//! `.omc/plans/2026-08-25-cross-session-inbound-admission.md`. Upstream
//! opencode has no cross-session messaging at all, so nothing here is a
//! TypeScript port. The port is **policy, not transport**: the socket scheme,
//! the binder, the liveness lock and the validation ladder are untouched, and
//! this module sits between the ladder and the inbox write on one door, and
//! between the inbox read and the conversation on the other.
//!
//! **Engine-free on purpose.** This file names no engine, holds no publisher
//! and awaits nothing. Everything the engine must do with a decision —
//! the mailbox write a release asks for, the inbox prune a deny asks for,
//! the timer a parity hold's deadline asks for, the event publication —
//! leaves this module as **data**: a returned [`Settlement`](crate::teammate::inbound::Settlement), a
//! [`SocketAdmission`](crate::teammate::inbound::SocketAdmission)/[`MailboxAdmission`](crate::teammate::inbound::MailboxAdmission), a deadline on a record, and the
//! ordered [`HoldTransition`](crate::teammate::inbound::HoldTransition) queue the engine drains into its own fanout.
//! Admission decides what the model *hears*; permission decides what the
//! model *does* — the two engines stay separate, and a peer message that has
//! been admitted still crosses every permission dialog its requests raise
//! (v2 §"Active versus idle delivery", evidence 622121-622134).
//!
//! # The resolver, and the parity matrix
//!
//! An explicit `cross_session_inbound` value **always wins** and is resolved
//! before the matrix is consulted — [`ResolvedInbound::decide`](crate::teammate::inbound::ResolvedInbound::decide)'s first
//! branch, so `self_sent` can never override it (v2 §"Explicit values",
//! evidence 680146-680160; v2 §"Cross-pass reconciliation" verdict 7,
//! evidence 620535-620560). Unset, [`decide_unset`](crate::teammate::inbound::decide_unset) carries the full parity
//! matrix (v2 §"The parity matrix, and when it actually applies", evidence
//! 620535-620617) under `honor_sender_mode = true`, and the collapsed
//! two-row path under `false`: prompting receiver → accept, bypass receiver
//! → hold `no_mode_asserted`, the sender class never consulted (v2
//! §"Cross-pass reconciliation" verdict 6, evidence 620525-620531).
//!
//! **D532 gave `SocketMessage` a real `from_mode`, and the matrix is now
//! reached through a never-loosen composition rather than a flag flip**
//! (Axis 5, **OQ2(a)**). `HONOR_SENDER_MODE` stays `false` as the *floor* —
//! see its own doc — and [`Inbound::admit_socket`](crate::teammate::inbound::Inbound::admit_socket)'s production call site
//! asks [`ResolvedInbound::decide`](crate::teammate::inbound::ResolvedInbound::decide) for the answer **twice**, once
//! collapsed (`honor_sender_mode = false`) and once honored (`= true`), and
//! keeps the **stricter** of the two (`strictest_of`): refuse outranks
//! hold outranks accept, so no wire field can ever *grant* — the composed
//! verdict's severity is always at least the collapse's own. Exactly one
//! row moves under this composition (`prompting` receiver, `bypass`
//! sender: accept → hold `mode_mismatch`), and one row is a **recorded
//! divergence from v2**: `(bypass, bypass)` stays held rather than
//! accepting, because an unproven wire attestation may not do a
//! credential's work (P2). `decide_unset`'s eight rows are reached exactly
//! as they always were — this composition sits **above** them, at the one
//! production call site, and changes nothing about the function itself.
//!
//! Two arms exist before their producers do, which is what fail-closed
//! means:
//!
//! - `self_sent` is an **always-false input at every production call site**:
//!   ganja holds no kernel peer identity at the route — the acceptor reads
//!   the peer uid and drops it, and no pid reaches the handler — so absence
//!   at a bypass receiver holds, which is conservative and honest. The
//!   kernel peer-pid plumbing and the ancestry walk are a follow-up bead
//!   (the plan's W4 ledger step files it), probe-first because the
//!   reference's own walk is macOS-gated with the Linux implementation
//!   untraced (v2 §"Own-child verification (`selfSent`)", evidence
//!   153877-153889; v2 §"Gaps left open by all four passes").
//! - An unreadable receiver mode holds `mode_unknown` (v2 §"Receiver
//!   permission classes"). Structurally unreachable while the mode is plain
//!   engine state — the arm is unit-tested directly. The unreadable-receiver
//!   hold outranks `self_sent` here, fail-closed: v2's matrix listing does
//!   not pin the ordering between those two arms, and holding is the
//!   conservative reading.
//!
//! # Ingress doors, and the classes with no counterpart
//!
//! Two doors reach this gate. The **socket door** ([`Inbound::admit_socket`](crate::teammate::inbound::Inbound::admit_socket))
//! is the canonical normal peer — full policy, then the full guard tier. The
//! **mailbox door** ([`Inbound::admit_mailbox`](crate::teammate::inbound::Inbound::admit_mailbox)) is the fail-closed demotion:
//! a lead-inbox entry from a writer on no roster and in no admitted set is
//! treated as a peer from `unknown` and run through the normal peer gate,
//! exactly the reference's demotion of ambiguous input (v2 §"Bridge ingress
//! classification", evidence 615737-615836, step 5's shape) — with the
//! unidentified guard tier, hop and queue-cap only (v2 §"Guard eligibility",
//! evidence 415199-415243). Roster team mail and the person's own input are
//! ungated and never reach this module: classification of *who wrote an
//! entry* stays with the caller, and unclassifiable means demoted, never
//! delivered-by-default. CC's **host-injected** and **coordinator
//! `peer-send-message`** classes (v2 §"Coordinator exception", evidence
//! 620685-620760; v2 §"Ingress-class matrix") have **no ganja counterpart**
//! — ganja has no host to inject and no coordinator backend — and nothing in
//! this code carries their names.
//!
//! # The kill-switch analogue, and the divergence
//!
//! CC centralizes availability in one predicate and, with it off, the
//! normal-peer resolver fails closed to refuse with cause `kill-switch`
//! (v2 §"Bundle gate (`Hg()`)", evidence 220730-220742, 620483-620492), a
//! check belonging to the normal-peer gate alone (v2 §"Kill switch, and its
//! asymmetry", evidence 620483-620530, 620720-620768). Ganja has no feature
//! flag and needs none: the socket exists only for a lead session, so
//! "feature off" is structural, and the `NoTeam` refusal at the top of the
//! engine's deliver arm plays the fail-closed role — which is why
//! [`RefuseCause`](crate::teammate::inbound::RefuseCause) here carries no kill-switch variant. Two divergences:
//! `NoTeam` answers a visible `404` where CC's kill-switch refuse is silent
//! (the socket's very existence already signals a lead session, and
//! fabricating an accept for a session that can never deliver would be a lie
//! with no privacy gained); and CC's asymmetry maps trivially, because
//! ganja's only gated classes both live behind a team's existence. An
//! explicit `refuse` drops inbound **without unbinding the socket** (v2
//! §"When the inbox is not bound", final paragraph).
//!
//! # Refuse is indistinguishable from accept, and the timing channel
//!
//! A policy refuse — and every guard drop — is answered **byte-identically**
//! to an accept, because CC's refuse path emits no receipt ("refused
//! messages do not notify the sender", v2 §"Explicit outcomes (`P8a`)",
//! evidence 620644-620683), while a hold **is** announced with its cause,
//! CC's own held receipt (v2 §"Receipts and sender UX", evidence
//! 220977-221015). The info-leak rationale: the socket is reachable by every
//! same-uid process, and a distinct refuse answer would let any of them
//! enumerate which sessions have inbound refused and hand a sender's model a
//! signal to retry against. A residual **timing** side channel remains — an
//! accept performs a mailbox write, a refuse does not — and is named rather
//! than papered over: CC's paths differ in work done too, and equalizing
//! timing against a same-uid observer is not a boundary this transport can
//! hold. [`SocketAdmission::Silent`](crate::teammate::inbound::SocketAdmission::Silent) is that contract spelled as a type.
//!
//! # The hold buffer: volatile by design, and the one-door asymmetry
//!
//! Held state lives in one process-local, in-memory buffer — **a crash loses
//! it; by design**. CC keeps held state on a process-global in-memory array
//! with no disk store (v2 §"Storage and capacity", evidence 220788-220818),
//! and ganja says the same in its own words: a durable held *store* would
//! make a crash deliver later what a person never reviewed, and would put
//! unreviewed foreign text at rest on disk. What differs per door is where
//! the message's **bytes** live (C1):
//!
//! - a **socket-door** hold is memory-only — the hold sits strictly in front
//!   of the mailbox write, so the bytes exist nowhere but the record;
//! - a **mailbox-door** hold leaves the entry in the durable inbox and
//!   tracks its identity in the in-memory held-index — the record is the
//!   review copy, and an unsettled hold (crash, shutdown, never reviewed) is
//!   neither lost nor delivered: the entry survives and is fail-closed
//!   re-gated at next start under then-current policy.
//!
//! Cap **100**, counting records of both doors (v2 §"Storage and capacity",
//! evidence 620883-620886; v2 §"Cross-pass reconciliation" verdict 13). The
//! 101st entry evicts the **oldest**, settled `expired` before the newcomer
//! appends — and an evicted mailbox-door hold is additionally pruned from
//! the inbox (the prune surfaced as data on the admission result), never
//! left to re-gate: an evicted-but-kept entry would re-enter the gate on the
//! next pass as freshly demoted, re-hold, and evict the then-oldest — a
//! rotating livelock that makes the cap meaningless. Eviction is a *decided*
//! capacity outcome; the no-lost-mail property is scoped to *undecided* ends.
//!
//! The record keeps the **control-plane/model-visible split** even though
//! ganja's wire carries no hop data yet: origin metadata — the door, the
//! asserted sender class, `self_sent` — lives beside the body, and nothing
//! ever composes it into the text (v2 §"Hop metadata is retained
//! separately", evidence 153248-153285, 415199-415235, applied
//! pre-emptively: a future sender's hop data lands on the control-plane side
//! by construction). The hop chain itself is consumed by the guard at the
//! door and not stored — today it is empty at every production call site.
//!
//! # Expiry, settlement, and first-settler-wins
//!
//! Per-record deadlines exist **only** for the parity causes
//! (`mode_mismatch`, `no_mode_asserted`); an explicit or `mode_unknown` hold
//! carries none and sits until policy change, capacity eviction, manual
//! settlement or shutdown. That narrowness is the reconciliation's most
//! operationally surprising verdict and is law (v2 §"Cross-pass
//! reconciliation", the expiry re-check, evidence 1227895-1227987,
//! 1265450-1265503); the deadline value is the curated `dialog_expiry` key
//! ([`crate::config::DialogExpiry`]), whose env override
//! (`CLAUDE_CODE_USER_DIALOG_TIMEOUT_MS`) is deliberately not ported —
//! ganja's environment surface is curated. CC's rearm-while-detached
//! behavior (evidence 154916-154928, 1227991-1228000) has no counterpart:
//! ganja's TUI has no detach. The timer task itself is the engine's — this
//! module computes the deadline, [`Inbound::expire`](crate::teammate::inbound::Inbound::expire) is what the timer
//! calls, and engine-side timers make expiry independent of any frontend.
//! Today only a TUI session leads a team, so the unattended case is a
//! team-leading session whose person is away; the way to run unattended on
//! purpose is an explicit `cross_session_inbound: "accept"` in a trusted
//! tier, never a bypass flag.
//!
//! Settlement outcomes are exactly `delivered` / `denied` / `expired` (v2
//! §"Receipts and sender UX" statuses minus `held`, the initial answer).
//! [`Inbound::release`](crate::teammate::inbound::Inbound::release) **re-checks the current policy first** — an approval
//! cannot override a policy that has since become refuse (v2 §"Reevaluation
//! and manual decision", evidence 620847-620877); a mode change re-decides
//! every held entry under its own recorded origin ([`Inbound::reevaluate`](crate::teammate::inbound::Inbound::reevaluate),
//! same section, evidence 620778-620845), and an entry whose verdict still
//! holds stays held **with its original cause and deadline** — v2 pins
//! re-evaluation's release and deny arms, not a re-causing, so this port
//! never rewrites a standing hold. Shutdown settles everything `expired`
//! ([`Inbound::shutdown_settle`](crate::teammate::inbound::Inbound::shutdown_settle); the reference bounds the wait, v2
//! §"Shutdown", evidence 620390-620431 — the bound lives with the engine's
//! teardown, which owns the waiting); a mailbox-door entry is deliberately
//! left in the inbox for next-start re-gating, while a socket-door hold is
//! gone with the process. A crash provides no settlement at all — the same
//! split applies.
//!
//! Every settlement is a first-settler-wins transition keyed by [`HeldId`](ganja_protocol::HeldId)
//! under one mutex: later settlers find the id gone — or claimed — and
//! no-op, and a settle naming an id nobody holds is ignored. Two orderings
//! are pinned against fallible IO:
//!
//! - **H2** — a mailbox-door drop prunes **first** and unindexes only after
//!   the prune succeeds ([`Settlement::PruneFirst`](crate::teammate::inbound::Settlement::PruneFirst), completed by
//!   [`Inbound::pruned`](crate::teammate::inbound::Inbound::pruned) or abandoned by [`Inbound::prune_failed`](crate::teammate::inbound::Inbound::prune_failed)): a
//!   failed prune leaves the identity indexed and the record held —
//!   fail-closed re-hold, retryable — where the inverse order would leave a
//!   durable, unindexed entry the next pass re-gates as fresh, so a denied
//!   message could resurrect as a new ask or, under an accept policy,
//!   deliver.
//! - **H1** — a mailbox-door release delivers the **hold-time summary
//!   snapshot**, never the entry's current one: the mailbox identity is
//!   `from|timestamp|text`, so `summary` sits outside the key and a same-uid
//!   writer could swap it in the durable entry under an unchanged identity
//!   between review and delivery. The text needs no such copy — mutating it
//!   changes the identity, the indexed identity vanishes, the record settles
//!   `expired` and the mutated bytes re-gate as fresh.
//!
//! # The guards, and what they are not
//!
//! After policy accepts and before the write — `accept` is necessary but not
//! sufficient (v2 §"Post-policy queue admission" preamble) —
//! [`PeerGuard::admit`](crate::teammate::inbound::PeerGuard::admit) runs. Since **D534** (N1, D1) it also runs
//! ahead of a *parity-cause* hold ([`HoldCause::ModeMismatch`](ganja_protocol::HoldCause::ModeMismatch), [`HoldCause::NoModeAsserted`](ganja_protocol::HoldCause::NoModeAsserted)) —
//! never an explicit or `mode_unknown` one, which this landing routes no
//! new traffic onto — using the same [`Origin`](crate::teammate::inbound::Origin) the accept path would have
//! built, so a would-be hold a same-uid writer floods is bucket-limited,
//! deduplicated and hop-checked exactly like an accept, and a guard-dropped
//! would-be hold answers byte-identically to one. `PeerGuard::admit` itself
//! is unchanged by any of this: what moved is where it is *called from*,
//! never what it does. `admit` returns exactly four reasons
//! ([`Dropped::HopRunaway`](crate::teammate::inbound::Dropped::HopRunaway), [`Dropped::HopLoop`](crate::teammate::inbound::Dropped::HopLoop), [`Dropped::Duplicate`](crate::teammate::inbound::Dropped::Duplicate),
//! [`Dropped::RateLimited`](crate::teammate::inbound::Dropped::RateLimited)); the 50-message queue cap is a **separate
//! enqueue-time test**, not an `admit` reason (v2 §"Cross-pass
//! reconciliation" verdict 5, evidence 415499), kept in a visibly separate
//! function so the distinction survives refactoring. Three eligibility
//! tiers (v2 §"Guard eligibility", evidence 415199-415243): the socket peer
//! is provenance-qualified and takes every guard; the demoted writer is
//! unidentified and takes hop and queue-cap only; non-peer origins never
//! reach the guard at all. The sender key is `from:<identity>` — and even a
//! qualified key is a **resource-control bucket, not an authenticated
//! identity boundary**, because ganja's `from` is sender-authored exactly as
//! CC's is (v2 §"Guard eligibility"; v2 §"Executive conclusion": the
//! envelope's `from` is sender-authored routing data, not an authenticated
//! principal).
//!
//! The hop checks were real logic over an empty chain until **D532**: ganja's
//! wire now carries a real `hop_chain` and `own_marker` at the socket door
//! (`admit_socket`'s [`Origin`](crate::teammate::inbound::Origin)), so both checks are live in production —
//! the mailbox door still passes `&[]`/`None`, stated rather than defaulted,
//! since a demoted writer's entry crossed no socket and carries no chain at
//! all. Two readings are **ganja-inferred and marked so** (M5): v2 pins the
//! constants, not the
//! comparator, so exceeding-drops (29 drops where 28 passes, 11 own-marker
//! occurrences drop where 10 pass) is this port's reading; and the
//! 256-tracked-sender bound is evicted least-recently-used, v2 recording
//! only the bound. The intra-`admit` check order (hop, then dedup, then
//! bucket, the dedup hash recorded only on success) is likewise ganja's own:
//! a duplicate does not spend a token, and a rate-limited retry of the same
//! body is not later mistaken for a duplicate. The **sender-side**
//! serialized hop cap of 32 (v2 §"Hop chain: two different caps", evidence
//! 153301-153329) has no ganja site because ganja emits no hops — recorded
//! here so nobody later reads 28 as "the" hop limit, the exact error the
//! reconciliation names: 28 is the RECEIVER threshold (v2 §"Hop chain: two
//! different caps", verdict 4).
//!
//! # Events: the ordered forwarder, and no bodies anywhere
//!
//! Hold and settle transitions are enqueued — under the same lock that made
//! the state change, so `PeerHeld` is strictly before its own
//! `PeerHoldSettled` — onto one in-order queue the engine's own task drains
//! into the real publisher (M10). Decoupled from the request path on
//! purpose: a lossless subscriber makes a publisher wait, and publishing
//! inside the socket door would let one wedged SSE client delay — and
//! jitter — every peer's POST, on a route whose latency is another
//! process's observable. What a stalled drain delays is event visibility,
//! never a decision. Body-bearing fields ride
//! [`RedactedText`](ganja_protocol::RedactedText) (M4), whose `Debug` prints a size and never the text,
//! and this module's tracing is the caller's to write from the typed
//! reasons, identities and ids these types carry — never bodies.
//!
//! # The authority boundary
//!
//! Peer text is another model's text with none of the receiving user's
//! authority (v2 §"Model visibility and the authority boundary", evidence
//! 561383-561413, 665973-665977). This module adds the admission decision
//! and keeps every pin the codebase already holds: `PartBody::Peer` is
//! excluded from `Part::as_text`, nothing frame-shaped crosses the socket,
//! and dialogs are answerable only by the person or a `LeadFrame`. Peer
//! slash commands cannot fire, by construction (M9): v2 records that CC's
//! peer text is not unconditionally inert (v2 §"Slash-command handling"),
//! and ganja needs no such carve-out analysis, because peer text arrives
//! only as `PartBody::Peer`, which never reaches the composer's command
//! parse.
//!
//! # Deliberately not implemented
//!
//! - **Sender-side `from-mode` and `hop-chain` emission, and receipts beyond
//!   the synchronous HTTP response** all **landed with D532/D534** (this
//!   plan) — the three items this list used to name here are built, at
//!   [`crate::subagent::SocketMessage`]/[`crate::teammate::receipts`], and
//!   not repeated as absences.
//! - **A durable held store** — process-local by design (v2 §"Storage and
//!   capacity"); durability would deliver unreviewed text after a crash.
//! - **The `selfSent` ancestry walk** — the gap note above; probe-first
//!   follow-up bead.
//! - **UUID replay dedup** — single-sourced in the reference (v2
//!   §"Single-sourced claims to treat as provisional", evidence
//!   1270714-1270767) and ganja's wire carries no message id; per the
//!   provisional rule, nothing is built on it.
//! - **Coordinator / host-injected ingress classes** — no ganja counterpart
//!   exists to classify.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    sync::Mutex,
    time::Duration,
};

use ganja_protocol::{
    Event, HeldId, HeldOutcome, HoldCause, PermissionMode, PolicySource, RedactedText, SessionId,
    team::{cap_chars, cap_for_display},
};
use ganja_team::{MailboxMessage, mailbox};
use tokio::{sync::mpsc, time::Instant};

use crate::config::{DialogExpiry, InboundPolicy};

// module-private constants — not public configuration (v2 §"Guard limits":
// "they are not public configuration fields")
const BUCKET_CAPACITY: f64 = 30.0; // v2 §"Guard limits", evidence 414939-414943
const REFILL_PER_SECOND: f64 = 0.5; //   ditto — one sustained message per 2 s
const DEDUP_WINDOW: Duration = Duration::from_secs(30); //   ditto (30000 ms)
const MAX_SELF_HOPS: usize = 10; //   ditto
const MAX_CHAIN_LENGTH: usize = 28; //   ditto; the RECEIVER threshold —
//   v2 §"Hop chain: two different caps" (verdict 4)
const TRACKED_SENDERS: usize = 256; // v2 §"Guard limits" table, evidence 414851-415090
const MAX_QUEUED_PEERS: usize = 50; // SEPARATE enqueue-time test, evidence 415452-415520

/// How many records the hold buffer keeps, counting **both doors** — v2
/// §"Storage and capacity", evidence 620883-620886; constant confirmed, v2
/// §"Cross-pass reconciliation" verdict 13.
const HELD_CAP: usize = 100;

/// The expandable body preview's caps: 8 lines / 1024 characters, ganja's
/// own numbers (user-ratified 2026-08-25 — v1-only supplementary fact (b)
/// records the caps' existence but no values).
const PREVIEW_LINES: usize = 8;
/// The character half of the preview cap; see [`PREVIEW_LINES`].
const PREVIEW_CHARS: usize = 1024;

/// The sender-mode flag's value as `decide_unset`'s own **collapsed**
/// branch — one half of the never-loosen composition (Axis 5, **OQ2(a)**),
/// not a switch [`Inbound::admit_socket`](crate::teammate::inbound::Inbound::admit_socket) flips.
///
/// v2's sender emits `from-mode` only when a flag whose embedded default is
/// off is on (v2 §"Sender mode is feature-gated", evidence 620372-620375,
/// 623133-623145), so a real deployment's traffic is usually unattested —
/// **"Treat the matrix as the specification and the two-row collapse as the
/// likely observed behavior, and check the flag before relying on
/// either."** (v2 §"The parity matrix, and when it actually applies",
/// evidence 620535-620617, quoted whole rather than truncated at the half
/// that suits the argument). Since **D532/D534** ganja's own production
/// call site checks both: it asks [`ResolvedInbound::decide`](crate::teammate::inbound::ResolvedInbound::decide) once with
/// this constant (`false`, the collapse) and once with `true` (the honored
/// eight-row matrix), and keeps the **stricter** answer
/// (`strictest_of`). This constant therefore stays the *floor* the
/// composition can never fall below — it is never itself the whole
/// decision — and the `true` half of [`decide_unset`](crate::teammate::inbound::decide_unset) is fully reachable
/// through the honored call, not dormant specification.
const HONOR_SENDER_MODE: bool = false;

/// This session's permission class, as the parity matrix reads it (**D523**).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverClass {
    /// The rules decide and dialogs ask — every session not started
    /// otherwise.
    Prompting,
    /// The D479 trio, or [`PermissionMode::Bypass`]: dialogs answer
    /// themselves.
    Bypass,
}

/// A sender's asserted class — the envelope's two tokens (v2 §"Receiver
/// permission classes"); no ganja wire carries one yet, so only tests and
/// the matrix function's signature build one (**D523**).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenderClass {
    /// The sender says it prompts.
    Prompting,
    /// The sender says it bypasses.
    Bypass,
}

/// Why the resolver refused, where it did (**D523**).
///
/// One variant, deliberately: the parity matrix's rows accept or hold, never
/// refuse, so the only refuse this resolver can produce is a configured one
/// — and the reference's other refuse, the kill switch, has no ganja
/// counterpart (the structural `NoTeam` refusal is the engine's, upstream of
/// this gate; see the module doc's divergence note).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefuseCause {
    /// A configured `cross_session_inbound: "refuse"` decided.
    Explicit {
        /// The config tier the explicit policy came from.
        source: PolicySource,
    },
}

/// What the resolver decided about one inbound message (**D523**).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Deliver it — subject to the guards, which run only on this path.
    Accept,
    /// Park it for a person's review.
    Hold(HoldCause),
    /// Drop it, telling the sender nothing.
    Refuse(RefuseCause),
}

/// Maps the engine's mode and the D479 startup seed onto a receiver class:
/// `Bypass` iff the mode is bypass or the session started under the trio;
/// everything else prompts.
///
/// Total over its inputs — the fail-closed `None` (an unreadable mode, held
/// `mode_unknown`) is the *resolver's* parameter, not this function's,
/// because these two inputs are plain engine state that always reads.
#[must_use]
pub fn classify_receiver(mode: PermissionMode, seeded_trio: bool) -> ReceiverClass {
    match mode {
        PermissionMode::Bypass => ReceiverClass::Bypass,
        PermissionMode::Ask if seeded_trio => ReceiverClass::Bypass,
        PermissionMode::Ask => ReceiverClass::Prompting,
    }
}

/// The parity matrix, unset policy's whole decision (**D523**): the full
/// eight rows under `honor_sender_mode = true`, the collapsed two rows —
/// the sender class never consulted — under `false`.
///
/// Explicit policy is resolved **before** this function
/// ([`ResolvedInbound::decide`]'s first branch), so nothing here can
/// override a configured value — including `self_sent` (v2 §"Cross-pass
/// reconciliation" verdict 7, evidence 620535-620560). The row order is the
/// module doc's: unreadable receiver holds first, fail-closed, then
/// `self_sent` accepts, then the honor split.
#[must_use]
pub fn decide_unset(
    receiver: Option<ReceiverClass>,
    sender: Option<SenderClass>,
    self_sent: bool,
    honor_sender_mode: bool,
) -> Verdict {
    let Some(receiver) = receiver else {
        // Fail-closed before anything else can vouch: a receiver that cannot
        // read its own mode holds even its own child's message.
        return Verdict::Hold(HoldCause::ModeUnknown);
    };
    if self_sent {
        return Verdict::Accept;
    }
    if !honor_sender_mode {
        // The collapsed path (v2 §"Cross-pass reconciliation" verdict 6,
        // evidence 620525-620531): `sender` is deliberately not read.
        return match receiver {
            ReceiverClass::Prompting => Verdict::Accept,
            ReceiverClass::Bypass => Verdict::Hold(HoldCause::NoModeAsserted),
        };
    }
    match (receiver, sender) {
        (ReceiverClass::Prompting, Some(SenderClass::Prompting)) => Verdict::Accept,
        (ReceiverClass::Prompting, Some(SenderClass::Bypass)) => {
            Verdict::Hold(HoldCause::ModeMismatch)
        }
        (ReceiverClass::Bypass, Some(SenderClass::Bypass)) => Verdict::Accept,
        (ReceiverClass::Bypass, Some(SenderClass::Prompting)) => {
            Verdict::Hold(HoldCause::ModeMismatch)
        }
        (ReceiverClass::Prompting, None) => Verdict::Accept,
        (ReceiverClass::Bypass, None) => Verdict::Hold(HoldCause::NoModeAsserted),
    }
}

/// Where one [`Verdict`] sits on the strictness ladder [`strictest_of`]
/// composes over: refuse outranks hold outranks accept.
const fn severity(verdict: &Verdict) -> u8 {
    match verdict {
        Verdict::Accept => 0,
        Verdict::Hold(_) => 1,
        Verdict::Refuse(_) => 2,
    }
}

/// The **never-loosen composition** (Axis 5, **OQ2(a)**): the stricter of
/// two verdicts decided for the same message, so no wire field can ever
/// *grant* — the result's severity is always at least each input's own,
/// which is what "never looser than the collapse" means as code rather
/// than as a sentence (**AC-49**).
///
/// A tie keeps `honored`'s cause: the two calls this composes differ only
/// in whether the sender's asserted mode was consulted at all, and the
/// honored answer is the more informative sentence when both land on the
/// same outcome (`decide_unset`'s `(Bypass, Some(Prompting))` row moves
/// `no_mode_asserted` → `mode_mismatch` this way, cause only). An explicit
/// policy answers identically on both branches — `ResolvedInbound::decide`
/// resolves it before `decide_unset` is ever reached — so the tie there is
/// a genuine no-op, not merely a harmless one.
#[must_use]
fn strictest_of(collapsed: Verdict, honored: Verdict) -> Verdict {
    if severity(&honored) >= severity(&collapsed) {
        honored
    } else {
        collapsed
    }
}

/// The resolved admission policy: what config said, if it said anything, and
/// which tier said it (**D523**).
///
/// The merged [`crate::config::Config`] keeps only the winning value — the
/// tier that established it is knowable only at the load seam that still
/// sees tiers — so the pairing is handed in whole by whoever constructs the
/// engine, and [`InboundPolicy`] itself is **imported**, never redefined:
/// the config key's vocabulary is defined exactly once, beside the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedInbound {
    /// The explicit policy and its tier, where any tier set one.
    explicit: Option<(InboundPolicy, PolicySource)>,
}

impl ResolvedInbound {
    /// A resolved policy: `None` is unset — a class-dependent default, not a
    /// fourth policy.
    #[must_use]
    pub const fn new(explicit: Option<(InboundPolicy, PolicySource)>) -> Self {
        Self { explicit }
    }

    /// Decides one message: the explicit value first — it always wins (v2
    /// §"Explicit values", evidence 680146-680160) — else the matrix.
    #[must_use]
    pub fn decide(
        &self,
        receiver: Option<ReceiverClass>,
        sender: Option<SenderClass>,
        self_sent: bool,
        honor_sender_mode: bool,
    ) -> Verdict {
        if let Some((policy, source)) = self.explicit {
            return match policy {
                InboundPolicy::Accept => Verdict::Accept,
                InboundPolicy::Hold => Verdict::Hold(HoldCause::Explicit { source }),
                InboundPolicy::Refuse => Verdict::Refuse(RefuseCause::Explicit { source }),
            };
        }
        decide_unset(receiver, sender, self_sent, honor_sender_mode)
    }

    /// Whether the configured policy is an explicit refuse — the one thing a
    /// release re-check asks (v2 §"Reevaluation and manual decision",
    /// evidence 620847-620877).
    fn refuses(&self) -> bool {
        matches!(self.explicit, Some((InboundPolicy::Refuse, _)))
    }
}

/// Which door a held message arrived through — the two residences the module
/// doc's one-door asymmetry describes (C1).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Door {
    /// The socket peer: the record holds the only copy of the bytes.
    Socket,
    /// The demoted mailbox writer: the bytes stay in the durable inbox under
    /// this identity, and the record is the review copy.
    Mailbox {
        /// §2.3's identity, the held-index key and the prune target.
        identity: mailbox::Identity,
    },
}

/// One held message: the review copy, its cause, and the control-plane
/// origin metadata the body never mixes with.
struct HeldMessage {
    /// Names this hold for settlement.
    id: HeldId,
    /// Which door it came through, and where its bytes live.
    door: Door,
    /// The sender's claimed identity — sender-authored routing data, never
    /// an authenticated principal.
    from: String,
    /// The body, held back from the model until something settles it
    /// delivered.
    text: String,
    /// The sender's one-line summary, where it wrote one — H1's snapshot on
    /// the mailbox door.
    summary: Option<String>,
    /// Why it was held.
    cause: HoldCause,
    /// When it was held, for the listing's age column.
    held_at: Instant,
    /// When it expires on its own — `Some` exactly for the parity causes
    /// (the expiry re-check).
    deadline: Option<Instant>,
    /// The class the sender asserted at arrival — today always `None`, kept
    /// on the control-plane side for re-evaluation under its own origin.
    sender: Option<SenderClass>,
    /// Whether the sender proved itself this session's own child — today
    /// always `false` (no kernel peer identity at the route).
    self_sent: bool,
    /// A drop decision whose prune is in flight (H2): claimed, so later
    /// settlers no-op, and cleared by [`Inbound::prune_failed`] so a failed
    /// prune re-holds instead of wedging.
    pending: Option<HeldOutcome>,
}

/// One row of the held listing: what `/held` and the status segment render
/// (**D524**).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldEntry {
    /// Names the hold, for a settle command.
    pub id: HeldId,
    /// The sender's claimed identity.
    pub from: String,
    /// Why it is held.
    pub cause: HoldCause,
    /// How long it has been held.
    pub age: Duration,
    /// How long until it expires on its own — `Some` exactly for the parity
    /// causes.
    pub expires_in: Option<Duration>,
    /// The sender's own summary, uncapped: display capping is the
    /// renderer's, as it is for every peer summary.
    pub summary: Option<RedactedText>,
    /// The body's opening, capped and control-stripped for the review
    /// surfaces.
    pub preview: RedactedText,
}

/// One ordered transition on its way to the engine's fanout (M10): the
/// queue's item, stamped into a real [`Event`] by the drain task that holds
/// the publisher — the one place that knows the session id.
#[derive(Debug, PartialEq, Eq)]
pub enum HoldTransition {
    /// A message was held; a frontend branches on `cause`.
    Held {
        /// Names the hold.
        id: HeldId,
        /// The sender's claimed identity.
        from: String,
        /// Why it was held.
        cause: HoldCause,
        /// The sender's summary, capped for the envelope.
        summary: Option<RedactedText>,
        /// The body's capped opening.
        preview: RedactedText,
        /// Milliseconds until self-expiry — `Some` exactly for the parity
        /// causes.
        expires_in_ms: Option<u64>,
    },
    /// A hold ended.
    Settled {
        /// The hold that ended.
        id: HeldId,
        /// How it ended.
        outcome: HeldOutcome,
    },
}

impl HoldTransition {
    /// The protocol event this transition becomes, stamped with the session
    /// the drain task reads off the engine — exactly where every non-turn
    /// publish stamps.
    #[must_use]
    pub fn into_event(self, session_id: SessionId) -> Event {
        match self {
            HoldTransition::Held {
                id,
                from,
                cause,
                summary,
                preview,
                expires_in_ms,
            } => Event::PeerHeld {
                session_id,
                id,
                from,
                cause,
                summary,
                preview,
                expires_in_ms,
            },
            HoldTransition::Settled { id, outcome } => Event::PeerHoldSettled {
                session_id,
                id,
                outcome,
            },
        }
    }
}

/// Why [`PeerGuard::admit`] dropped a message — exactly these four (v2
/// §"Guard limits", evidence 414851-415090); the queue cap is deliberately
/// not here (v2 §"Cross-pass reconciliation" verdict 5, evidence 415499).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropped {
    /// The hop chain exceeds the receiver threshold of 28.
    HopRunaway,
    /// This session's own marker appears in the chain more than 10 times.
    HopLoop,
    /// An identical body from the same sender key inside the 30 s window.
    Duplicate,
    /// The sender key's token bucket is empty.
    RateLimited,
}

/// Why an accepted-by-policy message was still not written — the typed
/// reason a caller traces, never answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// Explicit policy refused.
    Refused(RefuseCause),
    /// A guard dropped it.
    Guard(Dropped),
    /// The admitted-and-unconsumed queue is at its cap of 50 — the separate
    /// enqueue-time test.
    QueueFull,
}

/// A guard-eligibility tier (v2 §"Guard eligibility", evidence
/// 415199-415243). The third tier — non-peer origins — never reaches the
/// guard at all, so it has no spelling here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier<'a> {
    /// The socket peer: its identity passed the shape rule, so every guard
    /// runs — a resource-control qualification, not authentication.
    Qualified {
        /// The claimed `<name>@<team>` the sender key is built from.
        sender: &'a str,
    },
    /// The demoted mailbox writer: no usable provenance, so bucket and dedup
    /// are skipped and only the hop checks run.
    Unidentified,
}

/// What one admission attempt presents to the guard.
#[derive(Clone, Copy, Debug)]
pub struct Origin<'a> {
    /// Which eligibility tier this arrival is.
    pub tier: Tier<'a>,
    /// The message's hop chain — real input for the total hop checks. The
    /// socket door reads this from the wire since **D532**; the mailbox
    /// door still passes `&[]`, stated rather than defaulted, because a
    /// demoted writer's entry crossed no socket and has no chain.
    pub hop_chain: &'a [String],
    /// This session's own marker, for the loop check. The socket door
    /// reads this from `PeerFacts` since **D532**; the mailbox door still
    /// passes `None`, for the same reason its `hop_chain` is empty.
    pub own_marker: Option<&'a str>,
    /// The body, hashed for dedup — never stored by the guard.
    pub body: &'a str,
}

/// The peer-asserted facts one socket-door arrival carries (**D532**): the
/// sender's own claimed permission class, and the chain [`Origin`]'s hop
/// checks read. Bundled into one value so [`Inbound::admit_socket`]'s
/// callers — the engine, and every test that asserts none of this — pass
/// one argument rather than three parameters repeated at every call site.
#[derive(Clone, Copy, Debug)]
pub struct WireFacts<'a> {
    /// The sender's own asserted class, parsed from `from_mode` — [`None`]
    /// for a sender with no class to assert.
    pub sender: Option<SenderClass>,
    /// The message's hop chain, oldest first — empty for a sender with none
    /// to forward.
    pub hop_chain: &'a [String],
    /// This session's own bound-socket stem, for the loop check — [`None`]
    /// until this session itself has one bound.
    pub own_marker: Option<&'a str>,
}

impl WireFacts<'_> {
    /// No wire facts asserted at all — every caller with none of this to
    /// give, a test included.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            sender: None,
            hop_chain: &[],
            own_marker: None,
        }
    }
}

/// One tracked sender's bucket, dedup window and recency.
struct SenderState {
    /// Tokens remaining, refilled continuously at [`REFILL_PER_SECOND`].
    tokens: f64,
    /// When the bucket last refilled.
    refilled_at: Instant,
    /// Body hashes admitted inside the dedup window.
    recent: VecDeque<(u64, Instant)>,
    /// The guard clock's reading at this sender's last touch, for LRU
    /// eviction.
    touched: u64,
}

impl SenderState {
    /// A sender never seen before: a full bucket and an empty window.
    fn fresh(now: Instant) -> Self {
        Self {
            tokens: BUCKET_CAPACITY,
            refilled_at: now,
            recent: VecDeque::new(),
            touched: 0,
        }
    }
}

/// The accepted-peer queue guard (**D525**): per-sender token buckets and
/// dedup windows, bounded at 256 tracked senders with least-recently-used
/// eviction (the LRU choice is ganja's own, M5 — v2 records only the
/// bound), and the two hop checks.
///
/// Crates-registry search, per the standing rule, run at execution time
/// (`search_crates`: "token bucket rate limit", "governor leaky-bucket rate
/// limiter"): the pre-argued candidates `governor` and `leaky-bucket`, and
/// what the search surfaced (`rater`, `throttle-machines`, `better-bucket`,
/// `ratelock`, …), are async-runtime or lock-free-atomics machinery for
/// what is here five constants and ~30 lines of [`Instant`] arithmetic
/// under a mutex the gate already holds — a workspace dependency with a
/// rationale comment is not earned by that, so the bucket (and the
/// counter-scan LRU beside it, same reasoning) is hand-rolled here.
pub struct PeerGuard {
    /// Per-sender state, keyed `from:<identity>`.
    senders: HashMap<String, SenderState>,
    /// A monotonic touch counter — recency without reading any clock.
    clock: u64,
}

impl PeerGuard {
    /// An empty guard: nobody tracked, nothing admitted.
    #[must_use]
    pub fn new() -> Self {
        Self {
            senders: HashMap::new(),
            clock: 0,
        }
    }

    /// Admits or drops one arrival, returning exactly the four reasons.
    ///
    /// Order (ganja's own; the module doc marks it): hop checks — both
    /// tiers — then, qualified only, dedup then bucket, the body hash
    /// recorded only when everything passed, so a duplicate spends no token
    /// and a rate-limited retry is not later read as a duplicate.
    ///
    /// # Errors
    ///
    /// The [`Dropped`] reason the arrival was refused for.
    pub fn admit(&mut self, origin: &Origin<'_>) -> Result<(), Dropped> {
        // Exceeding drops: 29 drops where 28 passes — the comparator
        // direction is ganja-inferred (M5); v2 pins the constants alone.
        if origin.hop_chain.len() > MAX_CHAIN_LENGTH {
            return Err(Dropped::HopRunaway);
        }
        if let Some(marker) = origin.own_marker
            && origin.hop_chain.iter().filter(|hop| *hop == marker).count() > MAX_SELF_HOPS
        {
            return Err(Dropped::HopLoop);
        }
        let Tier::Qualified { sender } = origin.tier else {
            // The unidentified tier: no usable provenance means no bucket
            // and no dedup — the queue cap still applies, at the door.
            return Ok(());
        };

        let now = Instant::now();
        let state = self.touch(format!("from:{sender}"), now);

        let hash = body_hash(origin.body);
        state
            .recent
            .retain(|(_, at)| now.duration_since(*at) < DEDUP_WINDOW);
        if state.recent.iter().any(|(seen, _)| *seen == hash) {
            return Err(Dropped::Duplicate);
        }

        let elapsed = now.duration_since(state.refilled_at).as_secs_f64();
        state.tokens = (state.tokens + elapsed * REFILL_PER_SECOND).min(BUCKET_CAPACITY);
        state.refilled_at = now;
        if state.tokens < 1.0 {
            return Err(Dropped::RateLimited);
        }
        state.tokens -= 1.0;
        state.recent.push_back((hash, now));
        Ok(())
    }

    /// The sender's state, created full if unseen — evicting the least
    /// recently touched entry first when the table is at its bound — and
    /// touched either way.
    fn touch(&mut self, key: String, now: Instant) -> &mut SenderState {
        self.clock += 1;
        let clock = self.clock;
        if !self.senders.contains_key(&key) && self.senders.len() >= TRACKED_SENDERS {
            // A linear scan at eviction time: 256 entries, and eviction is
            // the rare path — an ordering structure would be bookkeeping on
            // every touch to save a scan almost never taken.
            if let Some(evict) = self
                .senders
                .iter()
                .min_by_key(|(_, state)| state.touched)
                .map(|(key, _)| key.clone())
            {
                self.senders.remove(&evict);
            }
        }
        let state = self
            .senders
            .entry(key)
            .or_insert_with(|| SenderState::fresh(now));
        state.touched = clock;
        state
    }
}

impl Default for PeerGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// The **separate** enqueue-time test (v2 §"Cross-pass reconciliation"
/// verdict 5, evidence 415499): admitted-and-unconsumed messages at the cap
/// mean the next accept is dropped at enqueue. Deliberately not a
/// [`PeerGuard::admit`] reason, and deliberately its own function, so the
/// distinction survives refactoring.
fn queue_full(admitted: usize) -> bool {
    admitted >= MAX_QUEUED_PEERS
}

/// How an admitted identity got in — and, for a released mailbox hold, the
/// H1 snapshot the delivery must carry.
enum Admitted {
    /// Gated at ingress: the entry delivers with its own fields.
    AtIngress,
    /// Released from a mailbox-door hold: deliver with the summary reviewed
    /// at hold time, never the entry's current one (H1).
    Released {
        /// The hold-time summary snapshot.
        summary: Option<String>,
    },
}

/// What the lead's inbox pass does with one entry's identity — the two-set
/// answer (admitted ⇒ deliver, held ⇒ skip, neither ⇒ classify).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PassDisposition {
    /// In the admitted set: deliver without re-running policy or guards —
    /// accepted is final, and the 1 s re-offer loop must not drain the
    /// bucket.
    Deliver,
    /// In the admitted set via a mailbox-door release: deliver carrying this
    /// hold-time summary snapshot — even a `None` overrides whatever the
    /// durable entry carries now (H1).
    DeliverReviewed {
        /// The summary reviewed at hold time.
        summary: Option<RedactedText>,
    },
    /// In the held-index: skip — the review copy is in the buffer and the
    /// entry must neither deliver nor re-gate.
    Skip,
    /// Unknown to the gate: classify and, where demoted, gate.
    Classify,
}

/// A socket-door release's payload: what the caller writes into the lead's
/// inbox through the ordinary delivery tail, handing the minted identity
/// back to [`Inbound::admit_identity`].
#[derive(Clone, PartialEq, Eq)]
pub struct ReleasedMessage {
    /// The sender's claimed identity, written as the message's `from`.
    pub from: String,
    /// The body.
    pub text: String,
    /// The sender's summary, as reviewed.
    pub summary: Option<String>,
}

impl std::fmt::Debug for ReleasedMessage {
    /// Sizes, never text — the same rule every body-bearing type here
    /// states, so a traced settlement cannot leak what a person reviewed.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReleasedMessage")
            .field("from", &self.from)
            .field("text", &format_args!("<{} bytes>", self.text.len()))
            .field(
                "summary",
                &format_args!("<{} bytes>", self.summary.as_deref().unwrap_or("").len()),
            )
            .finish()
    }
}

/// What one settlement decision requires of its caller.
#[derive(Debug, PartialEq, Eq)]
pub enum Settlement {
    /// Settled entirely in memory; the outcome is already on the transition
    /// queue and nothing is left to do.
    Done(HeldOutcome),
    /// A socket-door release, settled `delivered`: write the message through
    /// the ordinary delivery tail, then record the identity the write mints
    /// via [`Inbound::admit_identity`]. The write's failure is the delivery
    /// path's own failure channel, as it is for any peer write.
    Deliver(ReleasedMessage),
    /// A mailbox-door drop, **not yet settled** (H2): the record stays held
    /// and its identity indexed. Prune `identity` from the inbox; on success
    /// [`Inbound::pruned`] finishes the settlement, on failure
    /// [`Inbound::prune_failed`] releases the claim so the record re-holds,
    /// retryable.
    PruneFirst {
        /// The durable entry to prune before anything unindexes.
        identity: mailbox::Identity,
    },
}

/// One re-evaluated hold and what its new verdict requires.
#[derive(Debug, PartialEq, Eq)]
pub struct Reevaluated {
    /// The hold that re-decided.
    pub id: HeldId,
    /// What to do about it.
    pub settlement: Settlement,
}

/// The socket door's answer, computed synchronously so the HTTP response can
/// carry the outcome — a held answer is the reference's own held receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketAdmission {
    /// Accepted: write through the delivery tail and hand the minted
    /// identity to [`Inbound::admit_identity`].
    Deliver,
    /// Held for review; the answer names the cause, as the reference's held
    /// receipt names its reason.
    Held {
        /// Names this hold, for a receipt to key its eventual settlement to
        /// (**N3**, **D534**) — additive over the record itself, which
        /// stays byte-untouched: the engine pairs this id with the
        /// message's own `message_id` and vetted `reply_to` in
        /// [`crate::teammate::receipts`]'s own map, never on
        /// `HeldMessage` (this module's own record, private by design).
        id: HeldId,
        /// Why it was held.
        cause: HoldCause,
        /// A capacity eviction's mailbox-door victim, for the caller to
        /// prune — best-effort: a failed prune re-gates as a fresh hold,
        /// never delivers.
        evicted_prune: Option<mailbox::Identity>,
    },
    /// Refused or guard-dropped: answer **byte-identically** to the accept
    /// case and trace the typed reason — refused messages do not notify the
    /// sender.
    Silent(DropReason),
}

/// The mailbox door's answer, for the demoted (non-roster) writer's entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailboxAdmission {
    /// Accepted: deliver the entry; its identity is already in the admitted
    /// set — this door holds the identity, where the socket door's is minted
    /// by the write (M6's asymmetry).
    Deliver,
    /// Held: the entry stays in the durable inbox, its identity now in the
    /// held-index, the review copy in the buffer (C1).
    Held {
        /// Why it was held.
        cause: HoldCause,
        /// A capacity eviction's mailbox-door victim, as on the socket door.
        evicted_prune: Option<mailbox::Identity>,
    },
    /// Refused or dropped: prune the entry and trace the typed reason —
    /// there is no sender to answer on this door at all.
    Drop(DropReason),
}

/// Everything the gate holds, under one lock.
struct State {
    /// The resolved policy.
    resolved: ResolvedInbound,
    /// The parity holds' deadline vocabulary.
    expiry: DialogExpiry,
    /// The hold buffer, oldest first.
    buffer: VecDeque<HeldMessage>,
    /// Mailbox-door holds' identities: the pass skips these (C1).
    held_index: HashSet<mailbox::Identity>,
    /// Accepted-and-unconsumed identities: the pass delivers these without
    /// re-gating, and their count is the queue-cap's subject.
    admitted: HashMap<mailbox::Identity, Admitted>,
    /// The accepted-peer guards.
    guard: PeerGuard,
}

/// The admission gate (**D523**–**D525**): engine-owned, shared by
/// reference, every method `&self` over one internal mutex.
///
/// No method awaits and no guard outlives a call — every state change and
/// its transition enqueue happen under one lock acquisition, and the IO a
/// decision needs leaves as data for the caller to perform after the lock is
/// gone (drain-under-lock, settle-after-drop; the engine's
/// `await_holding_lock` gate is the machine check, trivially satisfied by a
/// module with no `async fn` at all).
pub struct Inbound {
    /// The gate's state.
    state: Mutex<State>,
    /// The ordered forwarder's sending half (M10): enqueued under the state
    /// lock, so transition order is state-change order.
    transitions: mpsc::UnboundedSender<HoldTransition>,
}

impl Inbound {
    /// A gate over `resolved` and `expiry`, and the ordered transition
    /// stream the engine's drain task turns into published events.
    ///
    /// A dropped receiver loses observability only, never a decision:
    /// enqueues onto a closed channel are deliberately ignored.
    #[must_use]
    pub fn new(
        resolved: ResolvedInbound,
        expiry: DialogExpiry,
    ) -> (Self, mpsc::UnboundedReceiver<HoldTransition>) {
        let (transitions, drain) = mpsc::unbounded_channel();
        let gate = Self {
            state: Mutex::new(State {
                resolved,
                expiry,
                buffer: VecDeque::new(),
                held_index: HashSet::new(),
                admitted: HashMap::new(),
                guard: PeerGuard::new(),
            }),
            transitions,
        };
        (gate, drain)
    }

    /// The socket door: the never-loosen composition, then — on accept
    /// always, and on a *parity-cause* hold since **D534** — the qualified
    /// guard tier, and on accept alone the separate queue cap. Runs
    /// synchronously in the deliver arm because the HTTP response must
    /// carry the outcome.
    #[must_use]
    pub fn admit_socket(
        &self,
        receiver: Option<ReceiverClass>,
        from: &str,
        text: &str,
        summary: Option<&str>,
        facts: WireFacts<'_>,
    ) -> SocketAdmission {
        let mut state = self.lock();
        let origin = Origin {
            tier: Tier::Qualified { sender: from },
            hop_chain: facts.hop_chain,
            own_marker: facts.own_marker,
            body: text,
        };

        // The never-loosen composition (Axis 5, **OQ2(a)**): composed over
        // `ResolvedInbound::decide`, never `decide_unset` directly — `decide`
        // resolves an explicit `cross_session_inbound` value in its first
        // branch, and composing beneath it would silently break that config
        // key (MA). Calling it twice is safe and cheap: where an explicit
        // policy is set both calls return the same verdict, so
        // `strictest_of` is a no-op there.
        let collapsed = state.resolved.decide(receiver, facts.sender, false, false);
        let honored = state.resolved.decide(receiver, facts.sender, false, true);

        match strictest_of(collapsed, honored) {
            Verdict::Refuse(cause) => SocketAdmission::Silent(DropReason::Refused(cause)),
            Verdict::Hold(cause) => {
                // The guard now runs ahead of the hold arm too (**N1**,
                // **D1**), scoped to the two parity causes this landing
                // newly routes traffic onto: an explicitly configured hold
                // and a fail-closed mode-unknown hold pass ungated, because
                // neither gains any new traffic here — the person who
                // configured `cross_session_inbound: "hold"` asked for
                // every message to reach review, and gating that path would
                // quietly deliver less than it names.
                let newly_routed =
                    matches!(cause, HoldCause::ModeMismatch | HoldCause::NoModeAsserted);
                if newly_routed && let Err(dropped) = state.guard.admit(&origin) {
                    return SocketAdmission::Silent(DropReason::Guard(dropped));
                }
                let evicted_prune = self.hold(
                    &mut state,
                    Door::Socket,
                    from.to_owned(),
                    text.to_owned(),
                    summary.map(str::to_owned),
                    cause,
                );
                // `hold()` stays byte-unchanged (**D534**, **N3**): the id
                // it just minted is read back off the buffer's own tail —
                // the last thing it does before returning — rather than
                // threaded through its signature.
                let id = state
                    .buffer
                    .back()
                    .expect("hold() always pushes exactly one record before returning")
                    .id
                    .clone();
                SocketAdmission::Held {
                    id,
                    cause,
                    evicted_prune,
                }
            }
            Verdict::Accept => {
                if let Err(dropped) = state.guard.admit(&origin) {
                    return SocketAdmission::Silent(DropReason::Guard(dropped));
                }
                if queue_full(state.admitted.len()) {
                    return SocketAdmission::Silent(DropReason::QueueFull);
                }
                SocketAdmission::Deliver
            }
        }
    }

    /// The mailbox door: the demoted writer's entry, gated like a peer from
    /// `unknown` — the same policy, the unidentified guard tier, the same
    /// queue cap. The caller has already ruled out roster members, admitted
    /// identities and held identities ([`Inbound::disposition`]), and
    /// dropped frame-shaped entries by name.
    #[must_use]
    pub fn admit_mailbox(
        &self,
        receiver: Option<ReceiverClass>,
        message: &MailboxMessage,
    ) -> MailboxAdmission {
        let identity = mailbox::identity(message);
        let mut state = self.lock();
        match state
            .resolved
            .decide(receiver, None, false, HONOR_SENDER_MODE)
        {
            Verdict::Refuse(cause) => MailboxAdmission::Drop(DropReason::Refused(cause)),
            Verdict::Hold(cause) => {
                state.held_index.insert(identity.clone());
                let evicted_prune = self.hold(
                    &mut state,
                    Door::Mailbox { identity },
                    message.from.clone(),
                    message.text.clone(),
                    message.summary.clone(),
                    cause,
                );
                MailboxAdmission::Held {
                    cause,
                    evicted_prune,
                }
            }
            Verdict::Accept => {
                let origin = Origin {
                    tier: Tier::Unidentified,
                    hop_chain: &[],
                    own_marker: None,
                    body: &message.text,
                };
                if let Err(dropped) = state.guard.admit(&origin) {
                    return MailboxAdmission::Drop(DropReason::Guard(dropped));
                }
                if queue_full(state.admitted.len()) {
                    return MailboxAdmission::Drop(DropReason::QueueFull);
                }
                state.admitted.insert(identity, Admitted::AtIngress);
                MailboxAdmission::Deliver
            }
        }
    }

    /// The two-set answer for one inbox entry: admitted delivers (with H1's
    /// snapshot where a release reviewed one), held skips, neither
    /// classifies.
    #[must_use]
    pub fn disposition(&self, identity: &mailbox::Identity) -> PassDisposition {
        let state = self.lock();
        match state.admitted.get(identity) {
            Some(Admitted::AtIngress) => PassDisposition::Deliver,
            Some(Admitted::Released { summary }) => PassDisposition::DeliverReviewed {
                summary: summary.clone().map(RedactedText::from),
            },
            None if state.held_index.contains(identity) => PassDisposition::Skip,
            None => PassDisposition::Classify,
        }
    }

    /// Records a socket-door identity as admitted — after the write that
    /// minted it (M6: the delivery tail mints timestamp and identity, so
    /// only the caller can hand them back), for the ingress accept and the
    /// released hold alike.
    pub fn admit_identity(&self, identity: mailbox::Identity) {
        self.lock().admitted.insert(identity, Admitted::AtIngress);
    }

    /// Reconciles both sets against what the pass actually found in the
    /// inbox: consumed admitted identities leave the set, and a held
    /// identity gone from the inbox settles its record `expired` — a review
    /// offer cannot outlive the bytes it reviews. Records with a prune in
    /// flight are skipped: their absence is the prune landing, and
    /// [`Inbound::pruned`] owns that settlement.
    pub fn reconcile(&self, present: &HashSet<mailbox::Identity>) {
        let mut state = self.lock();
        state
            .admitted
            .retain(|identity, _| present.contains(identity));

        let vanished: Vec<HeldId> = state
            .buffer
            .iter()
            .filter(|held| held.pending.is_none())
            .filter_map(|held| match &held.door {
                Door::Mailbox { identity } if !present.contains(identity) => Some(held.id.clone()),
                Door::Mailbox { .. } | Door::Socket => None,
            })
            .collect();
        for id in vanished {
            // The bytes are already gone, so there is no prune step: the
            // record leaves, the index forgets it, and the settle says
            // undecided.
            let Some(position) = position_of(&state.buffer, &id) else {
                continue;
            };
            let Some(held) = state.buffer.remove(position) else {
                continue;
            };
            if let Door::Mailbox { identity } = &held.door {
                state.held_index.remove(identity);
            }
            self.settle(&mut state, held.id, HeldOutcome::Expired);
        }
    }

    /// Releases one held message, re-checking the current policy first: a
    /// policy that has since become refuse turns the approval into a deny
    /// (v2 §"Reevaluation and manual decision", evidence 620847-620877).
    /// `None` for an id nobody holds — or one a drop already claimed —
    /// first-settler-wins.
    #[must_use]
    pub fn release(&self, id: &HeldId) -> Option<Settlement> {
        let mut state = self.lock();
        self.claimable(&state, id)?;
        if state.resolved.refuses() {
            return Some(self.drop_held(&mut state, id, HeldOutcome::Denied));
        }
        let position = position_of(&state.buffer, id)?;
        let held = state.buffer.remove(position)?;
        match held.door {
            Door::Socket => {
                self.settle(&mut state, held.id, HeldOutcome::Delivered);
                Some(Settlement::Deliver(ReleasedMessage {
                    from: held.from,
                    text: held.text,
                    summary: held.summary,
                }))
            }
            Door::Mailbox { identity } => {
                state.held_index.remove(&identity);
                state.admitted.insert(
                    identity,
                    Admitted::Released {
                        summary: held.summary,
                    },
                );
                self.settle(&mut state, held.id, HeldOutcome::Delivered);
                Some(Settlement::Done(HeldOutcome::Delivered))
            }
        }
    }

    /// Denies one held message. `None` for an unknown or already-claimed id.
    #[must_use]
    pub fn deny(&self, id: &HeldId) -> Option<Settlement> {
        let mut state = self.lock();
        self.claimable(&state, id)?;
        Some(self.drop_held(&mut state, id, HeldOutcome::Denied))
    }

    /// Expires one held message — what the engine's parity-hold timer calls
    /// at the deadline. `None` for an unknown or already-claimed id, which
    /// is how a timer racing an approval loses.
    #[must_use]
    pub fn expire(&self, id: &HeldId) -> Option<Settlement> {
        let mut state = self.lock();
        self.claimable(&state, id)?;
        Some(self.drop_held(&mut state, id, HeldOutcome::Expired))
    }

    /// Finishes a [`Settlement::PruneFirst`] whose prune landed: the record
    /// leaves the buffer, the identity leaves the held-index, and the
    /// claimed outcome settles — H2's ordering, second half.
    #[must_use]
    pub fn pruned(&self, id: &HeldId) -> Option<HeldOutcome> {
        let mut state = self.lock();
        let position = position_of(&state.buffer, id)?;
        let outcome = state.buffer[position].pending?;
        let held = state.buffer.remove(position)?;
        if let Door::Mailbox { identity } = &held.door {
            state.held_index.remove(identity);
        }
        self.settle(&mut state, held.id, outcome);
        Some(outcome)
    }

    /// Abandons a [`Settlement::PruneFirst`] whose prune failed: the claim
    /// clears and the record re-holds, still indexed, retryable — reachable
    /// for delivery only through a later loosened policy's re-evaluation or
    /// the next start's re-gate, never through the failure itself (H2).
    pub fn prune_failed(&self, id: &HeldId) {
        let mut state = self.lock();
        if let Some(position) = position_of(&state.buffer, id) {
            state.buffer[position].pending = None;
        }
    }

    /// Re-decides every held entry under its own recorded origin and the
    /// given receiver class (v2 §"Reevaluation and manual decision",
    /// evidence 620778-620845): now-accept delivers, now-refuse denies, a
    /// verdict that still holds leaves the record exactly as it was.
    /// Records with a prune in flight are already claimed and skipped.
    #[must_use]
    pub fn reevaluate(&self, receiver: Option<ReceiverClass>) -> Vec<Reevaluated> {
        let mut state = self.lock();
        let undecided: Vec<HeldId> = state
            .buffer
            .iter()
            .filter(|held| held.pending.is_none())
            .map(|held| held.id.clone())
            .collect();

        let mut actions = Vec::new();
        for id in undecided {
            let Some(position) = position_of(&state.buffer, &id) else {
                continue;
            };
            let held = &state.buffer[position];
            let verdict =
                state
                    .resolved
                    .decide(receiver, held.sender, held.self_sent, HONOR_SENDER_MODE);
            match verdict {
                Verdict::Hold(_) => {}
                Verdict::Accept => {
                    let Some(held) = state.buffer.remove(position) else {
                        continue;
                    };
                    let settlement = match held.door {
                        Door::Socket => Settlement::Deliver(ReleasedMessage {
                            from: held.from,
                            text: held.text,
                            summary: held.summary,
                        }),
                        Door::Mailbox { identity } => {
                            state.held_index.remove(&identity);
                            state.admitted.insert(
                                identity,
                                Admitted::Released {
                                    summary: held.summary,
                                },
                            );
                            Settlement::Done(HeldOutcome::Delivered)
                        }
                    };
                    self.settle(&mut state, id.clone(), HeldOutcome::Delivered);
                    actions.push(Reevaluated { id, settlement });
                }
                Verdict::Refuse(_) => {
                    let settlement = self.drop_held(&mut state, &id, HeldOutcome::Denied);
                    actions.push(Reevaluated { id, settlement });
                }
            }
        }
        actions
    }

    /// Settles every held entry `expired` — the shutdown pass, idempotent
    /// like its teardown siblings. A mailbox-door entry is deliberately left
    /// in the durable inbox for next-start re-gating (the no-lost-mail
    /// half), so no prune data leaves here; a claimed prune completing
    /// afterwards finds its id gone and no-ops.
    pub fn shutdown_settle(&self) {
        let mut state = self.lock();
        while let Some(held) = state.buffer.pop_front() {
            if let Door::Mailbox { identity } = &held.door {
                state.held_index.remove(identity);
            }
            self.settle(&mut state, held.id, HeldOutcome::Expired);
        }
    }

    /// The current held list, newest last — what `/held`, the approval
    /// dialog's countdown and the `N held` segment poll.
    #[must_use]
    pub fn held_messages(&self) -> Vec<HeldEntry> {
        let now = Instant::now();
        self.lock()
            .buffer
            .iter()
            .map(|held| HeldEntry {
                id: held.id.clone(),
                from: held.from.clone(),
                cause: held.cause,
                age: now.saturating_duration_since(held.held_at),
                expires_in: held.deadline.map(|at| at.saturating_duration_since(now)),
                summary: held.summary.clone().map(RedactedText::from),
                preview: RedactedText::from(preview_of(&held.text)),
            })
            .collect()
    }

    /// Replaces the resolved policy — the seam a config reload would use,
    /// and what lets a test exercise the release re-check and the
    /// re-evaluation's refuse arm today, while ganja's config does not
    /// change mid-session. Standing holds are not re-decided here: policy
    /// change re-evaluation is the caller's explicit
    /// [`Inbound::reevaluate`].
    pub fn replace_policy(&self, resolved: ResolvedInbound) {
        self.lock().resolved = resolved;
    }

    /// The state, or a panic that names this module: the lock is held only
    /// across straight-line transitions, so poison means a bug worth
    /// stopping on — the engine's own posture for its mode slot.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("the inbound gate's state is never poisoned")
    }

    /// Whether `id` names a record no settler has claimed.
    fn claimable(&self, state: &State, id: &HeldId) -> Option<()> {
        let position = position_of(&state.buffer, id)?;
        state.buffer[position].pending.is_none().then_some(())
    }

    /// One drop decision, per door: a socket record settles now; a mailbox
    /// record is claimed and waits for its prune (H2).
    fn drop_held(&self, state: &mut State, id: &HeldId, outcome: HeldOutcome) -> Settlement {
        let Some(position) = position_of(&state.buffer, id) else {
            // `claimable` just said yes under this same lock; the arm exists
            // so a refactor cannot turn a miss into a panic.
            return Settlement::Done(outcome);
        };
        match &state.buffer[position].door {
            Door::Socket => {
                let Some(held) = state.buffer.remove(position) else {
                    return Settlement::Done(outcome);
                };
                self.settle(state, held.id, outcome);
                Settlement::Done(outcome)
            }
            Door::Mailbox { identity } => {
                let identity = identity.clone();
                state.buffer[position].pending = Some(outcome);
                Settlement::PruneFirst { identity }
            }
        }
    }

    /// Appends one hold — evicting the oldest first at the cap, settled
    /// `expired` before the newcomer appends — and enqueues its `Held`
    /// transition. Returns the eviction's mailbox-door prune target, if any.
    fn hold(
        &self,
        state: &mut State,
        door: Door,
        from: String,
        text: String,
        summary: Option<String>,
        cause: HoldCause,
    ) -> Option<mailbox::Identity> {
        let mut evicted_prune = None;
        if state.buffer.len() >= HELD_CAP
            && let Some(oldest) = state.buffer.pop_front()
        {
            if let Door::Mailbox { identity } = &oldest.door {
                state.held_index.remove(identity);
                evicted_prune = Some(identity.clone());
            }
            self.settle(state, oldest.id, HeldOutcome::Expired);
        }

        let now = Instant::now();
        let deadline = deadline_for(cause, state.expiry).map(|wait| now + wait);
        let id = HeldId::ascending();
        let transition = HoldTransition::Held {
            id: id.clone(),
            from: from.clone(),
            cause,
            summary: summary
                .as_deref()
                .map(|summary| RedactedText::from(cap_for_display(summary).to_owned())),
            preview: RedactedText::from(preview_of(&text)),
            expires_in_ms: deadline.map(|at| {
                u64::try_from(at.saturating_duration_since(now).as_millis()).unwrap_or(u64::MAX)
            }),
        };
        state.buffer.push_back(HeldMessage {
            id,
            door,
            from,
            text,
            summary,
            cause,
            held_at: now,
            deadline,
            sender: None,
            self_sent: false,
            pending: None,
        });
        let _ = self.transitions.send(transition);
        evicted_prune
    }

    /// Enqueues one settlement transition — under the caller's lock, so it
    /// lands after the `Held` that opened this id and in decision order.
    fn settle(&self, _state: &mut State, id: HeldId, outcome: HeldOutcome) {
        let _ = self
            .transitions
            .send(HoldTransition::Settled { id, outcome });
    }
}

/// Where `id`'s record sits in the buffer.
fn position_of(buffer: &VecDeque<HeldMessage>, id: &HeldId) -> Option<usize> {
    buffer.iter().position(|held| held.id == *id)
}

/// The deadline a cause earns: the configured expiry for the parity causes,
/// nothing for an explicit or `mode_unknown` hold — the expiry re-check's
/// narrowness, kept exhaustive so a new cause is a compile error here.
fn deadline_for(cause: HoldCause, expiry: DialogExpiry) -> Option<Duration> {
    match cause {
        HoldCause::ModeMismatch | HoldCause::NoModeAsserted => expiry.deadline(),
        HoldCause::Explicit { .. } | HoldCause::ModeUnknown => None,
    }
}

/// The expandable body preview: the first [`PREVIEW_LINES`] lines, control
/// characters stripped save the newline and tab a review pane reads as
/// content, capped at [`PREVIEW_CHARS`].
fn preview_of(text: &str) -> String {
    let clipped = text
        .lines()
        .take(PREVIEW_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let stripped: String = clipped
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    cap_chars(&stripped, PREVIEW_CHARS).to_owned()
}

/// One body's dedup hash — hashed, compared, never stored as text.
fn body_hash(body: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
#[path = "inbound_tests.rs"]
mod tests;
