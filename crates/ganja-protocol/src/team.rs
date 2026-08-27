//! The teammate control channel: the frames two agents exchange through a
//! mailbox, and the two types that make a trust boundary something the
//! compiler checks rather than something a call site remembers to check.
//!
//! **Upstream opencode has no counterpart.** It has no teams, no mailbox and
//! no second agent to address, so there is no TypeScript here to port
//! behavior from. The specification is Claude Code's, read out of the
//! reference document: §5 for the frame schemas, §5.1 for the two reserved
//! sets, §5.2 rung 7 for what a plain-text message may not be, §5.3 for the
//! two-hundred-character display cap, and §8.4 for the Rust shapes this
//! module follows.
//!
//! **Casing is kept per family and never normalized** (**D494**). The
//! reference advises the opposite — "a port should normalize the casing
//! rather than reproduce the split", at §5 itself — and that advice is
//! declined here, because these frames are read and written by a real
//! `claude` process sharing the same mailbox. A normalized `request_id` on a
//! frame Claude spells `requestId` is a frame Claude drops. So the ten §5
//! frames and the two `sandbox_*` frames keep camelCase, and
//! `permission_request`/`permission_response` keep the snake_case Claude's
//! constructor-built pair happens to use. The inconsistency is the wire's,
//! not this module's, and every golden test in `team_tests.rs` pins one
//! half of it.
//!
//! One type here carries **no serde derives at all**, which is a deliberate
//! exception to this crate's rule that every type round-trips. [`LeadFrame`]
//! is not a value that crosses a wire — the frame inside one crosses it as a
//! [`Frame`]. What it is is a constructor with a condition attached, and a
//! `Deserialize` impl is precisely a constructor that skips it.
//!
//! # Changing [`MemberBackend`] is a version-skew event, and a wide one
//!
//! [`MemberView`] is `deny_unknown_fields` over a **typed** `backend`, so a
//! name added here — or **renamed**, as `"pane"` became `"ganja"` — is a name
//! an older `ganja-client` refuses to decode. The
//! blast radius is the part worth stating plainly, because the field's own
//! position hides it: the refusal is not scoped to the member carrying the new
//! name. [`TeamView::members`] decodes as one value, so a single `codex`
//! teammate makes the **entire** `GET /team` response unreadable to that
//! client — every other member's row included, and the team's own name with
//! them. The rename is the worse of the two, since it needs no new teammate
//! at all: an existing `ganja`-backed member is enough.
//!
//! That is the declared posture rather than a defect: this crate refuses what
//! it does not understand readably instead of guessing, and a client that
//! silently dropped the row it could not read would show a team missing a
//! member that is running. What the posture does not do is make skew free, so
//! the two ends are versioned together.

use std::fmt;

use serde::{Deserialize, Serialize, de};

/// How many characters of a display-only field ever reach a rendered
/// envelope: §5.3's `hWp`/`mWp`, both two hundred.
pub const DISPLAY_FIELD_CAP: usize = 200;

/// The ten frame kinds an agent may legitimately originate, in §5.1's own
/// order.
///
/// Claude Code's `SendMessage` refuses *plain text* that parses to one of
/// these and tells the caller to send the structured object form instead —
/// so the set is not a denylist, it is the list of frames that have a
/// legitimate sender other than the harness.
pub const AGENT_SENDABLE: [&str; 10] = [
    "permission_request",
    "permission_response",
    "sandbox_permission_request",
    "sandbox_permission_response",
    "shutdown_request",
    "shutdown_approved",
    "team_permission_update",
    "mode_set_request",
    "plan_approval_request",
    "plan_approval_response",
];

/// The five frame kinds only the harness may originate, in §5.1's own order.
///
/// Disjoint from [`AGENT_SENDABLE`] and refused outright rather than routed
/// to a structured form: an agent that could mint a `task_completed` could
/// close a task it never did, and one that could mint an `idle_notification`
/// could report a peer available on that peer's behalf.
pub const HARNESS_ONLY: [&str; 5] = [
    "idle_notification",
    "teammate_terminated",
    "task_assignment",
    "task_completed",
    "shutdown_rejected",
];

/// Why a teammate went idle (§5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleReason {
    /// The turn ended on its own and the teammate is waiting for work.
    Available,
    /// Something cut the turn short.
    Interrupted,
    /// The turn ended badly; `failure_reason` says how.
    Failed,
}

/// How the task a teammate was carrying ended (§5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletedStatus {
    /// Done, in the sense the assignment asked for.
    Resolved,
    /// Not done, and not going to be without something else moving first.
    Blocked,
    /// Attempted and failed.
    Failed,
}

/// A teammate reporting that its turn ended (§5, harness-only).
///
/// `summary` is §5.4's last peer DM rather than a description of the work:
/// evidence that the tool call existed in the transcript, never evidence that
/// anything was delivered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IdleNotification {
    /// The member name that went idle.
    pub from: String,
    /// ISO-8601, as every frame timestamp here is.
    pub timestamp: String,
    /// Absent on the frames Claude mints without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_reason: Option<IdleReason>,
    /// Capped at [`DISPLAY_FIELD_CAP`] wherever it is rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The task this teammate was carrying, if it was carrying one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_task_id: Option<String>,
    /// How that task ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_status: Option<CompletedStatus>,
    /// Capped at [`DISPLAY_FIELD_CAP`] wherever it is rendered, by §5.3's
    /// second cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

/// A teammate asking the lead to approve a plan (§5, agent-sendable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanApprovalRequest {
    /// The member name asking.
    pub from: String,
    /// ISO-8601.
    pub timestamp: String,
    /// Where the plan was written.
    pub plan_file_path: String,
    /// The plan itself, so the lead needs no filesystem access to read it.
    pub plan_content: String,
    /// What the matching response has to quote back.
    pub request_id: String,
}

