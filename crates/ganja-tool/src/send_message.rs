//! The `send_message` tool: one teammate says something to another.
//!
//! Spec: Claude Code's `SendMessage` — §5.2's validation ladder, in its order,
//! over §5.1's two reserved sets and §5.6's address forms. Upstream opencode
//! has no teammates at all, so there is nothing of its to port and every
//! sentence below is ganja's own.
//!
//! # It runs unasked (**D498**)
//!
//! `send_message` is deliberately **not** in
//! [`ASK_BY_DEFAULT`](ganja_permission::permission::ASK_BY_DEFAULT). Sending a
//! message to a named teammate is conversation, not authority: what changes
//! is one entry in a mailbox this user already owns, whatever the recipient
//! goes on to *do* is gated by the recipient's own rules, and the tool is
//! offered at all only where a team already exists. The permission that
//! matters was answered at **spawn** — whether this session may run a
//! teammate, and under which posture — and raising a dialog per message would
//! train a person to click through the one dialog that does carry a decision.
//!
//! The premise has to hold across a socket too, and since D505 it is held
//! there by rung 3 rather than assumed: a `uds:` address may name **only a
//! session socket of this user's** — [`socket::vet_address`]'s clauses, the
//! binder's own discipline turned toward the address — so what an unasked
//! call can reach is the lead of **any session of this user's, in any
//! project on this machine** (not only this team's members), over a
//! transport only this build's binder makes, and never `/var/run/`'s or
//! anybody else's listener. What crosses is plain text into a same-uid
//! lead's inbox; what comes back is bounded and read as typed answers by the
//! deliverer; how *much* may be sent is bounded by nothing yet — no rate, no
//! inbox ceiling, no batch cap (bead `ganja-code-qfk`). That, and not
//! "nothing leaves the session", is what keeps the tool conversation rather
//! than authority.
//!
//! # The ladder refuses in ganja's own words (**D497**)
//!
//! Every refusal is a `Refused` kind rendered through a declared constant.
//! The *kind* and the *order* are the contract the tests pin; the prose is
//! ganja's own and free to improve without breaking them. No Claude Code
//! sentence is copied — this tree copies only MIT text, with attribution — so
//! the information each refusal carries is the reference's and none of the
//! bytes are.
//!
//! A refusal is **information**, not control flow: it comes back as a failed
//! call whose message the model reads and acts on next, exactly as an unknown
//! agent type does in [`task`](crate::task). Nothing here ends a turn.
//!
//! # Rung 8's clauses (**D499**)
//!
//! §5.2's last rung reads "`shutdown_response` must be addressed to the lead",
//! but §5's own frame table has no `shutdown_response`: it has
//! `shutdown_request`, `shutdown_approved` and `shutdown_rejected`, and §5.1
//! puts `shutdown_approved` among the ten an agent may send while
//! `shutdown_rejected` is one of the five only the harness originates. Ganja
//! reads the rung as being about **`shutdown_approved`** and decides it in
//! three clauses:
//!
//! - a structured `shutdown_approved` must be addressed to the lead, because
//!   it answers the lead's own request and nobody else's;
//! - a structured frame from the harness-only five is refused **whoever it is
//!   addressed to** — §5.1's "no structured escape hatch" is a fact about the
//!   frame, not about the path it took, and reading it as only a plain-text
//!   rule would leave the object form as exactly the hatch §5.1 denies;
//! - a structured body that is no frame at all is refused too. That clause is
//!   ganja's own: the object form exists to answer a plan or a shutdown
//!   request, and a JSON blob that answers neither is prose the model should
//!   have sent as prose. The reference validates its structured path against
//!   the frame schemas, so its equivalent failure is a schema error rather
//!   than a rung.
//!
//! # Rung 4 judges names only
//!
//! §5.2's rung 4 refuses an `@` in `to`, and ganja applies it on the bare-name
//! branch alone: the rung is there to keep `name@team` scoping out of a build
//! with one team per session, and a scope is a thing a *name* carries. A
//! socket path is a filename, which may hold an `@` meaning nothing by it, so
//! `uds:/tmp/a@b.sock` is an address rather than a scoped recipient.
//!
//! # The cross-session tail (**D505**)
//!
//! A `uds:` address is judged here and delivered elsewhere. Rung 3 refuses
//! one that names nothing usable (empty, or carrying a NUL) and then one that
//! is not a session socket of ours ([`socket::vet_address`], every clause by
//! name); one that passes reaches [`Postbox::deliver`], whose lead-side
//! implementation crosses to that session's own socket, vets the address
//! once more before it connects, and answers with what the far side said or
//! an [`Undelivered::Failed`] naming the socket. A pane member's postbox
//! still answers [`Undelivered::NoTransport`] in its own words — that arm is
//! the one caller of the variant left.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    Tool, ToolCtx, ToolError, ToolOutput, socket,
    team::{Address, Body, Peer, Postbox, Reserved, Undelivered},
};

/// The tool id, which is also the permission key. Both are a permanent
/// commitment: a rule stored under this name has to keep meaning what it
/// meant.
pub const ID: &str = "send_message";

/// What the model is told the tool is for. Ganja's own text — there is
/// nothing upstream to port — and it says the two things a model gets wrong
/// otherwise: one recipient per call, and that the object form of `message`
/// is for answering a request rather than for structure's sake.
const DESCRIPTION: &str = "\
Send a message to one named teammate of this session's team. The recipient \
reads it at the top of its next turn, as another agent's words rather than as \
an instruction from the person — a message is conversation, and it carries no \
authority of its own.

