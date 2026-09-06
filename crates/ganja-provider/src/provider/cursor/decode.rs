//! The cursor wire's responses, decoded as they arrive.
//!
//! Spec: `.omc/research/cursor/spike-wire-facts.md`. Two shapes: the unary
//! model listing is bare protobuf with no framing at all (LIVE-OBSERVED —
//! the reference client's tolerance for a framed unary response is dead code
//! on the real server), decoded whole because a unary body is one message;
//! and the Run stream is Connect frames whose verdict rides the in-body
//! EndStream frame, mapped one frame at a time by [`Mapping`] so the reply
//! reaches the session while the server is still talking.

use buffa::Message as _;

use super::{ID, connect, proto};
use crate::protocol::FinishReason;
use crate::provider::{ProviderError, ProviderEvent};

/// The models the listing served, in the server's order.
///
/// # Errors
///
/// Returns [`ProviderError::Parse`] when the body is not the listing's
/// protobuf.
pub(super) fn model_list(body: &[u8]) -> Result<Vec<proto::ModelEntry>, ProviderError> {
    let decoded = proto::GetUsableModelsResponse::decode_from_slice(body).map_err(|error| {
        ProviderError::Parse(format!("the model listing did not decode: {error}"))
    })?;

    Ok(decoded.models)
}

/// Turns Run frames into events, one frame at a time.
///
/// The shape is the SSE wires' `Mapper`, spelled for Connect frames:
/// [`frame`](Self::frame) appends what one frame means, and
/// [`truncated`](Self::truncated) judges a body that ended without its
/// EndStream frame. [`ProviderEvent::Finish`] and [`ProviderEvent::Failed`]
/// are terminal — the stream layer hands out nothing after either.
///
/// **The exec arm is never skipped.** The server sends its context ask as an
/// exec request and waits on the answer before generating — a build that
/// skipped it hung a real turn in silence (LIVE-OBSERVED 2026-08-10, one
/// debug line then nothing until the process was killed). So [`frame`](Self::frame)
/// hands the ask up as a [`ContextAsk`] for the stream layer to answer on
/// the open request body. Every *other* exec kind — the tools cursor's server
/// asks its own client to run — is handed up as an [`ExecAsk`] instead, for
/// the stream layer to answer on that same body.
///
/// **This layer classifies; it does not decide.** [`exec_args`] reads each
/// kind's arguments into an [`ExecArgs`] and stops there, because what happens
/// next depends on a fact only the stream layer holds: the tool roster *this
/// request* declared. With one, an `mcp_args` naming a declared tool and the
/// seven native kinds of the redirect table are **bridged** — surfaced to
/// ganja's engine as ordinary tool calls, run there under its permission
/// engine, and answered here from the result (**D552**). Without one, or for a
/// kind outside the table, the same value is answered with **D550**'s typed
/// refusal in the kind's own vocabulary, falling back to **D486**'s
/// control-channel throw for a kind with no modelled arm. Either way the
/// server's agent loop reads an outcome it can act on and keeps generating,
/// which is one more turn surviving than the failure both of those replaced.
///
/// **The kv arm is never skipped either.** The server stores and reads
/// conversation state mid-turn over the kv channel and waits on every
/// exchange before it will end the turn — the 2026-08-10 live run left four
/// of them unanswered and then sat silent until timeout. So a kv get or set
/// is handed up as an [`Ask`] beside the context ask, answered by the
/// stream layer against the turn's blob store, and a kv kind beyond get and
/// set fails the turn with its field number on it — the exec channel's
/// no-hang discipline, applied to the second channel the server waits on.
///
/// Updates this build does not model — the tool-call, summary, token and
/// step arms, and whole server messages outside the update, exec and kv
/// channels — are skipped, not failed: the server adds arms between client
/// versions, and a turn that died on one would make every addition a
/// breaking change. A skipped update is logged at debug with its set field
/// numbers named where the plugin's descriptor knows them, which is where
/// "why is the reply shorter than the server's" is answered — by arm, not
/// by guesswork.
///
/// **`turn_ended` is noted; the verdict waits for the EndStream frame.**
/// The two are the application and the protocol saying different things —
/// "the turn is over" and "here is how the stream ended" — exactly the
/// Anthropic wire's `stop_reason`/`message_stop` split, and they are handled
/// its way: a clean EndStream finishes the turn, a body that dies after
/// `turn_ended` lost only its terminator and finishes too, and an EndStream
/// **error** after `turn_ended` fails the turn — the server's verdict
/// outranks the model's goodbye. The one-shot decode this replaces broke at
/// `turn_ended` and never read that verdict at all.
#[derive(Debug, Default)]
pub(super) struct Mapping {
    /// The server marked the turn ended, so the reply is complete with or
    /// without the terminator.
    ended: bool,
}

