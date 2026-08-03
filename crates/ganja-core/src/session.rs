//! The agent loop: one turn, as many model requests as its tool calls demand.
//!
//! Spec: upstream `packages/opencode/src/session/processor.ts` and
//! `prompt.ts`. A turn repeats — mark a step, ask the model, execute whatever
//! tools it called — until a request ends without tool calls, and everything
//! that happens lands in one assistant message: step markers, streamed text,
//! and each tool call from `Pending` through `Completed` or `Error`.
//!
//! Tool results are information, never control flow. A refused permission, an
//! unknown tool, unparseable arguments, a tool that failed — each becomes the
//! error text the model reads on the next request, and the loop continues.
//! Upstream stops the turn on a refusal unless
//! `experimental.continue_loop_on_deny` is set; this port adopts the continue
//! behaviour outright, because a refusal answers one call, not the question
//! the user asked. The two ways a turn ends early are the two that mean it:
//! the user cancelled, or the provider died.
//!
//! There is no step cap. Upstream's processor has none either — the bound in
//! `prompt.ts` is `agent.steps ?? Infinity`, off by default — so the escape
//! hatch is [`CancelTurn`](crate::protocol::Command::CancelTurn), exactly as
//! it is upstream.

use std::{ops::ControlFlow, path::PathBuf, sync::Arc};

use futures::StreamExt as _;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    permission::{Decision, Permissions},
    protocol::{
        Event, FinishReason, Message, Part, PartBody, PartId, PermissionId, PermissionReply,
        ToolState, Usage, now,
    },
    provider::{ChatRequest, Provider, ProviderEvent},
    tool::{FileTimes, Registry, ToolCtx, ToolError},
};

/// What the model reads when the user refuses a call, ported verbatim from
/// upstream `packages/core/src/v1/permission.ts` (`RejectedError`).
const REJECTED: &str = "The user rejected permission to use this specific tool call.";

/// What a buffered call reads when the provider died before it could run.
///
/// No upstream analogue: the AI SDK executes tools mid-stream, so a provider
/// that dies never leaves a parsed-but-unstarted call behind. Here calls run
/// after the stream ends, and a part that opened `Pending` has to close.
const STRANDED: &str = "the provider failed before this call could run";

/// Upstream `tool/invalid.ts`: a call that cannot run is answered through the
/// `invalid` tool, and this is the shape of its output.
fn invalid_call(detail: &str) -> String {
    format!("The arguments provided to the tool are invalid: {detail}")
}

/// What the engine keeps about the turn in flight; holding one is what makes
/// the engine busy.
pub(crate) struct TurnHandle {
    /// Stops the turn: the provider stream, a running tool, a permission wait.
    pub(crate) cancel: CancellationToken,
    /// The permission wait a reply command answers, when one is open. Shared
    /// with the turn task, which sets and clears it.
    pub(crate) permission: Arc<std::sync::Mutex<Option<PendingReply>>>,
}

/// One open permission request: the id a reply must name, and the channel the
/// turn task is blocked on.
pub(crate) struct PendingReply {
    pub(crate) id: PermissionId,
    pub(crate) sender: oneshot::Sender<PermissionReply>,
}

/// Everything one turn needs, gathered so the spawned task takes one argument.
pub(crate) struct Turn {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) model: String,
    /// Tools the model is offered, and this loop executes.
    pub(crate) tools: Arc<Registry>,
    /// Rules deciding which calls wait for the user.
    pub(crate) permissions: Arc<std::sync::Mutex<Permissions>>,
    /// Directory tool calls resolve relative paths against.
    pub(crate) cwd: PathBuf,
    /// Which files this session has read, shared by every call in it.
    pub(crate) files: Arc<FileTimes>,
    pub(crate) prompt: String,
    pub(crate) cancel: CancellationToken,
    /// Where an open permission request waits for its reply; the same cell the
    /// engine's [`TurnHandle`] routes replies into.
    pub(crate) pending: Arc<std::sync::Mutex<Option<PendingReply>>>,
    pub(crate) events: mpsc::Sender<Event>,
    pub(crate) slot: Arc<Mutex<Option<TurnHandle>>>,
    pub(crate) history: Arc<Mutex<Vec<Message>>>,
}

