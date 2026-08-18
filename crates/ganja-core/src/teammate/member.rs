//! What a process that **is** a teammate holds: its postbox, and its asks on
//! their way to the lead (§4.1, §5, §10.3).
//!
//! Upstream opencode has **no counterpart at all**: nothing there is a member
//! of anything. What is ported is Claude Code's pane teammate — a whole
//! process launched by some other session's lead, told who it is on its
//! command line and reached through its mailbox — and specifically the two
//! things such a process needs that neither [`crate::subagent::Postbox`] nor
//! [`crate::teammate::posture::Forwarding`] can be, because both of those are
//! built against a [`crate::teammate::TeammateRegistry`] the member process
//! has no share of. The plan is
//! `.omc/plans/2026-08-17-teammates-first-landing.md`.
//!
//! # [`MemberPostbox`] — the third postbox
//!
//! Same trait, same ladder, same anti-forgery rule: **the sender is bound at
//! construction and is never an argument.** The name comes off the launch
//! line the lead wrote (`--agent-name`), through the same grammar the lead
//! resolved it under, and no method takes a `from`. What differs is where the
//! roster comes from — the team file on disk, read fresh each time it is
//! asked, since a member watches the roster move under it rather than holding
//! the registry that moves it — and one standing entry: **the lead is always
//! addressable**, whether or not the file names it yet. §4.1 launches the
//! pane before its record is written, so the file may not exist for the first
//! milliseconds of the pane's life; the lead's inbox path is derivable from
//! the team name alone, and a teammate that could not reach its lead until
//! the lead finished writing about it would be a teammate that cannot answer
//! the shutdown it was just asked for.
//!
//! # [`Asks`] — the pane's `ForwardToLead` (**D-5**)
//!
//! An in-process teammate's dialogs cross to the lead on
//! [`crate::teammate::posture::Forwarded`]'s channel, which keeps a reply
//! oneshot. A pane has no channel: it has a mailbox and §5's two permission
//! frames, and that is what this rides. The ask goes out as a
//! `permission_request` written to the lead's inbox; the lead's own pass
//! ([`crate::teammate::lead_inbox`]) puts it in front of the same dialog its
//! in-process teammates use, and the answer comes back as a
//! `permission_response` written to this member's inbox.
//!
//! **This value is driven, not spawned.** The reference's §10.3-3 keeps the
//! engine untouched, and the frontend that runs a pane already owns both ends
//! of what is needed: it sees its own engine's `PermissionRequested` (that is
//! how it raises a dialog at all), and it reads its own inbox on the tick
//! that already reads everything else. So [`Asks::forward`] is what it calls
//! instead of raising the dialog, [`Asks::resolve`] is what its inbox pass
//! hands a lead's `permission_response` to, and the reply that comes back is
//! the frontend's to send as a `ReplyPermission` — the same shape its every
//! other answer to its engine takes. Nothing here holds the engine, spawns a
//! task, or waits: the wait is the engine's own, in the turn that asked, and
//! a cancelled turn ends it exactly as it ends every other dialog wait — with
//! a `PermissionReplied` the frontend hands to [`Asks::retire`]. There is
//! deliberately no timeout of this module's own; a dialog wait is indefinite
//! everywhere else in this build, and the hooks that bracket one bracket this
//! one too.
//!
//! **Only the lead's answer counts, and the type says so.** [`Asks::resolve`]
//! takes a [`LeadFrame`], which cannot be built from a peer's message (§7-2),
//! and answers only a request this member is still waiting on (§7-3, the
//! runner's `plan_approval` rule pointed at this frame). What a peer writes
//! into a member's inbox therefore cannot loosen anything: it never reaches
//! the engine, and a rule is stored only when the person at the lead's dialog
//! answered "always" to a question this member's own engine asked — the same
//! store, the same store the lead's own "always" would write.
//!
//! # The dialect: what ganja puts in Claude's opaque fields
//!
//! §5's permission family carries three fields as opaque values —
//! `permission_suggestions`, `updated_input`, `permission_updates` — and says
//! nothing about their shape. Two things have to cross that the frame has no
//! named slot for: the directories outside the project a call would work in
//! (which the lead's dialog must name, or it would be asking about something
//! narrower than what the answer covers), and whether the answer was "once"
//! or "always". Both ride the Agent SDK's own `PermissionUpdate` spellings —
//! `{"type":"addDirectories","directories":[…],"destination":"session"}` in
//! the request's suggestions, `{"type":"addRules","behavior":"allow",…}` in
//! the response's updates — because that is the one shape a real `claude` on
//! either end of this inbox already reads and writes, and an update whose
//! shape this build does not recognise is read as **once**: erring towards
//! asking again is the direction the permission layer errs in everywhere
//! else. `updated_input` echoes the request's `input`, since a peer that runs
//! with what the response says it may run with must be handed exactly what it
//! asked to run.
//!
//! Every link above is spelled out, for [`crate::teammate::posture`]'s reason:
//! the merged doc resolves in `teammate.rs`'s scope, where none of these names
//! exist.
//!
//! [`MemberPostbox`]: crate::teammate::member::MemberPostbox
//! [`Asks`]: crate::teammate::member::Asks
//! [`Asks::forward`]: crate::teammate::member::Asks::forward
//! [`Asks::resolve`]: crate::teammate::member::Asks::resolve
//! [`Asks::retire`]: crate::teammate::member::Asks::retire
//! [`LeadFrame`]: ganja_protocol::team::LeadFrame