/// A mid-stream question the server waits on, carried up to the stream
/// layer: the decode layer reads frames and holds no channel to answer one
/// on, so the layer that owns the request body sends the reply. Three kinds,
/// because the server waits on two channels and one of them carries two
/// different answers — the exec channel's context ask and its tool execs,
/// and the kv channel's blob exchanges.
#[derive(Debug, PartialEq)]
pub(super) enum Ask {
    Context(ContextAsk),
    Kv(KvAsk),
    /// A tool exec: bridged to a ganja tool (**D552**) or refused (**D550**,
    /// **D486**), which the roster decides and this layer does not.
    Exec(ExecAsk),
}

/// The server's context ask, ids and nothing else — presence is the whole
/// question.
#[derive(Debug, PartialEq)]
pub(super) struct ContextAsk {
    /// The exchange ids the answer must echo, verbatim — the plugin's own
    /// answers are built that way (proxy.ts:1307-1310).
    pub(super) id: Option<u32>,
    pub(super) exec_id: Option<String>,
}

/// One tool exec the server asked this client to run: the ids the answer
/// echoes, the kind's name, and that kind's arguments as far as this build
/// reads them.
///
/// **The exec id is present but not always sent.** An answer — a result or a
/// typed refusal — rides `ExecResponse`, which has an `exec_id = 15` to echo
/// the way the context answer does; the control-channel throw has no such
/// member — it is keyed on the numeric id alone (`ExecClientThrow`,
/// `index.js@6032526`) — so on that path the id is decoded and dropped rather
/// than invented a home for.
#[derive(Debug, PartialEq)]
pub(super) struct ExecAsk {
    pub(super) id: Option<u32>,
    pub(super) exec_id: Option<String>,
    pub(super) kind: String,
    pub(super) args: ExecArgs,
}

/// One exec's arguments, as far as two answers need them: the refusal's echo,
/// and the redirect's mapping onto a ganja tool.
///
/// The variants carry only that much, which is why several look alike and one
/// is empty: `delete_args` is refused and never bridged, so its variant holds
/// only the path its rejection echoes. The two shell kinds are separate
/// variants because the arm they answer on differs — a stream's rejection is
/// an *event* — where the two read kinds share one variant and a flag, since
/// a redacted read is the same `ReadResult` at a second field number.
#[derive(Debug, PartialEq)]
pub(super) enum ExecArgs {
    /// No modelled arm for this kind: D486's throw, still the catch-all.
    Unmodelled,
    Shell {
        command: String,
        working_directory: String,
    },
    ShellStream {
        command: String,
        working_directory: String,
    },
    Write {
        path: String,
        file_text: String,
    },
    Delete {
        path: String,
    },
    Grep {
        pattern: String,
        path: String,
        glob: String,
        case_insensitive: bool,
    },
    Read {
        /// `redacted_read_args = 29` rather than `read_args = 7`: the answer
        /// goes back at the matching number.
        redacted: bool,
        path: String,
        offset: Option<i32>,
        limit: Option<u32>,
    },
    Ls {
        path: String,
    },
    Mcp(McpCall),
    Fetch {
        url: String,
    },
}

