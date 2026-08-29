//! Which session a name points at, and what a person is told about the name
//! they typed (**D528**, with **D529**'s reminder half).
//!
//! No upstream counterpart: opencode has no cross-session addressing at all,
//! so there is no TypeScript to port. The specification is Claude Code's
//! sender-side resolver, cited as `v2 §"<section>", evidence <ranges>`; the
//! plan is `.omc/plans/2026-08-26-session-naming-mention-sender.md`. What
//! this module is, and is not: **resolution and prose, never transport**. It
//! opens no connection, writes no mailbox, raises no dialog and fires no
//! hook. A resolution is a read of the registration records `ganja-tool`
//! owns (**D527**); a reminder is text the model reads once. The only act
//! anywhere near here is the model's own later `send_message` call, which
//! crosses the unchanged eight-rung ladder, the unchanged address gate, and
//! the receiver's unchanged admission gate (**D523**–**D525**).
//!
//! # Roster first, and the trust that orders it
//!
//! Precedence is roster, then registry — v2's `in-process teammate > local
//! UDS session` (v2 §"Resolution precedence and the pin guard", evidence
//! 623321-623461) — and it is the **caller's**, not this module's: the
//! deliver arm consults its roster before reaching the resolver, and the
//! mention seam checks the roster before asking here. The order is a trust
//! statement rather than a convenience. A roster name is **lead-assigned**,
//! given at a spawn door a person consented at; a registry name is
//! **self-asserted** by whatever same-uid process wrote the file. When both
//! exist the assigned one wins, and the shadowed live session stays reachable
//! by the one spelling nothing can forge — its socket address.
//!
//! # The disambiguator is the stem
//!
//! v2's own `[ref]` — a hashed 6–12-hex prefix — is marked provisional there
//! and is **not ported**. Ganja's disambiguator is the session's socket
//! stem, and it costs no new derivation: the bind walk names a socket by the
//! session id's compact-hex prefix (`crates/ganja-serve/src/socket.rs:89-105`)
//! and extends that prefix past any name a live peer already holds
//! (`crates/ganja-serve/src/socket.rs:277-296`), so every live stem in one
//! directory is unique by construction. It is already an id prefix a person
//! can resume by, and already the filename inside the address, which is why
//! the **fully unambiguous spelling of any candidate is its `uds:` address**
//! ([`address_of`](crate::teammate::identity::address_of)) — a spelling `send_message`'s `to` accepts today, so
//! disambiguation needed no new grammar.
//!
//! # The pin guard, and what it does not do
//!
//! [`Identity`](crate::teammate::identity::Identity) records, per conversation, which session identity a name
//! resolved to **for a delivery the arm accepted** — [`Identity::pin`](crate::teammate::identity::Identity::pin), which
//! the deliver arm calls and nothing else does. Every later resolution of
//! that name checks the pin first, and a unique live holder whose id differs
//! from it is [`Resolution::Moved`](crate::teammate::identity::Resolution::Moved): delivery halts rather than following the
//! name to its new claimant (v2 §"Resolution precedence and the pin guard",
//! evidence 619672-620120 — the local-impersonation shape, collapsed onto
//! ganja's one live tier, so the guard is cross-identity rebinding
//! protection for every name-addressed send).
//!
//! Three boundaries the guard deliberately does not cross:
//!
//! - **Mentions consult pins and never create them.** Pointing at a session
//!   is not an addressing act, so [`Identity::resolve`](crate::teammate::identity::Identity::resolve) reads the map and
//!   writes it never — pinning is [`Identity::pin`](crate::teammate::identity::Identity::pin)'s alone.
//! - **A `uds:` send neither consults nor creates one.** An explicit address
//!   *is* an identity; there is nothing for a name guard to protect.
//!   [`Identity::resolve_address`](crate::teammate::identity::Identity::resolve_address) therefore ignores the map in both
//!   directions.
//! - **The map is volatile.** It clears on `NewSession`
//!   ([`Identity::clear_pins`](crate::teammate::identity::Identity::clear_pins)) and does not survive a restart or a resume:
//!   the reference records conversation-scoped behavior and no persistence,
//!   so persisting would be building on silence. The cost is named rather
//!   than hidden — **the first send after a resume is trust-on-first-use
//!   again**, which is the window every fresh conversation already has; the
//!   row-flush option is filed as a bead rather than built.
//!
//! # The reminder, and the one thing it never says
//!
//! [`reminder`](crate::teammate::identity::reminder) is the single rendering function behind every `@`-mention
//! block, the byte-identity discipline **D491** already uses for `$skill`
//! expansion: one function, and tests that compare against it. Its arms are
//! [`Mentioned`](crate::teammate::identity::Mentioned)'s, and each says the same two things in its own words —
//! what the token turned out to name, honestly labelled, and that
//! **mentioning it sent nothing**. A live-session name is labelled
//! self-chosen and unverified, a roster name lead-assigned (v2 §"What
//! `@session` does", evidence 655461-655530, 819290-819303); an ambiguous
//! mention asks the person which one they meant rather than choosing (same
//! section, evidence 658949-658990); a miss is information the model reads,
//! never control flow, the `skill::not_found` posture.
//!
//! Two things no reminder does. It **names no reply channel** — **D529**'s
//! own rule, which is that **a mention sends nothing**: there is no message
//! for anybody to answer, so a road home would be a road out of nothing, and
//! a test scans every rendering for the claim. (This paragraph cited D530's
//! asymmetry rule — a teamless session can send and cannot be addressed back
//! — until **D543**, 2026-08-30, retired that clause: such a session binds a
//! socket and registers a name. The rule stands on its own reason.) And it **lists nothing the person did not
//! point at**: a miss names no other session, because a model-facing roster
//! of live sessions is its own tool with its own trust story and this build
//! ships none.
//!
//! # Same-uid data reaching a model's context
//!
//! A registration record is writable by any process running as this user
//! (`ganja_tool::registry`'s own axiom), and what a reader renders from one
//! lands verbatim in the model's context. So every registry-sourced value a
//! rendering shows goes through `shown` first: one line, control characters
//! dropped, length capped. That is not a new regime — it applies at the read
//! side exactly the classes D527's name grammar refuses at the write side,
//! for the case the writer was not ganja.
//!
//! # What has no counterpart here
//!
//! v2's cloud and bridge tiers, and the `[ref]`-bearing remote rows they
//! carry, have nothing in this build to attach to: ganja's precedence list
//! has one live tier, the local socket directory. Stated rather than
//! invented — no name in this module is a stub for a tier that does not
//! exist. Nor is there a model-facing listing tool over this index: in v1
//! discovery rides the person's `@`-mentions and `uds:` spellings, and a
//! roster the model could enumerate is its own tool with its own trust
//! story (a filed bead holds the question).
//!
//! # What a resolution leaves out
//!
//! Two exclusions run before any match counts. **This session's own record**
//! never resolves and never lists — the reader's own-socket exclusion (v2
//! §"Liveness validation and garbage collection", evidence 221136-221268),
//! which is why every entry point takes the caller's session id rather than
//! remembering one: the engine's current session is minted, adopted, resumed
//! and re-minted underneath it. And **a record whose stem's lock nobody
//! holds** is stale, so a session that died without unregistering cannot make
//! its still-live namesake ambiguous. That liveness token is D527's flock and
//! nothing of this module's own — same section, same evidence, ganja keeping
//! only the spirit of the reference's process checks.
//!
//! # The cost of a fresh read
//!
//! Every resolution reads the directory again ([`Identity::resolve`](crate::teammate::identity::Identity::resolve)), and
//! that is correctness rather than laziness: a cached index would turn
//! "refuse the ambiguity" and "refuse the moved pin" into verdicts about the
//! past, and no invalidation signal exists — another process's bind is
//! invisible from here. The reference's own resolver consults its indexes
//! per call (v2 §"Resolution precedence and the pin guard", evidence
//! 623321-623461). The reads are a handful of small files at human cadence
//! (a send, a prompt), and they are **blocking**: a caller doing this inside
//! a turn wraps it the way `ganja-team`'s synchronous mailbox writes are
//! wrapped. An unreadable listing is [`Resolution::ListingFailed`](crate::teammate::identity::Resolution::ListingFailed) — an
//! incomplete search refuses rather than guesses, on both doors, which is
//! why the composition here is [`registry::list`](ganja_tool::registry::list) with [`registry::is_live`](ganja_tool::registry::is_live)
//! directly rather than `registry::holders`: that scan feeds a notice and may
//! skip a holder it cannot judge, where a resolver that skipped one would
//! deliver wrong.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ganja_tool::{registry, socket};