/// Why a turn ended, and what to say about it.
struct Outcome {
    reason: FinishReason,
    error: Option<String>,
}

impl Outcome {
    fn finished(reason: FinishReason) -> Self {
        Self {
            reason,
            error: None,
        }
    }

    fn cancelled() -> Self {
        Self {
            reason: FinishReason::Cancelled,
            error: None,
        }
    }

    fn failed(error: String) -> Self {
        Self {
            reason: FinishReason::Failed,
            error: Some(error),
        }
    }
}

/// A tool call as the provider streamed it, waiting for the step to end.
struct BufferedCall {
    /// The provider's id for the call, echoed back beside its result.
    id: String,
    /// Tool the model asked for, by name.
    name: String,
    /// The call's arguments, accumulated fragment by fragment; only valid
    /// JSON once assembled.
    json: String,
    /// The `Pending` part opened for the call when it started.
    part_id: PartId,
}

/// Runs one turn to its finish event.
pub(crate) async fn run_turn(turn: Turn) {
    let mut assistant = Message::assistant(turn.model.clone());
    let outcome = drive(&turn, &mut assistant).await;
    let completed = assistant.complete();

    // A turn that died before its first fragment leaves nothing worth sending
    // back as context — and some providers reject an empty assistant message.
    // Step markers alone do not count; text or a tool call does.
    if assistant.has_content() {
        turn.history.lock().await.push(assistant.clone());
    }

    // Released before the finish event is queued so that a prompt sent in
    // reaction to it is never rejected as busy.
    *turn.slot.lock().await = None;

    if let Some(outcome) = outcome {
        let _ = turn
            .events
            .send(Event::MessageFinished {
                message_id: assistant.id,
                reason: outcome.reason,
                usage: assistant.usage,
                error: outcome.error,
                completed,
            })
            .await;
    }
}

/// Runs the step loop, accumulating everything into `assistant` and returning
/// why the turn ended, or [`None`] once the subscriber is gone and there is
/// nobody left to tell.
async fn drive(turn: &Turn, assistant: &mut Message) -> Option<Outcome> {
    let user = Message::user(turn.prompt.clone());
    turn.history.lock().await.push(user.clone());

    turn.events
        .send(Event::MessageStarted { message: user })
        .await
        .ok()?;
    turn.events
        .send(Event::MessageStarted {
            message: assistant.clone(),
        })
        .await
        .ok()?;

    // The provider-reported spend of the steps so far. [`None`] until a step
    // reports one, so a provider that says nothing yields a message that says
    // nothing, rather than a fabricated zero.
    let mut total: Option<Usage> = None;

    loop {
        let step = stream_step(turn, assistant).await;

        if let Some(usage) = step.usage {
            total = Some(add_usage(total.unwrap_or_default(), usage));
            assistant.usage = total;
        }

        match step.end {
            StepEnd::Interrupted(stop) => return stop,
            StepEnd::Finished { reason, mut calls } => {
                if calls.is_empty() {
                    // A request that ended without calling anything is the
                    // model done talking, and its reason is the turn's.
                    return Some(Outcome::finished(reason));
                }

                // Calls resolve sequentially in arrival order: a later call
                // is allowed to depend on an earlier one's effect, and
                // interleaved permission dialogs would be unreadable anyway.
                calls.reverse();
                while let Some(call) = calls.pop() {
                    if let ControlFlow::Break(stop) = resolve(turn, assistant, &call).await {
                        let error = ToolError::Cancelled.to_string();
                        // The interrupted call itself first — a no-op when
                        // `resolve` already closed it — then everything
                        // queued behind it.
                        close_unresolved(turn, assistant, &call, &error).await;
                        calls.reverse();
                        fail_buffered(turn, assistant, &mut calls, &error).await;
                        return stop;
                    }
                }
            }
        }
    }
}

/// How one model request ended.
enum StepEnd {
    /// The stream ran out, and these calls now want to run.
    Finished {
        reason: FinishReason,
        calls: Vec<BufferedCall>,
    },
    /// The turn is over — cancelled, failed, or abandoned — and any buffered
    /// call has already been closed.
    Interrupted(Option<Outcome>),
}