/// A model calling a tool this client declared (**D552**).
///
/// The arguments are decoded here rather than carried as protobuf, so the
/// stream layer answers one question — is this call ours, and what is it —
/// without reaching back into the wire's own types. [`arguments`](Self::arguments)
/// is [`None`] for a value shape [`super::value::decode`] cannot read, which
/// fails that one call with `McpResult.error` and never the turn.
#[derive(Debug, PartialEq)]
pub(super) struct McpCall {
    /// What the model called, `McpArgs.name = 1`.
    pub(super) name: String,
    /// What the declaration named the tool, `tool_name = 5`. The bridge
    /// matches on this and falls back to [`name`](Self::name); the shipped
    /// client sets both from one declaration (`index.js@5699717`).
    pub(super) tool_name: String,
    /// The id the answer is keyed on and the engine's transcript part carries.
    pub(super) tool_call_id: String,
    /// Who is said to serve the tool. Empty means the server sent none, which
    /// is not the same as sending somebody else's name.
    pub(super) provider_identifier: String,
    /// `smart_mode_approval_only = 7`: a preflight that must execute nothing.
    pub(super) approval_only: bool,
    /// The argument object, or [`None`] when the map could not be read.
    pub(super) arguments: Option<serde_json::Value>,
}

impl McpCall {
    /// The tool this call names: the declaration's own
    /// [`tool_name`](Self::tool_name), falling back to
    /// [`name`](Self::name) when the server sent none.
    ///
    /// One function because two of them disagreed: the roster match read
    /// `tool_name` while the refusal sentence read `name`, so a call the
    /// bridge looked up under one spelling was refused under the other
    /// (**D552**). Every reader of "which tool is this" goes through here.
    pub(super) fn called(&self) -> &str {
        if self.tool_name.is_empty() { &self.name } else { &self.tool_name }
    }
}

/// One kv exchange the server opened: the id the answer must echo
/// (proxy.ts:1075-1077), and which of the two operations the oneof carried.
#[derive(Debug, PartialEq)]
pub(super) struct KvAsk {
    pub(super) id: Option<u32>,
    pub(super) op: KvOp,
}

/// The kv channel's whole vocabulary — get and set are the oneof's only
/// arms (agent_pb.ts:7941, :7948).
#[derive(Debug, PartialEq)]
pub(super) enum KvOp {
    /// The server reading back what it stored; answered from the blob store,
    /// found or not.
    Get { blob_id: Vec<u8> },
    /// The server storing state for the turn; answered with the empty ack.
    Set { blob_id: Vec<u8>, data: Vec<u8> },
}