/// The lead's answer to a [`PlanApprovalRequest`] (§5, agent-sendable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanApprovalResponse {
    /// The request this answers. An answer nothing is waiting for is stale
    /// and dropped (§7-3).
    pub request_id: String,
    /// `approved`, and **not** `approve` — §5 calls the spelling out because
    /// it is the one a reader guesses wrong.
    pub approved: bool,
    /// What the lead wants changed, when it wants something changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
    /// ISO-8601.
    pub timestamp: String,
    /// Claude's own mode vocabulary, carried as text rather than as ganja's
    /// `PermissionMode`: the two are not the same set, and mapping one to the
    /// other is a decision with a refusal in it (D496), which belongs to
    /// whoever applies the frame rather than to the type that carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}

/// A teammate asking to be shut down (§5, agent-sendable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShutdownRequest {
    /// What the matching approval or rejection quotes back.
    pub request_id: String,
    /// The member name asking.
    pub from: String,
    /// Why, when there is a why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// ISO-8601.
    pub timestamp: String,
}

/// The lead approving a [`ShutdownRequest`] (§5, agent-sendable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShutdownApproved {
    /// The request this approves.
    pub request_id: String,
    /// Who approved it.
    pub from: String,
    /// ISO-8601.
    pub timestamp: String,
    /// The pane to tear down, where the teammate has one. Claude overloads
    /// this field with `"leader"` and `"in-process"` sentinels; ganja reads
    /// what it is given and models its own surface as a backend type instead
    /// (§8.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// Claude's backend vocabulary — observed as `"tmux"` and
    /// `"in-process"`, and therefore not ganja's [`MemberBackend`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_type: Option<String>,
}

/// The lead refusing a [`ShutdownRequest`] (§5, harness-only).
///
/// Harness-only where its approving twin is not: an agent that could mint
/// this could refuse a shutdown the lead granted, and keep itself alive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShutdownRejected {
    /// The request this refuses.
    pub request_id: String,
    /// Who refused it.
    pub from: String,
    /// Why. Required here, where [`ShutdownRequest::reason`] is optional.
    pub reason: String,
    /// ISO-8601.
    pub timestamp: String,
}

/// The harness handing a teammate a task (§5, harness-only).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskAssignment {
    /// What a later `task_completed` quotes back.
    pub task_id: String,
    /// The one-line subject.
    pub subject: String,
    /// The body of the assignment.
    pub description: String,
    /// Who assigned it.
    pub assigned_by: String,
    /// ISO-8601.
    pub timestamp: String,
}

/// The harness recording that a task is done (§5, harness-only).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskCompleted {
    /// Who completed it, when the frame says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The task. The one field of this frame that is never absent.
    pub task_id: String,
    /// The subject, repeated so a reader needs no lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_subject: Option<String>,
    /// ISO-8601, and optional here where the other frames require it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// The harness reporting that a teammate is gone (§5, harness-only).
///
/// One field, and §5 calls that out: no `from`, no `timestamp`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TeammateTerminated {
    /// What to tell the reader.
    pub message: String,
}

/// The lead setting a teammate's permission mode (§5, agent-sendable).
///
/// No timestamp, which §5 also calls out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModeSetRequest {
    /// Claude's mode vocabulary as text, for [`PlanApprovalResponse`]'s
    /// reason: the mapping into ganja's own two modes has a refusal in it
    /// (D496), and a strict enum here would drop the frame before anything
    /// could refuse it by name.
    pub mode: String,
    /// Who is asking. Load-bearing: a handler for this frame is reachable
    /// only through [`LeadFrame`], which checks this against the team's
    /// recorded lead (§7-2).
    pub from: String,
}

/// A teammate asking for a tool call to be permitted (§5, agent-sendable).
///
/// This family is where Claude's casing splits: it is built by a constructor
/// rather than by a schema, and the constructor writes snake_case. Kept
/// verbatim under D494.
///
/// # Stricter than the reference attests, deliberately
///
/// §5 attests `be` — schema strictness — for the **ten** frames above and
/// says of this family only that it is constructor-built, so
/// `deny_unknown_fields` on these four is ganja's choice rather than a
/// reading. It is kept because the failure it causes is the safe one: an
/// unknown key from a real `claude` peer makes the *ask* fail to decode,
/// and a permission ask that does not decode is one nobody is asked about
/// — where the tolerant reading would let a frame this build only half
/// understands drive a dialog. [`Frame::reserved_kind`] still recognizes it
/// by tag, so the message is refused as a frame rather than delivered as
/// prose either way. Revisit at AC-13's live run against a real `claude`
/// binary, which is the only thing that can show a key this does not
/// declare.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequest {
    /// What the matching response quotes back.
    pub request_id: String,
    /// The `<name>@<team>` identity of the asker.
    pub agent_id: String,
    /// The tool as its registry names it.
    pub tool_name: String,
    /// The call within the asking turn.
    pub tool_use_id: String,
    /// What to show the person deciding.
    pub description: String,
    /// The call's arguments, carried as a value rather than re-declared per
    /// tool — the same reason this crate carries a tool call's input as one.
    pub input: serde_json::Value,
    /// Rule suggestions the dialog may offer. Claude's constructor always
    /// writes the key, so it is required here rather than defaulted.
    pub permission_suggestions: Vec<serde_json::Value>,
}

/// Which arm of a [`PermissionResponse`] is the live one (§5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionResponseSubtype {
    /// [`PermissionResponse::response`] carries the answer.
    Success,
    /// [`PermissionResponse::error`] carries the reason.
    Error,
}