/// The tag every reminder block wears, so the model can tell one from the
/// person's own words at a glance — `$skill` expansion's `<skill_content>`
/// in the same position, for the same reason.
pub const TAG: &str = "session_mention";

/// The scheme a socket address is spelled with: the one `send_message`'s
/// `to` accepts for another session (`crates/ganja-tool/src/send_message.rs`,
/// rung 2's own constant). Spelled again here because the ladder's copy is
/// private to the crate that owns the ladder, and every candidate this
/// module names must carry an address a `to` would take verbatim.
pub const ADDRESS_SCHEME: &str = "uds:";

/// The most code points of a value some other process wrote that a rendering
/// will show. Names are capped at sixty-four by D527's grammar — a refusal at
/// a writer that is ganja, a truncation at a reader that cannot refuse — and
/// this is the same idea with room for a working directory beside it.
const MOST_SHOWN_POINTS: usize = 256;

/// What a candidate for a name looks like to whoever has to choose: enough
/// to tell two sessions apart, and the one spelling that cannot be
/// misunderstood.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The name as that session typed it — comparison folds, storage does
    /// not, so this is not necessarily the case the caller asked in.
    pub name: String,
    /// Its socket stem: unique among live sessions by the bind walk's own
    /// construction, which is what makes it the disambiguator.
    pub stem: String,
    /// The directory it was launched in, which is usually what tells two
    /// same-named sessions apart for the person looking at them.
    pub cwd: PathBuf,
    /// The exact `uds:` spelling that addresses it, ready to be a `to`.
    pub address: String,
}

