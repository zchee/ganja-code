//! `ChatRequest` → the Run stream's opening message.
//!
//! Spec: `.omc/research/cursor/spike-wire-facts.md` — the server refuses a
//! stream whose first message is not a run request, so this module builds
//! exactly that one message. What it carries is the minimal set the message
//! definitions in `cursor.proto` model: the model asked for (named twice,
//! because the server still reads the deprecated description beside the
//! forward-looking one), an empty conversation state marked present, and
//! the newest user message inline.
//!
//! **The newest user message, deliberately.** Everything a conversation
//! already holds — earlier turns, tool calls and their results — travels on
//! cursor's wire as content-addressed state over the stream's kv half.
//! [`kv_answer`] speaks that channel's serving side — mid-turn the server
//! stores blobs with this client and reads its own back, and it will not
//! end the turn while one is unanswered — but composing *history* into
//! blobs the request could name is still ahead, so the request carries what
//! it can carry truthfully and the rest arrives with the state machinery.
//!
//! **The advertised tools, on the other hand, are sent** (**D552**).
//! [`declaration`] turns `ChatRequest.tools` into the roster cursor's own
//! client-declared MCP channel carries — on the run request's
//! `mcp_tools = 4` and on every `RequestContext.tools = 7` answer, which is
//! where the shipped client puts them — so the server's agent loop calls
//! ganja's tools by name and this client answers from what its own engine ran.
//! A request that declares no tools declares no roster, which is what a title
//! or summary one-shot does and is byte-identical to what this module sent
//! before the bridge existed.
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
//! # Tool execs are refused with the kind's typed arm (**D550**, amending **D486**)
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
//! **What diverges, and what no longer does.** There is no upstream
//! counterpart to weigh this against — upstream opencode v1.18.22 has no
//! cursor wire at all, so no ported behavior is being contradicted. The
//! divergence is from *cursor's own shipped client*, which executes these
//! asks against the user's machine and streams the results back. Ganja
//! answers them too now (**D552**, [`super::native`]) — but never here, and
//! never in this crate: a bridged exec is handed to ganja's engine as an
//! ordinary tool call, which is what puts it under the permission dialog, the
//! rules and the transcript the session already has. What this module still
//! refuses is everything the engine has no tool for, which is where D550's
//! typed arms and D486's throw stayed.
//!
//! **Why a refusal rather than a failure.** An unanswered exec is a hang:
//! the server holds generation until the client says something, so the
//! choice is never between refusing and staying quiet. And the reason
//! string names ganja and the kind, because it is read by the server's own
//! agent loop — a refusal is information that loop can act on, the way a
//! denied tool call is information ganja's own loop acts on, and the turn
//! survives it.
//!
//! **Why the kind's own arm rather than a throw.** D486 refused every exec
//! on the control channel, copying the one branch of the shipped
//! dispatcher that had been read: a `throw` carrying the exec id and a
//! reason, then a `stream_close` carrying the id (`index.js@4272747`). That
//! branch is the client's **no-handler** path — what it writes when nothing
//! claims a server message at all. A *decline* is answered elsewhere and
//! differently: the handler returns the kind's typed `rejected` arm with the
//! reason in it (`index.js@5487600` for a delete, `@5329600` for a shell,
//! twelve such sites across the handlers). The distinction is what the
//! model on the other end reads — a rejection is a tool outcome it adapts
//! to, a throw is a client that broke — and this build was sending the
//! broken-client shape for a decision it had made deliberately.
//!
//! So [`refusal_answer`] answers ten kinds in their own vocabulary:
//! `ExecResponse` carrying the kind's rejected arm at the kind's own field
//! number, echoing back what the args named — the command, the path, the
//! url — then the `stream_close` that ends every exec, refused or served.
//! Two of the ten have no rejected arm in the shipped descriptor at all
//! (`grep_result`, `fetch_result`), so their refusal travels as the error
//! arm, which is the only place their result can say anything.
//!
//! Seven of those ten now reach this function only when the bridge declined
//! them — the tool they map to is not on this request's roster — which is what
//! makes "refused" still mean something after **D552**: it is a statement
//! about what this turn is offering, not about what this client can do.
//!
//! **The throw survives as the catch-all**, and that is the half of D486
//! that was load-bearing: its channel is keyed on the numeric id alone,
//! naming neither kind nor `exec_id`, so an exec of a kind no table here
//! knows — one newer than this file — is still refusable, and no exec kind
//! is left to fail a turn.
//!
//! # The switchboard: asking the server for less
//!
//! [`context_answer`] also fills three members of `RequestContext` that
//! narrow what the server's loop will ask this client for at all
//! (`cursor.proto`'s own comment on that message says which and why). The
//! one that is not a literal is `web_fetch_enabled`, which the caller
//! computes with [`super::serves_fetch`] from the request's own tool
//! roster: a fetch exec has somewhere to go exactly when this turn declared
//! tools. A refusal answered well is still worse than an ask never made.