use std::{collections::HashMap, path::PathBuf, sync::Mutex};

use async_trait::async_trait;
use ganja_protocol::team::{
    Frame, LeadFrame, PermissionRequest, PermissionResponse, PermissionResponseBody,
    PermissionResponseSubtype,
};
use ganja_team::{
    LEAD, MailboxMessage, MemberName, TeamFile, TeamName, TeamsRoot, mailbox, record,
};
use serde_json::{Value, json};

use crate::{
    protocol::{Event, PermissionId, PermissionReply, SessionId},
    subagent::{LEADS, NO_SOCKET, RUNS_ON, UNADDRESSABLE, UNWRITTEN, WRITTEN},
    tool::team::{self, Address, Body, Peer, Reserved, Sent, Undelivered},
};

/// What is logged when a `permission_response` answers a request this member
/// is not waiting on — the runner's `IGNORED_STALE`, for this frame.
pub const IGNORED_STALE_ANSWER: &str =
    "a permission answer named a request this member is not waiting on, and was ignored";

/// The error a `permission_response` carries when the person at the lead's
/// dialog said no.
pub const REFUSED_AT_DIALOG: &str = "the lead refused this call at its permission dialog";

/// The `type` of the suggestion a request's outside directories ride in, and
/// of the update an "always" rides in — the Agent SDK's `PermissionUpdate`
/// discriminators, kept verbatim for the reason in the module doc.
const ADD_DIRECTORIES: &str = "addDirectories";
const ADD_RULES: &str = "addRules";
/// The `behavior` an update has to say for this build to read it as "always".
const ALLOW: &str = "allow";
/// The `destination` values written beside them: a directory disclosure lasts
/// the session, a stored rule lasts the project — which is what an "always"
/// answered at ganja's own dialog means.
const SESSION: &str = "session";
const PROJECT_SETTINGS: &str = "projectSettings";

/// Where a member's `send_message` calls are posted.
///
/// The sender is a field, set once from the launch line, and no method takes
/// a `from` — [`crate::subagent::Postbox`]'s rule, for its reason. The roster
/// is the team file's, read when it is asked for rather than cached, because
/// a member holds no registry and the file is the only place the team's
/// membership is written down.
pub struct MemberPostbox {
    /// The name every message written through this carries.
    sender: MemberName,
    /// The team whose file is the roster and whose inboxes are the addresses.
    team: TeamName,
    /// Where that team's documents live.
    root: TeamsRoot,
}

impl std::fmt::Debug for MemberPostbox {
    /// Which team and which sender, and nothing else — the same rule
    /// [`crate::subagent::Postbox`]'s renders under, because this lands in a
    /// `{:?}` of somebody's `ToolCtx`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemberPostbox")
            .field("team", &self.team)
            .field("sender", &self.sender)
            .finish_non_exhaustive()
    }
}

impl MemberPostbox {
    /// A member's own postbox, stamped with `name`.
    ///
    /// Takes the parsed [`MemberName`] rather than a string so that the one
    /// door onto a sender's name is the grammar's, and the launch line has
    /// already been through it by the time anything can be built here.
    #[must_use]
    pub fn new(name: MemberName, team: TeamName, root: TeamsRoot) -> Self {
        Self {
            sender: name,
            team,
            root,
        }
    }

    /// The team file, or [`None`] where there is not one to read.
    ///
    /// Synchronous, because [`team::Postbox::roster`] is: the file is a few
    /// hundred bytes and is written by rename, so a read never sees it torn.
    /// Absence is ordinary — §4.1 launches a pane before its record is
    /// written — and is logged at debug; anything else is a real problem with
    /// the team's directory and is said out loud.
    fn read_team(&self) -> Option<TeamFile> {
        let path = self.root.config_path(&self.team);
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(file) => Some(file),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "the team file would not decode");
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no team file yet");
                None
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "the team file could not be read");
                None
            }
        }
    }

    /// The team as this member may address it: everybody the file names but
    /// this member itself, and the lead whether or not the file names it.
    ///
    /// The lead is the constant name §2.2 derives `leadAgentId` from and the
    /// registry registers under, so it needs no file to be known — and it is
    /// listed first, once, whatever the file says, which is the invariant the
    /// tool's last rung reads (`ganja-tool`'s [`Peer::lead`]).
    fn peers(&self, file: Option<&TeamFile>) -> Vec<Peer> {
        let mut peers = vec![Peer {
            name: LEAD.to_owned(),
            description: Some(LEADS.to_owned()),
            lead: true,
        }];
        let Some(file) = file else {
            return peers;
        };
        peers.extend(
            file.members
                .iter()
                .filter(|member| !member.is_lead())
                // Not in its own roster: there is nothing to say to yourself
                // that a turn cannot say directly.
                .filter(|member| !member.name.eq_ignore_ascii_case(self.sender.as_str()))
                .map(|member| Peer {
                    name: member.name.clone(),
                    description: Some(format!(
                        "{RUNS_ON} {} backend",
                        member.surface().backend_type()
                    )),
                    lead: false,
                }),
        );

        peers
    }

    /// The member `name` names, matched as the team's own names are: case
    /// insensitively, with the canonical spelling coming back off the roster.
    fn recipient(&self, file: Option<&TeamFile>, name: &str) -> Option<Peer> {
        self.peers(file)
            .into_iter()
            .find(|peer| peer.name.eq_ignore_ascii_case(name))
    }
}