/// What a name turned out to point at.
///
/// Four of the five variants are refusals, and that is the design rather
/// than a shortfall: ambiguity, a moved pin and a listing that could not be
/// taken all **refuse instead of guessing**, the same posture the receiver's
/// admission gate holds on the other end of the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one live session, and no pin says otherwise.
    Session {
        /// The name as that session typed it.
        name: String,
        /// Its full bare UUIDv7 — what a pin holds and an own-session
        /// exclusion compares.
        id: String,
        /// Its socket stem.
        stem: String,
        /// The socket it bound.
        socket: PathBuf,
        /// The directory it was launched in.
        cwd: PathBuf,
    },
    /// Several live sessions hold the name, so choosing one would be a guess
    /// about which was meant.
    Ambiguous {
        /// The name as asked.
        name: String,
        /// Everyone holding it.
        candidates: Vec<Candidate>,
    },
    /// The pin guard: this conversation already addressed a different
    /// session under this name.
    Moved {
        /// The name as asked.
        name: String,
        /// The stem of the session the name used to reach.
        pinned_stem: String,
        /// Who holds it now.
        candidates: Vec<Candidate>,
    },
    /// No live session answers to it.
    NoneSuch {
        /// The name as asked.
        name: String,
    },
    /// The registry could not be read, so the search was never taken: a
    /// failure to look, never a verdict about the name.
    ListingFailed {
        /// What the filesystem, or a liveness probe, said.
        error: String,
    },
}

/// What one pin holds: the identity a name reached, and the stem a refusal
/// names it by.
///
/// The stem rides along because it cannot be re-derived later — a session
/// whose record is gone still has to be nameable in the sentence that says
/// the name moved, and only the resolution that pinned it knew which prefix
/// that session had bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pinned {
    /// The full bare UUIDv7 the name reached.
    pub session_id: String,
    /// The socket stem it bound.
    pub stem: String,
}

/// The engine's identity handle: where the registry lives, and what this
/// conversation has already addressed.
///
/// Held as an `Arc<Identity>` beside the admission gate's `Arc<Inbound>`, and
/// shared for the same reason — a lead hands its engine to a socket server
/// and to its own loop at once. Every method takes `&self`; the map is a
/// plain `std` mutex held across no await, because nothing here awaits.
#[derive(Debug)]
pub struct Identity {
    /// The socket directory every read walks: seeded at assembly so the
    /// hidden `--socket-dir` override reaches the resolver exactly as it
    /// reaches the binder and the lister.
    directory: PathBuf,
    /// Name key (ASCII-folded, [`registry::same_name`]'s own fold) to the
    /// identity this conversation addressed under it.
    pins: Mutex<HashMap<String, Pinned>>,
}

