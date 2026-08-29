//! Whether a teammate's pane is still that teammate's, asked on a timer.
//!
//! One poll for both shapes that hold a pane — a shim member in its CLI's own
//! TUI ([`crate::shim_tui`]) and a `ganja` or `claude` pane
//! ([`crate::pane`]) — because there is exactly one question to ask and one
//! rule for reading the answer, and two copies of that rule would be two
//! places for the rule to drift. What differs between the two is everything
//! *after* the answer: a shim reads its CLI's last words and answers the mail
//! left in its inbox, and a pane member has neither to do. So what is shared
//! is the cadence, the vocabulary and the listing check; the loop each member
//! runs and what it does with a [`Gone`](crate::liveness::Gone) stay where the
//! differences are.
//!
//! Extracted for **D541**, which gave the pane backends the poll the shim
//! members already ran.

use std::time::Duration;

use crate::reaper::Pane;
use crate::tmux::Server;

/// How often a member's loop asks whether its pane is still running.
///
/// A question a delivery cannot be the only thing to ask: under
/// [`ganja_core::teammate::Delivery::FireAndForget`] a TUI that quits between
/// messages is, to this side, indistinguishable from one that is thinking —
/// until the next paste fails into it, which may be never. And a `ganja` pane
/// answers no delivery at all from this side, so for it there is no next paste
/// to fail: the poll is the *only* thing that would ever notice (**D541**).
/// Two seconds is four inbox passes ([`crate::shim::POLL`]): a dead pane is a
/// matter of what is on a person's screen and of a roster row that no longer
/// answers, not of a turn's correctness, so it need not be noticed faster than
/// a person would notice it; and each ask is one `list-panes` client per
/// member, which at this cadence costs less than the inbox read beside it.
pub const LIVENESS_POLL: Duration = Duration::from_secs(2);

/// How a pane stopped being its member's ([`gone`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gone {
    /// Not listed live: dead under `remain-on-exit`, or closed. A corpse, if
    /// tmux kept one, is still this member's, and its screen may be read.
    Dead,
    /// Listed live under another pid: the id was recycled, somebody respawned
    /// something into it. Not ours any more and not ours to touch — no
    /// capture, no close.
    Recycled,
}

/// Whether `pane` has stopped being `teammate`'s. [`None`] while it is still
/// running under its recorded pair.
///
/// A listing that *fails* says nothing either way and leaves the member as it
/// is — "no proof, no retire", the reaper's own rule — because the cost of a
/// wrong [`None`] is one more poll, and the cost of a wrong [`Some`] is a
/// teammate retired out from under a person.
pub async fn gone(server: &Server, pane: &Pane, teammate: &str) -> Option<Gone> {
    let live = match server.panes().await {
        Ok(live) => live,
        Err(error) => {
            tracing::debug!(
                teammate,
                pane = pane.id,
                %error,
                "a liveness listing failed; the member is left as it is"
            );
            return None;
        }
    };
    match live.iter().find(|listed| listed.id == pane.id) {
        Some(listed) if pane.is(listed) => None,
        Some(_) => Some(Gone::Recycled),
        None => Some(Gone::Dead),
    }
}