#[async_trait]
impl team::Postbox for MemberPostbox {
    fn classify(&self, text: &str) -> Reserved {
        // The one parse `ganja-protocol` owns, exactly as the lead's postbox
        // answers it: the tool may not name that crate, so no list of frame
        // names exists on its side to fall out of step with.
        match Frame::reserved_kind(text) {
            None => Reserved::No,
            Some(kind) if Frame::is_agent_sendable_kind(kind) => Reserved::AgentSendable { kind },
            Some(kind) => Reserved::HarnessOnly { kind },
        }
    }

    async fn deliver(&self, to: Address, body: Body) -> Result<Sent, Undelivered> {
        let name = match to {
            Address::Local(name) => name,
            Address::Uds { .. } => {
                return Err(Undelivered::NoTransport {
                    reason: NO_SOCKET.to_owned(),
                });
            }
        };
        // Off the runtime's worker threads, like every other read of these
        // documents from inside a turn: `ganja-team` is synchronous on
        // purpose.
        let this = self.clone_for_read();
        let file = tokio::task::spawn_blocking(move || this.read_team())
            .await
            .unwrap_or_default();
        let Some(recipient) = self.recipient(file.as_ref(), &name) else {
            return Err(Undelivered::Unknown);
        };
        let member = MemberName::parse(&recipient.name).map_err(|error| Undelivered::Failed {
            reason: format!("{UNADDRESSABLE} {:?}: {error}", recipient.name),
        })?;

        let (text, summary) = match body {
            Body::Text { text, summary } => (text, summary),
            Body::Frame(document) => (document.to_string(), None),
        };
        let mut message = MailboxMessage::new(self.sender.as_str(), text, record::now_iso8601());
        message.summary = summary;

        let path = self.root.inbox_path(&self.team, &member);
        let written = tokio::task::spawn_blocking(move || mailbox::write(&path, message))
            .await
            .map_err(|error| error.to_string())
            .and_then(|written| written.map_err(|error| error.to_string()));

        match written {
            Ok(_) => Ok(Sent {
                to: member.into_inner(),
                note: WRITTEN.to_owned(),
            }),
            Err(reason) => Err(Undelivered::Failed {
                reason: format!("{UNWRITTEN} {reason}"),
            }),
        }
    }

    fn roster(&self) -> Vec<Peer> {
        self.peers(self.read_team().as_ref())
    }
}

impl MemberPostbox {
    /// A copy that can be moved onto a blocking thread to read the file: the
    /// three fields are all owned values, and this is the only reason a
    /// postbox is ever duplicated.
    fn clone_for_read(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            team: self.team.clone(),
            root: self.root.clone(),
        }
    }
}

/// A member's permission asks on their way to the lead, and the lead's answers
/// on their way back (**D-5**, the pane half).
///
/// The module doc says how it is driven; what it holds is exactly the set of
/// asks this member's engine has raised and nobody has answered yet, keyed by
/// the request id — which is the engine's own [`PermissionId`], minted
/// UUIDv7, so a lead answering several panes cannot confuse two of them.
pub struct Asks {
    /// The name every request is written under.
    name: MemberName,
    /// §2.2's `<name>@<team>`, which is what a `permission_request` carries as
    /// its asker.
    agent_id: String,
    /// Where the requests go.
    lead_inbox: PathBuf,
    /// What is still waiting on the lead, and which tool each was about.
    pending: Mutex<HashMap<PermissionId, String>>,
}

impl std::fmt::Debug for Asks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Asks")
            .field("name", &self.name)
            .field("waiting", &self.waiting())
            .finish_non_exhaustive()
    }
}

/// Why an ask did not reach the lead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unforwarded {
    /// The event handed in was not a `PermissionRequested`, which is a
    /// contract broken by the caller rather than anything about the lead.
    NotAnAsk,
    /// The request could not be written to the lead's inbox. Carries the
    /// mailbox's own sentence, because what went wrong with the file is what
    /// a person reads next.
    Unwritten(String),
}

impl std::fmt::Display for Unforwarded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnAsk => formatter.write_str("the event was not a permission request"),
            Self::Unwritten(reason) => write!(
                formatter,
                "the permission request could not be written to the lead's inbox: {reason}"
            ),
        }
    }
}

impl std::error::Error for Unforwarded {}

/// What a `permission_response` from the lead was worth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// It answered an ask this member was waiting on: the reply is what the
    /// frontend sends its engine as a `ReplyPermission` naming `id`.
    Answered {
        /// The request it answered — the engine's own id for the dialog.
        id: PermissionId,
        /// What the lead's dialog decided.
        reply: PermissionReply,
    },
    /// It named a request nothing here is waiting on — answered already, or
    /// never asked — and was ignored, with a log line saying so (§7-3).
    Stale {
        /// The request id it named.
        request_id: String,
    },
    /// The lead's frame was not a `permission_response` at all, so it is not
    /// this value's to read; the caller's own handlers own the rest.
    NotAnAnswer {
        /// The frame's `type`.
        kind: &'static str,
    },
}