use std::collections::HashMap;
use std::fmt::Write as _;

use buffa::Message as _;

use super::{ID, decode, proto};
use crate::auth::pkce;
use crate::protocol::{PartBody, Role};
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
    let mut bytes =
        pkce::random_bytes::<16>().map_err(|error| ProviderError::Transport(error.to_string()))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let mut rendered = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            rendered.push('-');
        }
        write!(rendered, "{byte:02x}").expect("writing hex into a String cannot fail");
    }

    Ok(rendered)
}

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

/// The bytes of the stream's opening message, assembled from `request`.
///
/// # Errors
///
/// Returns [`ProviderError::Transport`] when no message id can be minted;
/// see [`fresh_id`].
pub(super) fn run_message(request: &ChatRequest) -> Result<Vec<u8>, ProviderError> {
    let model = proto::ModelEntry::default()
        .with_model_id(&request.model)
        .with_display_model_id(&request.model)
        .with_display_name(&request.model)
        .with_display_name_short(&request.model);

    let action = proto::ConversationAction {
        user_message_action: buffa::MessageField::some(proto::UserMessageAction {
            user_message: buffa::MessageField::some(
                proto::UserMessage::default()
                    .with_text(newest_user_text(request))
                    .with_message_id(fresh_id()?),
            ),
            ..Default::default()
        }),
        ..Default::default()
    };

    let mut run = proto::RunRequest {
        conversation_state: buffa::MessageField::some(proto::ConversationState::default()),
        action: buffa::MessageField::some(action),
        model_details: buffa::MessageField::some(model),
        requested_model: buffa::MessageField::some(
            proto::RequestedModel::default().with_model_id(&request.model),
        ),
        ..Default::default()
    };

    // The roster, on the run request's own channel — declared before the first
    // token, so it does not depend on winning the request_context_args race.
    // Absent entirely on a request offering no tools, which is what keeps a
    // title or summary one-shot byte-identical to what it always was.
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
/// `web_fetch` is [`super::serves_fetch`]'s verdict on the request this
/// turn opened with, computed once at stream start and carried here as a
/// plain `bool` — the wire cannot name an engine type and does not need to.
/// The other two switchboard members are literals, and
/// `web_search_enabled = 17` is deliberately not among them; `cursor.proto`
/// states the reasoning on `RequestContext` itself, and a test in this
/// module's tests asserts its absence so a later tidy-up reddens.
///
/// `roster` is this request's tools on the declaration's *second* channel,
/// which the shipped client refreshes on every context answer. Empty for a
/// request that declared none, and an empty repeated field encodes to nothing
/// at all.
pub(super) fn context_answer(
    ask: decode::ContextAsk,
    system: Option<&str>,
    web_fetch: bool,
    roster: &[ToolDefinition],
) -> Vec<u8> {
    let context = proto::RequestContext {
        cloud_rule: system.map(str::to_owned).filter(|text| !text.is_empty()),
        tools: declaration(roster),
        mcp_file_system_options: buffa::MessageField::some(
            proto::McpFileSystemOptions::default().with_enabled(false),
        ),
        web_fetch_enabled: Some(web_fetch),
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
/// Always the close, and always last: the shipped client writes it after a
/// handler's final frame and after a no-handler throw alike
/// (`index.js@4272747`), because the close is what tells the server the exec
/// is over rather than still running. A refused `shell_stream_args` is
/// therefore exactly one `ShellStream{rejected}` event and then the close,
/// with nothing between — the streamed kind's shape for "it did not run",
/// where a served one would have written stdout events first.
pub(super) fn refusal_answer(ask: &decode::ExecAsk) -> Vec<Vec<u8>> {
    let reason = refusal_reason(&ask.kind);

    refusal_answer_because(ask, &reason)
}

/// The same, under a reason of the caller's own.
///
/// For a refusal that is about *this exec's arguments* rather than about its
/// kind: [`REFUSAL`] says ganja does not run the kind at all, which is a
/// falsehood when the kind is one this build serves and only these arguments
/// are unusable ([`super::native::argument_refusal`]).
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
        decode::ExecArgs::Read { path, .. } => {
            response.read_result = buffa::MessageField::some(read_rejected(path, reason));
        }
        decode::ExecArgs::RedactedRead { path, .. } => {
            response.redacted_read_result = buffa::MessageField::some(read_rejected(path, reason));
        }
        decode::ExecArgs::Ls { path } => {
            response.ls_result = buffa::MessageField::some(proto::LsResult {
                rejected: buffa::MessageField::some(
                    proto::LsRejected::default().with_path(path).with_reason(reason),
                ),
                ..Default::default()
            });
        }
        decode::ExecArgs::Mcp(call) => {
            response.mcp_result = buffa::MessageField::some(proto::McpResult {
                rejected: buffa::MessageField::some(
                    // `called()` and not `name`: the roster is matched under
                    // the declaration's own `tool_name`, so naming the other
                    // field here would refuse one tool by another's name.
                    proto::McpRejected::default().with_reason(mcp_refusal_reason(call.called())),
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

/// What the server's agent loop is told about a refused exec, `{kind}`
/// substituted.
///
/// It names ganja, so the sentence reads as a client's policy rather than a
/// malfunction; it names the kind, so the loop can tell a refused shell from
/// a refused file read and choose differently; and it says *why*, because a
/// loop that reads "this client cannot" retries and a loop that reads "this
/// client will not, and its tools run elsewhere" stops asking. Ganja's own
/// words — the shipped client's decline reasons are the user's free text,
/// so there is nothing here to port.
const REFUSAL: &str = "ganja does not run {kind} for a provider: its tools run for its own \
                       session, under its own permission engine.";

/// What an MCP call is refused with when **this request declared no roster at
/// all**, `{name}` substituted.
///
/// The name is the honest subject: with nothing declared there is no roster to
/// be missing from, so the answer is about the name that was called rather
/// than about a policy on running it. A request that *did* declare a roster
/// answers an unknown name on the `tool_not_found` arm instead, carrying that
/// roster — which is the arm this sentence used to be a stand-in for
/// (**D552**, [`super::bridge`]).
const MCP_REFUSAL: &str = "no tool named {name} is served by this client";

/// What a call this client **does** serve is refused with when the wire cannot
/// hold its Run open, `{name}` substituted.
///
/// [`MCP_REFUSAL`]'s sentence would be false here: the tool is on the roster,
/// and what is missing is the pause. A wire with no key to park under is a
/// fixture replay or a request carrying no message to key on, so this arm is
/// reachable by no shipped session — but a false sentence is not made harmless
/// by being rare, and the reason a loop reads has to be the reason that holds
/// (**D552**).
const UNBRIDGEABLE: &str = "ganja serves {name}, but this request could not be paused to run it: \
                            the tool's answer would have had nowhere to go";

/// [`REFUSAL`] with the kind in it.
fn refusal_reason(kind: &str) -> String {
    REFUSAL.replace("{kind}", kind)
}

/// [`MCP_REFUSAL`] with the called tool's name in it.
pub(super) fn mcp_refusal_reason(name: &str) -> String {
    MCP_REFUSAL.replace("{name}", name)
}

/// [`UNBRIDGEABLE`] with the called tool's name in it.
pub(super) fn unbridgeable_reason(name: &str) -> String {
    UNBRIDGEABLE.replace("{name}", name)
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
            blobs.insert(blob_id, data);

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
/// with the set that stored it, and never the data.
fn blob_key(id: &[u8]) -> String {
    id.iter().take(8).fold(String::with_capacity(16), |mut rendered, byte| {
        let _ = write!(rendered, "{byte:02x}");
        rendered
    })
}

/// The text of the conversation's newest user **turn**: every user message
/// from the last one back to the reply before it — but never back past
/// [`ChatRequest::turn_start`] — their text parts in order, joined the way
/// distinct parts read as distinct paragraphs.
///
/// A run rather than one message, because the engine adds to a turn by
/// appending user messages rather than by editing the last one — a steer
/// drained at a step boundary, and the team guards' request-only block after
/// a reply (D547) — and a wire that sent only the newest of them would answer
/// a guard block while dropping the steer beside it, which is what this did
/// until 2026-09-02. What came before the run is history this wire does not
/// carry yet.
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
/// than guessing.
///
/// **One shape the clamp does not close**, named rather than left to be
/// discovered: a continuation block emitted on the arm where nothing was
/// steered makes the request `[prompt, reply, block]`, and the block still
/// reaches this wire without the prompt it is about. The clamp raises the
/// run's lower bound and never lowers it — lowering it here would mean
/// reaching back past the assistant's reply, whose text this wire does not
/// send — so closing that one means carrying more than the newest user turn,
/// which is the history-over-blobs work this module's own header defers.
///
/// Empty when the conversation holds no user message at all, which is not a
/// request the engine builds — sending the empty message is more honest than
/// refusing a request this module was still asked to encode.
fn newest_user_text(request: &ChatRequest) -> String {
    let messages = &request.messages;
    let Some(newest) = messages.iter().rposition(|message| matches!(message.role, Role::User))
    else {
        return String::new();
    };
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

    messages[first..=newest]
        .iter()
        .flat_map(|message| message.parts.iter())
        // Every variant is named, and the wildcard that used to stand here is
        // gone on purpose: this was the one place in the workspace where a
        // new `PartBody` would compile silently into "not text", and a part
        // this wire ought to send is not something to discover from a user's
        // bug report.
        .filter_map(|part| match &part.body {
            PartBody::Text { text } => Some(text.as_str()),
            // A peer's words are rendered into the user turn at request
            // assembly (D495); a wire never encodes a peer part as a message
            // of its own.
            PartBody::Peer { .. }
            | PartBody::File { .. }
            | PartBody::Tool { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Reasoning { .. }
            | PartBody::ReasoningText { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