/// What a successful [`PermissionResponse`] answers with (§5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionResponseBody {
    /// The arguments to run with, which the decision may have edited.
    pub updated_input: serde_json::Value,
    /// Rules to store, carried as values for [`PermissionRequest::input`]'s
    /// reason.
    pub permission_updates: Vec<serde_json::Value>,
}

/// The answer to a [`PermissionRequest`] (§5, agent-sendable).
///
/// §5 spells this as a discriminated union on `subtype`, and serde can only
/// express that shape beside a shared `request_id` through `#[serde(flatten)]`
/// — which is mutually exclusive with the `deny_unknown_fields` every other
/// frame here carries. Strictness won: the union becomes a tag and two
/// optional arms.
///
/// The fields are **private** because a `pub` field is a constructor
/// ([`PeerPayload`]'s rule, for the same reason), and a struct literal writing
/// `subtype: Success` beside `error: Some(…)` would mint exactly the crossed
/// pair the union was supposed to make unrepresentable.
/// [`PermissionResponse::success`] and [`PermissionResponse::error`] are
/// therefore the only ways to build one, and they cannot cross the arms.
///
/// A **decoded** value still can, because the wire belongs to somebody else
/// and serde reaches the fields whatever their visibility;
/// [`PermissionResponse::is_consistent`] is what a reader asks about one of
/// those.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionResponse {
    request_id: String,
    subtype: PermissionResponseSubtype,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response: Option<PermissionResponseBody>,
}

impl PermissionResponse {
    /// The success arm: an answer, and no error.
    #[must_use]
    pub fn success(request_id: impl Into<String>, response: PermissionResponseBody) -> Self {
        Self {
            request_id: request_id.into(),
            subtype: PermissionResponseSubtype::Success,
            error: None,
            response: Some(response),
        }
    }

    /// The error arm: a reason, and no answer.
    #[must_use]
    pub fn error(request_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            subtype: PermissionResponseSubtype::Error,
            error: Some(error.into()),
            response: None,
        }
    }

    /// The request this answers.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Which arm the frame claims is the live one.
    #[must_use]
    pub fn subtype(&self) -> PermissionResponseSubtype {
        self.subtype
    }

    /// The reason, on the error arm.
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The answer, on the success arm.
    #[must_use]
    pub fn response(&self) -> Option<&PermissionResponseBody> {
        self.response.as_ref()
    }

    /// Whether the arms match the tag — true for everything the two
    /// constructors mint, and worth asking of anything decoded.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        match self.subtype {
            PermissionResponseSubtype::Success => self.response.is_some() && self.error.is_none(),
            PermissionResponseSubtype::Error => self.error.is_some() && self.response.is_none(),
        }
    }
}

/// The host a sandbox permission is being asked about (§5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostPattern {
    /// The host itself.
    pub host: String,
}

/// A sandboxed worker asking to reach a host (§5, agent-sendable).
///
/// Note the casing: this pair sits in §5's permission-family paragraph but is
/// spelled camelCase, so "the permission family is snake_case" is a rule with
/// exactly two exceptions and both are here.
///
/// ganja has no sandbox, so nothing in this tree originates one of these. The
/// variant exists so a frame arriving from a `claude` peer is *recognized* —
/// [`Frame::reserved_kind`] answers from the tag alone, so even a body this
/// build cannot decode is refused as a frame rather than carried as prose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxPermissionRequest {
    /// What the matching response quotes back.
    pub request_id: String,
    /// The `<name>@<team>` identity of the worker.
    pub worker_id: String,
    /// Its display name.
    pub worker_name: String,
    /// Its assigned color.
    pub worker_color: String,
    /// The host it wants.
    pub host_pattern: HostPattern,
    /// Spelled as the frame family's other timestamps are. The reference
    /// leaves this field's type unstated — its only typed sighting of a
    /// `createdAt` is the team file's, which is epoch millis — so this is a
    /// choice rather than a reading, and the classifier above is what keeps a
    /// wrong choice from turning a frame into prose.
    pub created_at: String,
}

/// The answer to a [`SandboxPermissionRequest`] (§5, agent-sendable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxPermissionResponse {
    /// The request this answers.
    pub request_id: String,
    /// The host it was about.
    pub host: String,
    /// Whether it may be reached.
    pub allow: bool,
    /// ISO-8601.
    pub timestamp: String,
}

/// Team-wide permission rules arriving over the mailbox (§5.1, §7-1).
///
/// **The reference publishes no schema for this frame** — only its name, in
/// the reserved set and in the security controls. So there is nothing to
/// model faithfully, and this is a passthrough: whatever the object carried
/// is kept, unread.
///
/// That is enough, because ganja does exactly one thing with this frame:
/// **drops it by name.** §7-1 is the highest-ranked control in the reference
/// for a reason — the mailbox is not an escalation channel, and there is no
/// code path from a message to a permission-rule write. The variant exists so
/// the drop can name what it dropped, and so a body nobody parses cannot
/// become the reason a frame is mistaken for prose.
///
/// One asymmetry follows from the passthrough and is worth naming: decoding
/// strips the `type` tag before this struct sees it, so a `payload` that
/// *contains* its own `"type"` key can only have been put there by hand, and
/// re-encoding such a value writes the tag twice. Nothing in this tree does
/// that — the frame is never minted here, only decoded and dropped — and no
/// round trip of a decoded value can reach it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamPermissionUpdate {
    /// Whatever the sender put beside the tag, kept verbatim and unread.
    #[serde(flatten)]
    pub payload: serde_json::Map<String, serde_json::Value>,
}

