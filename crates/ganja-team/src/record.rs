//! Claude Code's own file formats: the team file, its member records, and one
//! message at rest in an inbox.
//!
//! **Upstream opencode has no counterpart.** The specification is Claude
//! Code's, read out of the reference document — §2.2 for the team file and the
//! two member shapes, §2.3 for the inbox message schema and its envelope,
//! §9.1 for what `subscriptions` is worth (**D497**).
//!
//! These are **somebody else's documents**, and everything odd about the
//! shapes below follows from that one fact.
//!
//! *Unknown keys survive, in position.* Every shape here carries a
//! `#[serde(flatten)] extra` of [`IndexMap`], so a key a newer Claude Code
//! writes is read, kept, and written back after the known fields in the order
//! it arrived. `serde_json::Map` cannot do that job — without the
//! `preserve_order` feature it is a `BTreeMap`, so an unknown key would come
//! back alphabetized, and turning that feature on would reorder every `Map` in
//! the workspace through feature unification. Two limitations are recorded
//! rather than hidden. A nested object *inside* an unknown key's value is a
//! `serde_json::Value::Object` and would still reorder on rewrite; no captured
//! document has one yet. And "in position" means *after the known fields*, not
//! back where it was found — which the 2026-03-era team files on this machine
//! **witness**: their `config.json` carries a top-level `description` between
//! `name` and `createdAt`, and a rewrite here would move it to the tail. The
//! documents a modern Claude Code writes carry no unknown key at all, so byte
//! identity against those is unaffected; a reader who ever meets one of the
//! old files should expect that one move and nothing else.
//!
//! *This is why the shapes are not in `ganja-protocol`.* That crate's posture
//! is the exact opposite one — an exhaustive vocabulary with
//! `deny_unknown_fields`, refusing a peer that grew a field rather than
//! guessing at it. A passthrough shape declared there would contradict the
//! doctrine at the point it is stated, so `ganja-protocol` carries ganja's own
//! `TeamView`/`MemberView` projection for anything that renders a team, and
//! Claude's documents stay here with the file I/O.
//!
//! *Bytes are a compatibility surface.* Documents are written with
//! [`document`] — `serde_json::to_string_pretty`, two-space indent, no
//! trailing newline — which is what `JSON.stringify(value, null, 2)` produces,
//! and the keys are emitted in the order a real `claude` emits them. A rewrite
//! that reordered or reindented would still parse; it would also make every
//! `git diff` of a shared directory unreadable, and it would be the first
//! thing to suspect when a byte-identity test failed for an unrelated reason.
//!
//! *The key orders come from real documents, not from the reference.* §2.2's
//! member records are marked `[OBS]` and are reflowed for the page, so their
//! printed order is not evidence of anything; the survey behind AC-1b is — 29
//! team directories under `~/.claude/teams`, 65 member records and 131
//! messages, spanning a 2026-03 era and the modern one. Where the two disagree
//! the bytes win, and they do disagree: the orders below are not the ones this
//! module first shipped. Each shape says which documents settled it, and says
//! plainly where the evidence is thin.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use jiff::Timestamp;
use serde::ser::{self, SerializeMap as _};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::team::{LEAD, MemberName, TeamName};

/// `backendType` for a member that runs inside a process rather than a pane.
pub const BACKEND_IN_PROCESS: &str = "in-process";

/// `backendType` for a member that owns a tmux pane.
pub const BACKEND_TMUX: &str = "tmux";

/// `backendType` for a member driven as a headless `codex exec` child.
///
/// ganja's own word, not Claude's: §2.2's vocabulary has no name for a
/// teammate that is somebody else's CLI, because Claude has no such teammate.
/// Safe to mint because a shim member is recorded in **ganja's** teams root
/// only — the claude root is never written for one — so these strings never
/// reach a directory a real `claude` owns.
pub const BACKEND_CODEX: &str = "codex";

/// `backendType` for a member driven as a resident `agy` child.
pub const BACKEND_AGY: &str = "agy";

/// `backendType` for a member driven as a headless `grok` child.
pub const BACKEND_GROK: &str = "grok";

/// The `tmuxPaneId` a lead's record carries instead of a pane id (§2.2).
pub const PANE_LEADER: &str = "leader";

/// The `tmuxPaneId` an in-process teammate's record carries instead of a pane
/// id (§2.2).
pub const PANE_IN_PROCESS: &str = "in-process";

/// The envelope version `writeToMailbox` stamps on every message (§2.3).
pub const MESSAGE_VERSION: u32 = 1;

/// The `type` a message at rest carries; `writeToMailbox` forces it (§2.3).
pub const MESSAGE_TYPE: &str = "message";