/// What one model request produced.
struct Step {
    end: StepEnd,
    /// What the request spent, when the provider said.
    usage: Option<Usage>,
}

/// Runs one model request: step marker, stream, step-finish marker.
async fn stream_step(turn: &Turn, assistant: &mut Message) -> Step {
    // Every request opens with a step marker, upstream's `step-start` part,
    // so a transcript shows where one request ended and the next began.
    let marker = Part {
        id: PartId::ascending(),
        body: PartBody::StepStart,
    };
    assistant.parts.push(marker.clone());
    if let ControlFlow::Break(stop) = deliver(
        turn,
        Event::PartStarted {
            message_id: assistant.id.clone(),
            part: marker,
        },
    )
    .await
    {
        return Step {
            end: StepEnd::Interrupted(stop),
            usage: None,
        };
    }

    let request = {
        let history = turn.history.lock().await;
        let mut messages = history.clone();
        // Later steps carry the reply so far — its text, its tool calls and
        // their results — which is how the model reads what its calls
        // returned. The first step has nothing to add.
        if assistant.has_content() {
            messages.push(assistant.clone());
        }

        ChatRequest {
            model: turn.model.clone(),
            system: None,
            messages,
            tools: turn.tools.definitions(),
        }
    };

    let mut events = match turn.provider.stream(request, turn.cancel.clone()).await {
        Ok(events) => events,
        Err(error) => {
            return Step {
                end: StepEnd::Interrupted(Some(Outcome::failed(error.to_string()))),
                usage: None,
            };
        }
    };

    // The text part this step's fragments accumulate into, once one is open.
    // Steps do not share one: text after a tool round is a new thought, and
    // upstream gives it a new part.
    let mut open: Option<PartId> = None;
    // What this one request cost. The last report wins, which is also how the
    // engine treated usage before the loop existed: providers accumulate
    // internally and report complete counts.
    let mut usage: Option<Usage> = None;
    let mut calls: Vec<BufferedCall> = Vec::new();

    /// Closes the buffered calls and hands back the interruption, so every
    /// early exit below stays one expression.
    macro_rules! interrupt {
        ($stop:expr, $error:expr) => {{
            fail_buffered(turn, assistant, &mut calls, $error).await;
            return Step {
                end: StepEnd::Interrupted($stop),
                usage,
            };
        }};
    }

    let reason = loop {
        // Biased so that a cancel already in hand always wins the race
        // against a fragment that happens to be ready, which is what bounds
        // how long a cancelled turn can keep streaming.
        let event = tokio::select! {
            biased;
            () = turn.cancel.cancelled() => {
                interrupt!(Some(Outcome::cancelled()), &ToolError::Cancelled.to_string());
            }
            event = events.next() => event,
        };

        let Some(event) = event else {
            // A stream that ends after a cancel ended because of it; one that
            // ends without a finish has said all it is going to.
            if turn.cancel.is_cancelled() {
                interrupt!(
                    Some(Outcome::cancelled()),
                    &ToolError::Cancelled.to_string()
                );
            }
            break FinishReason::Completed;
        };

        match event {
            ProviderEvent::TextDelta(delta) => {
                let part_id = match &open {
                    Some(part_id) => part_id.clone(),
                    None => {
                        let part = Part::text(String::new());
                        let part_id = part.id.clone();
                        assistant.parts.push(part.clone());
                        open = Some(part_id.clone());

                        if let ControlFlow::Break(stop) = deliver(
                            turn,
                            Event::PartStarted {
                                message_id: assistant.id.clone(),
                                part,
                            },
                        )
                        .await
                        {
                            interrupt!(stop, &ToolError::Cancelled.to_string());
                        }

                        part_id
                    }
                };

                // Addressed by id, not by position: a tool part opened
                // mid-step means the text part is no longer the newest one.
                if let Some(text) = assistant
                    .parts
                    .iter_mut()
                    .find(|part| part.id == part_id)
                    .and_then(Part::as_text_mut)
                {
                    text.push_str(&delta);
                }

                if let ControlFlow::Break(stop) = deliver(
                    turn,
                    Event::PartDelta {
                        message_id: assistant.id.clone(),
                        part_id,
                        delta,
                    },
                )
                .await
                {
                    interrupt!(stop, &ToolError::Cancelled.to_string());
                }
            }
            ProviderEvent::ToolCallStart { id, name } => {
                if calls.iter().any(|call| call.id == id) {
                    tracing::debug!(id, "the provider started the same call twice");
                    continue;
                }

                let part = Part::tool(id.clone(), name.clone());
                calls.push(BufferedCall {
                    id,
                    name,
                    json: String::new(),
                    part_id: part.id.clone(),
                });
                assistant.parts.push(part.clone());

                if let ControlFlow::Break(stop) = deliver(
                    turn,
                    Event::PartStarted {
                        message_id: assistant.id.clone(),
                        part,
                    },
                )
                .await
                {
                    interrupt!(stop, &ToolError::Cancelled.to_string());
                }
            }
            // Argument fragments accumulate silently: the part stays
            // `Pending` until the call resolves, and frontends read the
            // parsed input off the `Running` state rather than assembling raw
            // JSON themselves. An orphan fragment is dropped the way upstream
            // drops orphan reasoning deltas.
            ProviderEvent::ToolCallDelta { id, json } => {
                match calls.iter_mut().find(|call| call.id == id) {
                    Some(call) => call.json.push_str(&json),
                    None => tracing::debug!(id, "argument fragment for an unknown call"),
                }
            }
            // Arrival order already decides execution order, and the
            // arguments are only read once the stream ends, so the completion
            // marker carries nothing the loop needs.
            ProviderEvent::ToolCallEnd { .. } => {}
            ProviderEvent::Usage(reported) => usage = Some(reported),
            // A provider that died mid-stream keeps whatever it already
            // streamed — the transcript is honest about how far it got — but
            // the turn is reported as failed, and any call the model asked
            // for before dying is closed unrun: the next request needs every
            // opened call to carry a result.
            ProviderEvent::Failed(error) => {
                interrupt!(Some(Outcome::failed(error.to_string())), STRANDED);
            }
            ProviderEvent::Finish(reason) => break reason,
            // Reasoning has no protocol part yet. Dropping it keeps the
            // transcript honest instead of pasting thinking into the reply.
            ProviderEvent::ReasoningDelta(_) => {
                tracing::debug!("reasoning has no rendered part yet");
            }
        }
    };

    // The request is over: mark what it spent, upstream's `step-finish` part.
    // The marker is born complete, so this is the one append whose
    // `PartStarted` already carries content.
    let marker = Part {
        id: PartId::ascending(),
        body: PartBody::StepFinish {
            usage: usage.unwrap_or_default(),
        },
    };
    assistant.parts.push(marker.clone());
    if let ControlFlow::Break(stop) = deliver(
        turn,
        Event::PartStarted {
            message_id: assistant.id.clone(),
            part: marker,
        },
    )
    .await
    {
        interrupt!(stop, &ToolError::Cancelled.to_string());
    }

    Step {
        end: StepEnd::Finished { reason, calls },
        usage,
    }
}

