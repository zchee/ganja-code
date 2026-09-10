//! `ChatRequest` → the Run stream's opening message.
//!
//! Spec: `.omc/research/cursor/spike-wire-facts.md` — the server refuses a
//! stream whose first message is not a run request, so this module builds
//! exactly that one message. What it carries is the set the message
//! definitions in `cursor.proto` model: the model asked for (named twice,
//! because the server still reads the deprecated description beside the
//! forward-looking one), the conversation state **composed** from the
//! transcript, the conversation's id, and the action — the newest user turn
//! inline, or a resume over the state when there is no newer user turn to
//! send.
//!
//! **The state is composed, not empty** (**D553**). Everything a conversation
//! already holds — earlier turns, tool calls and their results — travels on
//! cursor's wire as content-addressed blobs the request *names* and the
//! server *fetches* over the stream's kv half. [`super::history`] is the
//! composition: it walks `ChatRequest.messages` into the entries the server
//! builds its prompt from and the turns beside them, hashes each into the
//! store a fresh Run is seeded with, and decides the action from the
//! request's own shape. [`run_message`] only spells what it decided.
//! [`kv_answer`] speaks the channel's serving side — the server reads the
//! composed blobs back by id, stores blobs of its own mid-turn and reads
//! those back too, and will not end the turn while one exchange is
//! unanswered.
//!
//! **The advertised tools, on the other hand, are sent** (**D552**).
//! [`declaration`] turns `ChatRequest.tools` into the roster cursor's own
//! client-declared MCP channel carries — on the run request's
//! `mcp_tools = 4` and on every `RequestContext.tools = 7` answer, which is
//! where the shipped client puts them — so the server's agent loop calls
//! ganja's tools by name and this client answers from what its own engine ran.
//! A request that declares no tools — a title or summary one-shot — declares
//! no roster, and [`declaration`] says why that costs no byte.
//!
//! **The system prompt rides the answer, not the request.** The
//! descriptor's one inline member for it, `custom_system_prompt = 8`, is an
//! allowlist-gated override ("Allowlisted for specific teams only", the
//! reference plugin's `src/proto/agent_pb.ts:2782`) that the plugin never
//! sets — and sending it LIVE-FAILED an ordinary seat's turn with 400
//! invalid_argument: "unknown option '--system-prompt'". Where the plugin's
//! system text really travels is `RequestContext.cloudRule`, its answer to
//! the server's mid-stream `requestContextArgs` exec (`src/proxy.ts:1132`;
//! its comment records that plain system messages are ignored server-side).
//! [`context_answer`] is that reply, spoken on the same open request body
//! the run request went out on — so `ChatRequest.system` reaches the model
//! on the one channel the server honors, and never through the member it
//! demonstrably refuses.
//!
//! # Tool execs, and which of them ganja runs (**D550**, **D552**, amending **D486**)
//!
//! Cursor's server does not only *ask for* context mid-turn; it asks the
//! client to **run tools** for it — a shell command, a file read, an MCP
//! call — as exec requests on the same channel, and it holds generation
//! until each one is answered. The live-observed instance is
//! `shell_stream_args` (the args oneof's field 14), which arrived on an
//! ordinary turn and, until [`refusal_answer`], ended it: every exec kind
//! but the context ask became a `ProviderEvent::Failed` naming the kind,
//! because leaving it unanswered would have hung the turn instead.
//!
//! **Most of them are now answered** (**D552**). This request declares a
//! roster ([`declaration`], above), so a `mcp_args` naming one of those tools
//! is a call for ganja to run; and seven native kinds — `read_args` and
//! `redacted_read_args`, `shell_stream_args`, `grep_args`, `ls_args`,
//! `write_args`, `fetch_args` — are redirected onto the six ganja tools that
//! do the same job ([`super::native`]). Neither is run on this side. The Run
//! **pauses** instead, holding the request body open while ganja's engine
//! executes the call through its own four phases and answering on the body
//! that was never closed ([`super::bridge`]) — which is the whole point, since
//! only the engine can raise a permission dialog and write a transcript part.
//!
//! **What diverges, and what no longer does.** There is no upstream
//! counterpart to weigh this against — upstream opencode v1.18.22 has no
//! cursor wire at all, so no ported behavior is being contradicted. The
//! divergence is from *cursor's own shipped client*, which executes these asks
//! against the user's machine and streams the results back, and it survives
//! D552 intact: **ganja still never runs the server's asks blind.** What it
//! runs is its own tools, named by its own roster, under the session's own
//! rules — and a call for a tool this request did not declare is refused
//! exactly as before. The pause, the run-level heartbeat's cadence, the shape
//! of the key a resume is found by and the composition of an `mcpResult` from
//! what a tool produced are **behaviour derived** from the `opencode-cursor`
//! plugin's proxy at `a37a6ba9a6d6d8d176bb68248f59240271f46767` (MIT; see
//! `THIRD_PARTY_NOTICES.md`, and the two modules that implement them for the
//! per-site citations). No code is copied. What this module still refuses is
//! everything the engine has no tool for, which is where D550's typed arms and
//! D486's throw stayed.
//!
//! [`refusal_answer`] answers ten kinds in their own vocabulary:
//! `ExecResponse` carrying the kind's rejected arm at the kind's own field
//! number, echoing back what the args named — the command, the path, the
//! url — then the `stream_close` that ends every exec, refused or served.
//! Two of the ten have no rejected arm in the shipped descriptor at all
//! (`grep_result`, `fetch_result`), so their refusal travels as the error
//! arm, which is the only place their result can say anything; a kind with
//! no modelled arm at all takes D486's control-channel throw, keyed on the
//! numeric id alone, which is what keeps a kind newer than this file
//! refusable. Seven of the ten reach this function only when the bridge
//! declined them — the tool they map to is not on this request's roster —
//! which is what makes "refused" still mean something after **D552**: it is
//! a statement about what this turn is offering, not about what this client
//! can do. Why a decline is the kind's own arm rather than the throw, and a
//! refusal rather than a failed turn, are **D550**'s rulings, stated in full
//! in `crates/ganja-provider/AGENTS.md`.
//!
//! # The switchboard: asking the server for less
//!
//! [`context_answer`] also fills three members of `RequestContext` that
//! narrow what the server's loop will ask this client for at all
//! (`cursor.proto`'s own comment on that message says which and why). The
//! one that is not a literal is `web_fetch_enabled`, which it computes from
//! the roster it is handed through the predicate [`super::serves_fetch`]
//! answers off the request: a fetch exec has somewhere to go exactly when
//! this turn declared tools. A refusal answered well is still worse than an
//! ask never made.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::ops::RangeInclusive;