impl Default for Identity {
    /// The directory this user's session sockets really live in.
    ///
    /// The one spelling of the default, so an assembly that was handed no
    /// `--socket-dir` override reads what the binder binds in and the lister
    /// lists from rather than deriving a second answer of its own.
    fn default() -> Self {
        Self::new(socket::directory())
    }
}

impl Identity {
    /// A handle over `directory`.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into(), pins: Mutex::new(HashMap::new()) }
    }

    /// What `name` points at right now, this conversation's pins consulted.
    ///
    /// A fresh read every call (the module doc says why): list the records,
    /// drop this session's own, keep the ones whose name folds equal, and
    /// only then probe those for liveness — the order matters, because a
    /// probe may create an absent `.lock` and probing every record would
    /// leave one beside every name in the directory.
    ///
    /// **Consult-only.** No path through this function writes the pin map;
    /// [`Identity::pin`] is the one door that does, and the deliver arm is
    /// its one caller.
    pub fn resolve(&self, name: &str, own_session: &str) -> Resolution {
        let live = match self.live_holders(name, own_session) {
            Ok(live) => live,
            Err(error) => return error,
        };

        if live.len() > 1 {
            // Ambiguity outranks the pin deliberately: both refuse, and
            // "several sessions answer to this" is the more precise thing to
            // say than "the one I knew about moved".
            return Resolution::Ambiguous {
                name: name.to_owned(),
                candidates: live.iter().map(|held| self.candidate(held)).collect(),
            };
        }
        let Some(found) = live.into_iter().next() else {
            return Resolution::NoneSuch { name: name.to_owned() };
        };

        if let Some(pinned) = self.pinned(name)
            && pinned.session_id != found.record.session_id
        {
            tracing::debug!(
                name,
                pinned = pinned.stem,
                now = found.stem,
                "a name this conversation addressed now reaches a different session"
            );

            return Resolution::Moved {
                name: name.to_owned(),
                pinned_stem: pinned.stem,
                candidates: vec![self.candidate(&found)],
            };
        }

        Resolution::Session {
            name: found.record.name,
            id: found.record.session_id,
            socket: self.socket_path(&found.stem),
            stem: found.stem,
            cwd: found.record.cwd,
        }
    }

    /// What `address` points at: the live session whose socket it names, or
    /// nobody.
    ///
    /// The `uds:` door, and the mirror of the `uds:` send rule — **pins are
    /// neither consulted nor created**, because an explicit address is
    /// already an identity and there is nothing for a name guard to protect.
    /// The comparison is exact: no canonicalization, so a second spelling of
    /// one socket reads as no record, which the address-miss rendering says
    /// honestly rather than dressing up as a session that is gone.
    ///
    /// Only [`Resolution::Session`], [`Resolution::NoneSuch`] and
    /// [`Resolution::ListingFailed`] can come back — a socket path names at
    /// most one file, so there is no ambiguity for this door to have.
    pub fn resolve_address(&self, address: &Path, own_session: &str) -> Resolution {
        let registered = match registry::list(&self.directory) {
            Ok(registered) => registered,
            Err(error) => return self.listing_failed(&error),
        };

        for held in registered {
            if held.record.session_id == own_session || self.socket_path(&held.stem) != address {
                continue;
            }
            return match registry::is_live(&self.directory, &held.stem) {
                Ok(true) => Resolution::Session {
                    name: held.record.name,
                    id: held.record.session_id,
                    socket: self.socket_path(&held.stem),
                    stem: held.stem,
                    cwd: held.record.cwd,
                },
                // A record whose session is gone is no record for this
                // purpose, and its liveness is the only thing a probe that
                // failed leaves unknown — so the strict answer stands here
                // too.
                Ok(false) => Resolution::NoneSuch { name: address.display().to_string() },
                Err(error) => self.listing_failed(&error),
            };
        }

        Resolution::NoneSuch { name: address.display().to_string() }
    }

    /// Records that `name` reached `session_id` at `stem` for a delivery this
    /// conversation's arm accepted.
    ///
    /// The deliver arm's door and nobody else's, called on an accepted
    /// **text** body only: the pin protects the *choice* of recipient, so it
    /// is taken at resolution and before the connect, and a body the arm
    /// refuses — a frame, which does not cross a socket however the socket
    /// was named — pins nothing at all.
    pub fn pin(&self, name: &str, session_id: &str, stem: &str) {
        self.pins()
            .insert(key(name), Pinned { session_id: session_id.to_owned(), stem: stem.to_owned() });
    }

    /// What this conversation already addressed under `name`, if anything.
    ///
    /// Public because it is the observable half of the guard: a test that
    /// asserts a mention pinned nothing, and a caller that wants to say what
    /// a name used to reach, both read it here rather than re-deriving it.
    #[must_use]
    pub fn pinned(&self, name: &str) -> Option<Pinned> {
        self.pins().get(&key(name)).cloned()
    }

    /// Forgets every pin — `NewSession`'s door.
    ///
    /// A new conversation has addressed nobody, so carrying the old one's
    /// choices into it would be guarding a history that no longer exists.
    pub fn clear_pins(&self) {
        self.pins().clear();
    }

    /// The pin map, held for exactly the call that asked for it.
    ///
    /// Poisoning is treated as unreachable, the admission gate's own posture
    /// for the same shape of state: nothing under this lock can panic, so a
    /// poisoned map would mean a panic that already ended the thing this
    /// guard protects.
    fn pins(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pinned>> {
        self.pins.lock().expect("the identity pin map is never poisoned")
    }

    /// The live records holding `name`, this session's own excluded, or the
    /// [`Resolution::ListingFailed`] that says the search was never taken.
    fn live_holders(
        &self,
        name: &str,
        own_session: &str,
    ) -> Result<Vec<registry::Registered>, Resolution> {
        let registered = match registry::list(&self.directory) {
            Ok(registered) => registered,
            Err(error) => return Err(self.listing_failed(&error)),
        };

        let mut live = Vec::new();
        for held in registered {
            if held.record.session_id == own_session
                || !registry::same_name(&held.record.name, name)
            {
                continue;
            }
            match registry::is_live(&self.directory, &held.stem) {
                Ok(true) => live.push(held),
                Ok(false) => {}
                // Stricter than the registry's own notice scan, and
                // deliberately: a holder whose liveness cannot be judged is a
                // search that did not finish, and a resolver that skipped it
                // could deliver to the wrong session rather than merely warn
                // about one too few.
                Err(error) => return Err(self.listing_failed(&error)),
            }
        }

        Ok(live)
    }

    /// One candidate row from a record the walk found.
    fn candidate(&self, held: &registry::Registered) -> Candidate {
        Candidate {
            name: held.record.name.clone(),
            stem: held.stem.clone(),
            cwd: held.record.cwd.clone(),
            address: address_of(&self.socket_path(&held.stem)),
        }
    }

    /// Where `stem`'s socket is — the one spelling, so the candidate's
    /// address, the resolved socket and a caller's own path cannot drift.
    fn socket_path(&self, stem: &str) -> PathBuf {
        self.directory.join(format!("{stem}.{}", socket::EXTENSION))
    }

    /// The refusal an unreadable registry earns, traced with the directory
    /// and never with anything a record held.
    fn listing_failed(&self, error: &std::io::Error) -> Resolution {
        tracing::debug!(
            directory = %self.directory.display(),
            %error,
            "a session name could not be looked up because the registry could not be read"
        );

        Resolution::ListingFailed { error: error.to_string() }
    }
}