/// A message whose text is JSON with a recognized `type` (§5).
///
/// The enum is the anti-spoofing check (§8.4): a validator asks
/// [`Frame::is_agent_sendable`] once instead of carrying two hardcoded string
/// lists that can drift apart.
///
/// A sixteenth variant cannot be classified by accident, and that is the
/// compiler's doing rather than a review's: [`Frame::kind`] and
/// [`Frame::is_agent_sendable`] are both exhaustive matches, so adding one
/// without deciding which reserved set it belongs to does not build. The two
/// consts are then checked *against* those matches by
/// `the_two_reserved_sets_are_disjoint_and_total`, which is what keeps the
/// name lists — the form [`Frame::is_agent_sendable_kind`] and
/// [`Frame::reserved_kind`] answer in — from drifting away from the types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// See [`IdleNotification`].
    IdleNotification(IdleNotification),
    /// See [`PlanApprovalRequest`].
    PlanApprovalRequest(PlanApprovalRequest),
    /// See [`PlanApprovalResponse`].
    PlanApprovalResponse(PlanApprovalResponse),
    /// See [`ShutdownRequest`].
    ShutdownRequest(ShutdownRequest),
    /// See [`ShutdownApproved`].
    ShutdownApproved(ShutdownApproved),
    /// See [`ShutdownRejected`].
    ShutdownRejected(ShutdownRejected),
    /// See [`TaskAssignment`].
    TaskAssignment(TaskAssignment),
    /// See [`TaskCompleted`].
    TaskCompleted(TaskCompleted),
    /// See [`TeammateTerminated`].
    TeammateTerminated(TeammateTerminated),
    /// See [`ModeSetRequest`].
    ModeSetRequest(ModeSetRequest),
    /// See [`PermissionRequest`].
    PermissionRequest(PermissionRequest),
    /// See [`PermissionResponse`].
    PermissionResponse(PermissionResponse),
    /// See [`SandboxPermissionRequest`].
    SandboxPermissionRequest(SandboxPermissionRequest),
    /// See [`SandboxPermissionResponse`].
    SandboxPermissionResponse(SandboxPermissionResponse),
    /// See [`TeamPermissionUpdate`].
    TeamPermissionUpdate(TeamPermissionUpdate),
}

impl Frame {
    /// The `type` discriminator this frame travels under.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::IdleNotification(_) => "idle_notification",
            Self::PlanApprovalRequest(_) => "plan_approval_request",
            Self::PlanApprovalResponse(_) => "plan_approval_response",
            Self::ShutdownRequest(_) => "shutdown_request",
            Self::ShutdownApproved(_) => "shutdown_approved",
            Self::ShutdownRejected(_) => "shutdown_rejected",
            Self::TaskAssignment(_) => "task_assignment",
            Self::TaskCompleted(_) => "task_completed",
            Self::TeammateTerminated(_) => "teammate_terminated",
            Self::ModeSetRequest(_) => "mode_set_request",
            Self::PermissionRequest(_) => "permission_request",
            Self::PermissionResponse(_) => "permission_response",
            Self::SandboxPermissionRequest(_) => "sandbox_permission_request",
            Self::SandboxPermissionResponse(_) => "sandbox_permission_response",
            Self::TeamPermissionUpdate(_) => "team_permission_update",
        }
    }

    /// Whether an agent may originate this frame (§5.1).
    ///
    /// Ten kinds may, five may not, and the split is the point: the first ten
    /// have a legitimate sender other than the harness, so a messaging tool
    /// refuses them as *plain text* while accepting them in structured form;
    /// the other five have no legitimate agent sender at all and are refused
    /// with no escape hatch.
    ///
    /// Written as an exhaustive match rather than as a lookup in
    /// [`AGENT_SENDABLE`], because a lookup answers `false` for a name it has
    /// never heard of — so a sixteenth variant would compile, classify itself
    /// harness-only without anyone deciding that, and be missed by
    /// [`Frame::reserved_kind`] at the same time, which is delivery as prose.
    /// The match makes that a build failure instead.
    #[must_use]
    pub fn is_agent_sendable(&self) -> bool {
        match self {
            Self::PermissionRequest(_)
            | Self::PermissionResponse(_)
            | Self::SandboxPermissionRequest(_)
            | Self::SandboxPermissionResponse(_)
            | Self::ShutdownRequest(_)
            | Self::ShutdownApproved(_)
            | Self::TeamPermissionUpdate(_)
            | Self::ModeSetRequest(_)
            | Self::PlanApprovalRequest(_)
            | Self::PlanApprovalResponse(_) => true,
            Self::IdleNotification(_)
            | Self::TeammateTerminated(_)
            | Self::TaskAssignment(_)
            | Self::TaskCompleted(_)
            | Self::ShutdownRejected(_) => false,
        }
    }

    /// The same question asked of a `type` discriminator rather than of a
    /// decoded frame.
    ///
    /// A validator that has classified some text with [`Frame::reserved_kind`]
    /// holds a kind and nothing else — the body may be one it cannot decode,
    /// which is exactly the case rung 7 exists for — so it needs this rather
    /// than a `contains` of its own on [`AGENT_SENDABLE`]. Answers `false` for
    /// any name outside the fifteen, which is the same answer as "not
    /// something an agent may send".
    #[must_use]
    pub fn is_agent_sendable_kind(kind: &str) -> bool {
        AGENT_SENDABLE.contains(&kind)
    }

    /// The reserved kind some text names, if it names one (§5.2 rung 7).
    ///
    /// **Answers from the `type` field alone**, and deliberately: Claude's own
    /// `isStructuredProtocolMessage` keys on the tag, and a message whose tag
    /// says `shutdown_approved` is a frame whatever else is in it. Deciding
    /// this by whether the whole body parses would hand an attacker the
    /// simplest possible bypass — send a frame with one field missing, watch
    /// it fail to decode, and have it delivered as prose instead.
    ///
    /// Non-JSON, a JSON value that is not an object, an object with no `type`,
    /// a non-string `type`, and any tag outside the fifteen all answer
    /// [`None`].
    ///
    /// The returned name is this module's own `'static` spelling, not a
    /// borrow of the caller's text.
    ///
    /// # Any `type` wins, not the first and not the last
    ///
    /// JSON permits an object to repeat a key, and readers disagree about
    /// which one counts: `JSON.parse` — what a real `claude` peer reads a
    /// mailbox entry with — takes the **last**, serde's derived code refuses
    /// the document outright, and `serde_json::Value` takes the last as well.
    /// Any of those three is a bypass here, because a disagreement about
    /// which `type` counts is a text one side delivers as prose while the
    /// other acts on it as a frame. So this reads **every** entry of the
    /// object and classifies as reserved if *any* `type` names one of the
    /// fifteen.
    ///
    /// That is deliberately stricter than every reader it might face, and
    /// that is the only safe direction for a gate: the question rung 7 asks
    /// is not "what does this text mean" but "could anything downstream read
    /// this as a frame". A decoy first key, a decoy last key, and an escaped
    /// spelling of the key itself all classify.
    ///
    /// # Cost
    ///
    /// Every entry is *visited*, but only the key of each and the value of a
    /// `type` are ever looked at — everything else is skipped through
    /// [`serde::de::IgnoredAny`] without being built. This runs on every
    /// outbound message, most of which are prose, and materializing a
    /// megabyte of somebody's pasted JSON as a tree to look at one string is a
    /// cost with no answer in it.
    #[must_use]
    pub fn reserved_kind(text: &str) -> Option<&'static str> {
        match tag_of(text, Naming::Skip) {
            Tagged::Reserved(kind) => Some(kind),
            Tagged::NotAnObject | Tagged::Untagged | Tagged::Unknown { .. } => None,
        }
    }

    /// The same walk as [`Frame::reserved_kind`], reporting what it *found*
    /// rather than only whether the answer was one of the fifteen.
    ///
    /// [`Frame::reserved_kind`] compresses three different facts into
    /// [`None`]: text that is no JSON object at all, an object carrying no
    /// `type`, and an object whose `type` is a kind this build has never heard
    /// of. A guard deciding whether some text may be composed into a foreign
    /// CLI's prompt has to tell the third from the first two — a document
    /// shaped like a frame is a document some *other* build, or a newer one,
    /// would act on — and it has to be able to name the kind it refused, so
    /// the drop is something a reader can account for rather than a silent
    /// disappearance.
    ///
    /// Every rule [`Frame::reserved_kind`] documents holds here unchanged,
    /// because it is literally this walk: any `type` naming one of the fifteen
    /// wins over any position and over any other `type`.
    ///
    /// # Cost
    ///
    /// One [`String`] for the unknown name, and only in that arm. The
    /// no-allocation promise [`Frame::reserved_kind`]'s own `Cost` section
    /// makes is kept by that reader asking this walk not to keep the name.
    #[must_use]
    pub fn classify(text: &str) -> Tagged {
        tag_of(text, Naming::Keep)
    }
}

