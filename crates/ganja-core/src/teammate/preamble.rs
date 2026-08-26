//! What a teammate is told before its task (**D514**): who it is, who its
//! lead is, and how — or whether — it answers.
//!
//! Upstream opencode has no teammates and so no preamble; Claude Code's worker
//! preamble is the reference, and no prose of it ships (**D497**). P25 wrote
//! one for the `claude` backend alone, because §5.5.1 made it necessary there
//! — a real `claude` that is not told its lead's name addresses `main`, and
//! the send fails. On 2026-08-22 a user directive extended it to every
//! backend: the preamble is the **first** message in a teammate's inbox, ahead
//! of its task, seeded by the registry through
//! [`TeammateBackend::preamble`] —
//! or by the backend itself where it owns the inbox — and the method is
//! required, so a backend that cannot say how its teammate answers cannot be
//! written at all.
//!
//! One frame, several channels. [`frame`](crate::teammate::preamble::frame) is the shape every preamble has:
//! name, team, lead, then one paragraph about answering, then the task. That
//! paragraph is the only thing that differs between backends, and it is each
//! backend's own: [`native`](crate::teammate::preamble::native) here, for the two surfaces that hold ganja's
//! `send_message` tool (the in-process teammate and the `ganja` pane);
//! [`crate::teammate::claude::preamble`] for a real `claude`'s `SendMessage`;
//! [`crate::teammate::shim_tui::preamble`] for a CLI's native TUI in a pane,
//! whose answers are read back out of that CLI's own transcript (**D515**,
//! which retired D512's send-only pane); and
//! [`crate::teammate::shim::preamble`] for a headless child, whose answers are
//! mail. The words are ganja's own throughout.
//!
//! The member record keeps the bare prompt: what is persisted verbatim is what
//! a person typed, and the preamble is what the registry wrapped around it.

use crate::teammate::SpawnSpec;

/// Who a teammate is and who it answers to, as a preamble names them.
///
/// Borrowed strings rather than a [`SpawnSpec`], so a test can compute the
/// exact first message a teammate reads from three names and a task without
/// building a whole spec — and compare it against the function that seeds it
/// rather than against a literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Names<'a> {
    /// The teammate's own name, and its mailbox's basename.
    pub name: &'a str,
    /// The team it joins.
    pub team: &'a str,
    /// The lead it answers to.
    pub lead: &'a str,
}

impl<'a> Names<'a> {
    /// The three names a spawn fixed.
    #[must_use]
    pub fn of(spec: &'a SpawnSpec) -> Self {
        Self {
            name: spec.name.as_str(),
            team: spec.team.as_str(),
            lead: spec.lead.as_str(),
        }
    }
}

/// The sentence every preamble opens on, and the **fingerprint** a foreign
/// CLI's own transcript is found by (**D515**).
///
/// Two readers, one spelling. A pane teammate's answers are read back out of
/// the transcript its CLI writes for itself
/// ([`crate::teammate::readback`]), and the only honest way to know which of
/// that CLI's sessions is *this member's* is that the session opens with the
/// message this side pasted: name and team together are unique to one member
/// of one team, and a CLI records what it was handed verbatim. Composing that
/// sentence here rather than at each reader is what keeps the fingerprint and
/// the preamble incapable of drifting apart — a preamble that reworded its
/// opening would otherwise leave every reader looking for a sentence nobody
/// sends any more.
#[must_use]
pub fn opening(who: Names<'_>) -> String {
    format!(
        "You are {name}, a teammate on the team {team}. Your lead is {lead}.",
        name = who.name,
        team = who.team,
        lead = who.lead,
    )
}

/// The shape every preamble has: who, one paragraph on answering, the task.
///
/// The task comes **last** and ends the message, so a teammate that reads only
/// the tail still reads what it was asked to do, and a test that pins the
/// seeded message can pin that it ends with the prompt.
#[must_use]
pub fn frame(who: Names<'_>, channel: &str, prompt: &str) -> String {
    format!(
        "{opening}\n\n{channel}\n\nYour task:\n\n{prompt}",
        opening = opening(who),
    )
}

/// The preamble for a teammate that holds ganja's own `send_message` tool —
/// the in-process teammate and the `ganja` pane.
///
/// Names the tool and its one argument that matters, because that is the
/// whole of how such a teammate reaches anybody; names `main` as an address
/// that reaches nobody, since a teammate that learned the habit elsewhere has
/// to be told it is wrong rather than merely not told it is right (`ganja-team`
/// reserves the name, so the tool refuses it as an unknown recipient); and
/// says the one thing about reporting that is true of both native surfaces —
/// that what it found reaches the lead only if it sends it.
#[must_use]
pub fn native(who: Names<'_>, prompt: &str) -> String {
    frame(
        who,
        &format!(
            "Address the lead by that name through your `send_message` tool — `to: \"{lead}\"`, \
             one recipient per call — and a teammate by the name that tool's own description \
             lists it under. Do not address \"main\": no member answers to that name. Everything after this arrives the same \
             way this did, through your inbox, each message opening with who sent it — and \
             nothing you find reaches the lead unless you send it.",
            lead = who.lead,
        ),
        prompt,
    )
}

#[cfg(test)]
#[path = "preamble_tests.rs"]
mod tests;