/// The `uds:` spelling of `socket` — what a `to` argument takes, and the one
/// address form in every candidate, refusal and reminder.
#[must_use]
pub fn address_of(socket: &Path) -> String {
    format!("{ADDRESS_SCHEME}{}", socket.display())
}

/// The socket path inside a `uds:` token, or `None` for a token that is not
/// one.
///
/// The scheme's one reader on this side: a classifier that hands a `uds:`
/// mention through, and the mention seam that must turn it into a path,
/// spell it here rather than each slicing four characters of their own.
#[must_use]
pub fn address_path(token: &str) -> Option<&Path> {
    let path = token.strip_prefix(ADDRESS_SCHEME)?;

    (!path.is_empty()).then(|| Path::new(path))
}

/// The pin map's key for `name`: the fold [`registry::same_name`] compares
/// under, so a pin taken on `@Backend` is the pin `@backend` finds.
///
/// ASCII-only, because that predicate is — two names differing only in
/// non-ASCII case are two names there, and a key that folded further would
/// make the guard disagree with the resolver about what one name is.
fn key(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// One line of a value some other process wrote, safe to put in front of a
/// model: control characters — newlines among them — and the angle brackets
/// that would close the block this side framed it in, dropped; length capped
/// at [`MOST_SHOWN_POINTS`] with the cut admitted.
///
/// A registration record is same-uid-writable, so a name in one is not
/// bounded by the grammar ganja's own writer passes; this applies that
/// grammar's classes at the read side, where refusing is not an option
/// because the person already pointed at the thing. Dropping the brackets is
/// the same move a shim pane's paste body already makes for the paste's own
/// close marker (**D512**): content that could end its own frame is
/// neutralized *before* it is framed, never after.
#[must_use]
fn shown(value: &str) -> String {
    let admits = |point: &char| !point.is_control() && *point != '<' && *point != '>';
    let kept: String = value.chars().filter(admits).take(MOST_SHOWN_POINTS).collect();

    if value.chars().filter(admits).count() > MOST_SHOWN_POINTS {
        format!("{kept}…")
    } else {
        kept
    }
}

/// What an `@` token turned out to name, in the reminder's own vocabulary.
///
/// One variant per rendering, on purpose: a shape that could hold an
/// unreachable combination — an ambiguous socket address, say — would be a
/// shape somebody has to write an `unreachable!` for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mentioned {
    /// A teammate on this session's roster: the lead-assigned name.
    Teammate {
        /// The name as mentioned.
        name: String,
        /// Whether that teammate is the team's lead.
        lead: bool,
    },
    /// Exactly one live session, reached by name.
    Session {
        /// The name as mentioned.
        token: String,
        /// The name as that session typed it.
        name: String,
        /// Its stem.
        stem: String,
        /// Its launch directory.
        cwd: PathBuf,
        /// Its `uds:` address.
        address: String,
    },
    /// Several live sessions hold the mentioned name.
    Ambiguous {
        /// The name as mentioned.
        token: String,
        /// Everyone holding it.
        candidates: Vec<Candidate>,
    },
    /// The mentioned name reached a different session earlier in this
    /// conversation.
    Moved {
        /// The name as mentioned.
        token: String,
        /// The stem it used to reach.
        pinned_stem: String,
        /// Who holds it now.
        candidates: Vec<Candidate>,
    },
    /// Nothing answers to the mentioned name.
    Vanished {
        /// The name as mentioned.
        token: String,
    },
    /// A `uds:` token whose socket a live record names.
    Addressed {
        /// The address as mentioned.
        address: String,
        /// The name that session registered.
        name: String,
        /// Its stem.
        stem: String,
        /// Its launch directory.
        cwd: PathBuf,
    },
    /// A `uds:` token no live record names — which is **not** the same as a
    /// session being gone, and the rendering says so.
    AddressMiss {
        /// The address as mentioned.
        address: String,
    },
    /// The registry could not be read, so the token was never checked.
    ///
    /// Ganja's own arm rather than one the plan enumerates, and required for
    /// the same reason the address miss is its own sentence: rendering an
    /// unreadable listing as "nothing answers to that name" would be a claim
    /// this reader cannot make.
    Unchecked {
        /// The token as mentioned.
        token: String,
        /// What went wrong reading the registry.
        error: String,
    },
}