impl Asks {
    /// The asks of the member `name` in `team`, addressed to that team's lead.
    #[must_use]
    pub fn new(name: MemberName, team: &TeamName, root: &TeamsRoot) -> Self {
        Self {
            agent_id: name.agent_id(team),
            lead_inbox: root.inbox_path(team, &MemberName::lead()),
            name,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Writes one of this member's dialogs to the lead as a
    /// `permission_request`, and remembers that it is waiting on the answer.
    ///
    /// The wait is recorded **before** the write and taken back if the write
    /// fails, so an answer can never arrive for an ask this side does not yet
    /// know it made. A failed write is the caller's to refuse: nothing was
    /// asked of anybody, and a dialog nobody could see has exactly one honest
    /// answer.
    ///
    /// # Errors
    ///
    /// [`Unforwarded::NotAnAsk`] for an event of any other shape, and
    /// [`Unforwarded::Unwritten`] when the lead's inbox would not take the
    /// frame.
    pub async fn forward(&self, request: &Event) -> Result<(), Unforwarded> {
        let Event::PermissionRequested { id, tool, .. } = request else {
            return Err(Unforwarded::NotAnAsk);
        };
        let ask = ask_of(&self.agent_id, request).ok_or(Unforwarded::NotAnAsk)?;
        let message = MailboxMessage::from_frame(
            self.name.as_str(),
            &Frame::PermissionRequest(ask),
            record::now_iso8601(),
        )
        .map_err(|error| Unforwarded::Unwritten(error.to_string()))?;

        self.pending
            .lock()
            .expect("the asks are never poisoned")
            .insert(id.clone(), tool.clone());

        let path = self.lead_inbox.clone();
        let written = tokio::task::spawn_blocking(move || mailbox::write(&path, message))
            .await
            .map_err(|error| error.to_string())
            .and_then(|written| written.map_err(|error| error.to_string()));
        if let Err(reason) = written {
            self.pending
                .lock()
                .expect("the asks are never poisoned")
                .remove(id);
            tracing::warn!(
                member = self.name.as_str(),
                request = id.as_str(),
                %reason,
                "a permission request could not be written to the lead"
            );

            return Err(Unforwarded::Unwritten(reason));
        }
        tracing::info!(
            member = self.name.as_str(),
            request = id.as_str(),
            tool,
            "a permission ask was forwarded to the lead"
        );

        Ok(())
    }

    /// Reads the lead's answer to one of this member's asks.
    ///
    /// Takes a [`LeadFrame`] rather than the response inside it, so that a
    /// frame anybody but the lead wrote cannot be handed in at all (§7-2). An
    /// answer to a request nothing is waiting on is ignored and logged rather
    /// than applied (§7-3): a stale answer is one to a question this
    /// conversation has already moved past — answered at the pane's own
    /// dialog, or cancelled with its turn — and acting on it would let a late
    /// frame decide a call nobody is asking about now.
    ///
    /// The wait is cleared here, so the *next* copy of the same answer is
    /// stale. That is safe where the runner's `plan_approval` had to wait for
    /// a delivery, because what the caller does with the reply — a
    /// `ReplyPermission` — is a command its engine never refuses.
    #[must_use]
    pub fn resolve(&self, lead: LeadFrame) -> Resolved {
        let kind = lead.frame().kind();
        let Frame::PermissionResponse(response) = lead.into_inner() else {
            return Resolved::NotAnAnswer { kind };
        };
        let id = PermissionId::from(response.request_id().to_owned());
        let waited = self
            .pending
            .lock()
            .expect("the asks are never poisoned")
            .remove(&id);
        let Some(tool) = waited else {
            tracing::info!(
                member = self.name.as_str(),
                request = id.as_str(),
                "{IGNORED_STALE_ANSWER}"
            );

            return Resolved::Stale {
                request_id: id.as_str().to_owned(),
            };
        };
        let reply = reply_of(&response);
        tracing::info!(
            member = self.name.as_str(),
            request = id.as_str(),
            tool,
            ?reply,
            "the lead answered a forwarded permission ask"
        );

        Resolved::Answered { id, reply }
    }

    /// Forgets an ask that is no longer waiting — because its engine published
    /// the `PermissionReplied` that ends every dialog wait, answered from
    /// wherever it was answered or refused by a cancel.
    ///
    /// Answers whether anything was forgotten. A miss is ordinary: an ask
    /// [`Asks::resolve`] already answered is retired twice, and that is what
    /// keeps a later copy of the same answer stale rather than a leak.
    pub fn retire(&self, id: &PermissionId) -> bool {
        self.pending
            .lock()
            .expect("the asks are never poisoned")
            .remove(id)
            .is_some()
    }

    /// How many asks are waiting on the lead.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.pending
            .lock()
            .expect("the asks are never poisoned")
            .len()
    }
}

/// The `permission_request` a member writes for one of its engine's dialogs.
///
/// [`None`] for an event of any other shape. Public because it is one half of
/// the dialect the module doc describes, and the lead's pass reads the frame
/// back with the other half ([`dialog_of`]) — pinned together, so the two
/// sides cannot drift.
#[must_use]
pub fn ask_of(agent_id: &str, request: &Event) -> Option<PermissionRequest> {
    let Event::PermissionRequested {
        id,
        call_id,
        tool,
        title,
        args,
        directories,
        ..
    } = request
    else {
        return None;
    };

    Some(PermissionRequest {
        request_id: id.as_str().to_owned(),
        agent_id: agent_id.to_owned(),
        tool_name: tool.clone(),
        tool_use_id: call_id.clone(),
        description: title.clone(),
        input: args.clone(),
        permission_suggestions: directory_suggestions(directories),
    })
}

/// The dialog a lead raises for a member's `permission_request`, in the one
/// shape its frontend already knows how to show.
///
/// `session_id` is the **lead's** own: the frame names its asker by agent id,
/// not by session, and the conversation whose screen this dialog lands on is
/// the lead's — which is what a client filtering events by session would
/// attribute it to anyway.
///
/// **The dialog's id is minted here, never taken from the frame.** The
/// frame's `request_id` is a member-supplied string, and the lead's frontend
/// keys its open dialogs — its own engine's and its teammates' — on the
/// [`PermissionId`] this carries: two members reusing one id, or an id equal
/// to one of the lead's own, would misroute or orphan a dialog. A fresh
/// UUIDv7 keeps that keyspace the lead's alone; the frame's own id travels
/// beside the dialog in whatever answers it (`lead_inbox`'s `Answer`), which
/// is where it is spent — on the `permission_response` the asker reads back.
#[must_use]
pub fn dialog_of(session_id: SessionId, request: PermissionRequest) -> Event {
    Event::PermissionRequested {
        session_id,
        id: PermissionId::ascending(),
        call_id: request.tool_use_id,
        tool: request.tool_name,
        title: request.description,
        args: request.input,
        directories: suggested_directories(&request.permission_suggestions),
    }
}

/// The `permission_response` a lead writes once its dialog is answered.
///
/// `input` is echoed as `updated_input` — the call ran with what it asked to
/// run with — and "always" is one `addRules` update beside it; a refusal is
/// the error arm with [`REFUSED_AT_DIALOG`]. See the module doc's dialect.
#[must_use]
pub fn response_of(
    request_id: &str,
    tool: &str,
    input: &Value,
    reply: PermissionReply,
) -> PermissionResponse {
    match reply {
        PermissionReply::Once => PermissionResponse::success(
            request_id,
            PermissionResponseBody {
                updated_input: input.clone(),
                permission_updates: Vec::new(),
            },
        ),
        PermissionReply::Always => PermissionResponse::success(
            request_id,
            PermissionResponseBody {
                updated_input: input.clone(),
                permission_updates: vec![json!({
                    "type": ADD_RULES,
                    "behavior": ALLOW,
                    "rules": [{ "toolName": tool }],
                    "destination": PROJECT_SETTINGS,
                })],
            },
        ),
        PermissionReply::Reject => PermissionResponse::error(request_id, REFUSED_AT_DIALOG),
    }
}

/// A `permission_response` refusing an ask for a reason of the caller's — a
/// dialog that could not be shown, rather than one that was answered "no".
#[must_use]
pub fn refused(request_id: &str, reason: &str) -> PermissionResponse {
    PermissionResponse::error(request_id, reason)
}

/// What a lead's `permission_response` decides, in the engine's own words.
///
/// The error arm refuses. The success arm allows once, or always when it
/// carries an `addRules`/`allow` update — the SDK's spelling and the only one
/// read; anything else in the list is left as once, erring towards asking
/// again. A frame whose arms contradict its tag (`is_consistent` false — the
/// wire is somebody else's) refuses too, because a decoded document that
/// disagrees with itself is not a decision anybody made.
#[must_use]
pub fn reply_of(response: &PermissionResponse) -> PermissionReply {
    if !response.is_consistent() {
        return PermissionReply::Reject;
    }
    match response.subtype() {
        PermissionResponseSubtype::Error => PermissionReply::Reject,
        PermissionResponseSubtype::Success => {
            let always = response.response().is_some_and(|body| {
                body.permission_updates.iter().any(|update| {
                    update.get("type").and_then(Value::as_str) == Some(ADD_RULES)
                        && update.get("behavior").and_then(Value::as_str) == Some(ALLOW)
                })
            });
            if always {
                PermissionReply::Always
            } else {
                PermissionReply::Once
            }
        }
    }
}

/// The directories a call would work in outside the project, as the request's
/// suggestions carry them: one `addDirectories` update, or nothing at all
/// when there are none — the common case's frame stays exactly as small as
/// Claude's own constructor writes it.
fn directory_suggestions(directories: &[String]) -> Vec<Value> {
    if directories.is_empty() {
        return Vec::new();
    }

    vec![json!({
        "type": ADD_DIRECTORIES,
        "directories": directories,
        "destination": SESSION,
    })]
}

/// The other half: every directory any `addDirectories` suggestion names, in
/// order, so the lead's dialog can say what the answer really covers.
fn suggested_directories(suggestions: &[Value]) -> Vec<String> {
    suggestions
        .iter()
        .filter(|update| update.get("type").and_then(Value::as_str) == Some(ADD_DIRECTORIES))
        .filter_map(|update| update.get("directories").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ganja_protocol::team::{Frame, LeadFrame, PermissionResponse, PermissionResponseBody};
    use ganja_team::{
        LEAD, MailboxMessage, MemberName, MemberRecord, Spawn, Surface, TeamFile, TeamName,
        TeamsRoot, mailbox, record,
    };
    use serde_json::json;

    use super::{
        Asks, IGNORED_STALE_ANSWER, MemberPostbox, REFUSED_AT_DIALOG, Resolved, Unforwarded,
        ask_of, dialog_of, reply_of, response_of,
    };
    use crate::{
        protocol::{Event, PermissionId, PermissionReply, SessionId},
        tool::team::{Address, Body, Peer, Postbox as _, Reserved, Undelivered},
    };

    /// A teams root under a throwaway home, and the names both sides use.
    struct Team {
        _home: tempfile::TempDir,
        root: TeamsRoot,
        team: TeamName,
    }

    impl Team {
        fn new() -> Self {
            let home = tempfile::tempdir().expect("a temporary home");
            let root = TeamsRoot::new(home.path().join("teams"));

            Self {
                _home: home,
                root,
                team: TeamName::parse("session-abcd1234").expect("a team name"),
            }
        }

        /// Writes a team file naming the lead and `teammates`, the way a lead
        /// writes one.
        fn write_file(&self, teammates: &[&str]) {
            let mut file = TeamFile::new(&self.team, "lead-session", "/tmp", record::now_millis());
            for name in teammates {
                let member = MemberName::parse(name).expect("a member name");
                file.members.push(MemberRecord::teammate(
                    &member,
                    &self.team,
                    Spawn {
                        agent_type: "general".to_owned(),
                        model: "recorder-model".to_owned(),
                        prompt: "hold the fort".to_owned(),
                        cwd: "/tmp".to_owned(),
                        color: "blue".to_owned(),
                        plan_mode_required: false,
                        surface: Surface::Pane {
                            id: "%3".to_owned(),
                        },
                    },
                    record::now_millis(),
                ));
            }
            let path = self.root.config_path(&self.team);
            std::fs::create_dir_all(path.parent().expect("a team directory")).expect("mkdir");
            std::fs::write(&path, record::document(&file).expect("the file encodes"))
                .expect("the team file writes");
        }

        fn postbox(&self, name: &str) -> MemberPostbox {
            MemberPostbox::new(
                MemberName::parse(name).expect("a member name"),
                self.team.clone(),
                self.root.clone(),
            )
        }

        fn inbox(&self, name: &str) -> std::path::PathBuf {
            self.root
                .inbox_path(&self.team, &MemberName::parse(name).expect("a member name"))
        }

        fn held(&self, name: &str) -> Vec<MailboxMessage> {
            mailbox::read(&self.inbox(name))
                .map(|contents| contents.valid)
                .unwrap_or_default()
        }
    }

    /// The lead is addressable from the first millisecond of a pane's life,
    /// before the team file that will name the pane exists.
    #[tokio::test]
    async fn a_member_can_reach_its_lead_before_the_team_file_exists() {
        let team = Team::new();
        let postbox = team.postbox("worker");

        let roster = postbox.roster();
        assert_eq!(
            roster,
            [Peer {
                name: LEAD.to_owned(),
                description: Some(super::LEADS.to_owned()),
                lead: true,
            }],
            "the lead, and only the lead, with no file to read"
        );

        let sent = postbox
            .deliver(
                Address::Local("Team-Lead".to_owned()),
                Body::Text {
                    text: "the parser is done".to_owned(),
                    summary: Some("parser".to_owned()),
                },
            )
            .await
            .expect("the lead's inbox takes it");
        assert_eq!(sent.to, LEAD, "the canonical spelling comes back");

        let held = team.held(LEAD);
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].from, "worker");
        assert_eq!(held[0].text, "the parser is done");
        assert_eq!(held[0].summary.as_deref(), Some("parser"));
    }