use buffa::Message as _;

use super::history::{self, Action, Composed};
use super::{ID, decode, proto};
use crate::auth::pkce;
use crate::protocol::Role;
use crate::provider::{ChatRequest, ProviderError};
use crate::tool::ToolDefinition;

/// The name this client serves its tools under, in `provider_identifier = 4`
/// on every declaration and matched on every incoming call (**D552**).
///
/// It says who *serves* the tool, and `"ganja"` is true: this is not a claim
/// to be an MCP server, it is the channel cursor calls MCP being used by the
/// client that runs the tools. A call naming anything else is a call meant for
/// somebody else and is answered `server_not_found` rather than executed.
pub(super) const PROVIDER_IDENTIFIER: &str = "ganja";

/// A fresh RFC 9562 v4 id in the spelling `crypto.randomUUID()` mints, which
/// is the shape the recorded client stamps on messages and requests alike.
///
/// # Errors
///
/// Returns [`ProviderError::Transport`] when the platform's random source
/// fails: nothing was sent, and nothing was refused.
pub(super) fn fresh_id() -> Result<String, ProviderError> {
    let bytes =
        pkce::random_bytes::<16>().map_err(|error| ProviderError::Transport(error.to_string()))?;

    Ok(render_v4(bytes))
}

// Sixteen bytes as a v4-shaped UUID. The layout moved to
// `crate::provider::ids` with `history::derived`, which is its other caller,
// when **D556** gave that derivation a third consumer outside this module; it
// is re-exported here because `fresh_id` above is written in terms of it, and
// because the two ids on a run request being the same *shape* is this
// module's own arrangement to state.
pub(super) use crate::provider::ids::render_v4;

