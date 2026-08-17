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

use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;

use crate::team::{LEAD, MemberName, TeamName};

/// `backendType` for a member that runs inside a process rather than a pane.
pub const BACKEND_IN_PROCESS: &str = "in-process";

/// `backendType` for a member that owns a tmux pane.
pub const BACKEND_TMUX: &str = "tmux";

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
}

impl Surface {
    /// Reads §2.2's overloaded field back into a surface.
    ///
    /// Anything that is neither sentinel is a pane id, which is how a pane is
    /// recognized in the first place — there is no separate marker, and a
    /// `backendType` disagreeing with this is somebody else's inconsistency to
    /// notice, not a reason to refuse the document.
    #[must_use]
    pub fn read(tmux_pane_id: &str) -> Self {
        match tmux_pane_id {
            PANE_LEADER => Self::Leader,
            PANE_IN_PROCESS => Self::InProcess,
            id => Self::Pane { id: id.to_owned() },
        }
    }

    /// What this surface writes into `tmuxPaneId`.
    #[must_use]
    pub fn tmux_pane_id(&self) -> &str {
        match self {
            Self::Leader => PANE_LEADER,
            Self::InProcess => PANE_IN_PROCESS,
            Self::Pane { id } => id,
        }
    }

    /// What this surface writes into `backendType`.
    ///
    /// A lead's record says `in-process` in §2.2's own excerpt, even though the
    /// lead is not a teammate at all — the two fields answer different
    /// questions and only `tmuxPaneId` distinguishes the three cases.
    #[must_use]
    pub fn backend_type(&self) -> &str {
        match self {
            Self::Leader | Self::InProcess => BACKEND_IN_PROCESS,
            Self::Pane { .. } => BACKEND_TMUX,
        }
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
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

/// The two orders of §2.2, chosen by the record's own shape.
///
/// A `serialize_map` rather than a `serialize_struct` because the key set is
/// decided at runtime — which is also what the derive does under a
/// `#[serde(flatten)]`, so this changes the machinery not at all, only which
/// keys go through it and when.
impl Serialize for MemberRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;

        // The two orders share only their first two keys; from `agentType`
        // onwards nothing lines up, which is why this is a branch and not a
        // pair of conditional inserts.
        map.serialize_entry("agentId", &self.agent_id)?;
        map.serialize_entry("name", &self.name)?;

        if self.carries_teammate_fields() {
            if let Some(color) = &self.color {
                map.serialize_entry("color", color)?;
            }
            map.serialize_entry("joinedAt", &self.joined_at)?;
            map.serialize_entry("tmuxPaneId", &self.tmux_pane_id)?;
            map.serialize_entry("subscriptions", &self.subscriptions)?;
            map.serialize_entry("agentType", &self.agent_type)?;
            if let Some(model) = &self.model {
                map.serialize_entry("model", model)?;
            }
            if let Some(prompt) = &self.prompt {
                map.serialize_entry("prompt", prompt)?;
            }
            if let Some(plan_mode_required) = &self.plan_mode_required {
                map.serialize_entry("planModeRequired", plan_mode_required)?;
            }
            map.serialize_entry("cwd", &self.cwd)?;
            if let Some(backend_type) = &self.backend_type {
                map.serialize_entry("backendType", backend_type)?;
            }
            if let Some(is_active) = &self.is_active {
                map.serialize_entry("isActive", is_active)?;
            }
        } else {
            map.serialize_entry("agentType", &self.agent_type)?;
            map.serialize_entry("joinedAt", &self.joined_at)?;
            map.serialize_entry("tmuxPaneId", &self.tmux_pane_id)?;
            map.serialize_entry("cwd", &self.cwd)?;
            map.serialize_entry("subscriptions", &self.subscriptions)?;
            if let Some(backend_type) = &self.backend_type {
                map.serialize_entry("backendType", backend_type)?;
            }
        }

        for (key, value) in &self.extra {
            map.serialize_entry(key, value)?;
        }

        map.end()
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// Normalized to [`MESSAGE_TYPE`] on read when absent, and forced to it on
    /// write.
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
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
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
fn iso8601(millis: u64) -> String {
    let seconds = i64::try_from(millis / 1_000).unwrap_or(i64::MAX);
    let subsecond = millis % 1_000;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let time = seconds.rem_euclid(86_400);

    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{subsecond:03}Z",
        hour = time / 3_600,
        minute = (time % 3_600) / 60,
        second = time % 60,
    )
}

/// Days since the epoch to a proleptic Gregorian date.
///
/// Hinnant's `civil_from_days`, the same arithmetic three other crates in this
/// workspace each keep a copy of — the copies exist because the layering
/// forbids a shared one, not because anybody thinks four is a good number.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01, so a leap day lands at the end of a year
    // and the month arithmetic below needs no special case for February.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * shifted_month + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    })
    .unwrap_or(1);

    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use ganja_protocol::team::{Frame, IdleNotification, IdleReason};