/// Resolves one buffered call: parse, gate, run, and put the result — or the
/// reason there is none — where the model reads it next.
///
/// Breaks only when the turn itself is over: the user cancelled, or the
/// subscriber is gone. Everything else, including a refusal and a tool that
/// failed, continues the loop.
async fn resolve(
    turn: &Turn,
    assistant: &mut Message,
    call: &BufferedCall,
) -> ControlFlow<Option<Outcome>> {
    let args = match parse_args(&call.json) {
        Ok(args) => args,
        Err(error) => {
            let message = invalid_call(&error);
            return fail_call(turn, assistant, call, serde_json::json!({}), &message).await;
        }
    };

    let Some(tool) = turn.tools.get(&call.name) else {
        // Upstream reroutes an unavailable tool through `invalid.ts` carrying
        // the AI SDK's `NoSuchToolError` message, and this is that message.
        let names: Vec<String> = turn
            .tools
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        let available = if names.is_empty() {
            "No tools are available.".to_owned()
        } else {
            format!("Available tools: {}.", names.join(", "))
        };
        let message = invalid_call(&format!(
            "Model tried to call unavailable tool '{}'. {available}",
            call.name
        ));

        return fail_call(turn, assistant, call, args, &message).await;
    };

    let decision = turn
        .permissions
        .lock()
        .expect("the permission rules are never poisoned")
        .check(&call.name, &args);

    if decision == Decision::Ask {
        match wait_permission(turn, call, tool.describe(&args), &args).await? {
            PermissionReply::Once => {}
            PermissionReply::Always => turn
                .permissions
                .lock()
                .expect("the permission rules are never poisoned")
                .remember_always(&call.name, &args),
            // A refusal is information, not a turn abort: the model reads it
            // as the call's result and decides what to do next.
            PermissionReply::Reject => {
                return fail_call(turn, assistant, call, args, REJECTED).await;
            }
        }
    }

    let started = now();
    if let Some(part) = set_tool_state(
        assistant,
        &call.part_id,
        ToolState::Running {
            input: args.clone(),
            started,
        },
    ) {
        deliver(
            turn,
            Event::PartUpdated {
                message_id: assistant.id.clone(),
                part,
            },
        )
        .await?;
    }

    // The tool gets a child of the turn's token so a cancel reaches it, and
    // the race below is what refuses to wait for a tool that ignores it:
    // losing the race drops the tool's future, which is as killed as this
    // process can make it. The shell tool owns group-killing its child.
    let ctx = ToolCtx {
        cwd: turn.cwd.clone(),
        cancel: turn.cancel.child_token(),
        call_id: call.id.clone(),
        files: Arc::clone(&turn.files),
    };
    let result = tokio::select! {
        biased;
        () = turn.cancel.cancelled() => Err(ToolError::Cancelled),
        result = tool.run(args.clone(), &ctx) => result,
    };

    match result {
        Ok(output) => {
            if let Some(part) = set_tool_state(
                assistant,
                &call.part_id,
                ToolState::Completed {
                    input: args,
                    output: output.output,
                    title: output.title,
                    metadata: output.metadata,
                    started,
                    completed: now(),
                },
            ) {
                deliver(
                    turn,
                    Event::PartUpdated {
                        message_id: assistant.id.clone(),
                        part,
                    },
                )
                .await?;
            }

            ControlFlow::Continue(())
        }
        // A cancelled call ends the turn. The part update travels the
        // terminal path — a plain send, like the finish event — because
        // racing it against the cancel that caused it would drop it.
        Err(error @ ToolError::Cancelled) => {
            if let Some(part) = set_tool_state(
                assistant,
                &call.part_id,
                ToolState::Error {
                    input: args,
                    error: error.to_string(),
                    started,
                    completed: now(),
                },
            ) {
                let _ = turn
                    .events
                    .send(Event::PartUpdated {
                        message_id: assistant.id.clone(),
                        part,
                    })
                    .await;
            }

            ControlFlow::Break(Some(Outcome::cancelled()))
        }
        // The message is what the model sees next, so it is the error's own
        // words — `ToolError` promises they say what went wrong in terms the
        // model can act on.
        Err(error) => {
            if let Some(part) = set_tool_state(
                assistant,
                &call.part_id,
                ToolState::Error {
                    input: args,
                    error: error.to_string(),
                    started,
                    completed: now(),
                },
            ) {
                deliver(
                    turn,
                    Event::PartUpdated {
                        message_id: assistant.id.clone(),
                        part,
                    },
                )
                .await?;
            }

            ControlFlow::Continue(())
        }
    }
}