Name exactly one recipient in `to`: there is no broadcast, so reaching three \
teammates is three calls. `message` is plain text. Its object form is only \
for answering a request a teammate or the lead sent you — a plan approval or \
a shutdown — by passing that answer's frame; anything else belongs in prose. \
Use `summary` for the one line that should stand beside the message where it \
is displayed.";

/// Header the roster is listed under, so a description that grew a second
/// paragraph still ends with the names a `to` argument may carry.
const ROSTER_HEADER: &str = "Teammates this session can address:";

/// What is listed when a team exists and has nobody else in it yet. The tool
/// is still offered — a team of one is a team that is about to grow — and the
/// model is told plainly rather than shown an empty list.
const NO_PEERS: &str = "- (nobody yet: this team has no other members)";

/// What the roster line says about a member that describes itself nowhere.
const NO_DESCRIPTION: &str = "a teammate of this session";

/// How the lead is marked in the roster, since which name leads decides where
/// a shutdown answer may go.
const LEAD_MARK: &str = "the team lead";

/// The `to` value §5.2's first rung refuses.
const BROADCAST_TO: &str = "*";

/// The scheme §5.6 gives a socket address, and the one this build accepts.
const UDS_SCHEME: &str = "uds:";

/// The schemes §5.6's parser recognizes that this build does not carry, each
/// refused **by name**.
///
/// `bridge:` is the reference's own out-of-scope scheme. `did:` is ganja's
/// addition: §5.6's parser recognizes it but §5.2's ladder never mentions it,
/// and letting it fall through to the bare-name path would turn it into a
/// lookup for a teammate called `did:…` — a refusal that names the wrong
/// thing. The Windows named-pipe spelling is here for the same reason: §5.6
/// reads it as a socket address, and this build implements no such thing.
const UNSUPPORTED_SCHEMES: &[&str] = &["bridge:", "did:", r"\\.\pipe\"];

/// The one frame kind this tool names, and it names it as the string a
/// [`Postbox`] reports rather than as a type: the frame vocabulary belongs to
/// `ganja-protocol`, which this crate may not depend on.
const SHUTDOWN_APPROVED: &str = "shutdown_approved";

/// §5.3's `hWp`: the most of a summary an envelope ever shows.
///
/// Capped here as well as where the envelope is rendered. Two caps for one
/// limit is not redundancy — it means no path can hand an unbounded
/// model-authored string across the seam in the first place, whatever the far
/// side later does with it.
///
/// Public because it is the near half of a number written twice: the far half
/// is `ganja_protocol::team::DISPLAY_FIELD_CAP`, which this crate may not name
/// — the internal-dependency allowlist is exactly `ganja-permission` — so the
/// two cannot share a definition. `ganja-core` sees both and owes the one-line
/// equality pin that keeps them from drifting apart in silence.
pub const SUMMARY_CAP: usize = 200;

/// What a delivered message reads back as, ahead of the deliverer's own
/// account of what became of it.
const DELIVERED: &str = "Message sent to";

/// Rung zero: the tool was offered without a team behind it. Not reachable
/// through the engine, which registers it only where a team exists.
const NO_TEAM: &str = "This session has no team to send a message to.";

/// Rung 1: broadcast.
const BROADCAST: &str = "There is no broadcast here: a call carries one recipient, named, so reaching three teammates is three calls.";

/// Rung 2: a scheme this build carries no transport for.
const UNSUPPORTED_SCHEME: &str = "A teammate is addressed by its bare name, and another session by a uds: socket path. There is no transport here for the scheme";

/// Rung 3: a socket address that names no socket.
const INVALID_SOCKET_PATH: &str =
    "A uds: address must name the socket to reach, and this one names nothing usable:";

/// Rung 3: a socket address that is not a session socket of ours — the
/// clause it failed follows.
const NOT_A_SESSION_SOCKET: &str = "A uds: address names another ganja session's socket — a session-named socket of this user's, in a private socket directory of this user's — and this one does not:";

/// Rung 4: a scoped recipient.
const SCOPED_RECIPIENT: &str = "There is one team per session, so a recipient is a bare name rather than a name scoped to somewhere:";

/// Rung 5: a message with nothing in it.
const WHITESPACE: &str = "A message needs something in it; this one is blank.";

/// Rung 6: the structured form across a socket.
const STRUCTURED_OVER_SOCKET: &str = "A protocol frame does not cross a socket: a uds: recipient takes plain text. Send prose, or address a member of this team by name.";

/// Rung 7, for §5.1's ten: plain text that reads as a frame an agent may send,
/// which therefore names the structured door.
const PROTOCOL_FRAME: &str = "Plain text may not be a teammate protocol frame. To answer a plan or a shutdown request, pass that answer as the object form of `message`; anything else should be prose. This text reads as the frame";

/// Rung 7, for §5.1's five: plain text that reads as a frame nobody but the
/// harness originates, which therefore names no door at all.
const LIFECYCLE_FRAME: &str = "Plain text may not be a teammate lifecycle frame; those are the harness's to send, and there is no form of this call that sends one. Send prose instead. This text reads as the frame";

/// Rung 8: an object that is no frame.
const STRUCTURED_NOT_A_FRAME: &str = "The object form of `message` carries a protocol frame answering a plan or a shutdown request, and this object is none. Send what you meant as plain text.";

/// Rung 8: a frame only the harness originates, whatever it is addressed to.
const HARNESS_ONLY_FRAME: &str = "This frame is the harness's to originate, whoever it is addressed to, so the object form does not carry it either. The frame is";

/// Rung 8: the shutdown answer, addressed to somebody other than the lead.
const SHUTDOWN_APPROVED_NOT_TO_LEAD: &str = "A shutdown_approved answers the lead's own request, so the lead is the only address it may carry.";

/// What the model reads when there is a message but nobody by that name.
const UNKNOWN_RECIPIENT: &str = "Nobody in this team goes by that name:";

/// Why a call did not send anything, as one of the ladder's rungs.
///
/// The kind is the contract and the sentence is not: a test asserts *which*
/// rung refused and in which order the rungs bite, so the prose above can be
/// improved without rewriting the tests that guard the behavior. Payloads are
/// `&'static str` only — the scheme refused, the frame recognized — so that a
/// caller comparing two of these compares kinds rather than assembled text;
/// the parts that vary per call (the address, the lead's name) are arguments
/// to [`Refused::sentence`] instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// The tool ran without a [`Postbox`] behind it.
    NoTeam,
    /// Rung 1: `to` was `*`.
    Broadcast,
    /// Rung 2: `to` carried a scheme this build has no transport for.
    UnsupportedScheme {
        /// The scheme, as it is spelled in a `to` argument.
        scheme: &'static str,
    },
    /// Rung 3: a `uds:` address whose path is empty or unusable.
    InvalidSocketPath,
    /// Rung 3: a `uds:` address that names something other than a session
    /// socket of ours, by the first clause it failed (**D505**).
    NotASessionSocket {
        /// Which clause of the address gate refused it.
        why: socket::AddressRefusal,
    },
    /// Rung 4: `to` was scoped with `@` rather than a bare name.
    ScopedRecipient,
    /// Rung 5: the message was whitespace.
    Whitespace,
    /// Rung 6: the structured form, addressed to a socket.
    StructuredOverSocket,
    /// Rung 7: plain text parsing to one of §5.1's ten agent-sendable frames.
    ProtocolFrame {
        /// The frame the text read as.
        kind: &'static str,
    },
    /// Rung 7: plain text parsing to one of §5.1's five harness-only frames.
    LifecycleFrame {
        /// The frame the text read as.
        kind: &'static str,
    },
    /// Rung 8: the structured form carrying something that is no frame.
    StructuredNotAFrame,
    /// Rung 8: a harness-only frame in the structured form, refused whoever it
    /// was addressed to.
    HarnessOnlyFrame {
        /// The frame the object read as.
        kind: &'static str,
    },
    /// Rung 8: `shutdown_approved` addressed to somebody other than the lead.
    ShutdownApprovedNotToLead,
}