/// Which surface a member runs on, read off the one field that says so.
///
/// §2.2 overloads `tmuxPaneId` as a surface discriminator: `"leader"` for the
/// lead, `"in-process"` for a teammate in the lead's own process, and a real
/// `%N` for a pane. §8.4's advice is to model the surface rather than pass the
/// sentinel around, so the sentinel is kept at the serialization boundary —
/// [`MemberRecord::tmux_pane_id`] is what lands on disk, verbatim — and every
/// reader asks [`MemberRecord::surface`] instead of comparing strings.
///
/// The pane's *birth* is deliberately not here. `%N` recycles, so identifying
/// a live pane needs the `(id, birth)` pair — but birth is a fact about a
/// running tmux server, not a field in Claude's document, and inventing a key
/// for it would be an unstated amendment to a format somebody else owns. It
/// belongs to whatever holds the running pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Surface {
    /// The lead's own record.
    Leader,
    /// A teammate running inside the lead's process.
    InProcess,
    /// A teammate with a pane of its own.
    Pane {
        /// The `%N` tmux gave it.
        id: String,
    },
    /// A teammate that is a CLI this process shims for (**D508**).
    ///
    /// *Shim* rather than *foreign*: the `claude` backend is a foreign binary
    /// too and is not in this class. What these three have in common is that
    /// they do not speak the mailbox at all, so ganja stands between the
    /// mailbox and the CLI — which is also the word the rest of the tree
    /// already uses for them.
    ///
    /// Since P28 (**D512**) a shim member may hold a tmux pane of its own —
    /// its CLI's native TUI, spoken to through the pane — and `pane` is that
    /// pane's `%N` when it does. [`None`] is the headless child, which owns
    /// no pane; the two are one variant because `backendType` says the same
    /// thing for both (the CLI's name), and only `tmuxPaneId` differs.
    Shim {
        /// Which CLI drives it.
        cli: ShimCli,
        /// The `%N` tmux gave its pane, when the CLI runs in one.
        pane: Option<String>,
    },
}

/// Which CLI a [`Surface::Shim`] member is driven by.
///
/// A vocabulary of its own rather than ganja's `MemberBackend`, so the variant
/// cannot be built holding a surface that is not shimmed at all — the three
/// names here are exactly the three that have no other way of being recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShimCli {
    /// A headless `codex exec` child.
    Codex,
    /// A resident `agy` child.
    Agy,
    /// A headless `grok` child.
    Grok,
}

impl Surface {
    /// Reads §2.2's overloaded field back into a surface.
    ///
    /// Anything that is neither sentinel is a pane id, which is how a pane is
    /// recognized in the first place — there is no separate marker, and a
    /// `backendType` disagreeing with this is somebody else's inconsistency to
    /// notice, not a reason to refuse the document.
    ///
    /// # This read never answers [`Surface::Shim`], and that is deliberate
    ///
    /// A headless shim member writes [`PANE_IN_PROCESS`] into `tmuxPaneId`,
    /// so it reads back as [`Surface::InProcess`] — the round trip is **lossy
    /// in one direction on purpose**, and [`MemberRecord::surface`] inherits
    /// the loss.
    ///
    /// The alternative was a fourth sentinel, and it is worse than it looks.
    /// This function classifies any non-sentinel string as a pane id, so a
    /// `"codex"` in that field would be read by every build that exists today
    /// — and by a real `claude` sharing the directory — as
    /// `Surface::Pane { id: "codex" }`: a pane that can never exist, handed to
    /// code whose whole job is to act on panes. Reusing the in-process
    /// sentinel means every reader that does not know about shims classifies
    /// the surface *safely*, and what each of them then believes is true: an
    /// older ganja sees an in-process member it renders but cannot drive, and
    /// it cannot; a real `claude` sees an in-process member whose mailbox
    /// address works, and it does, because the shim reads that inbox.
    ///
    /// A shim member **in a pane** (P28, **D512**) writes the real `%N`, and
    /// so reads back as [`Surface::Pane`] — lossy the other way, and safe as
    /// far as this crate can speak for: the pane exists, so every reader that
    /// acts on panes acts on a pane that is there, and ganja's own kills
    /// identity-check the `(pane_id, birth)` pair against the running server,
    /// where no record carries a birth — ganja cannot end this one by mistake.
    /// What a *foreign* reader does with a real `%N` is that reader's: a real
    /// `claude` sharing the directory is not known to check anything, and
    /// where the in-process sentinel gave it nothing to act on a pane id gives
    /// it a pane. The residual is bounded rather than absent — the worst such
    /// a reader can do is end a pane that is on screen in front of a person,
    /// who sees it go. What is lost on the read is only which CLI sat in it,
    /// and that is in `backendType`, where it always was.
    ///
    /// The one reader that genuinely needs shim-ness — the lead-restart sweep
    /// — reads `backendType` directly, which [`MemberRecord::teammate`]
    /// populates from this surface.
    #[must_use]
    pub fn read(tmux_pane_id: &str) -> Self {
        match tmux_pane_id {
            PANE_LEADER => Self::Leader,
            PANE_IN_PROCESS => Self::InProcess,
            id => Self::Pane { id: id.to_owned() },
        }
    }