/// Asks the user about one call and waits for the answer.
///
/// Every `PermissionRequested` that reaches the subscriber is answered by
/// exactly one `PermissionReplied` — the user's, or the refusal a cancel is —
/// so a frontend can retire its dialog unconditionally.
async fn wait_permission(
    turn: &Turn,
    call: &BufferedCall,
    title: String,
    args: &serde_json::Value,
) -> ControlFlow<Option<Outcome>, PermissionReply> {
    let (sender, receiver) = oneshot::channel();
    let id = PermissionId::ascending();
    *turn
        .pending
        .lock()
        .expect("the pending permission is never poisoned") = Some(PendingReply {
        id: id.clone(),
        sender,
    });

    if let ControlFlow::Break(stop) = deliver(
        turn,
        Event::PermissionRequested {
            id: id.clone(),
            call_id: call.id.clone(),
            tool: call.name.clone(),
            title,
            args: args.clone(),
        },
    )
    .await
    {
        // The request never reached the subscriber, so no reply is owed.
        retract_pending(turn);
        return ControlFlow::Break(stop);
    }

    let received = tokio::select! {
        biased;
        () = turn.cancel.cancelled() => None,
        reply = receiver => reply.ok(),
    };

    let Some(reply) = received else {
        // Cancelled while waiting. The reply below travels the terminal path
        // unconditionally: it is the answer this request was promised.
        retract_pending(turn);
        let _ = turn
            .events
            .send(Event::PermissionReplied {
                id,
                reply: PermissionReply::Reject,
            })
            .await;
        return ControlFlow::Break(Some(Outcome::cancelled()));
    };

    match deliver(
        turn,
        Event::PermissionReplied {
            id: id.clone(),
            reply,
        },
    )
    .await
    {
        ControlFlow::Continue(()) => ControlFlow::Continue(reply),
        ControlFlow::Break(stop) => {
            // The user's answer lost its race against a cancel and was never
            // queued; what the request gets instead is the refusal the cancel
            // means, and the call does not run either way.
            if stop.is_some() {
                let _ = turn
                    .events
                    .send(Event::PermissionReplied {
                        id,
                        reply: PermissionReply::Reject,
                    })
                    .await;
            }

            ControlFlow::Break(stop)
        }
    }
}