impl Mentioned {
    /// Where a name's resolution lands in the reminder's vocabulary.
    #[must_use]
    pub fn of_name(asked: &str, resolution: Resolution) -> Self {
        let token = asked.to_owned();
        match resolution {
            Resolution::Session { name, stem, socket, cwd, .. } => {
                Self::Session { token, name, stem, cwd, address: address_of(&socket) }
            }
            Resolution::Ambiguous { candidates, .. } => Self::Ambiguous { token, candidates },
            Resolution::Moved { pinned_stem, candidates, .. } => {
                Self::Moved { token, pinned_stem, candidates }
            }
            Resolution::NoneSuch { .. } => Self::Vanished { token },
            Resolution::ListingFailed { error } => Self::Unchecked { token, error },
        }
    }

    /// Where an address's resolution lands.
    ///
    /// [`Identity::resolve_address`] answers with three of the five
    /// variants; the two a socket lookup cannot produce fold into the
    /// address miss, whose sentence — no live record names this address, and
    /// the address may still be tried — stays true of them too. A fold
    /// rather than an `unreachable!`, because a rendering nobody reaches is
    /// cheaper than a panic somebody might.
    #[must_use]
    pub fn of_address(address: &str, resolution: Resolution) -> Self {
        match resolution {
            Resolution::Session { name, stem, cwd, .. } => {
                Self::Addressed { address: address.to_owned(), name, stem, cwd }
            }
            Resolution::ListingFailed { error } => {
                Self::Unchecked { token: address.to_owned(), error }
            }
            Resolution::NoneSuch { .. }
            | Resolution::Ambiguous { .. }
            | Resolution::Moved { .. } => Self::AddressMiss { address: address.to_owned() },
        }
    }
}

