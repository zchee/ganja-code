//! The three sentences a model reads when a call was refused rather than run.
//!
//! Spec: upstream `packages/core/src/v1/permission.ts` — `RejectedError` and
//! `DeniedError`, ported verbatim. Both are the engine's, and they lived there
//! until **D552** (W4 of the cursor tool bridge) hoisted them here: a **wire**
//! has to tell a permission refusal from a failed tool without being able to
//! see the engine. Cursor's server asks for a tool result in one of two shapes
//! — a refusal the person made, or a call that ran and failed — and the only
//! thing that reaches the wire is the text of an `Error` tool part. Matching
//! that text against these constants is what makes the distinction, and
//! `ganja-tool` is the one crate the engine and every wire may both name.
//!
//! Nothing here decides anything, and nothing about what a refusal *decides*
//! moved. The permission engine still renders the bytes; these are the bytes
//! it renders.
//!
//! **D556** added the third sentence and
//! [`is_refusal`](crate::permission_text::is_refusal). A `PreToolUse` hook
//! that blocks a call routes the same `fail_call` a denied rule does, so the
//! two must read alike on every wire — and until now they did not: cursor's
//! classifier knew the two permission sentences and reported a hook-refused
//! call as one that ran and broke. The predicate here is the **one** place the
//! refusal vocabulary is enumerated, so a fourth sentence is added here or
//! nowhere.

/// What the model reads when the user refuses a call at the dialog.
///
/// Upstream `RejectedError`, verbatim.
pub const REJECTED: &str = "The user rejected permission to use this specific tool call.";

/// Everything a rule-refusal sentence says before the rendered rules.
///
/// Upstream `DeniedError`. The rules travel with the message, as upstream's
/// do — a model told only that it may not do something tries the same thing
/// spelled differently, where one told *which rule* stopped it can work out
/// what else the rule covers — so this is a prefix rather than a whole
/// sentence, and the trailing space before the rules is part of it.
pub const DENIED_PREFIX: &str = "The user has specified a rule which prevents you from using \
     this specific tool call. Here are some of the relevant rules ";

/// Everything a hook-refusal sentence says before the hook's own reason.
///
/// D458's `PreToolUse` block, whose text lived at `ganja_core::session`'s
/// `blocked_by_hook` until **D556** moved it here (the engine reads it from
/// this constant a wave later, so the constant lands before its second
/// reader). A prefix rather than a whole sentence for [`DENIED_PREFIX`]'s
/// reason: the hook's own reason travels with it, because a model told only
/// that something was refused retries it spelled differently.
///
/// It is a refusal and not a failure, which is the whole reason a wire needs
/// to know it: root `AGENTS.md` says a hook block "routes the same
/// `fail_call` a denied rule does", so a wire that classified this text as a
/// tool that ran and broke would tell its server the opposite of what
/// happened.
pub const HOOK_REFUSED_PREFIX: &str = "A PreToolUse hook refused this tool call: ";

/// Whether an `Error` tool part's text is a refusal rather than a failure.
///
/// The three sentences above and nothing else: an unknown tool, arguments
/// that would not parse, a command that exited non-zero — each of those is a
/// call that ran and failed, which is a different thing to tell a server than
/// a call a person or a rule or a hook declined.
///
/// This predicate is the only enumeration of that vocabulary in the
/// workspace. A wire matching one of these constants directly would be a
/// second, and the day a fourth sentence appears the second is the one that
/// goes quietly out of date.
#[must_use]
pub fn is_refusal(error: &str) -> bool {
    error == REJECTED || error.starts_with(DENIED_PREFIX) || error.starts_with(HOOK_REFUSED_PREFIX)
}

#[cfg(test)]
#[path = "permission_text_tests.rs"]
mod tests;