/// Clears the reply slot for a request that will never be answered.
fn retract_pending(turn: &Turn) {
    turn.pending
        .lock()
        .expect("the pending permission is never poisoned")
        .take();
}

/// Moves the call's part to [`ToolState::Error`] carrying `error`, which is
/// what the model reads as the call's result on the next request.
async fn fail_call(
    turn: &Turn,
    assistant: &mut Message,
    call: &BufferedCall,
    input: serde_json::Value,
    error: &str,
) -> ControlFlow<Option<Outcome>> {
    let stamp = now();
    let Some(part) = set_tool_state(
        assistant,
        &call.part_id,
        ToolState::Error {
            input,
            error: error.to_owned(),
            started: stamp,
            completed: stamp,
        },
    ) else {
        return ControlFlow::Continue(());
    };

    deliver(
        turn,
        Event::PartUpdated {
            message_id: assistant.id.clone(),
            part,
        },
    )
    .await
}

/// Closes every remaining buffered call with an [`ToolState::Error`] saying
/// why it never ran.
///
/// A turn on its way out must not leave a part `Pending` or `Running`: the
/// next request has to answer every call the model opened, and a frontend has
/// to be able to stop its spinners. The updates travel the terminal path —
/// plain sends — because this only runs when the turn is already over, and
/// racing them against the cancel that caused them would drop them.
async fn fail_buffered(
    turn: &Turn,
    assistant: &mut Message,
    calls: &mut Vec<BufferedCall>,
    error: &str,
) {
    for call in std::mem::take(calls) {
        close_unresolved(turn, assistant, &call, error).await;
    }
}

/// Closes one call that will never run, unless it already has a terminal
/// state, in which case there is nothing to close and nothing is sent.
async fn close_unresolved(turn: &Turn, assistant: &mut Message, call: &BufferedCall, error: &str) {
    let input = parse_args(&call.json).unwrap_or_else(|_| serde_json::json!({}));
    let stamp = now();
    if let Some(part) = set_tool_state(
        assistant,
        &call.part_id,
        ToolState::Error {
            input,
            error: error.to_owned(),
            started: stamp,
            completed: stamp,
        },
    ) {
        let _ = turn
            .events
            .send(Event::PartUpdated {
                message_id: assistant.id.clone(),
                part,
            })
            .await;
    }
}

