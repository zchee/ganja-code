//! The two sentences a model reads when a call was refused rather than run.
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