/// What [`Frame::classify`] found at the top level of some text.
///
/// The three not-a-frame answers are kept apart because they are different
/// facts about the sender: prose is somebody talking, an untagged object is
/// somebody's data, and a tagged object this build cannot name is a frame
/// nobody here can read — which is the one of the three that is evidence of
/// skew rather than of content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tagged {
    /// Not a JSON object: prose, an array, a bare string or number, or
    /// malformed JSON. Ordinary content, and the common answer.
    NotAnObject,
    /// A JSON object, and not one entry of it is `type`.
    Untagged,
    /// A JSON object whose `type` names one of the fifteen (§5.1).
    Reserved(&'static str),
    /// A JSON object carrying a `type` this build has never heard of.
    Unknown {
        /// What it called itself, when the value was a string; [`None`] when
        /// it was a number, an array or an object.
        ///
        /// Absence is not the same as no `type` at all — a `{"type": 42}` is
        /// still tagged, and a guard reading this must not be told otherwise
        /// merely because there was no name to report.
        name: Option<String>,
    },
}

/// The `'static` spelling of a reserved kind, if the text names one.
///
/// One place, so [`Frame::reserved_kind`] and [`Frame::is_agent_sendable_kind`]
/// cannot come to disagree about what the fifteen are called.
fn reserved_name(tag: &str) -> Option<&'static str> {
    AGENT_SENDABLE
        .into_iter()
        .chain(HARNESS_ONLY)
        .find(|kind| *kind == tag)
}

/// Whether the walk keeps the name of a `type` outside the fifteen.
///
/// Both readers do the same walk and differ only here.
/// [`Frame::reserved_kind`] answers a yes-or-no question on every outbound
/// message and has a documented no-allocation promise to keep, so it asks for
/// [`Naming::Skip`] and the name is never built; [`Frame::classify`] is the
/// one that has to report the kind it refused.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Naming {
    /// Build the [`String`] for an unknown tag.
    Keep,
    /// Do not: the caller cannot tell an unnamed unknown from a named one.
    Skip,
}