    /// What this surface writes into `tmuxPaneId`.
    ///
    /// A headless shim teammate answers the **in-process** sentinel rather
    /// than one of its own; [`Surface::read`] owns why. A shim teammate in a
    /// pane answers that pane's real id, exactly as a `ganja` or `claude` pane
    /// does — the pane is there, and a reader acting on it acts on something
    /// real.
    #[must_use]
    pub fn tmux_pane_id(&self) -> &str {
        match self {
            Self::Leader => PANE_LEADER,
            Self::InProcess | Self::Shim { pane: None, .. } => PANE_IN_PROCESS,
            Self::Pane { id } | Self::Shim { pane: Some(id), .. } => id,
        }
    }

    /// What this surface writes into `backendType`.
    ///
    /// A lead's record says `in-process` in §2.2's own excerpt, even though the
    /// lead is not a teammate at all — the two fields answer different
    /// questions and only `tmuxPaneId` distinguishes the three cases.
    ///
    /// This is the field that carries shim-ness, and the only one: it is where
    /// a reader that needs to tell a foreign CLI from a teammate in this
    /// process — or from a `ganja` pane — has to look, precisely because
    /// `tmuxPaneId` deliberately does not say.
    #[must_use]
    pub fn backend_type(&self) -> &str {
        match self {
            Self::Leader | Self::InProcess => BACKEND_IN_PROCESS,
            Self::Pane { .. } => BACKEND_TMUX,
            Self::Shim { cli, .. } => cli.backend_type(),
        }
    }
}

impl ShimCli {
    /// What this CLI writes into `backendType`.
    ///
    /// An exhaustive match, so a fourth CLI is a build failure here rather
    /// than a member recorded under a name nothing reads.
    #[must_use]
    pub const fn backend_type(self) -> &'static str {
        match self {
            Self::Codex => BACKEND_CODEX,
            Self::Agy => BACKEND_AGY,
            Self::Grok => BACKEND_GROK,
        }
    }

    /// [`ShimCli::backend_type`] read back, or [`None`] for anything else.
    ///
    /// The other direction of the same table, beside it for
    /// [`Surface::read`]'s reason: two spellings of which strings name which
    /// CLI is how a record comes to be written under one name and read under
    /// another. Answers [`None`] for [`BACKEND_IN_PROCESS`] and
    /// [`BACKEND_TMUX`] as much as for a string nothing here minted — neither
    /// of those names a shim.
    #[must_use]
    pub fn read(backend_type: &str) -> Option<Self> {
        [Self::Codex, Self::Agy, Self::Grok]
            .into_iter()
            .find(|cli| cli.backend_type() == backend_type)
    }
}

/// What a spawn decides about a teammate, gathered so building the record is
/// one call rather than eight arguments.
///
/// `prompt` is the full spawn prompt, and it **is persisted verbatim** (D-7,
/// §7-8): a credential written into a teammate's prompt lands on this disk in
/// cleartext. That is Claude Code's behavior and it is adopted deliberately
/// rather than by omission — a teammate that could not read its own
/// instructions after a restart would be a different feature — which is why it
/// is said here, in `AGENTS.md`, and once more in the spawn confirmation a
/// person sees.
///
/// Which is also why its [`Debug`] is hand-written and renders `prompt` as a
/// byte count: the field is documented as a place credentials land, so it must
/// not be the field a `{:?}` in some caller's error path prints.
#[derive(Clone, PartialEq, Eq)]
pub struct Spawn {
    /// `agentType`, which is the `task` tool's `subagent_type` (§2.2).
    pub agent_type: String,
    /// The model this teammate runs as.
    pub model: String,
    /// The color assigned to it — keyed on the *agent id* rather than the
    /// name, per §2.2, which is the caller's business to have done.
    pub color: String,
    /// The spawn prompt, verbatim. See the type's own note.
    pub prompt: String,
    /// Whether this teammate must start in plan mode.
    pub plan_mode_required: bool,
    /// The surface it runs on.
    pub surface: Surface,
    /// The working directory it was spawned in.
    pub cwd: String,
}

impl fmt::Debug for Spawn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Spawn")
            .field("agent_type", &self.agent_type)
            .field("model", &self.model)
            .field("color", &self.color)
            .field("prompt", &Redacted(Some(&self.prompt)))
            .field("plan_mode_required", &self.plan_mode_required)
            .field("surface", &self.surface)
            .field("cwd", &self.cwd)
            .finish()
    }
}