impl Mapping {
    /// Maps `frame`, appending whatever it means to `events`; a returned
    /// [`Ask`] is the server waiting, and the caller must answer it.
    pub(super) fn frame(
        &mut self,
        frame: &connect::Frame,
        events: &mut Vec<ProviderEvent>,
    ) -> Option<Ask> {
        if frame.is_end_stream() {
            match connect::end_stream_error(&frame.payload) {
                Ok(Some((code, message))) => {
                    events.push(ProviderEvent::Failed(verdict(&code, &message)));
                }
                Ok(None) => events.push(ProviderEvent::Finish(FinishReason::Completed)),
                Err(error) => events.push(ProviderEvent::Failed(error)),
            }
            return None;
        }

        let message = match proto::ServerMessage::decode_from_slice(&frame.payload) {
            Ok(message) => message,
            Err(error) => {
                // Every data frame is a server message and every one of them
                // means something, so stepping past a broken one would
                // silently drop part of the reply.
                events.push(ProviderEvent::Failed(ProviderError::Parse(format!(
                    "a server message did not decode: {error}"
                ))));
                return None;
            }
        };

        if let Some(exec) = message.exec_request.as_option() {
            if exec.request_context_args.is_set() {
                return Some(Ask::Context(ContextAsk {
                    id: exec.id,
                    exec_id: exec.exec_id.clone(),
                }));
            }

            // Every other kind is a tool the server is asking this client to
            // run. The server stops generating until an exec is answered
            // either way, so it is handed up to be answered — bridged to
            // ganja's own tool where the roster has one, refused in the
            // kind's own vocabulary where it does not — and never left
            // silent, which is the hang this arm's modelling exists to end.
            let (kind, args) = exec_args(exec);
            return Some(Ask::Exec(ExecAsk {
                id: exec.id,
                exec_id: exec.exec_id.clone(),
                kind,
                args,
            }));
        }

        if let Some(kv) = message.kv_request.as_option() {
            if let Some(get) = kv.get_blob_args.as_option() {
                return Some(Ask::Kv(KvAsk {
                    id: kv.id,
                    op: KvOp::Get { blob_id: get.blob_id.clone().unwrap_or_default() },
                }));
            }
            if let Some(set) = kv.set_blob_args.as_option() {
                return Some(Ask::Kv(KvAsk {
                    id: kv.id,
                    op: KvOp::Set {
                        blob_id: set.blob_id.clone().unwrap_or_default(),
                        data: set.blob_data.clone().unwrap_or_default(),
                    },
                }));
            }

            // The server waits on every kv exchange the way it waits on
            // every exec, so a kind this build cannot answer ends the turn
            // with its number on it rather than reproduce the silence the
            // unanswered channel produced live.
            events.push(ProviderEvent::Failed(ProviderError::Parse(format!(
                "cursor made a kv request this build cannot answer ({}); \
                 leaving it unanswered would hang the turn",
                kv_kind(kv)
            ))));
            return None;
        }

        let Some(update) = message.interaction_update.as_option() else {
            tracing::debug!(
                provider = ID,
                fields = ?message
                    .__buffa_unknown_fields
                    .iter()
                    .map(|field| field.number)
                    .collect::<Vec<_>>(),
                "skipped a server message outside the update channel"
            );
            return None;
        };

        if let Some(delta) = update.text_delta.as_option() {
            events.push(ProviderEvent::TextDelta(delta.text.clone().unwrap_or_default()));
        } else if let Some(delta) = update.thinking_delta.as_option() {
            // The plugin forwards thinking to its clients beside reply text,
            // marked as thinking (proxy.ts:1059-1061); here that mark is the
            // event the Anthropic wire's thinking blocks already arrive as.
            let text = delta.text.clone().unwrap_or_default();
            // Logged at the skip log's own level because the next boundary
            // question should read a log rather than patch one in: whether a
            // thinking message is a token, a sentence or a whole block is
            // what decides if a message edge could ever mean anything.
            tracing::debug!(provider = ID, bytes = text.len(), "thinking delta");
            events.push(ProviderEvent::ReasoningDelta(text));
        } else if update.thinking_completed.is_set() {
            // The plugin's own boundary between two thinking blocks,
            // live-observed (2026-08-25) between the thought groups of one
            // claude-fable-5-thinking turn: without it consecutive thoughts
            // splice into one block — the transcript's account of that
            // stream read "…to see if those work.Since tool calls…".
            // Announced unconditionally: the frame also closes a stream's
            // last block, where the loop finds nothing open and says
            // nothing.
            events.push(ProviderEvent::ReasoningBreak);
        } else if update.turn_ended.is_set() {
            self.ended = true;
        } else if update.heartbeat.is_set() {
            // Liveness, carrying nothing.
        } else {
            // The arms are named so the next live run reads as a list of
            // decisions rather than a count of mysteries: every number here
            // is one the plugin's descriptor declares and this build chose
            // not to model.
            tracing::debug!(
                provider = ID,
                arms = ?update
                    .__buffa_unknown_fields
                    .iter()
                    .map(|field| update_arm(field.number))
                    .collect::<Vec<_>>(),
                "skipped an update this build does not model"
            );
        }

        None
    }

    /// Reports a body that ended without its EndStream frame.
    ///
    /// After `turn_ended` the reply was complete and only the terminator was
    /// lost — the Anthropic wire's reading of a body cut off after the stop
    /// reason. Before it, reply text nobody can recover is gone, and calling
    /// that a short answer would be the lie this variant exists to prevent.
    pub(super) fn truncated(&mut self, events: &mut Vec<ProviderEvent>) {
        if self.ended {
            events.push(ProviderEvent::Finish(FinishReason::Completed));
            return;
        }

        events.push(ProviderEvent::Failed(ProviderError::Transport(
            "the response body ended before the exchange finished".to_owned(),
        )));
    }
}