    use super::{
        MailboxMessage, MemberRecord, Spawn, Surface, TeamFile, document, iso8601, now_millis,
    };
    use crate::team::{MemberName, TeamName};

    fn team() -> TeamName {
        TeamName::parse("session-224cbeab").expect("a valid team name")
    }

    /// Every one of the survey's 24 modern lead records, key for key.
    const LEAD_ORDER: &str = "{\n  \"agentId\": \"team-lead@session-224cbeab\",\n  \
         \"name\": \"team-lead\",\n  \"agentType\": \"team-lead\",\n  \
         \"joinedAt\": 1786734033621,\n  \"tmuxPaneId\": \"leader\",\n  \"cwd\": \"/w\",\n  \
         \"subscriptions\": [],\n  \"backendType\": \"in-process\"\n}";

    /// Every one of the survey's 26 modern teammate records, key for key. Note
    /// `color` fourth and `agentType` seventh — the two places the lead order
    /// cannot be bent to agree.
    const TEAMMATE_ORDER: &str = "{\n  \"agentId\": \"demo-worker-1@session-224cbeab\",\n  \
         \"name\": \"demo-worker-1\",\n  \"color\": \"blue\",\n  \
         \"joinedAt\": 1786734154864,\n  \"tmuxPaneId\": \"%142\",\n  \
         \"subscriptions\": [],\n  \"agentType\": \"general-purpose\",\n  \
         \"model\": \"claude-opus-5[1m]\",\n  \"prompt\": \"do the thing\",\n  \
         \"planModeRequired\": false,\n  \"cwd\": \"/w\",\n  \"backendType\": \"tmux\",\n  \
         \"isActive\": true\n}";

    fn worker() -> MemberRecord {
        let name = MemberName::parse("demo-worker-1").expect("a valid member name");

        MemberRecord::teammate(
            &name,
            &team(),
            Spawn {
                agent_type: "general-purpose".to_owned(),
                model: "claude-opus-5[1m]".to_owned(),
                color: "blue".to_owned(),
                prompt: "do the thing".to_owned(),
                plan_mode_required: false,
                surface: Surface::Pane {
                    id: "%142".to_owned(),
                },
                cwd: "/w".to_owned(),
            },
            1_786_734_154_864,
        )
    }

    #[test]
    fn a_lead_record_is_written_in_the_lead_order_and_never_says_model_null() {
        let record = MemberRecord::lead(&team(), "/w", 1_786_734_033_621);
        let rendered = document(&record).expect("a record encodes");

        assert!(
            !rendered.contains("null"),
            "the five teammate-only fields are absent from a lead record, not null: {rendered}"
        );
        // The emitted order is the format, so this is asserted as bytes rather
        // than as a set of fields.
        assert_eq!(rendered, LEAD_ORDER);
        assert!(record.is_lead());
        assert_eq!(record.surface(), Surface::Leader);
    }

    #[test]
    fn a_teammate_record_is_written_in_the_other_order_entirely() {
        let record = worker();

        assert_eq!(document(&record).expect("a record encodes"), TEAMMATE_ORDER);
        assert_eq!(
            record.surface(),
            Surface::Pane {
                id: "%142".to_owned()
            }
        );
    }

    #[test]
    fn each_record_shape_round_trips_the_bytes_it_was_read_from() {
        // Decoding is order-insensitive, so this asserts the pair the format
        // contract actually rests on: whatever a real `claude` wrote comes back
        // out unchanged. A single declaration cannot do it — `agentType` is
        // third here and seventh there.
        for original in [LEAD_ORDER, TEAMMATE_ORDER] {
            let record: MemberRecord = serde_json::from_str(original).expect("a record decodes");

            assert_eq!(document(&record).expect("a record encodes"), original);
        }
    }