/// One member of a team, in Claude's own document shape (§2.2).
///
/// **Two key orders, one struct, and a hand-written [`Serialize`] to choose
/// between them.** A lead record and a teammate record are not the same shape
/// with fields missing — they are written in *irreconcilable orders*, which no
/// single declaration can express:
///
/// ```text
/// lead     agentId name agentType joinedAt tmuxPaneId cwd subscriptions backendType
/// teammate agentId name color joinedAt tmuxPaneId subscriptions agentType model
///          prompt planModeRequired cwd backendType isActive
/// ```
///
/// `agentType` is third in one and seventh in the other; `cwd` precedes
/// `subscriptions` in one and trails it by five keys in the other. All 24 lead
/// records and all 26 teammate records in the modern half of the survey agree
/// with their own line and no record straddles the two, so this is the format
/// rather than a sample of it.
///
/// This module first shipped one declaration order with the five teammate-only
/// fields skipping when absent, on the reading that §2.2's excerpt printed the
/// order. **Real documents falsified that**, and the correction is confined to
/// [`Serialize`]: [`Deserialize`] stays derived, because decoding is
/// order-insensitive and the flatten passthrough needs no help.
///
/// *Not an untagged enum,* and that argument survives the falsification
/// unchanged: untagged deserialization picks an arm by trying them, so a
/// teammate record missing one field would quietly decode as a lead instead of
/// saying what was wrong. Two arms would also have to keep the shared eight
/// fields in step by hand — which is now a bigger job, not a smaller one.
///
/// *The discriminant is the record's shape, not its name.* A record is written
/// in the teammate order when it carries any of the five teammate-only fields
/// (`model`, `color`, `prompt`, `planModeRequired`, `isActive`), and in the
/// lead order otherwise — so "a lead-ordered record never emits the five" is a
/// tautology rather than a promise. [`MemberRecord::is_lead`] answers a
/// different question — *is this the team's lead* — and would be the wrong
/// discriminant for a reason the survey supplies: a 2026-03-era lead record is
/// named `team-lead` and **carries `model` anyway**, so keying the order on the
/// name would drop that value on rewrite. Keying it on the shape moves the key
/// instead, which is the trade the flatten passthrough already makes.
///
/// The name is a `String` rather than a [`MemberName`] on purpose: the type
/// marks the door a *created* name goes through, and refusing to decode a
/// document a real `claude` wrote is not this crate's call to make.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberRecord {
    /// `<name>@<team>` — derived, never minted (§2.2).
    pub agent_id: String,
    /// The bare name, which is also the mailbox address.
    pub name: String,
    /// Teammate-only: its assigned color.
    #[serde(default)]
    pub color: Option<String>,
    /// Unix milliseconds at registration.
    pub joined_at: u64,
    /// §2.2's overloaded surface discriminator; read it through
    /// [`MemberRecord::surface`].
    pub tmux_pane_id: String,
    /// Vestigial (§9.1): written `[]` at every creation site and read nowhere,
    /// in Claude Code and here alike.
    ///
    /// **The reference's advice to omit it is declined**, and the reason is
    /// byte identity rather than doubt about the finding. Left out of the
    /// declaration, a real document's `subscriptions` would be captured by
    /// `extra` and re-emitted at the tail instead of in the middle of the
    /// record — so the one thing omitting it would cost is the round-trip the
    /// whole format contract rests on. Declared, it is written `[]` and never
    /// populated, which is what "vestigial" buys in practice; a rewrite carries
    /// whatever it read, so a file that somehow holds something keeps it.
    #[serde(default)]
    pub subscriptions: Vec<Value>,
    /// The `task` tool's `subagent_type`; `team-lead` for the lead.
    pub agent_type: String,
    /// Teammate-only: the model it runs as.
    #[serde(default)]
    pub model: Option<String>,
    /// Teammate-only: the spawn prompt, in cleartext. See [`Spawn::prompt`].
    #[serde(default)]
    pub prompt: Option<String>,
    /// Teammate-only: whether it must start in plan mode.
    #[serde(default)]
    pub plan_mode_required: Option<bool>,
    /// The working directory the member runs in.
    pub cwd: String,
    /// `in-process` or `tmux`, as Claude spells them.
    ///
    /// Optional only for tolerance, never for a document this build writes:
    /// every modern record carries it and both constructors set it, but the
    /// 2026-03-era lead records in the survey **omit it entirely**, and
    /// refusing to decode one of those would be refusing a file a real `claude`
    /// wrote (§2.4's posture, and this crate's whole reason for existing).
    #[serde(default)]
    pub backend_type: Option<String>,
    /// Teammate-only: whether the member is still live.
    #[serde(default)]
    pub is_active: Option<bool>,
    /// Every key this build has never heard of, after the known fields and in
    /// the order they arrived.
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

/// Every key a [`MemberRecord`] can emit — the union of §2.2's two orders.
///
/// Exists so the passthrough guard has something to check against and so a test
/// can assert the emitted key set never leaves it. Order is the teammate one,
/// which is the superset; the lead order is a subset in a different sequence,
/// which is the whole reason [`Serialize`] is hand-written.
const MEMBER_KEYS: [&str; 13] = [
    "agentId",
    "name",
    "color",
    "joinedAt",
    "tmuxPaneId",
    "subscriptions",
    "agentType",
    "model",
    "prompt",
    "planModeRequired",
    "cwd",
    "backendType",
    "isActive",
];

/// Every key a [`TeamFile`] emits (§2.2).
const TEAM_FILE_KEYS: [&str; 5] = ["name", "createdAt", "leadAgentId", "leadSessionId", "members"];

