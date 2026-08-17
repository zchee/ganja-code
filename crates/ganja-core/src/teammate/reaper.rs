//! Panes a lead left behind when it died (P25b).
//!
//! Upstream opencode has **no counterpart**. The hazard is §10.10's: a lead
//! that is killed rather than shut down leaves its teammates' panes running,
//! and the next lead in that worktree reads a team file full of members whose
//! panes belong to nobody. Reaping them at startup is what keeps a crashed
//! session from costing somebody a screen full of orphans.
//!
//! # The rule that makes reaping safe, and it is the whole module today
//!
//! **tmux recycles `%N`.** A pane id alone therefore identifies a pane only
//! until that pane dies, after which the same id may name somebody's editor.
//! So a recorded pane is matched on the **pair** — the id *and* the birth time
//! tmux reports beside it — and a live pane whose id matches but whose birth
//! does not is emphatically not the recorded one and is never killed.
//!
//! That comparison is the part of this module that can be written and tested
//! before there is a pane to kill, and it is the part AC-12 turns on; the
//! listing, the sweep at startup and the `kill-pane` call itself land in P25b
//! beside [`crate::teammate::tmux`]'s own calls.

use crate::teammate::Handle;

/// A pane as something recorded it: the id, and the birth that disambiguates a
/// recycled one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    /// The `%N` tmux gave it.
    pub id: String,
    /// Its start time, as tmux reports it.
    pub birth: String,
}

impl Pane {
    /// Reads the pair off a handle, if the handle names a pane at all.
    #[must_use]
    pub fn of(handle: &Handle) -> Option<Self> {
        match handle {
            Handle::Pane { pane_id, birth } => Some(Self {
                id: pane_id.clone(),
                birth: birth.clone(),
            }),
            Handle::InProcess(_) => None,
        }
    }

    /// Whether `live` is really the pane this one recorded.
    ///
    /// Both halves must agree. A pane whose id matches and whose birth does not
    /// is a **recycled id** — a different pane wearing the dead one's name —
    /// and answering `true` here would be how a reaper kills a stranger's
    /// window.
    #[must_use]
    pub fn is(&self, live: &Self) -> bool {
        self.id == live.id && self.birth == live.birth
    }
}

#[cfg(test)]
mod tests {
    use super::Pane;
    use crate::teammate::Handle;

    fn pane(id: &str, birth: &str) -> Pane {
        Pane {
            id: id.to_owned(),
            birth: birth.to_owned(),
        }
    }

    /// The pair is read off the handle a spawn produced, which is the only
    /// place a live pane's birth is known.
    #[test]
    fn a_pane_handle_carries_the_pair_a_reaper_matches_on() {
        let handle = Handle::Pane {
            pane_id: "%142".to_owned(),
            birth: "1755400000".to_owned(),
        };

        assert_eq!(Pane::of(&handle), Some(pane("%142", "1755400000")));
    }

    #[test]
    fn a_recycled_pane_id_is_not_the_pane_that_was_recorded() {
        let recorded = pane("%142", "1755400000");

        assert!(recorded.is(&pane("%142", "1755400000")));
        // Same id, later birth: tmux handed `%142` out again.
        assert!(!recorded.is(&pane("%142", "1755499999")));
        // And a different id is not it either, however old.
        assert!(!recorded.is(&pane("%143", "1755400000")));
    }
}