    #[test]
    fn a_legacy_lead_keeps_its_model_rather_than_losing_it_to_the_lead_order() {
        // A 2026-03-era lead: named `team-lead`, carrying `model`, and with no
        // `backendType` at all. Keying the order on the name would write the
        // eight-key lead order and drop the model on the floor; keying it on
        // the shape moves the key instead — the same trade the flatten
        // passthrough makes for an unknown one.
        let original = "{\n  \"agentId\": \"team-lead@web-pages\",\n  \
             \"name\": \"team-lead\",\n  \"agentType\": \"team-lead\",\n  \
             \"model\": \"claude-opus-4-1\",\n  \"joinedAt\": 1782579031759,\n  \
             \"tmuxPaneId\": \"leader\",\n  \"cwd\": \"/w\",\n  \"subscriptions\": []\n}";
        let record: MemberRecord = serde_json::from_str(original).expect("a record decodes");

        assert!(record.is_lead(), "the role is still the lead's");
        assert_eq!(
            document(&record).expect("a record encodes"),
            "{\n  \"agentId\": \"team-lead@web-pages\",\n  \"name\": \"team-lead\",\n  \
             \"joinedAt\": 1782579031759,\n  \"tmuxPaneId\": \"leader\",\n  \
             \"subscriptions\": [],\n  \"agentType\": \"team-lead\",\n  \
             \"model\": \"claude-opus-4-1\",\n  \"cwd\": \"/w\"\n}"
        );
        // The absent `backendType` stays absent rather than becoming a guess,
        // which is the whole reason that field is an `Option`.
        assert_eq!(record.backend_type, None);
    }

    #[test]
    fn an_unknown_key_survives_a_rewrite_in_position() {
        // `zeta` before `alpha` on purpose: a `BTreeMap` passthrough would
        // hand them back the other way round, and that is the failure this
        // test exists to catch.
        let original = "{\n  \"agentId\": \"w@t\",\n  \"name\": \"w\",\n  \"color\": \"blue\",\n  \
             \"joinedAt\": 1,\n  \"tmuxPaneId\": \"in-process\",\n  \"subscriptions\": [],\n  \
             \"agentType\": \"general-purpose\",\n  \"model\": \"m\",\n  \"prompt\": \"p\",\n  \
             \"planModeRequired\": false,\n  \"cwd\": \"/w\",\n  \
             \"backendType\": \"in-process\",\n  \"isActive\": true,\n  \"zeta\": \"kept\",\n  \
             \"alpha\": {\n    \"nested\": true\n  }\n}";
        let record: MemberRecord = serde_json::from_str(original).expect("a record decodes");

        assert_eq!(record.extra.keys().collect::<Vec<_>>(), ["zeta", "alpha"]);
        assert_eq!(document(&record).expect("a record encodes"), original);

        // And on the other key order, where the unknown keys follow a shorter
        // known set — the branch must reach the passthrough too.
        let original = "{\n  \"agentId\": \"team-lead@t\",\n  \"name\": \"team-lead\",\n  \
             \"agentType\": \"team-lead\",\n  \"joinedAt\": 1,\n  \"tmuxPaneId\": \"leader\",\n  \
             \"cwd\": \"/w\",\n  \"subscriptions\": [],\n  \"backendType\": \"in-process\",\n  \
             \"zeta\": \"kept\",\n  \"alpha\": {\n    \"nested\": true\n  }\n}";
        let record: MemberRecord = serde_json::from_str(original).expect("a record decodes");

        assert_eq!(record.extra.keys().collect::<Vec<_>>(), ["zeta", "alpha"]);
        assert_eq!(document(&record).expect("a record encodes"), original);

        // The same for a team file, whose unknown keys sit beside `members`.
        let original = "{\n  \"name\": \"t\",\n  \"createdAt\": 1,\n  \
             \"leadAgentId\": \"team-lead@t\",\n  \
             \"leadSessionId\": \"224cbeab-4e62-497c-aa8f-d05cc33ce7ba\",\n  \
             \"members\": [],\n  \"zeta\": 1,\n  \"alpha\": 2\n}";
        let file: TeamFile = serde_json::from_str(original).expect("a team file decodes");
        assert_eq!(document(&file).expect("a team file encodes"), original);

        // And for a message, where the unknown key follows the whole envelope.
        let original = "{\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \"summary\": \"s\",\n  \
             \"timestamp\": \"2026-08-17T00:00:00.000Z\",\n  \"msgV\": 1,\n  \
             \"msg_id\": \"x\",\n  \"type\": \"message\",\n  \"read\": false,\n  \
             \"zeta\": \"kept\",\n  \"alpha\": \"kept\"\n}";
        let message: MailboxMessage = serde_json::from_str(original).expect("a message decodes");
        assert_eq!(document(&message).expect("a message encodes"), original);
    }