/// Every key a [`MailboxMessage`] emits (§2.3), and so the ones a message's
/// passthrough map may not carry.
///
/// Tied to that struct's declaration and to `mailbox::validate`'s field lists
/// by `the_schema_key_list_is_exactly_what_a_message_serializes`, because all
/// three are hand-written and a tenth field would otherwise be governed by none
/// of them.
pub(crate) const SCHEMA_KEYS: [&str; 9] =
    ["type", "from", "text", "timestamp", "read", "color", "summary", "msgV", "msg_id"];

/// The passthrough keys a declared field already spells, each as the sentence
/// the refusal carries.
///
/// The guard [`crate::mailbox::write`] takes before it touches an inbox and
/// the two [`Serialize`] impls below take before they emit a byte, for one
/// reason: a map holding a key the shape also declares would emit that key
/// **twice**, and a reader taking the last one would read something the writer
/// never meant. JSON does not forbid it and `serde_json` will happily write it,
/// so the refusal has to be here.
///
/// Unreachable from a document read off disk — a declared key is captured by its
/// field before the flatten map ever sees it — and unreachable from either
/// constructor, which start `extra` empty. It is checked anyway, because the one
/// way to get here is hand-building a record, the cost of being wrong is a
/// corrupt file in a directory somebody else is reading, and the check is a
/// lookup against a fixed list.
pub(crate) fn shadowed(extra: &IndexMap<String, Value>, declared: &[&str]) -> Vec<String> {
    extra
        .keys()
        .filter(|key| declared.contains(&key.as_str()))
        .map(|key| {
            format!(
                "{key}: the shape declares this key, so a passthrough map may not also carry it"
            )
        })
        .collect()
}

/// [`shadowed`], as the error a [`Serialize`] impl answers with — the first
/// offender, since a serializer error carries one sentence.
fn refuse_shadowed<E>(extra: &IndexMap<String, Value>, declared: &[&str]) -> Result<(), E>
where
    E: ser::Error,
{
    match shadowed(extra, declared).into_iter().next() {
        Some(first) => Err(E::custom(first)),
        None => Ok(()),
    }
}

/// The two orders of §2.2, chosen by the record's own shape.
///
/// A `serialize_map` rather than a `serialize_struct` because the key set is
/// decided at runtime — which is also what the derive does under a
/// `#[serde(flatten)]`, so this changes the machinery not at all, only which
/// keys go through it and when.
///
/// **The opening destructure is the exhaustiveness net.** A hand-written
/// `Serialize` is the one place in this crate where adding a field to a struct
/// and forgetting it here compiles cleanly and silently drops the value on every
/// rewrite — of somebody else's document. Binding every field by name means the
/// next field added to [`MemberRecord`] is a compile error in this function
/// instead, and the bindings are what the body emits so a stale `self.` access
/// cannot creep back in.
impl Serialize for MemberRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let Self {
            agent_id,
            name,
            color,
            joined_at,
            tmux_pane_id,
            subscriptions,
            agent_type,
            model,
            prompt,
            plan_mode_required,
            cwd,
            backend_type,
            is_active,
            extra,
        } = self;
        refuse_shadowed(extra, &MEMBER_KEYS)?;

        let mut map = serializer.serialize_map(None)?;

        // The two orders share only their first two keys; from `agentType`
        // onwards nothing lines up, which is why this is a branch and not a
        // pair of conditional inserts.
        map.serialize_entry("agentId", agent_id)?;
        map.serialize_entry("name", name)?;

        if self.carries_teammate_fields() {
            if let Some(color) = color {
                map.serialize_entry("color", color)?;
            }
            map.serialize_entry("joinedAt", joined_at)?;
            map.serialize_entry("tmuxPaneId", tmux_pane_id)?;
            map.serialize_entry("subscriptions", subscriptions)?;
            map.serialize_entry("agentType", agent_type)?;
            if let Some(model) = model {
                map.serialize_entry("model", model)?;
            }
            if let Some(prompt) = prompt {
                map.serialize_entry("prompt", prompt)?;
            }
            if let Some(plan_mode_required) = plan_mode_required {
                map.serialize_entry("planModeRequired", plan_mode_required)?;
            }
            map.serialize_entry("cwd", cwd)?;
            if let Some(backend_type) = backend_type {
                map.serialize_entry("backendType", backend_type)?;
            }
            if let Some(is_active) = is_active {
                map.serialize_entry("isActive", is_active)?;
            }
        } else {
            map.serialize_entry("agentType", agent_type)?;
            map.serialize_entry("joinedAt", joined_at)?;
            map.serialize_entry("tmuxPaneId", tmux_pane_id)?;
            map.serialize_entry("cwd", cwd)?;
            map.serialize_entry("subscriptions", subscriptions)?;
            if let Some(backend_type) = backend_type {
                map.serialize_entry("backendType", backend_type)?;
            }
        }

        for (key, value) in extra {
            map.serialize_entry(key, value)?;
        }

        map.end()
    }
}