/// The tools of `request` as cursor's own client-declared roster (**D552**).
///
/// One [`proto::McpToolDefinition`] per entry, in the order the engine
/// advertised them, with `name` and `tool_name` both set from the registry
/// name — the shipped client's builder sets both from one declaration
/// (`index.js@5699717`), and the live probe confirmed both arrive back on the
/// call. The schema rides `input_schema_json = 6` alone: it is already a JSON
/// string on this side, and two encodings of one schema are two things that
/// can disagree. `cursor.proto`'s own comment records that the server was
/// measured accepting either field.
///
/// Empty in, empty out — and an empty repeated field encodes to nothing at
/// all, which is why a request declaring no tools sends bytes identical to the
/// ones this wire sent before the bridge.
pub(super) fn declaration(tools: &[ToolDefinition]) -> Vec<proto::McpToolDefinition> {
    tools
        .iter()
        .map(|tool| {
            proto::McpToolDefinition::default()
                .with_name(&tool.name)
                .with_description(&tool.description)
                .with_provider_identifier(PROVIDER_IDENTIFIER)
                .with_tool_name(&tool.name)
                .with_input_schema_json(tool.schema.to_string())
        })
        .collect()
}

/// The bytes of the stream's opening message, assembled from `request` and
/// the state `composed` from it.
///
/// The state's two lists are the composition's blob ids verbatim; the action
/// is whichever the composition decided — a user message stamped with a
/// fresh id, the reference's random `crypto.randomUUID()` (`proxy.ts:849`),
/// or the fieldless resume — and `conversation_id` is the composition's, the
/// field the reference sends on every request (`proxy.ts:877`).
///
/// # Errors
///
/// Returns [`ProviderError::Transport`] when no message id can be minted;
/// see [`fresh_id`].
pub(super) fn run_message(
    request: &ChatRequest,
    composed: &Composed,
) -> Result<Vec<u8>, ProviderError> {
    let model = proto::ModelEntry::default()
        .with_model_id(&request.model)
        .with_display_model_id(&request.model)
        .with_display_name(&request.model)
        .with_display_name_short(&request.model);

    let action = match &composed.action {
        Action::User { text } => proto::ConversationAction {
            user_message_action: buffa::MessageField::some(proto::UserMessageAction {
                user_message: buffa::MessageField::some(
                    proto::UserMessage::default().with_text(text).with_message_id(fresh_id()?),
                ),
                ..Default::default()
            }),
            ..Default::default()
        },
        // No newer user turn to send: the server continues from the composed
        // state, which already holds the assistant's step and what its tools
        // answered. Fieldless, as both the reference and the shipped client's
        // own retry send it.
        Action::Resume => proto::ConversationAction {
            resume_action: buffa::MessageField::some(proto::ResumeAction::default()),
            ..Default::default()
        },
    };

    let mut run = proto::RunRequest {
        conversation_state: buffa::MessageField::some(proto::ConversationState {
            root_prompt_messages_json: composed.root.clone(),
            turns: composed.turns.clone(),
            ..Default::default()
        }),
        action: buffa::MessageField::some(action),
        model_details: buffa::MessageField::some(model),
        conversation_id: composed.conversation_id.clone(),
        requested_model: buffa::MessageField::some(
            proto::RequestedModel::default().with_model_id(&request.model),
        ),
        ..Default::default()
    };

    // The roster, on the run request's own channel — declared before the first
    // token, so it does not depend on winning the request_context_args race.
    // Absent entirely on a request offering no tools; `declaration` says why
    // that is byte-identical to the request this wire sent before the bridge.
    let declared = declaration(&request.tools);
    if !declared.is_empty() {
        run.mcp_tools = buffa::MessageField::some(proto::McpTools {
            mcp_tools: declared,
            ..Default::default()
        });
    }

    Ok(proto::ClientMessage { run_request: buffa::MessageField::some(run), ..Default::default() }
        .encode_to_vec())
}