    #[test]
    fn every_message_shape_the_survey_holds_round_trips_unchanged() {
        // The modern envelope first — one real document, `session-44cd25e1`'s
        // `inboxes/worker-mask.json`, with the body cut to a line. `type` next
        // to `read` at the tail is the finding; §2.3's listing order is not it.
        //
        // The rest are the 2026-03 era, which is where `color` and every
        // stamp-free shape come from. One declaration order serves all five,
        // which is the reason this type needs no hand-written `Serialize` the
        // way `MemberRecord` does.
        for original in [
            "{\n  \"from\": \"team-lead\",\n  \"text\": \"GO\",\n  \"summary\": \"unblock\",\n  \
             \"timestamp\": \"2026-08-17T00:00:00.000Z\",\n  \"msgV\": 1,\n  \
             \"msg_id\": \"0198c0de-dead-7000-8000-000000000000\",\n  \"type\": \"message\",\n  \
             \"read\": false\n}",
            "{\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \"summary\": \"s\",\n  \
             \"timestamp\": \"2026-03-01T00:00:00.000Z\",\n  \"color\": \"blue\",\n  \
             \"read\": false\n}",
            "{\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \
             \"timestamp\": \"2026-03-01T00:00:00.000Z\",\n  \"color\": \"blue\",\n  \
             \"read\": false\n}",
            "{\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \
             \"timestamp\": \"2026-03-01T00:00:00.000Z\",\n  \"type\": \"message\",\n  \
             \"read\": false\n}",
            "{\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \
             \"timestamp\": \"2026-03-01T00:00:00.000Z\",\n  \"read\": false\n}",
        ] {
            let message: MailboxMessage =
                serde_json::from_str(original).expect("a message decodes");

            assert_eq!(document(&message).expect("a message encodes"), original);
        }
    }

    #[test]
    fn a_document_carries_no_trailing_newline() {
        let rendered =
            document(&TeamFile::new(&team(), "s", "/w", 1)).expect("a team file encodes");

        assert!(!rendered.ends_with('\n'), "{rendered:?}");
        assert!(rendered.contains("\n  \"name\""), "two-space indent");
    }

    #[test]
    fn a_frame_body_reads_back_as_a_frame_and_prose_does_not() {
        let frame = Frame::IdleNotification(IdleNotification {
            from: "w".to_owned(),
            timestamp: "2026-08-17T00:00:00.000Z".to_owned(),
            idle_reason: Some(IdleReason::Available),
            summary: None,
            completed_task_id: None,
            completed_status: None,
            failure_reason: None,
        });
        let carried = MailboxMessage::from_frame("w", &frame, "2026-08-17T00:00:00.000Z")
            .expect("a frame encodes");

        assert_eq!(carried.frame(), Some(frame));
        assert_eq!(
            MailboxMessage::new("w", "just words", "2026-08-17T00:00:00.000Z").frame(),
            None
        );
    }

    #[test]
    fn the_clock_is_spelled_the_way_javascript_spells_it() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        // 2026-08-17T12:34:56.789Z, checked against the calendar rather than
        // against this function.
        assert_eq!(iso8601(1_786_970_096_789), "2026-08-17T12:34:56.789Z");
        // A leap day, which is what the shifted-epoch arithmetic above is for.
        assert_eq!(iso8601(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
        assert!(now_millis() > 1_700_000_000_000, "the clock reads forward");
    }
}