/// Renders everything except the spawn prompt, which is rendered as its size.
///
/// §2.2 persists the **full spawn prompt verbatim** (see [`Spawn::prompt`]), so
/// a credential a caller put in one is in this struct. A derived `Debug` would
/// put it into every `{:?}` — an error context, a `tracing` field, a panic
/// message — which is the leak `tests/no_bodies_in_logs.rs` exists to catch.
/// Everything else here is addressing: a name, a pane, a model, a directory, and
/// keeping those is what makes a record still debuggable.
impl fmt::Debug for MemberRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemberRecord")
            .field("agent_id", &self.agent_id)
            .field("name", &self.name)
            .field("color", &self.color)
            .field("joined_at", &self.joined_at)
            .field("tmux_pane_id", &self.tmux_pane_id)
            .field("subscriptions", &self.subscriptions)
            .field("agent_type", &self.agent_type)
            .field("model", &self.model)
            .field("prompt", &Redacted(self.prompt.as_deref()))
            .field("plan_mode_required", &self.plan_mode_required)
            .field("cwd", &self.cwd)
            .field("backend_type", &self.backend_type)
            .field("is_active", &self.is_active)
            .field("extra", &self.extra)
            .finish()
    }
}

/// A string rendered as its length, for the fields that carry somebody's words.
///
/// `<11 bytes>` and `None` are both answers a person debugging can use; the
/// eleven bytes themselves are content, and content does not belong in a
/// rendering that anything at all might log. The spelling matches
/// [`crate::mailbox::Identity`]'s, so one grep finds every place this rule is
/// applied.
struct Redacted<'a>(Option<&'a str>);

impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(text) => write!(formatter, "<{} bytes>", text.len()),
            None => formatter.write_str("None"),
        }
    }
}

impl MemberRecord {
    /// §2.2's lead record: the five teammate-only fields absent, the surface
    /// [`Surface::Leader`], and the name the constant [`LEAD`].
    #[must_use]
    pub fn lead(team: &TeamName, cwd: impl Into<String>, joined_at: u64) -> Self {
        let name = MemberName::lead();

        Self {
            agent_id: name.agent_id(team),
            name: LEAD.to_owned(),
            color: None,
            joined_at,
            tmux_pane_id: Surface::Leader.tmux_pane_id().to_owned(),
            subscriptions: Vec::new(),
            agent_type: LEAD.to_owned(),
            model: None,
            prompt: None,
            plan_mode_required: None,
            cwd: cwd.into(),
            backend_type: Some(Surface::Leader.backend_type().to_owned()),
            is_active: None,
            extra: IndexMap::new(),
        }
    }

    /// §2.2's teammate record: all five teammate-only fields present, so the
    /// shape is a teammate's whatever the spawn decided.
    #[must_use]
    pub fn teammate(name: &MemberName, team: &TeamName, spawn: Spawn, joined_at: u64) -> Self {
        Self {
            agent_id: name.agent_id(team),
            name: name.as_str().to_owned(),
            color: Some(spawn.color),
            joined_at,
            tmux_pane_id: spawn.surface.tmux_pane_id().to_owned(),
            subscriptions: Vec::new(),
            agent_type: spawn.agent_type,
            model: Some(spawn.model),
            prompt: Some(spawn.prompt),
            plan_mode_required: Some(spawn.plan_mode_required),
            cwd: spawn.cwd,
            backend_type: Some(spawn.surface.backend_type().to_owned()),
            is_active: Some(true),
            extra: IndexMap::new(),
        }
    }

    /// Which surface this member runs on, read off §2.2's overloaded field.
    #[must_use]
    pub fn surface(&self) -> Surface {
        Surface::read(&self.tmux_pane_id)
    }

    /// Whether this record is the team's lead.
    ///
    /// The team's *role*, which is not the same question as which of §2.2's two
    /// key orders this record is written in — see the type's own note, and the
    /// private `carries_teammate_fields`, which is what decides that.
    #[must_use]
    pub fn is_lead(&self) -> bool {
        self.name == LEAD
    }

    /// Whether any of the five teammate-only fields is present, which is what
    /// picks the key order [`Serialize`] writes.
    ///
    /// Every modern record answers this the way its name would — a lead has
    /// none of the five, a teammate has all five — so on the documents byte
    /// identity is asserted against, shape and role agree. They part on a
    /// 2026-03-era lead, which carries `model`: this says `true` and writes the
    /// value out in the teammate order rather than dropping it.
    fn carries_teammate_fields(&self) -> bool {
        self.model.is_some()
            || self.color.is_some()
            || self.prompt.is_some()
            || self.plan_mode_required.is_some()
            || self.is_active.is_some()
    }
}