/// Walks `text` as one JSON object, reading the value of every `type`.
///
/// The map walk is hand-written rather than derived, and that is the whole
/// point of it — a derived `Deserialize` errors on a repeated key, and an
/// error here would mean "not a frame", which is exactly the answer an
/// attacker wants for a frame they prefixed with a decoy `type`.
///
/// Anything that is not a JSON object, and anything with trailing input after
/// one, is [`Tagged::NotAnObject`]: the trailing check is
/// `serde_json::from_str`'s own, kept because this walk stands in for a call
/// that had it and a text both readers disagree about is the bypass this whole
/// module is built to refuse.
fn tag_of(text: &str, naming: Naming) -> Tagged {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let Ok(found) = de::DeserializeSeed::deserialize(TagWalk { naming }, &mut deserializer) else {
        return Tagged::NotAnObject;
    };

    match deserializer.end() {
        Ok(()) => found,
        Err(_) => Tagged::NotAnObject,
    }
}

/// The object walk, carrying what the caller wants done with an unknown name.
#[derive(Clone, Copy)]
struct TagWalk {
    naming: Naming,
}

impl<'de> de::DeserializeSeed<'de> for TagWalk {
    type Value = Tagged;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(self)
    }
}

impl<'de> de::Visitor<'de> for TagWalk {
    type Value = Tagged;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: de::MapAccess<'de>,
    {
        let mut reserved = None;
        let mut unknown = None;

        // The loop runs to the end even once an answer is in hand: stopping
        // early would leave the document half-read, and the point of reading
        // it all is that no position in the object is privileged.
        while let Some(key) = map.next_key::<TagKey>()? {
            match key {
                TagKey::Type => match map.next_value_seed(TagValue {
                    naming: self.naming,
                })? {
                    TagSeen::Reserved(kind) => reserved = reserved.or(Some(kind)),
                    // First unknown rather than last, so a decoy cannot change
                    // which kind a refusal names. It is reported only when no
                    // entry named one of the fifteen.
                    TagSeen::Unknown(name) => unknown = unknown.or(Some(name)),
                },
                TagKey::Other => drop(map.next_value::<de::IgnoredAny>()?),
            }
        }

        Ok(match (reserved, unknown) {
            // Reserved wins over everything, from whichever entry it came —
            // the strictness rung 7 depends on.
            (Some(kind), _) => Tagged::Reserved(kind),
            (None, Some(name)) => Tagged::Unknown { name },
            (None, None) => Tagged::Untagged,
        })
    }
}

/// Whether an object key is the tag, decided without allocating: the visitor
/// is handed the key already unescaped, so `"type"` is `type` here as it
/// is to every other reader.
enum TagKey {
    /// The key is `type`.
    Type,
    /// Anything else, whose value is skipped unread.
    Other,
}

impl<'de> Deserialize<'de> for TagKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct KeyVisitor;

        impl de::Visitor<'_> for KeyVisitor {
            type Value = TagKey;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object key")
            }

            fn visit_str<E>(self, key: &str) -> Result<TagKey, E>
            where
                E: de::Error,
            {
                Ok(if key == "type" {
                    TagKey::Type
                } else {
                    TagKey::Other
                })
            }
        }

        deserializer.deserialize_str(KeyVisitor)
    }
}

/// What one `type` entry spelled.
///
/// Not an [`Option`], because "outside the fifteen" and "no `type` here" are
/// the two facts [`Frame::classify`] exists to keep apart, and a walk that
/// collapsed them at the value would have nothing left to report at the
/// object.
enum TagSeen {
    /// The value was a string naming one of the fifteen.
    Reserved(&'static str),
    /// The value named no frame this build knows. [`Some`] carries what it
    /// called itself, which is [`None`] when the value was not a string at all
    /// — and also when the caller asked for [`Naming::Skip`], which is why
    /// only [`Frame::classify`] may read this as "not a string".
    Unknown(Option<String>),
}

/// Reads one `type` value into what it spelled.
///
/// Every shape a JSON value can take has an arm, and none of them is an error:
/// a `type` that is a number, an array or an object names no frame, but
/// *failing* on one would abandon the rest of the document and answer "not a
/// frame" for a text whose second `type` is `shutdown_approved`.
#[derive(Clone, Copy)]
struct TagValue {
    naming: Naming,
}

impl<'de> de::DeserializeSeed<'de> for TagValue {
    type Value = TagSeen;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'de> de::Visitor<'de> for TagValue {
    type Value = TagSeen;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_str<E>(self, tag: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(match reserved_name(tag) {
            Some(kind) => TagSeen::Reserved(kind),
            None => TagSeen::Unknown(match self.naming {
                Naming::Keep => Some(tag.to_owned()),
                Naming::Skip => None,
            }),
        })
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TagSeen::Unknown(None))
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TagSeen::Unknown(None))
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TagSeen::Unknown(None))
    }

    fn visit_i128<E>(self, _: i128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TagSeen::Unknown(None))
    }

    fn visit_u128<E>(self, _: u128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TagSeen::Unknown(None))
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TagSeen::Unknown(None))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TagSeen::Unknown(None))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(TagSeen::Unknown(None))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        while seq.next_element::<de::IgnoredAny>()?.is_some() {}

        Ok(TagSeen::Unknown(None))
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: de::MapAccess<'de>,
    {
        while map
            .next_entry::<de::IgnoredAny, de::IgnoredAny>()?
            .is_some()
        {}

        Ok(TagSeen::Unknown(None))
    }
}

/// A frame that provably came from the team's lead.
///
/// §7-2 is a check the original performs at each call site — `msg.from ===
/// "team-lead"` before applying a `plan_approval_response` or a
/// `mode_set_request` — and §8.4's advice is to put the check in the type
/// instead. That is this: a lead-only handler takes a `LeadFrame`, and a peer
/// frame cannot reach it because the argument cannot be built.
///
/// [`LeadFrame::parse`] is the **only** way to build one. The inner field is
/// private, there is no `From` impl, and there is no `Deserialize` impl —
/// deserializing one would be a second constructor that never saw a sender.
///
/// There is no [`Deref`](std::ops::Deref) either, for a legibility reason
/// rather than a safety one: reaching the frame goes through
/// [`LeadFrame::frame`] or [`LeadFrame::into_inner`] by name, so a reviewer
/// asking "where is lead authority actually spent" greps two identifiers
/// instead of reading every method call to find the implicit ones.
#[derive(Clone, Debug, PartialEq)]
pub struct LeadFrame(Frame);