    /// The roster is the file's, minus the reader, and a peer the file names
    /// is reachable while one it does not is unknown.
    #[tokio::test]
    async fn a_members_roster_is_the_team_file_without_itself() {
        let team = Team::new();
        team.write_file(&["worker", "reviewer"]);
        let postbox = team.postbox("worker");

        let names: Vec<String> = postbox.roster().into_iter().map(|peer| peer.name).collect();
        assert_eq!(names, [LEAD, "reviewer"], "the lead first, then the peers");
        assert_eq!(
            postbox.roster().iter().filter(|peer| peer.lead).count(),
            1,
            "exactly one lead"
        );

        postbox
            .deliver(
                Address::Local("reviewer".to_owned()),
                Body::Text {
                    text: "look at the parser".to_owned(),
                    summary: None,
                },
            )
            .await
            .expect("a named peer is reachable");
        assert_eq!(team.held("reviewer").len(), 1);

        assert_eq!(
            postbox
                .deliver(
                    Address::Local("nobody".to_owned()),
                    Body::Text {
                        text: "hello?".to_owned(),
                        summary: None,
                    },
                )
                .await,
            Err(Undelivered::Unknown),
            "a name the file does not hold is nobody"
        );
    }

    /// The sender is the postbox's, never the message's: a member built as
    /// `worker` writes `worker`, whatever the body claims about itself.
    #[tokio::test]
    async fn a_member_postbox_stamps_the_name_it_was_built_with() {
        let team = Team::new();
        let postbox = team.postbox("worker");

        postbox
            .deliver(
                Address::Local(LEAD.to_owned()),
                Body::Frame(json!({
                    "type": "shutdown_approved",
                    "requestId": "req-1",
                    "from": LEAD,
                    "timestamp": record::now_iso8601(),
                })),
            )
            .await
            .expect("the frame is written");

        let held = team.held(LEAD);
        assert_eq!(held[0].from, "worker", "the envelope says who wrote it");
    }