/// A team's `config.json` (§2.2).
///
/// Its key order **is** its declaration order — all 29 documents in the survey
/// agree, and the 2026-03 era differs only by an unknown `description` the
/// passthrough carries. So unlike [`MemberRecord`] this needs no branch; it has
/// a hand-written [`Serialize`] anyway, for the one thing a derive cannot do:
/// refuse a passthrough key that shadows a declared one. A `Debug` is derived,
/// because the only field that could carry somebody's words is `members`, and a
/// [`MemberRecord`]'s own `Debug` already redacts the prompt.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamFile {
    /// The team's name, which is also its directory.
    pub name: String,
    /// Unix milliseconds at creation.
    pub created_at: u64,
    /// The lead's `<name>@<team>` identity.
    pub lead_agent_id: String,
    /// The lead's session id — a bare UUID, which is what makes the team name
    /// derivable from it (§2.1's `session-<first 8 hex>`).
    pub lead_session_id: String,
    /// Everyone in the team, the lead included.
    pub members: Vec<MemberRecord>,
    /// Every key this build has never heard of, after the known fields and in
    /// the order they arrived.
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

/// Declaration order, plus the shadow refusal a derive has no way to express.
///
/// The destructure is the same exhaustiveness net [`MemberRecord`]'s
/// [`Serialize`] takes, and for the same reason: this file's whole job is
/// round-tripping somebody else's document, so a field added and forgotten here
/// would delete data rather than fail.
impl Serialize for TeamFile {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let Self { name, created_at, lead_agent_id, lead_session_id, members, extra } = self;
        refuse_shadowed(extra, &TEAM_FILE_KEYS)?;

        let mut map = serializer.serialize_map(None)?;

        map.serialize_entry("name", name)?;
        map.serialize_entry("createdAt", created_at)?;
        map.serialize_entry("leadAgentId", lead_agent_id)?;
        map.serialize_entry("leadSessionId", lead_session_id)?;
        map.serialize_entry("members", members)?;

        for (key, value) in extra {
            map.serialize_entry(key, value)?;
        }

        map.end()
    }
}

impl TeamFile {
    /// A new team holding only its lead.
    #[must_use]
    pub fn new(
        team: &TeamName,
        lead_session_id: impl Into<String>,
        cwd: impl Into<String>,
        created_at: u64,
    ) -> Self {
        let lead = MemberRecord::lead(team, cwd, created_at);

        Self {
            name: team.as_str().to_owned(),
            created_at,
            lead_agent_id: lead.agent_id.clone(),
            lead_session_id: lead_session_id.into(),
            members: vec![lead],
            extra: IndexMap::new(),
        }
    }

    /// The member of this team named `name`, if it holds one.
    #[must_use]
    pub fn member(&self, name: &str) -> Option<&MemberRecord> {
        self.members.iter().find(|member| member.name == name)
    }
}

/// One message at rest in an inbox (§2.3).
///
/// Seven schema fields plus the two `writeToMailbox` stamps. Three of them are
/// worth a sentence each.
///
/// `read` stays in the schema and is **never written `true`**: §3.1 shows a
/// delivered message is *pruned*, not flagged, so the field is a tombstone
/// nothing ever sets. It is kept because a real `claude` writing into the same
/// inbox writes it, and a document that dropped it would not round-trip.
///
/// `text` is either prose or a JSON-encoded protocol frame — one field, two
/// meanings, which is why [`MailboxMessage::frame`] exists rather than callers
/// each trying `from_str` at their own seam.
///
/// **The field order below is a real envelope's, not §2.3's listing order.**
/// §2.3 prints the schema `type, from, text, timestamp, read?, color?,
/// summary?` and then names `msgV`/`msg_id` as stamps a write adds — which
/// this module first read as a key order and shipped as one. The modern
/// document that settled it says otherwise:
///
/// ```text
/// from text summary timestamp msgV msg_id type read
/// ```
///
/// `type` is at the *tail*, next to `read`, not at the head. One declaration
/// order reproduces that and every legacy shape in the survey byte for byte —
/// `from text [summary] timestamp [color] [msgV msg_id] [type] [read]` — so
/// unlike [`MemberRecord`] this needs no hand-written [`Serialize`]: a derive
/// emits declaration order, and skipping an absent option closes the gap.
///
/// *`color`'s slot is this plan's assumption, and the thinnest thing here.* No
/// modern document carries one — the only modern message in the survey has no
/// `color` — so its position among the stamps is unwitnessed. It is declared
/// immediately after `timestamp` because that is the only place it has **ever**
/// been seen: all 76 of the survey's 2026-03-era messages that carry a `color`
/// put it exactly there, directly before `read`. Should a modern message ever
/// carry one and disagree, this is one line to move; **AC-13**'s live capture
/// against a real `claude` is what would say so, and nothing in CI can.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxMessage {
    /// Who sent it — a member name, or [`crate::team::LEAD`].
    pub from: String,
    /// The body: prose, or a JSON-encoded [`ganja_protocol::team::Frame`].
    pub text: String,
    /// A one-line summary, where the sender wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// ISO-8601, and half of the identity a delivery is reconciled by.
    pub timestamp: String,
    /// The sender's color, where the sender had one. See the type's own note
    /// on why this slot is an assumption rather than a finding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The schema version [`MESSAGE_VERSION`], stamped at write.
    #[serde(rename = "msgV", default, skip_serializing_if = "Option::is_none")]
    pub msg_v: Option<u32>,
    /// The identity stamped at write — and **not** what a delivery is
    /// reconciled by; that is [`crate::mailbox::identity`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    /// Absent in the legacy shapes and left absent on read, so they round-trip;
    /// forced to [`MESSAGE_TYPE`] on write.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The tombstone that is never written `true`. See the type's own note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<bool>,
    /// Every key this build has never heard of, after the known fields and in
    /// the order they arrived.
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