/// What an EndStream error means to the session that asked.
///
/// `unauthenticated` is the one code whose repair is a command this build
/// ships, so it becomes [`ProviderError::Auth`] and names it. Everything
/// else is the provider answering unsuccessfully — [`ProviderError::Status`]
/// — under the HTTP status the Connect protocol itself assigns the code,
/// because the wire's own status was a 200 with the failure in the body and
/// "200: invalid_argument" is a sentence that reads as a defect.
fn verdict(code: &str, message: &str) -> ProviderError {
    if code == "unauthenticated" {
        return ProviderError::Auth(format!(
            "cursor rejected the credential: {message}; run `ganja auth login {ID}`"
        ));
    }

    ProviderError::Status {
        status: connect::http_status(code),
        message: format!("connect error {code}: {message}"),
    }
}

/// Reads an exec's kind and its arguments, as far as an answer needs them.
///
/// The ten kinds `cursor.proto` models decode into fields of their own, so
/// they are recognised by presence and read straight off the args. Everything
/// else still arrives as unknown fields — [`exec_kind`] names those — and is
/// answered on D486's control channel.
///
/// A modelled kind's args may be *present and empty*: an exec whose payload
/// this build decodes none of still identifies its kind by the field it
/// arrived on, and an empty echo is the honest answer about a path nobody
/// sent. That is why every string read here defaults rather than refuses. The
/// numbers do not: an absent `offset` is a window the server did not ask to
/// narrow, which is a different thing from asking for line zero, so those stay
/// [`Option`] all the way to the tool call they become.
fn exec_args(exec: &proto::ExecRequest) -> (String, ExecArgs) {
    /// An optional string field as the echo carries it: absent and empty are
    /// one answer, because the arm has no way to say "the server did not
    /// send this".
    fn echoed(value: &Option<String>) -> String {
        value.clone().unwrap_or_default()
    }

    let named = |kind: &str, args| (kind.to_owned(), args);

    if let Some(args) = exec.shell_args.as_option() {
        return named(
            "shell_args",
            ExecArgs::Shell {
                command: echoed(&args.command),
                working_directory: echoed(&args.working_directory),
            },
        );
    }
    if let Some(args) = exec.shell_stream_args.as_option() {
        return named(
            "shell_stream_args",
            ExecArgs::ShellStream {
                command: echoed(&args.command),
                working_directory: echoed(&args.working_directory),
            },
        );
    }
    if let Some(args) = exec.write_args.as_option() {
        return named(
            "write_args",
            ExecArgs::Write { path: echoed(&args.path), file_text: echoed(&args.file_text) },
        );
    }
    if let Some(args) = exec.delete_args.as_option() {
        return named("delete_args", ExecArgs::Delete { path: echoed(&args.path) });
    }
    if let Some(args) = exec.grep_args.as_option() {
        return named(
            "grep_args",
            ExecArgs::Grep {
                pattern: echoed(&args.pattern),
                path: echoed(&args.path),
                glob: echoed(&args.glob),
                case_insensitive: args.case_insensitive.unwrap_or(false),
            },
        );
    }
    if let Some(args) = exec.read_args.as_option() {
        return named(
            "read_args",
            ExecArgs::Read {
                redacted: false,
                path: echoed(&args.path),
                offset: args.offset,
                limit: args.limit,
            },
        );
    }
    if let Some(args) = exec.redacted_read_args.as_option() {
        return named(
            "redacted_read_args",
            ExecArgs::Read {
                redacted: true,
                path: echoed(&args.path),
                offset: args.offset,
                limit: args.limit,
            },
        );
    }
    if let Some(args) = exec.ls_args.as_option() {
        return named("ls_args", ExecArgs::Ls { path: echoed(&args.path) });
    }
    if let Some(args) = exec.mcp_args.as_option() {
        return named(
            "mcp_args",
            ExecArgs::Mcp(McpCall {
                name: echoed(&args.name),
                tool_name: echoed(&args.tool_name),
                tool_call_id: echoed(&args.tool_call_id),
                provider_identifier: echoed(&args.provider_identifier),
                approval_only: args.smart_mode_approval_only.unwrap_or(false),
                arguments: super::value::arguments(&args.args),
            }),
        );
    }
    if let Some(args) = exec.fetch_args.as_option() {
        return named("fetch_args", ExecArgs::Fetch { url: echoed(&args.url) });
    }

    (exec_kind(exec), ExecArgs::Unmodelled)
}