/// The one rendering of an `@`-mention reminder — **D491**'s identity-pin
/// discipline, so a test compares against this function rather than against a
/// second copy of its words.
///
/// The block is ordinary conversation text (`Part::text`), which is what it
/// is: information the model reads once. Every arm says that mentioning
/// something sent nothing, names no reply channel, and labels what it found
/// by how much of it is checkable.
#[must_use]
pub fn reminder(mentioned: &Mentioned) -> String {
    match mentioned {
        Mentioned::Teammate { name, lead } => {
            let name = shown(name);
            let opening = if *lead {
                format!("@{name} names this team's lead on this session's roster.")
            } else {
                format!("@{name} names a teammate on this session's roster.")
            };

            block(
                &name,
                vec![
                    format!(
                        "{opening} That name is lead-assigned — it was given at the spawn door \
                         this session opened — so it identifies exactly one teammate, and nothing \
                         self-asserted stands behind it."
                    ),
                    String::new(),
                    format!(
                        "Mentioning it sent nothing. If the request calls for communicating with \
                         it, call send_message with to: {name:?}."
                    ),
                ],
            )
        }
        Mentioned::Session { token, name, stem, cwd, address } => {
            let (token, name) = (shown(token), shown(name));
            let (stem, address) = (shown(stem), shown(address));

            block(
                &token,
                vec![
                    format!(
                        "@{token} resolves to one live session of yours: registered name \
                         {name:?}, stem {stem}, working directory {}. That name is self-chosen \
                         and unverified — the session wrote it into its own registration record, \
                         and nothing here checks it against anything; the stem and the address \
                         are what actually identify it.",
                        shown(&cwd.display().to_string())
                    ),
                    String::new(),
                    format!(
                        "Mentioning it sent nothing. If the request calls for communicating with \
                         that session, call send_message with to: {token:?}, or with to: \
                         {address:?} to address it by socket rather than by name."
                    ),
                ],
            )
        }
        Mentioned::Ambiguous { token, candidates } => {
            let token = shown(token);
            let mut body = vec![
                format!(
                    "@{token} resolves to more than one live session, so which one was meant is \
                     not something this side may guess at:"
                ),
                String::new(),
            ];
            body.extend(candidates.iter().map(candidate_line));
            body.push(String::new());
            body.push(
                "Mentioning it sent nothing, and a send by that bare name would be refused for \
                 the same reason. Ask the person which one they meant, then call send_message \
                 with that session's uds: address."
                    .to_owned(),
            );

            block(&token, body)
        }
        Mentioned::Moved { token, pinned_stem, candidates } => {
            let (token, pinned_stem) = (shown(token), shown(pinned_stem));
            let mut body = vec![
                format!(
                    "@{token} named a different session earlier in this conversation — the one \
                     whose stem is {pinned_stem} — and now names another. A registered name is \
                     self-asserted, so a name that moved is no evidence that the session did:"
                ),
                String::new(),
            ];
            body.extend(candidates.iter().map(candidate_line));
            body.push(String::new());
            body.push(
                "Mentioning it sent nothing, and a send by that bare name would be refused for \
                 the same reason. Confirm with the person which session they mean, then call \
                 send_message with that session's uds: address."
                    .to_owned(),
            );

            block(&token, body)
        }
        Mentioned::Vanished { token } => {
            let token = shown(token);

            block(
                &token,
                vec![format!(
                    "@{token} names no teammate on this session's roster and no live session. \
                     Mentioning it sent nothing, and there is nothing to address under that name."
                )],
            )
        }
        Mentioned::Addressed { address, name, stem, cwd } => {
            let (address, name, stem) = (shown(address), shown(name), shown(stem));

            block(
                &address,
                vec![
                    format!(
                        "@{address} points at one live session of yours: registered name \
                         {name:?}, stem {stem}, working directory {}. The name is self-chosen and \
                         unverified; the address is not — it is the socket that was pointed at.",
                        shown(&cwd.display().to_string())
                    ),
                    String::new(),
                    format!(
                        "Mentioning it sent nothing. If the request calls for communicating with \
                         that session, call send_message with to: {address:?}."
                    ),
                ],
            )
        }
        Mentioned::AddressMiss { address } => {
            let address = shown(address);

            block(
                &address,
                vec![
                    format!(
                        "No live session's registration record names {address}. That is not the \
                         same as the session being gone: a socket can outlive its record, and a \
                         session bound by a build that registers no name answers the wire while \
                         answering no listing."
                    ),
                    String::new(),
                    format!(
                        "Mentioning it sent nothing. The address may still be tried: call \
                         send_message with to: {address:?}."
                    ),
                ],
            )
        }
        Mentioned::Unchecked { token, error } => {
            let (token, error) = (shown(token), shown(error));

            block(
                &token,
                vec![
                    format!(
                        "@{token} could not be checked: this session's socket directory could not \
                         be read ({error}). An unreadable listing is not an empty one, so nothing \
                         here says whether anything answers to that name."
                    ),
                    String::new(),
                    "Mentioning it sent nothing. A uds: socket address, if the person has one, \
                     reaches send_message without consulting the listing."
                        .to_owned(),
                ],
            )
        }
    }
}