impl Refused {
    /// What the model reads, and acts on next.
    ///
    /// `to` is the address as the model wrote it, and `lead` the team's lead
    /// where the roster names one — both are per-call facts, which is why they
    /// arrive here rather than sitting in the variant.
    #[must_use]
    fn sentence(self, to: &str, lead: Option<&str>) -> String {
        match self {
            Self::NoTeam => NO_TEAM.to_owned(),
            Self::Broadcast => BROADCAST.to_owned(),
            Self::UnsupportedScheme { scheme } => format!("{UNSUPPORTED_SCHEME} {scheme}"),
            Self::InvalidSocketPath => format!("{INVALID_SOCKET_PATH} {to:?}"),
            Self::NotASessionSocket { why } => format!("{NOT_A_SESSION_SOCKET} {to:?}: {why}."),
            Self::ScopedRecipient => format!("{SCOPED_RECIPIENT} {to:?}"),
            Self::Whitespace => WHITESPACE.to_owned(),
            Self::StructuredOverSocket => STRUCTURED_OVER_SOCKET.to_owned(),
            Self::ProtocolFrame { kind } => format!("{PROTOCOL_FRAME} {kind}."),
            Self::LifecycleFrame { kind } => format!("{LIFECYCLE_FRAME} {kind}."),
            Self::StructuredNotAFrame => STRUCTURED_NOT_A_FRAME.to_owned(),
            Self::HarnessOnlyFrame { kind } => format!("{HARNESS_ONLY_FRAME} {kind}."),
            Self::ShutdownApprovedNotToLead => lead.map_or_else(
                || SHUTDOWN_APPROVED_NOT_TO_LEAD.to_owned(),
                |lead| format!("{SHUTDOWN_APPROVED_NOT_TO_LEAD} This team's lead is {lead}."),
            ),
        }
    }
}

/// What the model passes as `message`.
///
/// Untagged, which is the reference's own shape (`{"message": {"type": …}}`
/// beside a bare string): the ordinary case stays one string, and the door for
/// answering a request is an object rather than a second argument nobody
/// would find. An object is taken as a `Map` rather than as a free
/// [`serde_json::Value`] so that a number or a list is a schema error the
/// model can see, instead of a body the ladder has to refuse a second way.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum Message {
    /// Plain text, which is what a message to a teammate normally is
    Text(String),
    /// A protocol frame answering a plan or shutdown request
    Frame(serde_json::Map<String, serde_json::Value>),
}

/// What the model passes to `send_message`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The teammate to send to, by the bare name it is listed under
    to: String,
    /// What to send: plain text, or the frame object that answers a request
    message: Message,
    /// One line naming what this message is about, shown beside it
    #[serde(default)]
    summary: Option<String>,
}

/// Sends a message to a teammate.
pub struct SendMessageTool {
    /// The description plus the roster of teammates *this* caller may
    /// address. Rendered once, the way [`crate::task::TaskTool`]'s is: the
    /// engine rebuilds the registry when the team changes, so a tool that
    /// re-rendered per call would be answering a question nobody asked.
    description: String,
}

impl SendMessageTool {
    /// Builds the tool as one caller sees it: the description, then every
    /// teammate in `roster`.
    #[must_use]
    pub fn new(roster: &[Peer]) -> Self {
        Self {
            description: describe(roster),
        }
    }
}

