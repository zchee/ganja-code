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
//! addressable**, whether or not the file names it yet. The launch line is
//! typed only once the record is on disk ([`crate::teammate::pane`]), so an
//! absent file is defence in depth rather than the ordinary path — the lead's
//! inbox path is derivable from the team name alone, and a teammate that
//! could not reach its lead until the file was readable would be a teammate
//! that cannot answer the shutdown it was just asked for.
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
    teammate::postbox::{self, LEADS, NO_SOCKET},
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
                    // The backend word here is Claude's own (`tmux`,
                    // `in-process`), because a TeamFile carries only Claude's
                    // vocabulary — where the lead's roster speaks ganja's.
                    description: Some(postbox::peer_description(member.surface().backend_type())),
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
        postbox::classify_reserved(text)
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

        postbox::write_to_peer(
            self.sender.as_str(),
            &self.root,
            &self.team,
            &recipient,
            body,
        )
        .await
        // The minted identity is the admission gate's key (M6), and a member
        // gates nothing: only the lead's socket door records it.
        .map(|(sent, _)| sent)
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
        let ask = ask_of(&self.agent_id, request).ok_or(Unforwarded::NotAnAsk)?;
        // The ask carries the dialog's own id and tool, so nothing re-matches
        // the event: `ask_of` is the one reader of its shape.
        let id = PermissionId::from(ask.request_id.clone());
        let tool = ask.tool_name.clone();
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
        let written = crate::teammate::blocking_io(move || {
            mailbox::write_bounded(&path, message, Some(postbox::INBOX_CEILING))
        })
        .await;
        if let Err(reason) = written {
            self.pending
                .lock()
                .expect("the asks are never poisoned")
                .remove(&id);
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
#[path = "member_tests.rs"]
mod tests;
