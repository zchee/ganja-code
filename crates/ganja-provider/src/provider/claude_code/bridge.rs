//! Ganja's tools, run by ganja, asked for by the CLI's own model.
//!
//! Spec: run 1 and run 3 of the recording. The order there is `can_use_tool`
//! first — 3 ms, 1 ms and 1 ms ahead of `tools/call` on all three
//! tool-calling runs — so the **primary** path is the exercised one and the
//! secondary arm exists for a build that asks the other way round.
//!
//! # Nothing here runs a tool
//!
//! Not a slogan: it is the invariant the whole design rests on, and it is
//! pinned by a test in `ganja-core` rather than by a gate here. The CLI asks,
//! the wire surfaces the ask as an ordinary `ToolCallStart`/`Delta`/`End`
//! plus `Finish(ToolCalls)`, the **engine** runs its own permission dialog
//! and its own tool, and the next `stream()` on the same key carries the
//! result in a `Tool` part. This module reads that part and answers from it.
//! A redirect that ran the call here would have no dialog, no rules and no
//! transcript row, and no dependency gate in this workspace can see the
//! difference.
//!
//! # Two answers, and one of them is not a failure
//!
//! `permission_text::is_refusal` decides. A part whose error is one of the
//! three refusal sentences is answered `deny{message}`, and the CLI then
//! never sends `tools/call` at all. Anything else — an unknown tool, bad
//! arguments, a command that exited non-zero — is a call that **ran and
//! failed**, so it is answered `allow` and the failure travels as the tool's
//! own `CallToolResult{is_error: true}`, which is what the model should read.
//!
//! # `updatedPermissions` is never sent
//!
//! There is no field for it on the answer this module builds, and a test
//! asserts the serialized answer carries no such key. An `allow` without it
//! records no CLI-side session grant, so the next ask for the same tool asks
//! again — every call crosses ganja's own dialog, every time, which is the
//! only arrangement under which ganja's permission rules mean anything on
//! this wire.
//!
//! # A message that arrives mid-turn
//!
//! On the **allow** path it is deferred: the `CallToolResult` carries the
//! part's output alone and the message rides the next turn's first `user`
//! frame. The channel was measured working and useless — the CLI delivered
//! both blocks (M19 (a)) and the model named the second as tool-sourced text
//! wearing a user's voice and declined it in the reply the person reads
//! (M19 (b)). On the **deny** path the carry stays, because a `deny.message`
//! is text the model reads as the tool's own refusal rather than as a user's
//! voice.

use ganja_tool::permission_text::is_refusal;

use super::preamble::carried;
use super::rpc;
use crate::protocol::{Message, PartBody, ToolState};

/// The prefix the CLI puts on a tool this side declared.
///
/// Declared bare, presented to the model prefixed, and delivered back to
/// `tools/call` bare again (M6). So a `can_use_tool`'s `tool_name` is the one
/// place a prefixed name reaches this side, and it is stripped here.
pub const MODEL_FACING_PREFIX: &str = "mcp__ganja__";

/// One ask waiting on ganja's engine.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    /// The `control_request` id a `can_use_tool` answer echoes, or [`None`]
    /// on the secondary path, where the `tools/call` itself was the ask.
    pub request_id: Option<String>,
    /// The id every part of this call shares: `can_use_tool.tool_use_id`, the
    /// `tool_use` block's id, and `_meta["claudecode/toolUseId"]`.
    pub tool_use_id: String,
    /// The registry name, bare.
    pub name: String,
    /// The arguments the model produced.
    pub input: serde_json::Value,
    /// The `control_request` id of the `tools/call`, once it arrives.
    pub call_request_id: Option<String>,
    /// The JSON-RPC id that call's answer must echo.
    pub call_rpc_id: Option<serde_json::Value>,
}

/// What one parked ask is answered with.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolution {
    /// Which ask this answers.
    pub tool_use_id: String,
    /// The permission answer.
    pub permission: Permission,
    /// The `tools/call` answer, for the arms that reach one. A denied call is
    /// never called, so it has none.
    pub result: Option<rpc::CallToolResult>,
}

/// What a `can_use_tool` is answered with.
#[derive(Clone, Debug, PartialEq)]
pub enum Permission {
    /// The call may run, with the arguments it was asked about. **No
    /// `updatedPermissions`**: there is no field for one.
    Allow {
        /// The arguments, unchanged — this wire never rewrites a call.
        updated_input: serde_json::Value,
    },
    /// The call may not run, and this is what the model reads instead.
    Deny {
        /// The refusal, and on the deny path an owed message under
        /// [`super::preamble::MID_TURN_HEADER`].
        message: String,
    },
}

impl Permission {
    /// The answer as the CLI's `control_response` carries it.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        match self {
            Self::Allow { updated_input } => {
                serde_json::json!({"behavior": "allow", "updatedInput": updated_input})
            }
            Self::Deny { message } => serde_json::json!({"behavior": "deny", "message": message}),
        }
    }
}