/// The description, the roster header, and one line per teammate, sorted by
/// name so the order is the roster's own business rather than the team's
/// arrival order.
fn describe(roster: &[Peer]) -> String {
    let mut listed: Vec<&Peer> = roster.iter().collect();
    listed.sort_by(|left, right| left.name.cmp(&right.name));

    let lines: String = if listed.is_empty() {
        format!("\n{NO_PEERS}")
    } else {
        listed
            .iter()
            .map(|peer| {
                let about = peer.description.as_deref().unwrap_or(NO_DESCRIPTION);
                if peer.lead {
                    format!("\n- {}: {about} ({LEAD_MARK})", peer.name)
                } else {
                    format!("\n- {}: {about}", peer.name)
                }
            })
            .collect()
    };

    format!("{DESCRIPTION}\n\n{ROSTER_HEADER}{lines}")
}

/// The lead's name, where the roster names one.
fn lead_of(roster: &[Peer]) -> Option<String> {
    roster
        .iter()
        .find(|peer| peer.lead)
        .map(|peer| peer.name.clone())
}

/// §5.2's rungs 2 to 4: which address form `to` is, or which refusal it earns.
///
/// The three rungs are one function because their order is the whole point:
/// a scheme is recognized before its payload is judged, and both before the
/// bare-name rule, so `did:a@b` is refused for the scheme it names rather than
/// for the `@` it happens to contain.
fn parse_address(to: &str) -> Result<Address, Refused> {
    // Rung 2, and §5.6's two spellings of one thing: the scheme, and the bare
    // leading slash its own parser reads as the same address.
    let socket_path = to
        .strip_prefix(UDS_SCHEME)
        .or_else(|| to.starts_with('/').then_some(to));
    if let Some(path) = socket_path {
        // Rung 3. A NUL cannot travel in a socket path, and an empty one
        // names nothing at all; both are the model's mistake to see rather
        // than a connection to fail on later.
        if path.is_empty() || path.contains('\0') {
            return Err(Refused::InvalidSocketPath);
        }
        let path = std::path::PathBuf::from(path);
        // Rung 3 still, and the clause that makes the tool safe to run
        // unasked (D498, D505): only a session socket of ours is an address.
        // Inspected here, before the body is judged, so a call aimed at some
        // other listener on this machine is refused before anything is
        // composed for it.
        session_socket(&path)?;
        return Ok(Address::Uds { path });
    }
    if let Some(scheme) = UNSUPPORTED_SCHEMES
        .iter()
        .find(|scheme| to.starts_with(*scheme))
    {
        return Err(Refused::UnsupportedScheme { scheme });
    }

    // Rung 4, on the bare-name branch alone: the rule is about names — there
    // is one team per session, so a name needs no scope — and a socket path
    // that happens to hold an `@` is not a scoped address.
    if to.contains('@') {
        return Err(Refused::ScopedRecipient);
    }

    Ok(Address::Local(to.to_owned()))
}

/// Rung 3's second clause: `path` is a session socket of ours, or the
/// clause it fails. A build without Unix sockets has no such socket to name
/// and refuses every one.
fn session_socket(path: &std::path::Path) -> Result<(), Refused> {
    #[cfg(unix)]
    {
        socket::vet_address(path).map_err(|why| Refused::NotASessionSocket { why })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(Refused::InvalidSocketPath)
    }
}

/// §5.2's ladder, in its order: what this call may send, or the first rung it
/// fails.
///
/// Rungs 5 and 7 judge plain text and rungs 6 and 8 judge the structured form,
/// so the two branches never race each other — what the order buys is that
/// *within* a branch the earlier rung always answers first, and that the
/// address is settled before the body is judged at all. `lead` is the roster's
/// lead where it names one, read once by the caller for rung 8 and for the
/// sentence a refusal is rendered with.
fn validate(
    args: Args,
    postbox: &dyn Postbox,
    lead: Option<&str>,
) -> Result<(Address, Body), Refused> {
    // Rung 1.
    if args.to == BROADCAST_TO {
        return Err(Refused::Broadcast);
    }

    // Rungs 2 to 4.
    let address = parse_address(&args.to)?;

    let body = match args.message {
        Message::Text(text) => {
            // Rung 5.
            if text.trim().is_empty() {
                return Err(Refused::Whitespace);
            }
            // Rung 7. Prose crosses a socket exactly as it crosses a team, so
            // rung 6 has nothing to say on this branch.
            match postbox.classify(&text) {
                Reserved::No => {}
                Reserved::AgentSendable { kind } => return Err(Refused::ProtocolFrame { kind }),
                Reserved::HarnessOnly { kind } => return Err(Refused::LifecycleFrame { kind }),
            }
            Body::Text {
                text,
                summary: cap_summary(args.summary),
            }
        }
        Message::Frame(frame) => {
            // Rung 6, transferred from the scheme the reference applies it to:
            // it drops `bridge` and keeps `uds`, so the rule that structure
            // does not cross a session follows the scheme that was kept — and
            // what passes it is a member's name, which rung 8 reads.
            let name = match &address {
                Address::Uds { .. } => return Err(Refused::StructuredOverSocket),
                Address::Local(name) => name,
            };

            // Rung 8. Classified through the same seam the plain-text branch
            // uses, so one `Frame` parse on the far side answers both and no
            // list of frame names is kept here.
            let document = serde_json::Value::Object(frame);
            let kind = match postbox.classify(&document.to_string()) {
                Reserved::No => return Err(Refused::StructuredNotAFrame),
                // D499's second clause, decided before the recipient is even
                // looked at: this is what "regardless of recipient" means.
                Reserved::HarnessOnly { kind } => {
                    return Err(Refused::HarnessOnlyFrame { kind });
                }
                Reserved::AgentSendable { kind } => kind,
            };
            // D499's first clause. Compared case-insensitively: a name
            // differing only in case is not another teammate — the name
            // grammar admits both spellings of one identity — and the
            // reference's own peer-DM check compares the lead's name the same
            // way.
            if kind == SHUTDOWN_APPROVED
                && !lead.is_some_and(|lead| lead.eq_ignore_ascii_case(name))
            {
                return Err(Refused::ShutdownApprovedNotToLead);
            }

            Body::Frame(document)
        }
    };

    Ok((address, body))
}