/// Renders everything except what the sender wrote, which is rendered as a size.
///
/// `text` is a message body and `summary` is the sender's own prose about it —
/// both are user content, and a derived `Debug` would carry them into any `{:?}`
/// a caller reaches for. This is the same rule
/// [`Identity`](crate::mailbox::Identity) states and the same spelling, so one
/// grep finds every place it is applied; `tests/no_bodies_in_logs.rs` is what
/// keeps it true from the outside.
impl fmt::Debug for MailboxMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MailboxMessage")
            .field("from", &self.from)
            .field("text", &Redacted(Some(&self.text)))
            .field("summary", &Redacted(self.summary.as_deref()))
            .field("timestamp", &self.timestamp)
            .field("color", &self.color)
            .field("msg_v", &self.msg_v)
            .field("msg_id", &self.msg_id)
            .field("kind", &self.kind)
            .field("read", &self.read)
            .field("extra", &self.extra)
            .finish()
    }
}

impl MailboxMessage {
    /// A message ready to be written.
    ///
    /// The timestamp is handed in rather than read off the clock here, for the
    /// reason [`crate::TeamsRoot`] is handed in: a value a test can pin is a
    /// value a test can assert on, and half of a message's identity is this
    /// string. [`now_iso8601`] is the clock, for callers that want it.
    #[must_use]
    pub fn new(
        from: impl Into<String>,
        text: impl Into<String>,
        timestamp: impl Into<String>,
    ) -> Self {
        Self {
            from: from.into(),
            text: text.into(),
            summary: None,
            timestamp: timestamp.into(),
            color: None,
            msg_v: None,
            msg_id: None,
            kind: None,
            read: None,
            extra: IndexMap::new(),
        }
    }

    /// The same, carrying a protocol frame as its body.
    ///
    /// # Errors
    ///
    /// Whatever encoding the frame returned, which for the protocol's own
    /// shapes is nothing a caller can provoke.
    pub fn from_frame(
        from: impl Into<String>,
        frame: &ganja_protocol::team::Frame,
        timestamp: impl Into<String>,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self::new(from, serde_json::to_string(frame)?, timestamp))
    }

    /// The frame this message carries, if its body is one.
    ///
    /// A body that is prose — or a frame this build is too old to name — reads
    /// as [`None`], which is information the reader acts on rather than an
    /// error: an inbox shared with a newer peer will hold frames this one does
    /// not know, and refusing the whole message would lose the ones it does.
    #[must_use]
    pub fn frame(&self) -> Option<ganja_protocol::team::Frame> {
        serde_json::from_str(&self.text).ok()
    }
}

/// A document in the bytes Claude writes: two-space indent, one key per line,
/// **no trailing newline**.
///
/// `serde_json::to_string_pretty` is `JSON.stringify(value, null, 2)`, and
/// neither appends a newline. Adding one is the easy accident; this function
/// exists so there is one place that does not.
///
/// # Errors
///
/// Whatever the value's own `Serialize` returned.
pub fn document<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: ?Sized + Serialize,
{
    serde_json::to_string_pretty(value)
}

/// Unix milliseconds now — `joinedAt` and `createdAt`'s spelling of the clock.
///
/// A clock before the epoch reads as `0` rather than panicking: a wrong
/// timestamp is a cosmetic problem, and a member that could not be registered
/// because the machine's clock was wrong is not.
#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// The clock in a message timestamp's spelling.
#[must_use]
pub fn now_iso8601() -> String {
    iso8601(now_millis())
}

/// Unix milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
///
/// That spelling is `Date.prototype.toISOString`'s, which is what writes every
/// `timestamp` a real `claude` puts in a shared inbox. It matters beyond
/// looking right: the timestamp is one third of the identity key deliveries are
/// reconciled by (§2.3), so two builds spelling one instant differently would
/// deliver the same message twice.
///
/// A shared formatter would have to live in a crate, not a module.
/// `ganja-protocol` cannot be that crate: CI pins its external dependencies to
/// serde, serde_json and uuid, while this crate's internal allowlist is exactly
/// `ganja-protocol`. A thin jiff call site in each consumer is therefore the
/// only shape this dependency graph admits.
#[must_use]
pub(crate) fn iso8601(millis: u64) -> String {
    let millis = i64::try_from(millis).unwrap_or(i64::MAX);
    let timestamp = Timestamp::from_millisecond(millis).unwrap_or(Timestamp::MAX);

    format!("{timestamp:.3}")
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