/// Where an owed message went, for the resolve's own log line.
///
/// One line saying this is what a person debugging "the model ignored my
/// steer" has, so the two arms are named rather than inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarriedOn {
    /// It stayed owed and rides the next turn's first `user` frame.
    Deferred,
    /// It went out inside a `deny.message`, and its id joined `sent`.
    DenyMessage,
}

/// What resolving one request produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    /// One answer per parked ask, in the order they were parked.
    pub answers: Vec<Resolution>,
    /// Where the owed message went, when there was one.
    pub carried_on: Option<CarriedOn>,
    /// The id of a message written inside a `deny.message`, which the caller
    /// appends to `sent` — the deny path's carry **is** a write.
    pub carried_id: Option<String>,
}

/// Matches every parked ask to a `Tool` part of `turn` and answers it.
///
/// `owed` is the message that arrived while the tool ran, if any: at most one
/// is carried, on the first deny written, and it is carried nowhere at all
/// when every answer is an allow.
///
/// # Errors
///
/// Returns the `tool_use_id` of the first pending with no part. That is an
/// error and **never a new turn**: a keyed match with pendings and no results
/// means the engine and this wire disagree about what has been run, and
/// opening a turn on that disagreement would run something twice.
pub fn resolve(
    pendings: &[Pending],
    turn: &[Message],
    owed: Option<&Message>,
) -> Result<Resolved, String> {
    let mut answers = Vec::with_capacity(pendings.len());
    let mut carried_on = None;
    let mut carried_id = None;

    for pending in pendings {
        let state =
            part_for(&pending.tool_use_id, turn).ok_or_else(|| pending.tool_use_id.clone())?;

        let answer = match state {
            ToolState::Completed { output, .. } => Resolution {
                tool_use_id: pending.tool_use_id.clone(),
                permission: Permission::Allow { updated_input: pending.input.clone() },
                result: Some(rpc::CallToolResult {
                    content: vec![rpc::Content::text(output.clone())],
                    is_error: false,
                }),
            },
            ToolState::Error { error, .. } if is_refusal(error) => {
                // The one place a mid-turn message travels as text. It rides
                // the **first** deny and no other, so two denied asks in one
                // resolve carry it once.
                let message = match owed {
                    Some(owed) if carried_on.is_none() => {
                        carried_on = Some(CarriedOn::DenyMessage);
                        carried_id = Some(owed.id.as_str().to_owned());

                        format!("{error}\n\n{}", carried(owed))
                    }
                    _ => error.clone(),
                };

                Resolution {
                    tool_use_id: pending.tool_use_id.clone(),
                    permission: Permission::Deny { message },
                    result: None,
                }
            }
            // Ran and failed: the model reads the failure as the tool's own
            // answer, never as a refusal.
            ToolState::Error { error, .. } => Resolution {
                tool_use_id: pending.tool_use_id.clone(),
                permission: Permission::Allow { updated_input: pending.input.clone() },
                result: Some(rpc::CallToolResult {
                    content: vec![rpc::Content::text(error.clone())],
                    is_error: true,
                }),
            },
            // A part that is still pending or running is a part the engine
            // has not finished with, which is the same disagreement as a
            // missing one.
            ToolState::Pending { .. } | ToolState::Running { .. } => {
                return Err(pending.tool_use_id.clone());
            }
        };

        answers.push(answer);
    }

    if owed.is_some() && carried_on.is_none() {
        carried_on = Some(CarriedOn::Deferred);
    }

    Ok(Resolved { answers, carried_on, carried_id })
}

/// The state of the `Tool` part `turn` carries for `call_id`.
///
/// D462 keeps parts in call order, so a linear walk finds the right one and a
/// map would be a second index over the same list.
#[must_use]
pub fn part_for<'a>(call_id: &str, turn: &'a [Message]) -> Option<&'a ToolState> {
    turn.iter().flat_map(|message| &message.parts).find_map(|part| match &part.body {
        PartBody::Tool { call_id: part_id, state, .. } if part_id == call_id => Some(state),
        _ => None,
    })
}

/// The registry name behind a model-facing one.
///
/// The CLI prefixes what this side declared, so a `can_use_tool`'s
/// `tool_name` arrives prefixed and a `tools/call`'s `params.name` does not.
#[must_use]
pub fn registry_name(model_facing: &str) -> String {
    model_facing.strip_prefix(MODEL_FACING_PREFIX).unwrap_or(model_facing).to_owned()
}

/// What the model is told a tool is called.
#[must_use]
pub fn model_facing_name(registry: &str) -> String {
    format!("{MODEL_FACING_PREFIX}{registry}")
}

/// What a cancelled turn's parked asks are answered with.
///
/// The CLI is never left holding a question ganja will not answer: an
/// unanswered `can_use_tool` does not time out, so a cancel that simply
/// stopped reading would wedge the process for the life of the session.
#[must_use]
pub fn cancelled() -> Permission {
    Permission::Deny { message: "cancelled".to_owned() }
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;
