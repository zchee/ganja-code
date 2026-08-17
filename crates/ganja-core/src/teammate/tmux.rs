//! The tmux facts the two pane backends are built on.
//!
//! Upstream opencode has **no counterpart**; what is ported is Claude Code's
//! §4.1 spawn sequence and §10.2's step-by-step reading of it against this
//! tree. The pane calls themselves — `split-window` reporting
//! `#{pane_id} #{pane_start_time}` in one go, the pane-border title, `kill-pane
//! -t %N`, the liveness listing — land in P25b beside the backends that make
//! them ([`crate::teammate::pane`], [`crate::teammate::claude`]).
//!
//! What is here now is the half that is a fact about this *session* rather than
//! about tmux, and that the trait's refusal already depends on: whether a pane
//! can be had at all.
//!
//! # `$TMUX` is a capability, never a selector (**D501**)
//!
//! The backend is an explicit argument on both doors. This variable decides
//! whether the two pane values can *run*, and a session without it refuses them
//! readably rather than quietly spawning an in-process teammate instead:
//! somebody who asked for a window and silently got none has been told
//! something untrue about their own session, and self-hosting a detached tmux
//! server to conjure one is a non-goal of this landing.

/// What a pane spawn says when there is no tmux to put it in.
///
/// P25b's refusal, and the sentence AC-16 asserts — the *sentence*, because the
/// useful half of this answer is that the session, not the build, is what is
/// missing.
pub const REFUSED_NO_TMUX: &str = "there is no tmux session here ($TMUX is \
     unset), and ganja does not start one of its own; run ganja inside tmux, \
     or spawn this teammate in-process";

/// The variable tmux exports into every process it runs.
pub const TMUX: &str = "TMUX";

/// Whether this process is running inside a tmux pane.
///
/// Reads the environment on every call rather than once: a lead started outside
/// tmux and re-attached is not a case this build handles, but caching the
/// answer would make it a case it handles *wrongly* — and the read is one
/// `getenv`.
#[must_use]
pub fn hosted() -> bool {
    std::env::var_os(TMUX).is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::REFUSED_NO_TMUX;

    /// The refusal has to say which variable, because that is the whole of what
    /// somebody reading it can act on.
    #[test]
    fn the_refusal_names_the_variable_and_the_way_out() {
        assert!(REFUSED_NO_TMUX.contains("$TMUX"));
        assert!(REFUSED_NO_TMUX.contains("in-process"));
    }
}
