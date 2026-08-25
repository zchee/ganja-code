//! What every postbox shares, spelled once.
//!
//! Three implementations of [`crate::tool::team::Postbox`] exist — the lead's
//! and a teammate's in [`crate::subagent`], and a pane member's in
//! [`crate::teammate::member`] — and each binds its sender its own way, which
//! is the anti-forgery rule and stays theirs. What they answer *identically*
//! lives here instead of three times: the one classification of the frame
//! vocabulary, the write tail a resolved local recipient's delivery ends in,
//! and the sentences those two are read back in. Here rather than in
//! `subagent.rs`, so the member side reaches its shared half without deepening
//! the `teammate/ → subagent` edge that once existed only for these sentences.

use ganja_protocol::team::Frame;
use ganja_team::{MailboxMessage, MemberName, TeamName, TeamsRoot, mailbox, record};

use crate::{
    teammate::blocking_io,
    tool::team::{Body, Peer, Reserved, Sent, Undelivered},
};

/// A member of the team whose name the name grammar refuses — impossible
/// through this build's own registration, and answered rather than trusted.
pub(crate) const UNADDRESSABLE: &str =
    "This team is holding a member under a name that cannot be addressed:";

/// A write that did not land, ahead of what the mailbox said about it.
pub(crate) const UNWRITTEN: &str = "The message could not be written to that teammate's inbox:";

/// What became of a message that did land.
pub(crate) const WRITTEN: &str = "It is in that inbox and will be read on the next pass.";

/// What the roster says about the one member that is not a teammate.
pub(crate) const LEADS: &str = "the session that leads this team";

/// What it says about the ones that are, ahead of the surface each runs on.
pub(crate) const RUNS_ON: &str = "a teammate on the";

/// Why a `uds:` address is validated and then not delivered — by the one
/// postbox that still does not speak the socket, a pane member's
/// ([`crate::teammate::member::MemberPostbox`]). The lead's and an in-process
/// teammate's do (**D505**, `Postbox::deliver_over_socket` in
/// [`crate::subagent`]).
pub(crate) const NO_SOCKET: &str = "A message to another session travels over that session's socket, and this teammate's postbox does not speak it yet. A member of this team is reached by its bare name; another session, through the lead.";

/// One parse and one lookup, both `ganja-protocol`'s: the tool may not name
/// that crate, so this is the only place the frame vocabulary is known, and
/// there is no list of frame names anywhere on the tool's side to fall out of
/// step with it.
pub(crate) fn classify_reserved(text: &str) -> Reserved {
    match Frame::reserved_kind(text) {
        None => Reserved::No,
        Some(kind) if Frame::is_agent_sendable_kind(kind) => Reserved::AgentSendable { kind },
        Some(kind) => Reserved::HarnessOnly { kind },
    }
}

/// One roster row's description, over whichever backend word the caller's
/// document or registry holds.
pub(crate) fn peer_description(backend_word: &str) -> String {
    format!("{RUNS_ON} {backend_word} backend")
}

/// The write tail every local delivery ends in, once a roster resolved
/// `recipient`: the canonical name back through the grammar, the body down to
/// text, one stamped message into the inbox that name resolves to under
/// `root`/`team`.
///
/// Beside the [`Sent`] it answers with the §2.3 identity of the message **as
/// written** (M6, **D525**): the stamp — sender, timestamp, text — is minted
/// here and nowhere else, and the admission gate's admitted set is keyed by
/// it, so the one mint site hands the key back rather than letting a second
/// site re-derive and mis-stamp it. The two [`crate::tool::team::Postbox`]
/// implementations discard it; the engine's socket door records it.
///
/// # Errors
///
/// [`Undelivered::Failed`]: a roster name the grammar refuses (impossible
/// through this build's own registration, answered rather than unwrapped
/// because the cost of being wrong is a panic in somebody's turn), or a write
/// that did not land.
pub(crate) async fn write_to_peer(
    sender: &str,
    root: &TeamsRoot,
    team: &TeamName,
    recipient: &Peer,
    body: Body,
) -> Result<(Sent, mailbox::Identity), Undelivered> {
    let member = MemberName::parse(&recipient.name).map_err(|error| Undelivered::Failed {
        reason: format!("{UNADDRESSABLE} {:?}: {error}", recipient.name),
    })?;

    let (text, summary) = match body {
        Body::Text { text, summary } => (text, summary),
        // A frame crosses as the document its sender wrote. The far side
        // reads it back with the same one parse `classify_reserved` uses, so
        // re-encoding it through a typed value here would only be a second
        // spelling of one document.
        Body::Frame(document) => (document.to_string(), None),
    };
    let mut message = MailboxMessage::new(sender, text, record::now_iso8601());
    message.summary = summary;
    // Before the write, off the very message it stamps: `mailbox::write`
    // fills envelope fields (`kind`, `read`, `msg_v`, `msg_id`) that sit
    // outside the identity's three, so the key computed here is the key any
    // later read of the entry derives.
    let identity = mailbox::identity(&message);

    let path = root.inbox_path(team, &member);
    match blocking_io(move || mailbox::write(&path, message)).await {
        Ok(_) => Ok((
            Sent {
                to: member.into_inner(),
                note: WRITTEN.to_owned(),
            },
            identity,
        )),
        Err(reason) => Err(Undelivered::Failed {
            reason: format!("{UNWRITTEN} {reason}"),
        }),
    }
}