    #[test]
    fn a_member_postbox_classifies_with_the_protocols_own_lists() {
        let team = Team::new();
        let postbox = team.postbox("worker");

        assert_eq!(postbox.classify("just a message"), Reserved::No);
        assert_eq!(
            postbox.classify(r#"{"type":"shutdown_approved","requestId":"r1"}"#),
            Reserved::AgentSendable {
                kind: "shutdown_approved"
            }
        );
        assert_eq!(
            postbox.classify(r#"{"type":"idle_notification"}"#),
            Reserved::HarnessOnly {
                kind: "idle_notification"
            }
        );
    }

    #[tokio::test]
    async fn a_uds_address_is_validated_but_has_no_transport_yet() {
        let team = Team::new();
        let outcome = team
            .postbox("worker")
            .deliver(
                Address::Uds {
                    path: "/tmp/peer.sock".into(),
                },
                Body::Text {
                    text: "hello".to_owned(),
                    summary: None,
                },
            )
            .await;

        assert!(
            matches!(outcome, Err(Undelivered::NoTransport { .. })),
            "{outcome:?}"
        );
    }

    /// One dialog as the member's engine publishes it.
    fn ask(id: &str, directories: &[&str]) -> Event {
        Event::PermissionRequested {
            session_id: SessionId::from("member-session".to_owned()),
            id: PermissionId::from(id.to_owned()),
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            title: "rm -rf build".to_owned(),
            args: json!({"command": "rm -rf build"}),
            directories: directories.iter().map(|d| (*d).to_owned()).collect(),
        }
    }

    /// The two halves of the dialect agree: what a member writes is what the
    /// lead's dialog shows, directories included, and the answer comes back
    /// as the reply the person gave.
    #[test]
    fn the_dialect_round_trips_a_dialog_and_its_answer() {
        let request = ask("req-1", &["/srv/other"]);
        let frame = ask_of("worker@session-abcd1234", &request).expect("an ask");
        assert_eq!(frame.request_id, "req-1");
        assert_eq!(frame.agent_id, "worker@session-abcd1234");
        assert_eq!(frame.tool_name, "bash");
        assert_eq!(frame.tool_use_id, "call-1");
        assert_eq!(frame.description, "rm -rf build");
        assert_eq!(
            frame.permission_suggestions.len(),
            1,
            "one directory disclosure"
        );

        let dialog = dialog_of(SessionId::from("lead-session".to_owned()), frame);
        let Event::PermissionRequested {
            id,
            tool,
            title,
            args,
            directories,
            ..
        } = &dialog
        else {
            panic!("a dialog is a permission request");
        };
        assert_ne!(
            id.as_str(),
            "req-1",
            "the dialog's id is the lead's own mint, never the member's string"
        );
        assert_eq!(tool, "bash");
        assert_eq!(title, "rm -rf build");
        assert_eq!(args, &json!({"command": "rm -rf build"}));
        assert_eq!(directories, &["/srv/other"]);

        // No directories, no suggestion at all: the common frame stays small.
        let plain = ask_of("worker@session-abcd1234", &ask("req-2", &[])).expect("an ask");
        assert!(plain.permission_suggestions.is_empty());

        for reply in [
            PermissionReply::Once,
            PermissionReply::Always,
            PermissionReply::Reject,
        ] {
            let response = response_of("req-1", "bash", &json!({"command": "rm -rf build"}), reply);
            assert!(response.is_consistent());
            assert_eq!(reply_of(&response), reply, "{reply:?} survives the frame");
        }
        assert_eq!(
            response_of("req-1", "bash", &json!({}), PermissionReply::Reject).error_message(),
            Some(REFUSED_AT_DIALOG)
        );
    }

    /// An update this build does not recognise is read as once, and a frame
    /// whose arms contradict its tag is a refusal.
    #[test]
    fn an_unknown_update_is_once_and_an_inconsistent_frame_refuses() {
        let response = PermissionResponse::success(
            "req-1",
            PermissionResponseBody {
                updated_input: json!({}),
                permission_updates: vec![json!({"type": "setMode", "mode": "acceptEdits"})],
            },
        );
        assert_eq!(reply_of(&response), PermissionReply::Once);

        let crossed: PermissionResponse = serde_json::from_value(json!({
            "request_id": "req-1",
            "subtype": "success",
            "error": "but also no",
        }))
        .expect("the wire decodes it");
        assert!(!crossed.is_consistent());
        assert_eq!(reply_of(&crossed), PermissionReply::Reject);
    }

    fn asks(team: &Team) -> Asks {
        Asks::new(
            MemberName::parse("worker").expect("a member name"),
            &team.team,
            &team.root,
        )
    }

    fn lead_answers(response: PermissionResponse) -> LeadFrame {
        LeadFrame::parse(LEAD, LEAD, Frame::PermissionResponse(response)).expect("the lead's")
    }

    /// A forwarded ask is written to the lead as this member, and the lead's
    /// answer to it comes back as the reply — once.
    #[tokio::test]
    async fn a_forwarded_ask_reaches_the_lead_and_its_answer_comes_back_once() {
        let team = Team::new();
        let asks = asks(&team);

        asks.forward(&ask("req-1", &[]))
            .await
            .expect("the lead's inbox takes it");
        assert_eq!(asks.waiting(), 1);

        let held = team.held(LEAD);
        assert_eq!(held.len(), 1);
        assert_eq!(
            held[0].from, "worker",
            "asked as the member, by construction"
        );
        let Some(Frame::PermissionRequest(request)) = held[0].frame() else {
            panic!("the lead was handed something other than a permission request");
        };
        assert_eq!(request.request_id, "req-1");
        assert_eq!(request.agent_id, "worker@session-abcd1234");

        let resolved = asks.resolve(lead_answers(response_of(
            "req-1",
            "bash",
            &json!({}),
            PermissionReply::Always,
        )));
        assert_eq!(
            resolved,
            Resolved::Answered {
                id: PermissionId::from("req-1".to_owned()),
                reply: PermissionReply::Always,
            }
        );
        assert_eq!(asks.waiting(), 0, "answered means no longer waiting");

        // The same answer again is stale, and says so.
        let logged = Capture::default();
        let again = {
            let _guard = tracing::dispatcher::set_default(&logged.subscriber());
            asks.resolve(lead_answers(response_of(
                "req-1",
                "bash",
                &json!({}),
                PermissionReply::Always,
            )))
        };
        assert_eq!(
            again,
            Resolved::Stale {
                request_id: "req-1".to_owned()
            }
        );
        assert!(
            logged.text().contains(IGNORED_STALE_ANSWER),
            "the ignoring is not silent: {}",
            logged.text()
        );
    }

    /// A cancelled turn ends its dialog wait with a `PermissionReplied`, and
    /// retiring on it is what makes the lead's later answer stale rather than
    /// applied to a call that no longer exists.
    #[tokio::test]
    async fn a_retired_ask_makes_a_late_answer_stale() {
        let team = Team::new();
        let asks = asks(&team);
        asks.forward(&ask("req-1", &[])).await.expect("forwarded");

        assert!(asks.retire(&PermissionId::from("req-1".to_owned())));
        assert!(
            !asks.retire(&PermissionId::from("req-1".to_owned())),
            "retiring twice is a miss, not an error"
        );
        assert_eq!(asks.waiting(), 0);
        assert!(matches!(
            asks.resolve(lead_answers(response_of(
                "req-1",
                "bash",
                &json!({}),
                PermissionReply::Once
            ))),
            Resolved::Stale { .. }
        ));
    }

    /// A lead frame that is not an answer is handed straight back, so the
    /// caller's other handlers see it; and an event that is not an ask is
    /// refused rather than written.
    #[tokio::test]
    async fn only_answers_are_read_and_only_asks_are_forwarded() {
        let team = Team::new();
        let asks = asks(&team);

        let shutdown = LeadFrame::parse(
            LEAD,
            LEAD,
            Frame::ShutdownRequest(ganja_protocol::team::ShutdownRequest {
                request_id: "req-9".to_owned(),
                from: LEAD.to_owned(),
                reason: None,
                timestamp: record::now_iso8601(),
            }),
        )
        .expect("the lead's");
        assert_eq!(
            asks.resolve(shutdown),
            Resolved::NotAnAnswer {
                kind: "shutdown_request"
            }
        );

        let not_an_ask = Event::QuestionRejected {
            session_id: SessionId::from("member-session".to_owned()),
            id: crate::protocol::QuestionId::from("q-1".to_owned()),
        };
        assert_eq!(asks.forward(&not_an_ask).await, Err(Unforwarded::NotAnAsk));
        assert!(team.held(LEAD).is_empty(), "nothing was written");
    }

    /// A `tracing` subscriber a test can read back — the fixture
    /// `tests/teammate_frames.rs` keeps, for one synchronous call here.
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("the log is never poisoned")).into_owned()
        }

        fn subscriber(&self) -> tracing::Dispatch {
            tracing::Dispatch::new(
                tracing_subscriber::fmt()
                    .with_writer(self.clone())
                    .with_max_level(tracing::Level::TRACE)
                    .with_ansi(false)
                    .finish(),
            )
        }
    }

    impl std::io::Write for Capture {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the log is never poisoned")
                .extend_from_slice(buffer);

            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
}