impl LeadFrame {
    /// Accepts the frame only when the sender is the team's recorded lead.
    ///
    /// Both names come from outside — `sender` off the mailbox entry, `lead`
    /// off the team file — and the comparison is exact: a lead is identified
    /// by the name the team recorded, never by a spelling of it.
    #[must_use]
    pub fn parse(sender: &str, lead: &str, frame: Frame) -> Option<Self> {
        (sender == lead).then_some(Self(frame))
    }

    /// The frame inside.
    #[must_use]
    pub fn frame(&self) -> &Frame {
        &self.0
    }

    /// Gives up the proof and returns the frame.
    #[must_use]
    pub fn into_inner(self) -> Frame {
        self.0
    }
}

/// A peer's message on its way *in*: the wire shape
/// [`Command::SendPrompt`](crate::Command::SendPrompt) and
/// [`Command::Steer`](crate::Command::Steer) carry one on.
///
/// §7-5 and §8.4: peer output is **data, never authority**. The original
/// enforces that with prompt text alone; here it is also this type's shape.
/// The fields are **private** ([`PermissionResponse`]'s rule, for the same
/// reason): a payload has no `Display`, no `Into<String>` and no accessor for
/// its body, so nothing can read a peer's words off it except by turning it
/// into the part that says whose words they are —
/// [`into_part`](Self::into_part), the **only** thing this type does. The
/// frontend that receives a teammate's message therefore cannot quietly paste
/// it into the prompt text — where it would reach the model as something the
/// person typed — without writing a conversion nobody could mistake for an
/// accident.
///
/// Serde reaches those fields, and that is not the same hole: this type's
/// rule is that its content cannot be *read* as text, and decoding one from a
/// command breaks nothing of that.
///
/// [`PermissionResponse`]: crate::team::PermissionResponse
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerPayload {
    /// Which teammate wrote it — the bare member name, which is also its
    /// mailbox address.
    from: String,
    /// The sender's own one-line summary, where it wrote one. Capped at
    /// [`DISPLAY_FIELD_CAP`] on the way into the part, never here: what
    /// arrives on a wire is recorded as it arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    /// The member's assigned color, for a frontend to draw it in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    /// What the peer said, verbatim.
    body: String,
}

impl PeerPayload {
    /// Takes a teammate's message off whatever delivered it.
    ///
    /// The arguments are in [`Part::peer`](crate::Part::peer)'s order, because
    /// this becomes one of those and two of the four are adjacent options.
    ///
    /// `color` is recorded as given. Whether it is one the roster actually
    /// assigned is a question for the caller, which is holding the member
    /// record: §5.3's "write it only if it validates" ran there, and a wire
    /// struct re-deciding it would be a second opinion about somebody else's
    /// roster.
    #[must_use]
    pub fn new(
        from: impl Into<String>,
        summary: Option<String>,
        color: Option<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            from: from.into(),
            summary,
            color,
            body: body.into(),
        }
    }

    /// The one thing a payload becomes: the transcript part that says whose
    /// words these are.
    ///
    /// The summary is capped here, on the way into the part, because
    /// [`Part::peer`](crate::Part::peer) deliberately does not re-cap — a
    /// stored part must read back as the bytes it was written as.
    #[must_use]
    pub fn into_part(self) -> crate::Part {
        let summary = self.summary.map(|mut summary| {
            // Truncated in place: `cap_for_display` measures where the cut
            // goes, and the owned string is already here to be cut.
            summary.truncate(cap_for_display(&summary).len());
            summary
        });

        crate::Part::peer(self.from, summary, self.color, self.body)
    }
}

/// Truncates a display-only field to [`DISPLAY_FIELD_CAP`] characters.
///
/// Public because it is the one place the cap lives:
/// [`PeerPayload::into_part`] applies it to a summary, and whoever renders an
/// [`IdleNotification::failure_reason`] — or a summary that reached them by
/// some other path — applies the same function rather than a second constant
/// of its own.
#[must_use]
pub fn cap_for_display(text: &str) -> &str {
    cap_chars(text, DISPLAY_FIELD_CAP)
}