/// §5.3's cap, applied before the summary crosses the seam. A summary that is
/// only whitespace is no summary, and is dropped rather than carried as an
/// empty attribute.
fn cap_summary(summary: Option<String>) -> Option<String> {
    let summary = summary?;
    if summary.trim().is_empty() {
        return None;
    }
    if summary.chars().count() <= SUMMARY_CAP {
        return Some(summary);
    }

    Some(summary.chars().take(SUMMARY_CAP).collect())
}

#[async_trait]
impl Tool for SendMessageTool {
    fn id(&self) -> &str {
        ID
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let to = args
            .get("to")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        format!("message to {to}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let Some(postbox) = ctx.postbox.as_ref() else {
            return Err(ToolError::Failed(Refused::NoTeam.sentence(&args.to, None)));
        };

        let to = args.to.clone();
        // Read now rather than at construction: a teammate spawned since the
        // registry was built is addressable this turn, and a stale lead name
        // would refuse a shutdown answer that is in fact correctly addressed.
        let lead = lead_of(&postbox.roster());
        let (address, body) = match validate(args, postbox.as_ref(), lead.as_deref()) {
            Ok(sending) => sending,
            Err(refused) => {
                return Err(ToolError::Failed(refused.sentence(&to, lead.as_deref())));
            }
        };
        let structured = matches!(body, Body::Frame(_));

        match postbox.deliver(address, body).await {
            Ok(sent) => Ok(ToolOutput {
                title: format!("message to {}", sent.to),
                output: format!("{DELIVERED} {}. {}", sent.to, sent.note)
                    .trim_end()
                    .to_owned(),
                metadata: serde_json::json!({
                    "to": sent.to,
                    "structured": structured,
                }),
            }),
            Err(Undelivered::Unknown) => {
                Err(ToolError::Failed(format!("{UNKNOWN_RECIPIENT} {to:?}")))
            }
            // The deliverer's own sentence, passed through: what is missing or
            // what broke is its fact, and a wrapper here would only be a
            // second voice saying less.
            Err(Undelivered::NoTransport { reason } | Undelivered::Failed { reason }) => {
                Err(ToolError::Failed(reason))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::json;

    use super::{
        BROADCAST, DELIVERED, DESCRIPTION, HARNESS_ONLY_FRAME, INVALID_SOCKET_PATH, LEAD_MARK,
        LIFECYCLE_FRAME, NO_TEAM, NOT_A_SESSION_SOCKET, PROTOCOL_FRAME, ROSTER_HEADER, Refused,
        SCOPED_RECIPIENT, SHUTDOWN_APPROVED_NOT_TO_LEAD, STRUCTURED_NOT_A_FRAME,
        STRUCTURED_OVER_SOCKET, SendMessageTool, UNKNOWN_RECIPIENT, UNSUPPORTED_SCHEME, WHITESPACE,
        cap_summary, lead_of,
    };
    use crate::{
        Tool as _, ToolCtx, ToolError,
        socket::{AddressRefusal, SessionSocket},
        team::{Address, Body, Peer, Postbox, Reserved, Sent, Undelivered},
    };

    /// A handful of §5.1's ten, which is all any test here needs: the real
    /// answer is one `Frame::is_agent_sendable` call on the engine's side.
    const SENDABLE: &[&str] = &[
        "shutdown_approved",
        "shutdown_request",
        "plan_approval_response",
        "mode_set_request",
    ];

    /// A handful of §5.1's five.
    const HARNESS_ONLY: &[&str] = &[
        "shutdown_rejected",
        "idle_notification",
        "task_completed",
        "teammate_terminated",
    ];

    /// A postbox that classifies by a small table and reports whatever outcome
    /// the test handed it, recording what reached it.
    #[derive(Debug)]
    struct Fake {
        roster: Vec<Peer>,
        outcome: Result<Sent, Undelivered>,
        delivered: Mutex<Vec<(Address, Body)>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                roster: vec![
                    Peer {
                        name: "team-lead".to_owned(),
                        description: Some("runs the team".to_owned()),
                        lead: true,
                    },
                    Peer {
                        name: "worker-1".to_owned(),
                        description: None,
                        lead: false,
                    },
                ],
                outcome: Ok(Sent {
                    to: "worker-1".to_owned(),
                    note: "It reads the message at the top of its next turn.".to_owned(),
                }),
                delivered: Mutex::new(Vec::new()),
            }
        }

        fn answering(outcome: Result<Sent, Undelivered>) -> Self {
            Self {
                outcome,
                ..Self::new()
            }
        }
    }

    #[async_trait]
    impl Postbox for Fake {
        fn classify(&self, text: &str) -> Reserved {
            let Ok(document) = serde_json::from_str::<serde_json::Value>(text) else {
                return Reserved::No;
            };
            let Some(kind) = document.get("type").and_then(serde_json::Value::as_str) else {
                return Reserved::No;
            };
            if let Some(kind) = SENDABLE.iter().find(|known| **known == kind) {
                return Reserved::AgentSendable { kind };
            }
            if let Some(kind) = HARNESS_ONLY.iter().find(|known| **known == kind) {
                return Reserved::HarnessOnly { kind };
            }

            Reserved::No
        }

        async fn deliver(&self, to: Address, body: Body) -> Result<Sent, Undelivered> {
            self.delivered
                .lock()
                .expect("no test panics while holding this")
                .push((to, body));

            self.outcome.clone()
        }

        fn roster(&self) -> Vec<Peer> {
            self.roster.clone()
        }
    }

    fn ctx(postbox: Option<Arc<dyn Postbox>>) -> ToolCtx {
        let mut ctx = ToolCtx::fixture(std::env::temp_dir());
        ctx.postbox = postbox;
        ctx
    }

    /// Runs one call against `postbox` and reports what the model would read.
    async fn refusal(postbox: &Arc<Fake>, args: serde_json::Value) -> String {
        let tool = SendMessageTool::new(&postbox.roster());
        let ctx = ctx(Some(Arc::clone(postbox) as Arc<dyn Postbox>));
        match tool.run(args, &ctx).await {
            Err(ToolError::Failed(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The whole point of the ladder is its order, so the cases here are of
    /// two kinds and each is labelled as the one it is. Five fail **more than
    /// one** rung and assert that the earlier one answers — those are the
    /// order. The other six reach a rung nothing else is racing them for and
    /// assert only that it classifies what it was handed, because two rungs
    /// one argument cannot fail at once have no order to claim.
    #[tokio::test]
    async fn the_validation_ladder_refuses_in_order() {
        let postbox = Fake::new();
        let frame = json!({"type": "shutdown_approved", "requestId": "r1"});

        // 1 before 8: a broadcast is refused before its structured body is
        // ever looked at.
        assert_eq!(
            validate(&postbox, "*", json!(frame.clone()), None),
            Err(Refused::Broadcast)
        );
        // 2 before 4: the scheme names the refusal, not the `@` it carries.
        assert_eq!(
            validate(&postbox, "bridge:host@box", json!("hello"), None),
            Err(Refused::UnsupportedScheme { scheme: "bridge:" })
        );
        // 3 before 5: the address is settled before the body is judged.
        assert_eq!(
            validate(&postbox, "uds:", json!("   "), None),
            Err(Refused::InvalidSocketPath)
        );
        // 4 before 5, for the same reason.
        assert_eq!(
            validate(&postbox, "worker-1@other", json!("   "), None),
            Err(Refused::ScopedRecipient)
        );
        // 5, classified: whitespace is no frame and a frame is no whitespace,
        // so 5 and 7 cannot both be failed by one text and have no order
        // between them. What this asserts is that blank text is refused as
        // blank.
        assert_eq!(
            validate(&postbox, "worker-1", json!("   "), None),
            Err(Refused::Whitespace)
        );
        // 3 before 6: an address that is not a session socket of ours is
        // refused before its structured body is looked at — the clause that
        // keeps an unasked call off every other listener on this machine.
        assert_eq!(
            validate(
                &postbox,
                "uds:/var/run/docker.sock",
                json!(frame.clone()),
                None
            ),
            Err(Refused::NotASessionSocket {
                why: AddressRefusal::NotASessionName
            })
        );
        // 6 before 8: a socket refuses structure before the frame's own
        // clauses are reached.
        let socket = SessionSocket::new();
        assert_eq!(
            validate(&postbox, &socket.address(), json!(frame.clone()), None),
            Err(Refused::StructuredOverSocket)
        );
        // 7, the ten and the five, each naming the frame it read.
        assert_eq!(
            validate(&postbox, "worker-1", json!(frame.to_string()), None),
            Err(Refused::ProtocolFrame {
                kind: "shutdown_approved"
            })
        );
        assert_eq!(
            validate(
                &postbox,
                "worker-1",
                json!(json!({"type": "idle_notification"}).to_string()),
                None
            ),
            Err(Refused::LifecycleFrame {
                kind: "idle_notification"
            })
        );
        // 8, all three clauses, and all three classified rather than ordered:
        // the clauses key on disjoint verdicts — no frame is both unclassified
        // and harness-only, and the recipient clause wants a
        // `shutdown_approved`, which is agent-sendable and so never reaches
        // the harness-only arm. Hoisting the recipient clauses above the
        // harness-only one changes no outcome here, which is the proof there
        // is no order between them to assert.
        assert_eq!(
            validate(&postbox, "worker-1", json!({"kind": "not a frame"}), None),
            Err(Refused::StructuredNotAFrame)
        );
        // Addressed to a non-lead, so the refusal is visibly the frame's own
        // and not something the recipient earned (D499's second clause).
        assert_eq!(
            validate(
                &postbox,
                "worker-1",
                json!({"type": "shutdown_rejected", "reason": "no"}),
                None
            ),
            Err(Refused::HarnessOnlyFrame {
                kind: "shutdown_rejected"
            })
        );
        assert_eq!(
            validate(&postbox, "worker-1", json!(frame), None),
            Err(Refused::ShutdownApprovedNotToLead)
        );
    }

    /// One call through [`super::validate`], which is where the order lives.
    fn validate(
        postbox: &Fake,
        to: &str,
        message: serde_json::Value,
        summary: Option<&str>,
    ) -> Result<(Address, Body), Refused> {
        let args = serde_json::from_value(json!({
            "to": to,
            "message": message,
            "summary": summary,
        }))
        .expect("the fixture matches the argument schema");

        super::validate(args, postbox, lead_of(&postbox.roster()).as_deref())
    }

    /// `did:` is recognized by §5.6's parser and named by no rung of §5.2's
    /// ladder, so ganja refuses it by name rather than letting it become a
    /// lookup for a teammate called `did:…`.
    #[tokio::test]
    async fn a_did_address_is_refused_by_name() {
        let postbox = Arc::new(Fake::new());
        let message = refusal(
            &postbox,
            json!({"to": "did:example:123", "message": "hello"}),
        )
        .await;

        assert!(message.starts_with(UNSUPPORTED_SCHEME), "got {message}");
        assert!(message.contains("did:"), "the scheme is named: {message}");
        assert!(
            postbox.delivered.lock().expect("no panic").is_empty(),
            "a refused address reaches no postbox"
        );
    }

    /// D499's first clause: the shutdown answer answers the lead's request.
    #[tokio::test]
    async fn a_shutdown_approved_must_be_addressed_to_the_lead() {
        let postbox = Arc::new(Fake::new());
        let frame = json!({"type": "shutdown_approved", "requestId": "r1"});

        let message = refusal(
            &postbox,
            json!({"to": "worker-1", "message": frame.clone()}),
        )
        .await;
        assert!(
            message.starts_with(SHUTDOWN_APPROVED_NOT_TO_LEAD),
            "got {message}"
        );
        assert!(
            message.contains("team-lead"),
            "the lead is named where the roster knows it: {message}"
        );

        // Addressed to the lead, the same frame goes through — including when
        // the name is spelled in another case, which is not another teammate.
        let tool = SendMessageTool::new(&postbox.roster());
        let ctx = ctx(Some(Arc::clone(&postbox) as Arc<dyn Postbox>));
        tool.run(json!({"to": "Team-Lead", "message": frame}), &ctx)
            .await
            .expect("the lead may be sent the shutdown answer");
    }

    /// D499's second clause: §5.1's five have no door, and the object form is
    /// not a door either.
    #[tokio::test]
    async fn a_structured_harness_only_frame_is_refused_regardless_of_recipient() {
        let postbox = Arc::new(Fake::new());
        let frame = json!({"type": "teammate_terminated", "message": "gone"});

        for to in ["team-lead", "worker-1"] {
            let message = refusal(
                &postbox,
                json!({"to": to, "message": json!({"type": "shutdown_rejected", "reason": "no"})}),
            )
            .await;
            assert!(message.starts_with(HARNESS_ONLY_FRAME), "got {message}");
            assert!(message.contains("shutdown_rejected"), "got {message}");
        }

        // And the same frame as plain text is the other of §5.1's two
        // sentences: the one that names no escape hatch.
        let message = refusal(
            &postbox,
            json!({"to": "worker-1", "message": frame.to_string()}),
        )
        .await;
        assert!(message.starts_with(LIFECYCLE_FRAME), "got {message}");
        assert!(
            !message.contains("object form of `message`"),
            "the five are not offered the structured door: {message}"
        );
    }

    /// A socket address that passes rung 3 is delivery's problem, and what
    /// delivery says about it — a pane member's postbox still answering that
    /// it has no such transport, or the lead's naming a socket that did not
    /// answer — is passed through in the deliverer's own words.
    #[tokio::test]
    async fn a_socket_address_reaches_delivery_and_reads_back_the_deliverers_sentence() {
        let socket = SessionSocket::new();
        let absence = "This postbox does not speak the socket.";
        let postbox = Arc::new(Fake::answering(Err(Undelivered::NoTransport {
            reason: absence.to_owned(),
        })));

        let message = refusal(
            &postbox,
            json!({"to": socket.address(), "message": "hello"}),
        )
        .await;

        assert_eq!(
            message, absence,
            "the deliverer's sentence is passed through"
        );
        let delivered = postbox.delivered.lock().expect("no panic");
        assert_eq!(
            delivered.first().map(|(to, _)| to.clone()),
            Some(Address::Uds {
                path: socket.path.clone()
            }),
            "the address was validated here and handed over whole"
        );
    }

    /// **D505, the D498 premise across a socket**: a `uds:` address may name
    /// only a session socket of ours, refused at rung 3 — before the body is
    /// composed and before anything is connected. What is this tool's to pin
    /// is the mapping — [`AddressRefusal`] becomes
    /// [`Refused::NotASessionSocket`] and the rendered sentence names the
    /// clause — over one string clause and one filesystem clause; the gate's
    /// full clause table is `socket.rs`'s own test's.
    #[tokio::test]
    async fn a_uds_address_that_is_not_a_session_socket_of_ours_is_refused_by_name() {
        let postbox = Arc::new(Fake::answering(Ok(Sent {
            to: "nobody".to_owned(),
            note: "must not be reached".to_owned(),
        })));

        for (to, clause) in [
            ("uds:/var/run/docker.sock", AddressRefusal::NotASessionName),
            (
                "uds:/nonexistent-ganja-dir/0198c1a2.sock",
                AddressRefusal::DirectoryUnreadable,
            ),
        ] {
            let message = refusal(&postbox, json!({"to": to, "message": "hello"})).await;
            assert!(
                message.starts_with(NOT_A_SESSION_SOCKET),
                "{to}: the refusal is rung 3's own: {message}"
            );
            assert!(
                message.contains(&clause.to_string()),
                "{to}: and it names the clause: {message}"
            );
            assert_eq!(
                validate(&postbox, to, json!("hello"), None),
                Err(Refused::NotASessionSocket { why: clause }),
                "{to}"
            );
        }

        assert!(
            postbox.delivered.lock().expect("no panic").is_empty(),
            "nothing reached the deliverer"
        );

        // And one that is a session socket of ours passes rung 3 and reaches
        // delivery.
        let socket = SessionSocket::new();
        assert!(
            validate(&postbox, &socket.address(), json!("hello"), None).is_ok(),
            "a session socket of ours is an address"
        );
    }

    /// A message nobody answers to is information the model reads and retries
    /// on, not a dead turn.
    #[tokio::test]
    async fn an_unknown_recipient_is_reported_in_words() {
        let postbox = Arc::new(Fake::answering(Err(Undelivered::Unknown)));
        let message = refusal(&postbox, json!({"to": "nobody", "message": "hello"})).await;

        assert!(message.starts_with(UNKNOWN_RECIPIENT), "got {message}");
        assert!(message.contains("nobody"), "got {message}");
    }

    /// The delivered path: the body crosses whole, and the model reads what
    /// became of it.
    #[tokio::test]
    async fn a_delivered_message_reports_what_became_of_it() {
        let postbox = Arc::new(Fake::new());
        let tool = SendMessageTool::new(&postbox.roster());
        let ctx = ctx(Some(Arc::clone(&postbox) as Arc<dyn Postbox>));

        let output = tool
            .run(
                json!({"to": "worker-1", "message": "start on the parser", "summary": "kickoff"}),
                &ctx,
            )
            .await
            .expect("a plain message to a teammate sends");

        assert!(output.output.starts_with(DELIVERED), "got {output:?}");
        assert_eq!(output.metadata["structured"], json!(false));
        let delivered = postbox.delivered.lock().expect("no panic");
        assert_eq!(
            delivered.first().map(|(_, body)| body.clone()),
            Some(Body::Text {
                text: "start on the parser".to_owned(),
                summary: Some("kickoff".to_owned()),
            })
        );
    }

    /// AC-21: the wording is ganja's and may improve, but every refusal is
    /// rendered out of a constant a reviewer can find. The match below is
    /// exhaustive on purpose — a rung added without a constant does not
    /// compile here.
    #[test]
    fn every_refusal_is_a_declared_constant() {
        fn declared(refused: Refused) -> &'static str {
            match refused {
                Refused::NoTeam => NO_TEAM,
                Refused::Broadcast => BROADCAST,
                Refused::UnsupportedScheme { .. } => UNSUPPORTED_SCHEME,
                Refused::InvalidSocketPath => INVALID_SOCKET_PATH,
                Refused::NotASessionSocket { .. } => NOT_A_SESSION_SOCKET,
                Refused::ScopedRecipient => SCOPED_RECIPIENT,
                Refused::Whitespace => WHITESPACE,
                Refused::StructuredOverSocket => STRUCTURED_OVER_SOCKET,
                Refused::ProtocolFrame { .. } => PROTOCOL_FRAME,
                Refused::LifecycleFrame { .. } => LIFECYCLE_FRAME,
                Refused::StructuredNotAFrame => STRUCTURED_NOT_A_FRAME,
                Refused::HarnessOnlyFrame { .. } => HARNESS_ONLY_FRAME,
                Refused::ShutdownApprovedNotToLead => SHUTDOWN_APPROVED_NOT_TO_LEAD,
            }
        }

        let every = [
            Refused::NoTeam,
            Refused::Broadcast,
            Refused::UnsupportedScheme { scheme: "did:" },
            Refused::InvalidSocketPath,
            Refused::NotASessionSocket {
                why: AddressRefusal::NotASessionName,
            },
            Refused::ScopedRecipient,
            Refused::Whitespace,
            Refused::StructuredOverSocket,
            Refused::ProtocolFrame {
                kind: "mode_set_request",
            },
            Refused::LifecycleFrame {
                kind: "task_completed",
            },
            Refused::StructuredNotAFrame,
            Refused::HarnessOnlyFrame {
                kind: "shutdown_rejected",
            },
            Refused::ShutdownApprovedNotToLead,
        ];

        // The count moves with the ladder, and moving it is the moment to ask
        // whether the new rung earned its place.
        assert_eq!(every.len(), 13, "every kind the ladder can produce");
        for refused in every {
            let sentence = refused.sentence("worker-1", Some("team-lead"));
            assert!(
                sentence.contains(declared(refused)),
                "{refused:?} renders through its constant: {sentence}"
            );
        }
    }

    /// The tool offered without a team behind it still answers in words.
    #[tokio::test]
    async fn a_call_without_a_team_is_refused_readably() {
        let tool = SendMessageTool::new(&[]);
        let message = match tool
            .run(json!({"to": "worker-1", "message": "hello"}), &ctx(None))
            .await
        {
            Err(ToolError::Failed(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        };

        assert_eq!(message, NO_TEAM);
    }

    /// The roster is what makes a `to` argument answerable, so it is in the
    /// description, in name order, with the lead marked.
    #[test]
    fn the_description_lists_the_team_with_its_lead_marked() {
        let tool = SendMessageTool::new(&Fake::new().roster);
        let described = tool.description();

        assert!(described.starts_with(DESCRIPTION), "got {described}");
        let (_, listed) = described
            .split_once(ROSTER_HEADER)
            .expect("the roster header is appended");
        let roster: Vec<&str> = listed
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        assert_eq!(
            roster,
            vec![
                &format!("- team-lead: runs the team ({LEAD_MARK})")[..],
                "- worker-1: a teammate of this session",
            ]
        );
    }

    /// §5.3's cap, applied before anything crosses the seam.
    #[test]
    fn a_summary_is_capped_before_it_crosses_the_seam() {
        assert_eq!(cap_summary(None), None);
        assert_eq!(cap_summary(Some("  ".to_owned())), None);
        assert_eq!(
            cap_summary(Some("あ".repeat(300))).map(|summary| summary.chars().count()),
            Some(200),
            "counted in characters, so a multi-byte summary is not cut mid-character"
        );
    }
}