/// Replaces the state of the tool part `part_id`, returning the part as it
/// now stands for the event that reports it, or [`None`] when there is
/// nothing to report.
fn set_tool_state(assistant: &mut Message, part_id: &PartId, state: ToolState) -> Option<Part> {
    let part = assistant
        .parts
        .iter_mut()
        .find(|part| part.id == *part_id)?;
    if let PartBody::Tool { state: current, .. } = &mut part.body {
        // Terminal states stick: a call the turn already closed is not
        // reopened by a late race.
        if matches!(
            current,
            ToolState::Completed { .. } | ToolState::Error { .. }
        ) {
            return None;
        }
        *current = state;
    }

    Some(part.clone())
}

/// Parses a call's accumulated argument JSON.
///
/// An empty buffer parses as `{}`: a call to a tool with no parameters
/// streams no fragments at all, and upstream opens every call with `input:
/// {}` for the same reason. A non-object parses into `{"value": …}`, which is
/// upstream's wrapping. Anything unparseable is an error the model is told
/// about.
fn parse_args(json: &str) -> Result<serde_json::Value, String> {
    if json.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }

    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(value @ serde_json::Value::Object(_)) => Ok(value),
        Ok(other) => Ok(serde_json::json!({ "value": other })),
        Err(error) => Err(error.to_string()),
    }
}

/// Sums what two requests spent.
fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens.saturating_add(b.input_tokens),
        output_tokens: a.output_tokens.saturating_add(b.output_tokens),
        reasoning_tokens: a.reasoning_tokens.saturating_add(b.reasoning_tokens),
        cache_read_tokens: a.cache_read_tokens.saturating_add(b.cache_read_tokens),
        cache_write_tokens: a.cache_write_tokens.saturating_add(b.cache_write_tokens),
    }
}

/// Queues `event`, or breaks with the turn's report.
///
/// [`mpsc::Sender::send`] is cancel-safe: losing the race drops the event
/// without queueing it, which is what an abandoned turn wants. Waiting on a
/// full queue must not outlive a cancel, hence the race.
async fn deliver(turn: &Turn, event: Event) -> ControlFlow<Option<Outcome>> {
    tokio::select! {
        biased;
        () = turn.cancel.cancelled() => ControlFlow::Break(Some(Outcome::cancelled())),
        queued = turn.events.send(event) => match queued {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(None),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{add_usage, parse_args};
    use crate::protocol::Usage;

    #[test]
    fn arguments_parse_leniently_and_fail_loudly() {
        assert_eq!(
            parse_args("").expect("no fragments is a no-argument call"),
            serde_json::json!({})
        );
        assert_eq!(
            parse_args("   \n").expect("whitespace is still empty"),
            serde_json::json!({})
        );
        assert_eq!(
            parse_args(r#"{"path":"a.rs"}"#).expect("an object passes through"),
            serde_json::json!({"path": "a.rs"})
        );
        assert_eq!(
            parse_args("[1,2]").expect("a non-object is wrapped, as upstream wraps it"),
            serde_json::json!({"value": [1, 2]})
        );
        assert!(
            parse_args("{not json").is_err(),
            "malformed JSON is an error the model must hear about"
        );
    }

    #[test]
    fn usage_sums_field_by_field_and_saturates() {
        let summed = add_usage(
            Usage {
                input_tokens: 1,
                output_tokens: 2,
                reasoning_tokens: 3,
                cache_read_tokens: 4,
                cache_write_tokens: 5,
            },
            Usage {
                input_tokens: 10,
                output_tokens: 20,
                reasoning_tokens: 30,
                cache_read_tokens: 40,
                cache_write_tokens: 50,
            },
        );

        assert_eq!(
            summed,
            Usage {
                input_tokens: 11,
                output_tokens: 22,
                reasoning_tokens: 33,
                cache_read_tokens: 44,
                cache_write_tokens: 55,
            }
        );
        assert_eq!(
            add_usage(
                Usage {
                    input_tokens: u64::MAX,
                    ..Usage::default()
                },
                Usage {
                    input_tokens: 1,
                    ..Usage::default()
                },
            )
            .input_tokens,
            u64::MAX,
            "a sum never wraps into a tiny bill"
        );
    }
}