/// The bytes answering the server's context ask: the ids echoed the way the
/// plugin echoes them (`src/proxy.ts:1307-1310`), and the system prompt on
/// `RequestContext.cloud_rule`, the channel cursor's agent honors
/// (`src/proxy.ts:1133`).
///
/// An absent or empty prompt mirrors the plugin's no-prompt answer — its
/// `cloudRule` is `undefined` then, so the member is absent while the
/// context message itself is still present and still a success.
///
/// `roster` is this request's tools on the declaration's *second* channel,
/// which the shipped client refreshes on every context answer — empty for a
/// request that declared none, at no cost in bytes ([`declaration`]) — and it
/// is also what decides `web_fetch_enabled`, through the same
/// predicate [`super::serves_fetch`] answers off the request. The other two
/// switchboard members are literals, and `web_search_enabled = 17` is
/// deliberately not among them; `cursor.proto` states the reasoning on
/// `RequestContext` itself, and a test in this module's tests asserts its
/// absence so a later tidy-up reddens.
pub(super) fn context_answer(
    ask: decode::ContextAsk,
    system: Option<&str>,
    roster: &[ToolDefinition],
) -> Vec<u8> {
    let context = proto::RequestContext {
        cloud_rule: system.map(str::to_owned).filter(|text| !text.is_empty()),
        tools: declaration(roster),
        mcp_file_system_options: buffa::MessageField::some(
            proto::McpFileSystemOptions::default().with_enabled(false),
        ),
        web_fetch_enabled: Some(super::serves_fetch_for(roster)),
        read_lints_enabled: Some(false),
        ..Default::default()
    };
    let answer = proto::ExecResponse {
        id: ask.id,
        request_context_result: buffa::MessageField::some(proto::ContextResult {
            success: buffa::MessageField::some(proto::ContextSuccess {
                request_context: buffa::MessageField::some(context),
                ..Default::default()
            }),
            ..Default::default()
        }),
        exec_id: ask.exec_id,
        ..Default::default()
    };

    proto::ClientMessage { exec_response: buffa::MessageField::some(answer), ..Default::default() }
        .encode_to_vec()
}

/// The messages refusing one tool exec: the kind's own rejection followed by
/// the stream close that ends every exec (**D550**), or — for a kind this
/// build models no arm for — D486's control-channel throw followed by that
/// same close.
///
/// Always the close, and always last ([`stream_close`] says why), so a
/// refused `shell_stream_args` is exactly one `ShellStream{rejected}` event
/// and then the close, with nothing between — the streamed kind's shape for
/// "it did not run", where a served one would have written stdout events
/// first.
pub(super) fn refusal_answer(ask: &decode::ExecAsk) -> Vec<Vec<u8>> {
    let reason = refusal_reason(&ask.kind);

    refusal_answer_because(ask, &reason)
}