/// What the deliver arm says when a name holds more than one live session —
/// `Undelivered::Ambiguous`'s sentence, composed here because the resolver
/// owns the listing and the tool passes a reason through without learning
/// anything.
#[must_use]
pub fn ambiguous_refusal(name: &str, candidates: &[Candidate]) -> String {
    let mut lines = vec![
        format!(
            "More than one live session goes by {:?}, so delivering to one of them would be a \
             guess about which was meant. Nothing was sent. Address one by its uds: spelling:",
            shown(name)
        ),
        String::new(),
    ];
    lines.extend(candidates.iter().map(candidate_line));

    lines.join("\n")
}

/// What the deliver arm says when the pin guard halts a delivery —
/// `Undelivered::NameMoved`'s sentence.
#[must_use]
pub fn moved_refusal(name: &str, pinned_stem: &str, candidates: &[Candidate]) -> String {
    let mut lines = vec![
        format!(
            "{:?} now names a different session than the one this conversation already addressed \
             under it — the one whose stem is {}. Nothing was sent: a registered name is \
             self-asserted, so following it to a new claimant would be delivering to somebody \
             this conversation never chose. Confirm with the person, or address a session by its \
             uds: spelling:",
            shown(name),
            shown(pinned_stem)
        ),
        String::new(),
    ];
    lines.extend(candidates.iter().map(candidate_line));

    lines.join("\n")
}

/// What the deliver arm says when the registry could not be read —
/// `Undelivered::Failed`'s sentence, which is infrastructure rather than a
/// verdict and says so.
#[must_use]
pub fn listing_refusal(name: &str, error: &str) -> String {
    format!(
        "{:?} could not be looked up: this session's socket directory could not be read ({}). \
         Nothing was sent, and this is a failure to search rather than a verdict about the name. \
         A uds: socket address is delivered without consulting the listing.",
        shown(name),
        shown(error)
    )
}

/// One candidate as every listing spells it — the reminder's and the
/// refusals' alike, so a person and a model are shown the same row.
fn candidate_line(candidate: &Candidate) -> String {
    format!(
        "- {:?} — stem {}, working directory {}, address {}",
        shown(&candidate.name),
        shown(&candidate.stem),
        shown(&candidate.cwd.display().to_string()),
        shown(&candidate.address)
    )
}

/// A reminder block around `body`, tagged and opened with the token as the
/// person typed it.
fn block(token: &str, body: Vec<String>) -> String {
    let mut lines = Vec::with_capacity(body.len() + 2);
    lines.push(format!("<{TAG} token=\"@{token}\">"));
    lines.extend(body);
    lines.push(format!("</{TAG}>"));

    lines.join("\n")
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
