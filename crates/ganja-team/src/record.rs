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
//! the workspace through feature unification. One limitation is recorded
//! rather than hidden: a nested object *inside* an unknown key's value is a
//! `serde_json::Value::Object` and would still reorder on rewrite. Whether
//! that ever matters is a question a captured document answers, not this
//! comment.
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
//! and the fields are declared in the order §2.2 and §2.3 print them. A
//! rewrite that reordered or reindented would still parse; it would also make
//! every `git diff` of a shared directory unreadable, and it would be the
//! first thing to suspect when a byte-identity test failed for an unrelated
//! reason.

use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
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
/// **One struct, not two, and the five teammate-only fields skip when absent.**
/// §2.2's lead record carries five fields fewer than a teammate's — no `model`,
/// `color`, `prompt`, `planModeRequired` or `isActive` — so a single shape that
/// emitted `"model": null` for a lead would fail a byte-identity comparison
/// against the very first real file. Two shapes behind an untagged enum would
/// also work; they were declined because untagged deserialization decides which
/// arm to take by trying them, so a teammate record missing one field would
/// silently decode as a lead rather than say what was wrong — and because the
/// field *order* the format needs is exactly this struct's declaration order,
/// which two arms would have to keep in step by hand.
///
/// The name is a `String` rather than a [`MemberName`] on purpose: the type
/// marks the door a *created* name goes through, and refusing to decode a
/// document a real `claude` wrote is not this crate's call to make.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberRecord {
    /// `<name>@<team>` — derived, never minted (§2.2).
    pub agent_id: String,
    /// The bare name, which is also the mailbox address.
    pub name: String,
    /// The `task` tool's `subagent_type`; `team-lead` for the lead.
    pub agent_type: String,
    /// Teammate-only: the model it runs as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Teammate-only: its assigned color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Teammate-only: the spawn prompt, in cleartext. See [`Spawn::prompt`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Teammate-only: whether it must start in plan mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode_required: Option<bool>,
    /// Unix milliseconds at registration.
    pub joined_at: u64,
    /// §2.2's overloaded surface discriminator; read it through
    /// [`MemberRecord::surface`].
    pub tmux_pane_id: String,
    /// The working directory the member runs in.
    pub cwd: String,
    /// Vestigial (§9.1): written `[]` at every creation site and read nowhere,
    /// in Claude Code and here alike.
    ///
    /// **The reference's advice to omit it is declined**, and the reason is
    /// byte identity rather than doubt about the finding. Left out of the
    /// declaration, a real document's `subscriptions` would be captured by
    /// `extra` and re-emitted *after* `backendType` instead of before it — so
    /// the one thing omitting it would cost is the round-trip the whole format
    /// contract rests on. Declared, it is written `[]` and never populated,
    /// which is what "vestigial" buys in practice; a rewrite carries whatever
    /// it read, so a file that somehow holds something keeps it.
    #[serde(default)]
    pub subscriptions: Vec<Value>,
    /// `in-process` or `tmux`, as Claude spells them.
    pub backend_type: String,
    /// Teammate-only: whether the member is still live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,
    /// Every key this build has never heard of, after the known fields and in
    /// the order they arrived.
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
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
            agent_type: LEAD.to_owned(),
            model: None,
            color: None,
            prompt: None,
            plan_mode_required: None,
            joined_at,
            tmux_pane_id: Surface::Leader.tmux_pane_id().to_owned(),
            cwd: cwd.into(),
            subscriptions: Vec::new(),
            backend_type: Surface::Leader.backend_type().to_owned(),
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
            agent_type: spawn.agent_type,
            model: Some(spawn.model),
            color: Some(spawn.color),
            prompt: Some(spawn.prompt),
            plan_mode_required: Some(spawn.plan_mode_required),
            joined_at,
            tmux_pane_id: spawn.surface.tmux_pane_id().to_owned(),
            cwd: spawn.cwd,
            subscriptions: Vec::new(),
            backend_type: spawn.surface.backend_type().to_owned(),
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
    #[must_use]
    pub fn is_lead(&self) -> bool {
        self.name == LEAD
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
/// The field *order* below is §2.3's listing order followed by the two
/// envelope stamps, which is the best evidence available until a captured
/// document says otherwise; it is one declaration to reorder if it does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MailboxMessage {
    /// Normalized to [`MESSAGE_TYPE`] on read when absent, and forced to it on
    /// write.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Who sent it — a member name, or [`crate::team::LEAD`].
    pub from: String,
    /// The body: prose, or a JSON-encoded [`ganja_protocol::team::Frame`].
    pub text: String,
    /// ISO-8601, and half of the identity a delivery is reconciled by.
    pub timestamp: String,
    /// The tombstone that is never written `true`. See the type's own note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<bool>,
    /// The sender's color, where the sender had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// A one-line summary, where the sender wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The schema version [`MESSAGE_VERSION`], stamped at write.
    #[serde(rename = "msgV", default, skip_serializing_if = "Option::is_none")]
    pub msg_v: Option<u32>,
    /// The identity stamped at write — and **not** what a delivery is
    /// reconciled by; that is [`crate::mailbox::identity`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
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
            kind: None,
            from: from.into(),
            text: text.into(),
            timestamp: timestamp.into(),
            read: None,
            color: None,
            summary: None,
            msg_v: None,
            msg_id: None,
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

    #[test]
    fn a_lead_record_never_says_model_null() {
        let record = MemberRecord::lead(&team(), "/w", 1_786_734_033_621);
        let rendered = document(&record).expect("a record encodes");

        assert!(
            !rendered.contains("null"),
            "the five teammate-only fields are absent from a lead record, not null: {rendered}"
        );
        // Declaration order is the format, so this is asserted as bytes rather
        // than as a set of fields.
        assert_eq!(
            rendered,
            "{\n  \"agentId\": \"team-lead@session-224cbeab\",\n  \"name\": \"team-lead\",\n  \
             \"agentType\": \"team-lead\",\n  \"joinedAt\": 1786734033621,\n  \
             \"tmuxPaneId\": \"leader\",\n  \"cwd\": \"/w\",\n  \"subscriptions\": [],\n  \
             \"backendType\": \"in-process\"\n}"
        );
        assert!(record.is_lead());
        assert_eq!(record.surface(), Surface::Leader);
    }

    #[test]
    fn a_teammate_record_carries_all_five_of_its_own_fields() {
        let name = MemberName::parse("demo-worker-1").expect("a valid member name");
        let record = MemberRecord::teammate(
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
        );

        assert_eq!(
            document(&record).expect("a record encodes"),
            "{\n  \"agentId\": \"demo-worker-1@session-224cbeab\",\n  \
             \"name\": \"demo-worker-1\",\n  \"agentType\": \"general-purpose\",\n  \
             \"model\": \"claude-opus-5[1m]\",\n  \"color\": \"blue\",\n  \
             \"prompt\": \"do the thing\",\n  \"planModeRequired\": false,\n  \
             \"joinedAt\": 1786734154864,\n  \"tmuxPaneId\": \"%142\",\n  \"cwd\": \"/w\",\n  \
             \"subscriptions\": [],\n  \"backendType\": \"tmux\",\n  \"isActive\": true\n}"
        );
        assert_eq!(
            record.surface(),
            Surface::Pane {
                id: "%142".to_owned()
            }
        );
    }

    #[test]
    fn an_unknown_key_survives_a_rewrite_in_position() {
        // `zeta` before `alpha` on purpose: a `BTreeMap` passthrough would
        // hand them back the other way round, and that is the failure this
        // test exists to catch.
        let original = "{\n  \"agentId\": \"w@t\",\n  \"name\": \"w\",\n  \
             \"agentType\": \"general-purpose\",\n  \"joinedAt\": 1,\n  \
             \"tmuxPaneId\": \"in-process\",\n  \"cwd\": \"/w\",\n  \"subscriptions\": [],\n  \
             \"backendType\": \"in-process\",\n  \"zeta\": \"kept\",\n  \
             \"alpha\": {\n    \"nested\": true\n  }\n}";
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

        // And for a message, where the unknown key follows the two stamps.
        let original = "{\n  \"type\": \"message\",\n  \"from\": \"w\",\n  \"text\": \"hi\",\n  \
             \"timestamp\": \"2026-08-17T00:00:00.000Z\",\n  \"read\": false,\n  \
             \"msgV\": 1,\n  \"msg_id\": \"x\",\n  \"zeta\": \"kept\",\n  \"alpha\": \"kept\"\n}";
        let message: MailboxMessage = serde_json::from_str(original).expect("a message decodes");
        assert_eq!(document(&message).expect("a message encodes"), original);
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