/// `text`, cut to at most `cap` characters on a `char` boundary.
///
/// Counts `char`s and cuts on a `char` boundary, so a field of CJK is
/// shortened rather than panicked on. A `char` is not a grapheme: a flag or a
/// ZWJ sequence sitting across the cut is split inside itself and draws as a
/// different glyph. That is accepted rather than fixed — the cut is always
/// valid UTF-8 and never panics, and a segmentation dependency to make the
/// last glyph of a truncated field prettier is not a trade this crate's
/// three-entry dependency list should make.
///
/// The `cap` parameter exists because two caps read one rule: the §5.3
/// display cap above, and the wider bound `ganja-core` cuts a peer's
/// reflected words to.
#[must_use]
pub fn cap_chars(text: &str, cap: usize) -> &str {
    match text.char_indices().nth(cap) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// A peer's summary as a renderer shows it: a blank one is nothing at all,
/// and anything else is capped through [`cap_for_display`].
///
/// One function rather than three matches because a stored part is
/// deliberately storable with a summary that never went through
/// [`PeerPayload::into_part`]'s cap — so the engine's §5.3 envelope and both
/// frontends' renderers each re-apply the same projection, and it lives here
/// so they cannot drift.
#[must_use]
pub fn display_summary(summary: Option<&str>) -> Option<&str> {
    summary
        .filter(|summary| !summary.trim().is_empty())
        .map(cap_for_display)
}

/// Identifies one peer-to-socket message the sender minted, so a receipt can
/// later name what it settles (**D532**, **D534**).
///
/// A type of its own rather than a reused
/// [`MessageId`](crate::MessageId), for [`QuestionId`](crate::QuestionId)'s
/// reason: the two are answered by different questions even though both sort
/// in creation order. This one is minted per `SocketMessage` send, lives only
/// in the sender's volatile outstanding-receipt registry, and is settled by
/// [`Event::PeerReceipt`](crate::Event::PeerReceipt);
/// [`MessageId`](crate::MessageId) names a [`Message`](crate::Message)
/// permanently stored in this session's own transcript. A caller holding
/// both must not be able to pass a stored message's id where a peer send's
/// id belongs, or the reverse.
///
/// v2 §"Receipts and sender UX", evidence 220977-221015: local sends register
/// up to two hundred outstanding message ids, each later matched against
/// exactly one settlement.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PeerMessageId(String);

impl PeerMessageId {
    /// Mints an id that sorts after every id minted before it.
    #[must_use]
    pub fn ascending() -> Self {
        Self(crate::uuidv7())
    }

    /// The id as it travels the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for PeerMessageId {
    /// Adopts a text id verbatim, for a caller that already holds one as a
    /// [`String`] — a decoded wire body, or a test fixture — rather than
    /// through [`PeerMessageId::ascending`].
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// How a peer message this session *sent* ultimately settled, as reported
/// back over the receipt route (**D534**).
///
/// A type of its own rather than a reused
/// [`HeldOutcome`](crate::HeldOutcome), for the same rule that keeps
/// [`QuestionId`](crate::QuestionId) apart from
/// [`PermissionId`](crate::PermissionId): the two are answered by different
/// questions even where their spellings coincide, and here the coincidence
/// hides a real divergence rather than none at all.
/// [`HeldOutcome`](crate::HeldOutcome) is a **receiver's** own record of how
/// *its* hold ended, and its `Expired` covers three causes — the review
/// deadline, a capacity eviction, or a shutdown drain. A settlement that
/// crosses the receipt route as `expired`, by contrast, means **only** the
/// review deadline: a capacity eviction and a shutdown drain each settle the
/// receiver's own hold the same way locally but post nothing on this route
/// at all (D534's `N1`/`D3` corrections). Sharing
/// [`HeldOutcome`](crate::HeldOutcome) here would let that narrower
/// guarantee rot into a comment nobody reads instead of a fact the type
/// states.
///
/// Exactly the reference's four settlement statuses minus `held`, which
/// ganja already answers **synchronously**, in the very POST answer that was
/// held, rather than over this route (v2 §"Receipts and sender UX", evidence
/// 886033-886075, 886636-886697). An unknown status — `held` included —
/// refuses readably at deserialization rather than being guessed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerReceiptStatus {
    /// A person approved the held message and it reached its ordinary
    /// delivery path.
    Delivered,
    /// A person denied the held message; it goes no further.
    Denied,
    /// The review window ran out with nobody deciding. On this route that is
    /// the *only* way an entry ends up here — a capacity eviction or a
    /// shutdown drain settles the receiver's own hold the same way but never
    /// posts a receipt for it.
    Expired,
}

/// Which surface a member runs on, spelled as the `--backend` argument spells
/// it.
///
/// ganja's own vocabulary, and not Claude's `backendType` — that one is
/// carried as text where it appears on a frame, because it is somebody else's
/// word list.
///
/// Growing this enum is version skew with a wide blast radius; the module doc
/// says how wide and why the posture is still the right one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemberBackend {
    /// A teammate running inside this process.
    InProcess,
    /// A `ganja` pane of its own, and the default a spawn that names no
    /// backend gets.
    Ganja,
    /// A real `claude` pane.
    Claude,
    /// A headless `codex exec` child, driven one turn per message (**D508**).
    Codex,
    /// A resident `agy` child, driven one NDJSON line per message (**D508**).
    Agy,
    /// A headless `grok` child, driven one turn per message (**D508**).
    Grok,
}

/// One member of a team, as a reader of the team sees it.
///
/// **ganja's own projection, not Claude's member record.** Claude's document
/// is a passthrough shape that a real `claude` binary also reads, so it stays
/// where the file I/O is; this is a strict-fields view a frontend can render
/// without linking any of that. The split is what gives a ganja-only field
/// somewhere to live: [`MemberView::recent_calls`] exists here precisely
/// because writing it into Claude's file would be an unstated amendment to a
/// format somebody else owns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberView {
    /// The bare member name, which is also its mailbox address.
    pub name: String,
    /// The derived `<name>@<team>` identity.
    pub agent_id: String,
    /// The surface it runs on.
    pub backend: MemberBackend,
    /// Its assigned color, where one was assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Whether this member is the team's lead.
    pub is_lead: bool,
    /// A bounded ring of one-line summaries of what this member most recently
    /// did (D503), so the backend with no window of its own is not the least
    /// observable one. Live registry state: it is worthless once the process
    /// exits and misleading if a resumed session showed a stale one, which is
    /// the other half of why it is not persisted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_calls: Vec<String>,
}

/// A team, as a reader of it sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamView {
    /// The team's name, which is also its directory under the teams root.
    pub team: String,
    /// The lead's member name — what [`LeadFrame::parse`] compares against.
    pub lead: String,
    /// Everyone in it, the lead included.
    pub members: Vec<MemberView>,
}

#[cfg(test)]
#[path = "team_tests.rs"]
mod tests;