/// Names the kind of an exec with no modelled answer arm.
///
/// Those kinds arrive as unknown fields, and the field number *is* the kind:
/// the table is the shipped client's own `ExecServerMessage` oneof
/// (`index.js@6302201`) minus the ten kinds [`super::request::refusal_answer`]
/// answers in their own vocabulary, so it names exactly what still rides the throw. A number the
/// table does not know is reported as itself — still enough to go derive,
/// and still refusable, because the throw is keyed on the exec id rather
/// than on the kind — and span_context (= 19) rides beside the oneof
/// without being a kind, so it is passed over rather than blamed.
fn exec_kind(exec: &proto::ExecRequest) -> String {
    let named = |number: u32| {
        Some(match number {
            9 => "diagnostics_args",
            16 => "background_shell_spawn_args",
            17 => "list_mcp_resources_exec_args",
            18 => "read_mcp_resource_exec_args",
            21 => "record_screen_args",
            22 => "computer_use_args",
            23 => "write_shell_stdin_args",
            27 => "execute_hook_args",
            28 => "subagent_args",
            30 => "force_background_shell_args",
            31 => "force_background_subagent_args",
            36 => "mcp_state_exec_args",
            37 => "subagent_await_args",
            38 => "smart_mode_classifier_args",
            40 => "canvas_diagnostics_args",
            41 => "shell_allowlist_precheck_args",
            42 => "mcp_allowlist_precheck_args",
            43 => "web_fetch_allowlist_precheck_args",
            44 => "git_diff_request",
            45 => "pi_read_args",
            46 => "pi_bash_args",
            47 => "pi_edit_args",
            48 => "pi_write_args",
            49 => "pi_grep_args",
            50 => "pi_find_args",
            51 => "pi_ls_args",
            52 => "mini_swe_agent_bash_args",
            53 => "conversation_search_args",
            54 => "agent_store_conflict_args",
            56 => "adopt_args",
            _ => return None,
        })
    };

    let fields = &exec.__buffa_unknown_fields;
    if let Some(kind) = fields.iter().find_map(|field| named(field.number)) {
        return kind.to_owned();
    }

    match fields.iter().map(|field| field.number).find(|number| *number != 19) {
        Some(number) => format!("field {number}"),
        None => "no recognizable kind".to_owned(),
    }
}

/// Names a skipped update's arm the way the plugin's descriptor does.
///
/// The table is the plugin's InteractionUpdate oneof (agent_pb.ts:3160-
/// :3272); the arms this build models — text_delta = 1, thinking_delta = 4,
/// heartbeat = 13, turn_ended = 14 — never reach it, because a modeled arm
/// decodes into its field rather than into the unknowns. A number outside
/// the table is a server newer than the descriptor, reported as itself —
/// still enough to go derive.
fn update_arm(number: u32) -> String {
    let named = match number {
        2 => "tool_call_started",
        3 => "tool_call_completed",
        6 => "user_message_appended",
        7 => "partial_tool_call",
        8 => "token_delta",
        9 => "summary",
        10 => "summary_started",
        11 => "summary_completed",
        12 => "shell_output_delta",
        15 => "tool_call_delta",
        16 => "step_started",
        17 => "step_completed",
        _ => return format!("field {number}"),
    };

    format!("{named} ({number})")
}

/// Names the kind an unanswerable kv request carried.
///
/// Get and set are the plugin's whole oneof (agent_pb.ts:7941, :7948) and
/// both are modeled, so an unanswerable kind can only be an arm newer than
/// the descriptor, arriving as an unknown field whose number is the kind.
/// span_context (= 4, agent_pb.ts:7931) rides beside the oneof without
/// being a kind, so it is passed over rather than blamed.
fn kv_kind(kv: &proto::KvRequest) -> String {
    match kv.__buffa_unknown_fields.iter().map(|field| field.number).find(|number| *number != 4) {
        Some(number) => format!("field {number}"),
        None => "no recognizable kind".to_owned(),
    }
}

#[cfg(test)]
#[path = "decode_tests.rs"]
mod tests;