/// The same, under a reason of the caller's own.
///
/// For a refusal that is about *this exec's arguments* rather than about its
/// kind: [`refusal_reason`] says ganja does not run the kind at all, which is
/// a falsehood when the kind is one this build serves and only these arguments
/// are unusable ([`super::native::argument_refusal`]) — or when the tool is
/// served and what is missing is the pause ([`unbridgeable_reason`]).
pub(super) fn refusal_answer_because(ask: &decode::ExecAsk, reason: &str) -> Vec<Vec<u8>> {
    tracing::debug!(
        provider = ID,
        exec = ask.id,
        kind = ask.kind,
        typed = !matches!(ask.args, decode::ExecArgs::Unmodelled),
        call = tool_call_id(&ask.args),
        "refusing an exec cursor asked this client to run"
    );

    let closed = stream_close(ask.id);
    let refused = match rejection(ask, reason) {
        Some(response) => proto::ClientMessage {
            exec_response: buffa::MessageField::some(response),
            ..Default::default()
        },
        None => proto::ClientMessage {
            exec_control: buffa::MessageField::some(proto::ExecControl {
                throw: buffa::MessageField::some(proto::ExecThrow {
                    id: ask.id,
                    error: Some(reason.to_owned()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    };

    vec![refused.encode_to_vec(), closed.encode_to_vec()]
}

/// The kind's own rejection, at the kind's own result field — or `None` for
/// a kind with no modelled arm, whose refusal rides the throw instead.
///
/// Both ids are echoed, the way the context answer echoes them: an
/// `ExecClientMessage` has an `exec_id = 15` to put one in, which is the
/// difference from the throw.
fn rejection(ask: &decode::ExecAsk, reason: &str) -> Option<proto::ExecResponse> {
    let mut response =
        proto::ExecResponse { id: ask.id, exec_id: ask.exec_id.clone(), ..Default::default() };

    match &ask.args {
        decode::ExecArgs::Unmodelled => return None,
        decode::ExecArgs::Shell { command, working_directory } => {
            response.shell_result = buffa::MessageField::some(proto::ShellResult {
                rejected: buffa::MessageField::some(shell_rejected(
                    command,
                    working_directory,
                    reason,
                )),
                ..Default::default()
            });
        }
        decode::ExecArgs::ShellStream { command, working_directory } => {
            response.shell_stream = buffa::MessageField::some(proto::ShellStream {
                rejected: buffa::MessageField::some(shell_rejected(
                    command,
                    working_directory,
                    reason,
                )),
                ..Default::default()
            });
        }
        decode::ExecArgs::Write { path, .. } => {
            response.write_result = buffa::MessageField::some(proto::WriteResult {
                rejected: buffa::MessageField::some(
                    proto::WriteRejected::default().with_path(path).with_reason(reason),
                ),
                ..Default::default()
            });
        }
        decode::ExecArgs::Delete { path } => {
            response.delete_result = buffa::MessageField::some(proto::DeleteResult {
                rejected: buffa::MessageField::some(
                    proto::DeleteRejected::default().with_path(path).with_reason(reason),
                ),
                ..Default::default()
            });
        }
        decode::ExecArgs::Grep { .. } => {
            response.grep_result = buffa::MessageField::some(proto::GrepResult {
                error: buffa::MessageField::some(proto::GrepError::default().with_error(reason)),
                ..Default::default()
            });
        }
        decode::ExecArgs::Read { redacted, path, .. } => {
            let rejected = buffa::MessageField::some(read_rejected(path, reason));
            if *redacted {
                response.redacted_read_result = rejected;
            } else {
                response.read_result = rejected;
            }
        }
        decode::ExecArgs::Ls { path } => {
            response.ls_result = buffa::MessageField::some(proto::LsResult {
                rejected: buffa::MessageField::some(
                    proto::LsRejected::default().with_path(path).with_reason(reason),
                ),
                ..Default::default()
            });
        }
        decode::ExecArgs::Mcp(_) => {
            response.mcp_result = buffa::MessageField::some(proto::McpResult {
                rejected: buffa::MessageField::some(
                    proto::McpRejected::default().with_reason(reason),
                ),
                ..Default::default()
            });
        }
        decode::ExecArgs::Fetch { url } => {
            response.fetch_result = buffa::MessageField::some(proto::FetchResult {
                error: buffa::MessageField::some(
                    proto::FetchError::default().with_url(url).with_error(reason),
                ),
                ..Default::default()
            });
        }
    }

    Some(response)
}

/// The rejection both shell kinds carry; only the field it is set on differs.
fn shell_rejected(command: &str, working_directory: &str, reason: &str) -> proto::ShellRejected {
    proto::ShellRejected::default()
        .with_command(command)
        .with_working_directory(working_directory)
        .with_reason(reason)
}

/// The rejection both read kinds carry, likewise.
fn read_rejected(path: &str, reason: &str) -> proto::ReadResult {
    proto::ReadResult {
        rejected: buffa::MessageField::some(
            proto::ReadRejected::default().with_path(path).with_reason(reason),
        ),
        ..Default::default()
    }
}

/// What the server's agent loop is told about a refused exec of `kind`.
///
/// It names ganja, so the sentence reads as a client's policy rather than a
/// malfunction; it names the kind, so the loop can tell a refused shell from
/// a refused file read and choose differently; and it says *why*, because a
/// loop that reads "this client cannot" retries and a loop that reads "this
/// client will not, and its tools run elsewhere" stops asking. Ganja's own
/// words — the shipped client's decline reasons are the user's free text,
/// so there is nothing here to port.
fn refusal_reason(kind: &str) -> String {
    format!(
        "ganja does not run {kind} for a provider: its tools run for its own session, under its \
         own permission engine."
    )
}

/// What an MCP call naming `name` is refused with when **this request declared
/// no roster at all**.
///
/// The name is the honest subject: with nothing declared there is no roster to
/// be missing from, so the answer is about the name that was called rather
/// than about a policy on running it. A request that *did* declare a roster
/// answers an unknown name on the `tool_not_found` arm instead, carrying that
/// roster — which is the arm this sentence used to be a stand-in for
/// (**D552**, [`super::bridge`]).
pub(super) fn mcp_refusal_reason(name: &str) -> String {
    format!("no tool named {name} is served by this client")
}

/// What a call this client **does** serve, `name`, is refused with when the
/// wire cannot hold its Run open.
///
/// [`mcp_refusal_reason`]'s sentence would be false here: the tool is on the
/// roster, and what is missing is the pause. A wire with no key to park under
/// is a fixture replay or a request carrying no message to key on, so this arm
/// is reachable by no shipped session — but a false sentence is not made
/// harmless by being rare, and the reason a loop reads has to be the reason
/// that holds (**D552**).
pub(super) fn unbridgeable_reason(name: &str) -> String {
    format!(
        "ganja serves {name}, but this request could not be paused to run it: the tool's answer \
         would have had nowhere to go"
    )
}

/// The call id an MCP exec carried, for the refusal's log line — the one
/// value that correlates a refusal with the tool call the model made. Every
/// other kind identifies itself by its path or command, which the rejection
/// already echoes.
fn tool_call_id(args: &decode::ExecArgs) -> Option<&str> {
    match args {
        decode::ExecArgs::Mcp(call) => Some(call.tool_call_id.as_str()),
        _ => None,
    }
}

/// The message that ends an exec, whatever it was answered with.
///
/// Always sent, and always last: the shipped client writes it after a
/// handler's final frame and after a no-handler throw alike
/// (`index.js@4272747`), because the close is what tells the server the exec
/// is over rather than still running. `pub(super)` because the bridge's own
/// answers ([`super::native`], [`super::bridge`]) end the same way.
pub(super) fn stream_close(exec: Option<u32>) -> proto::ClientMessage {
    proto::ClientMessage {
        exec_control: buffa::MessageField::some(proto::ExecControl {
            stream_close: buffa::MessageField::some(proto::ExecStreamClose {
                id: exec,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The run-level liveness ping, `ClientMessage.client_heartbeat = 7`.
///
/// Sent every [`super::bridge::HEARTBEAT`] while a Run's request body is open,
/// which is what keeps a held Run alive while ganja's engine runs a bridged
/// tool. A live 25-second hold survived on this alone (**D552**'s Dv-10; the
/// recording is `tests/fixtures/cursor-mcp-tools-probe.txt`, (b)).
pub(super) fn run_heartbeat() -> proto::ClientMessage {
    proto::ClientMessage {
        client_heartbeat: buffa::MessageField::some(proto::ClientHeartbeat::default()),
        ..Default::default()
    }
}

/// The bytes answering one kv exchange, serviced against `blobs` — the
/// turn's in-memory blob store — the way the plugin's `handleKvMessage`
/// services its own (proxy.ts:1087-1120): a set stores the bytes and acks
/// with the empty result (proxy.ts:1113-1117), a get returns what was
/// stored or the not-found shape — a present result holding no data
/// (proxy.ts:1101-1105) — and every answer echoes the id the server minted
/// (proxy.ts:1075-1077).
///
/// The blob bytes are conversation state and never reach a log line: what
/// is logged is the id's leading hex and the sizes, the plugin's own debug
/// discipline.
pub(super) fn kv_answer(ask: decode::KvAsk, blobs: &mut HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let answer = match ask.op {
        decode::KvOp::Get { blob_id } => {
            let found = blobs.get(&blob_id).cloned();
            tracing::debug!(
                provider = ID,
                blob = blob_key(&blob_id),
                found = found.as_deref().map(<[u8]>::len),
                "answering the server's kv get"
            );
            let result = match found {
                Some(data) => proto::GetBlobResult::default().with_blob_data(data),
                None => proto::GetBlobResult::default(),
            };

            proto::KvResponse {
                id: ask.id,
                get_blob_result: buffa::MessageField::some(result),
                ..Default::default()
            }
        }
        decode::KvOp::Set { blob_id, data } => {
            tracing::debug!(
                provider = ID,
                blob = blob_key(&blob_id),
                size = data.len(),
                "answering the server's kv set"
            );
            // A set replaces what the id held, the reference's own store
            // (`proxy.ts:1108`, `blobStore.set`) — the measured-working shape,
            // and the one under which a server re-setting a key it treats as
            // mutable reads its own newest bytes back. A composed history
            // blob is content-addressed, so a re-set of one carries the bytes
            // already there; the one case that would not — a differing
            // re-set of an id this Run holds — is made visible by size, never
            // by content, which is how a live probe settles whether it ever
            // happens.
            if let Some(held) = blobs.insert(blob_id.clone(), data)
                && held != blobs[&blob_id]
            {
                tracing::debug!(
                    provider = ID,
                    blob = blob_key(&blob_id),
                    was = held.len(),
                    now = blobs[&blob_id].len(),
                    "a server set replaced bytes this run already held"
                );
            }

            proto::KvResponse {
                id: ask.id,
                set_blob_result: buffa::MessageField::some(proto::SetBlobResult::default()),
                ..Default::default()
            }
        }
    };

    proto::ClientMessage { kv_response: buffa::MessageField::some(answer), ..Default::default() }
        .encode_to_vec()
}

/// A blob id's leading eight bytes as hex — sixteen characters, the width
/// the plugin's own kv debug lines truncate to. Enough to correlate a get
/// with the set that stored it — or with the composition that minted it,
/// which logs its ids in the same spelling — and never the data.
pub(super) fn blob_key(id: &[u8]) -> String {
    id.iter().take(8).fold(String::with_capacity(16), |mut rendered, byte| {
        let _ = write!(rendered, "{byte:02x}");
        rendered
    })
}

/// The indices of the conversation's newest user **turn**: every user
/// message from the last one back to the reply before it — but never back
/// past [`ChatRequest::turn_start`] — or [`None`] when the conversation holds
/// no user message at all.
///
/// A run rather than one message, because the engine adds to a turn by
/// appending user messages rather than by editing the last one — a steer
/// drained at a step boundary, and the team guards' request-only block after
/// a reply (D547) — and a wire that sent only the newest of them would answer
/// a guard block while dropping the steer beside it, which is what this did
/// until 2026-09-02. What came before the run is **history**, and since
/// **D553** it travels too: [`history::entries`] reads this same bound to
/// decide where history ends and the action begins, so the two cannot
/// disagree about which message is the last of the conversation and which
/// the first of the turn.
///
/// **The run's lower bound is two facts, not one, and the second cannot be
/// read off `messages`.** The reply is the near bound; the turn's own opening
/// is the far one. A finished turn that took a steer leaves the steer in
/// history *after* its reply, so the next turn's request reads `[prompt,
/// reply, steer, prompt2]` — the same four roles, in the same order, as the
/// within-turn `[prompt, reply, steer, block]`, every one of them a
/// `Message::user` whose id and timestamp ascend across the boundary exactly
/// as they do within it. Nothing here distinguishes them, which is why the
/// engine states where this turn began and this walk is clamped to it rather
/// than guessing. A continuation block emitted where nothing was steered —
/// `[prompt, reply, block]` — is still a run of one, the block; the prompt
/// and the reply it is about are the history composed beside it.
pub(super) fn newest_user_run(request: &ChatRequest) -> Option<RangeInclusive<usize>> {
    let messages = &request.messages;
    let newest = messages.iter().rposition(|message| matches!(message.role, Role::User))?;
    let first = messages[..newest]
        .iter()
        .rposition(|message| !matches!(message.role, Role::User))
        .map_or(0, |reply| reply + 1)
        // Never past this turn's own opening: a steer the *previous* turn
        // consumed sits after that turn's reply, so the walk above would
        // reach back through it and re-send it as part of this prompt.
        .max(request.turn_start)
        // And never past the newest user message itself. `turn_start` is a
        // `pub` field on a `pub` struct, so its value is a caller's and not
        // this module's: a request whose last message is an assistant's, with
        // a marker pointing past the user message before it, would otherwise
        // slice `first > newest` and panic the wire. A run of one is the
        // honest answer to that — the newest user turn is still the newest
        // user message — where a panic is no answer at all.
        .min(newest);

    Some(first..=newest)
}

/// The text of `run`, [`newest_user_run`]'s slice: its messages' text parts
/// in order, joined the way distinct parts read as distinct paragraphs.
///
/// Takes the run rather than finding it, because its one caller —
/// `history::entries` — has already scanned for the run to decide where
/// history ends, and the boundary is one scan rather than two. A
/// conversation with no user message at all, which the engine never builds,
/// has no run to pass; that caller composes the empty message for it, which
/// is more honest than refusing a request this module was still asked to
/// encode.
pub(super) fn newest_user_text(request: &ChatRequest, run: RangeInclusive<usize>) -> String {
    request.messages[run].iter().flat_map(history::texts).collect::<Vec<_>>().join("\n\n")
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
