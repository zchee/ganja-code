//! Measurement scaffolding for the cursor tool bridge — **removed in W3a or
//! W4**, and shipped behaviour in neither.
//!
//! Spec: `.omc/plans/2026-09-04-cursor-tool-bridge.md`, W2. Five facts about
//! cursor's client-declared MCP tool channel cannot be read out of the
//! shipped bundle at all — whether the server *offers and calls* a tool a
//! third-party client declares, whether a deliberate multi-second pause
//! inside an exec survives, how a typed `rejected` on a native kind is
//! treated, whether `system_prompt_spec` is allowlist-gated the way
//! `custom_system_prompt` was, and which of the two schema fields the server
//! actually honors. Every one of them is a *server* behaviour, so the only
//! instrument is one live turn that declares a tool and writes down what came
//! back. This module is that instrument.
//!
//! **Why an environment flag rather than a config key.** A config key is a
//! surface: it earns a JSON-schema entry, a `--help` line, a migration story
//! and a deprecation when it goes. This is scaffolding with a scheduled
//! removal date and exactly one operator — the person running the probe — so
//! it is reached the way a probe is reached, by exporting a variable for one
//! command. `AC-9`'s repo walk in `spike_tests.rs` is what keeps that promise
//! honest: the identifier may appear in this file and its tests and nowhere
//! else in the workspace.
//!
//! **What it does when on.** Declares exactly one tool — `ganja_ping`, which
//! answers `pong` — on both roster channels the shipped client uses; sends the
//! run-level `client_heartbeat` every five seconds while the request body is
//! open; and, when the server calls that tool, holds the exec open for the
//! configured delay (optionally sending exec-level heartbeats) before
//! answering. Everything else on the wire is exactly what W1 already sends:
//! every *other* exec keeps D550's typed refusal, `read_args` included, which
//! is what makes measurement (c) free.
//!
//! **Nothing here is reached with the flag unset.** [`Spike::from_env`]
//! returns `None`, every call site below is behind an `Option`, and the
//! send-side test in `request_tests.rs` pins the run request's field numbers
//! so a regression shows up as bytes rather than as an argument.

use std::convert::Infallible;
use std::time::Duration;

use buffa::Message as _;
use futures::channel::mpsc;
use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at, sleep_until};
use tokio_util::sync::CancellationToken;

use super::{ID, connect, decode, proto};
use crate::provider::{ProviderError, setting};

/// Turns the whole spike on. Everything else below is read only when this is.
pub(super) const ENABLE_ENV: &str = "GANJA_CURSOR_SPIKE";

/// Seconds the `ganja_ping` exec is deliberately held open before it is
/// answered — measurement (b). Absent means `0`, which is no hold at all and
/// satisfies no measurement; the runbook's (b)/(b′) runs pass `25`.
pub(super) const DELAY_ENV: &str = "GANJA_CURSOR_SPIKE_DELAY_SECS";

/// Whether that hold also sends exec-level heartbeats — measurement (b′). The
/// two runs differ in this one variable, which is the whole point of a
/// separate flag rather than a smarter default.
pub(super) const EXEC_HEARTBEAT_ENV: &str = "GANJA_CURSOR_SPIKE_EXEC_HEARTBEAT";

/// Which schema field the declaration fills — measurement (e). `both` is the
/// default because (a) must be established before (e) can mean anything; the
/// one-field runs follow.
pub(super) const SCHEMA_ENV: &str = "GANJA_CURSOR_SPIKE_SCHEMA";

/// Whether the run request also carries `system_prompt_spec.append` —
/// measurement (d). Spelled as a word rather than a boolean because `append`
/// is one of the arm's two names and a later probe of `replace` should read
/// as a value of this variable rather than as a second one.
pub(super) const PROMPT_ENV: &str = "GANJA_CURSOR_SPIKE_PROMPT";

/// The one tool declared, on both channels. Deliberately namespaced and
/// deliberately useless: a name no real roster would collide with, and an
/// answer that proves the round trip without doing anything to the machine.
pub(super) const TOOL_NAME: &str = "ganja_ping";

/// What the declaration says the tool is for. The model reads this to decide
/// whether to call it, so it says what calling it does and admits what it is.
const DESCRIPTION: &str = "Answers pong. A probe.";

/// The server that is said to serve the tool. Synthetic — no such MCP server
/// exists — which is itself part of measurement (a): whether the server
/// resolves a provider identifier it has never seen is unobservable from the
/// bundle (`2026-09-04-cursor-agent-bundle-read.md` §3, "Unverified").
const PROVIDER_IDENTIFIER: &str = "ganja";

/// The tool's argument schema: an object that takes nothing. Held as JSON
/// text rather than as two hand-built encodings, because the typed form is
/// derived from *this* string — so the two schema fields cannot disagree,
/// which is the one way measurement (e) could produce a false answer.
const SCHEMA_JSON: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;

/// What the tool answers. One word, so a reply that echoes it is unambiguous
/// in a transcript.
const PONG: &str = "pong";

/// The marker sentence `system_prompt_spec.append` carries — measurement (d)
/// is read off whether the reply begins with that word, so it has to be one
/// no ordinary answer would produce and one the model can obey without
/// refusing.
pub(super) const MARKER: &str = "Begin every reply with the word PINEAPPLE.";

/// How often the run-level `client_heartbeat` goes out while the body is
/// open. The shipped client's own cadence for the exec heartbeat is three
/// seconds (`index.js@4272747`); five is this build's choice for the
/// run-level one, comfortably inside any plausible idle bound.
const RUN_HEARTBEAT: Duration = Duration::from_secs(5);

/// How often the exec-level heartbeat goes out during a hold, when it is on:
/// the shipped client's own three seconds, so measurement (b′) tests the
/// cadence the server was written against rather than one this build invented.
const EXEC_HEARTBEAT: Duration = Duration::from_secs(3);

/// Which of `McpToolDefinition`'s two schema fields the declaration fills.
///
/// The shipped client fills exactly one — its builder picks between them
/// (`index.js@5699717`) — so a both-fields declaration is already a departure
/// from any observed client, useful for establishing (a) and useless for (e).
/// The one-field values are what (e) is actually measured with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SchemaFields {
    /// Both `input_schema = 3` and `input_schema_json = 6`.
    Both,
    /// `input_schema = 3` alone — the `google.protobuf.Value` form.
    Typed,
    /// `input_schema_json = 6` alone — the JSON-string form.
    Json,
}

impl SchemaFields {
    /// Whether the typed `input_schema = 3` is filled.
    fn typed(self) -> bool {
        matches!(self, Self::Both | Self::Typed)
    }

    /// Whether the string `input_schema_json = 6` is filled.
    fn json(self) -> bool {
        matches!(self, Self::Both | Self::Json)
    }

    /// What the log line and the fixture call this run.
    fn spelled(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Typed => "3",
            Self::Json => "6",
        }
    }
}

/// One live run's settings, read once at construction and never again.
#[derive(Clone, Debug)]
pub(super) struct Spike {
    /// How long the `ganja_ping` exec is held before it is answered.
    delay: Duration,
    /// Whether that hold sends exec-level heartbeats.
    exec_heartbeat: bool,
    /// Which schema field(s) the declaration fills.
    schema: SchemaFields,
    /// Whether the run request carries the marker on
    /// `system_prompt_spec.append`.
    prompt_append: bool,
}

impl Spike {
    /// The spike's settings, or `None` when it is off.
    ///
    /// **Every malformed value refuses the run** rather than falling back to a
    /// default. A silent fallback here would produce a measurement of
    /// something other than what the operator asked for — a `SCHEMA=6` typo
    /// quietly measured as `both` would answer (e) with the one answer that
    /// cannot be detected as wrong — and an unusable run is cheaper than a
    /// wrong recording.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] naming the variable and what it
    /// holds, for a delay that is not a number, a schema selector outside
    /// `both`/`3`/`6`, a prompt mode outside `append`, or a boolean spelled as
    /// anything but `1`/`true`/`0`/`false`.
    pub(super) fn from_env() -> Result<Option<Self>, ProviderError> {
        Self::read(&setting)
    }

    /// [`from_env`](Self::from_env) against a lookup of the caller's choosing.
    ///
    /// Split out so the refusals above are tested without a test mutating the
    /// process environment — which in edition 2024 is `unsafe`, and which
    /// would make every other test in this binary depend on the order it ran
    /// in. The one real caller passes the real reader.
    pub(super) fn read(
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, ProviderError> {
        if !boolean(env, ENABLE_ENV)? {
            return Ok(None);
        }

        let delay = match env(DELAY_ENV) {
            Some(spelled) => Duration::from_secs(
                spelled
                    .parse()
                    .map_err(|_| refused(DELAY_ENV, &spelled, "a whole number of seconds"))?,
            ),
            None => Duration::ZERO,
        };

        let schema = match env(SCHEMA_ENV).as_deref() {
            None | Some("both") => SchemaFields::Both,
            Some("3") => SchemaFields::Typed,
            Some("6") => SchemaFields::Json,
            Some(other) => return Err(refused(SCHEMA_ENV, other, "`both`, `3` or `6`")),
        };

        let prompt_append = match env(PROMPT_ENV).as_deref() {
            None => false,
            Some("append") => true,
            Some(other) => return Err(refused(PROMPT_ENV, other, "`append`")),
        };

        let spike = Self {
            delay,
            exec_heartbeat: boolean(env, EXEC_HEARTBEAT_ENV)?,
            schema,
            prompt_append,
        };
        tracing::debug!(
            provider = ID,
            delay_secs = spike.delay.as_secs(),
            exec_heartbeat = spike.exec_heartbeat,
            schema = spike.schema.spelled(),
            prompt_append = spike.prompt_append,
            "cursor spike: enabled"
        );

        Ok(Some(spike))
    }

    /// The declaration, as both roster channels carry it: one tool, with the
    /// schema on whichever field(s) this run selected.
    pub(super) fn tools(&self) -> Vec<proto::McpToolDefinition> {
        let mut tool = proto::McpToolDefinition::default()
            .with_name(TOOL_NAME)
            .with_description(DESCRIPTION)
            .with_provider_identifier(PROVIDER_IDENTIFIER)
            .with_tool_name(TOOL_NAME);

        if self.schema.typed() {
            tool.input_schema = buffa::MessageField::some(json_value(&schema()));
        }
        if self.schema.json() {
            tool.input_schema_json = Some(SCHEMA_JSON.to_owned());
        }

        vec![tool]
    }

    /// The run request's `mcp_tools = 4` wrapper around [`tools`](Self::tools).
    pub(super) fn declaration(&self) -> proto::McpTools {
        proto::McpTools { mcp_tools: self.tools(), ..Default::default() }
    }

    /// The marker on `system_prompt_spec.append`, when this run is measuring
    /// (d); `None` otherwise, which leaves field 29 absent.
    ///
    /// Only the `append` arm is ever built: `replace` would discard the prompt
    /// `cloud_rule` already carries, so a refusal of it would be
    /// indistinguishable from a turn that simply lost its system prompt.
    pub(super) fn prompt_spec(&self) -> Option<proto::SystemPromptSpec> {
        self.prompt_append.then(|| proto::SystemPromptSpec::default().with_append(MARKER))
    }
}

/// Whether the exec the server is asking for is the declared probe.
///
/// Matched on `McpArgs.name = 1` — what the model called — rather than on
/// `tool_name = 5`, because the two are set from one declaration by the
/// shipped client's own builder (`index.js@5699717`) and `name` is the member
/// this build already decoded before the spike existed.
pub(super) fn is_ping(ask: &decode::ExecRefusal) -> bool {
    matches!(&ask.arm, decode::RefusalArm::Mcp { name, .. } if name == TOOL_NAME)
}

/// The messages answering the probe: the success arm carrying one word, then
/// the stream close that ends every exec, refused or served.
///
/// The close is the same second message a refusal sends, for the same reason
/// (`request::refusal_answer`): it is what tells the server the exec is over
/// rather than still running.
pub(super) fn pong(ask: &decode::ExecRefusal) -> Vec<Vec<u8>> {
    tracing::debug!(
        provider = ID,
        exec = ask.id,
        "cursor spike: answering the probe with mcp_result.success"
    );

    let served = proto::ClientMessage {
        exec_response: buffa::MessageField::some(proto::ExecResponse {
            id: ask.id,
            exec_id: ask.exec_id.clone(),
            mcp_result: buffa::MessageField::some(proto::McpResult {
                success: buffa::MessageField::some(proto::McpSuccess {
                    content: vec![proto::McpContentItem {
                        text: buffa::MessageField::some(
                            proto::McpTextContent::default().with_text(PONG),
                        ),
                        ..Default::default()
                    }],
                    is_error: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    vec![served.encode_to_vec(), closed(ask.id)]
}

/// How a hold ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Held {
    /// The full delay passed; the exec may be answered.
    Elapsed,
    /// The turn was cancelled while the exec was held.
    Cancelled,
    /// The request body closed, so no answer can reach the server.
    Closed,
}

/// Holds the probe's exec open for the configured delay, keeping the run
/// alive with heartbeats while it waits — the measurement itself.
///
/// `run` is the fold's own run-level interval rather than a second one, so
/// the cadence a log line records is the cadence the whole turn ran at. The
/// exec-level interval is created here because it exists only for the
/// duration of one hold.
///
/// Cancel-safe by construction: `Interval::tick` and `sleep_until` both are,
/// the deadline is absolute so re-arming it each pass costs nothing, and the
/// cancellation arm is `biased` first so a cancel is never outrun by a
/// heartbeat that came due in the same poll.
/// [`hold`] against the duplex the fold owns, which is the one caller.
///
/// A wrapper rather than the borrow dance at the call site: it keeps the
/// fold's own arm one line long, so W3a removes this whole path by deleting a
/// `match` arm and this file.
pub(super) async fn held(
    duplex: &mut super::Duplex,
    exec: Option<u32>,
    cancel: &CancellationToken,
) -> Held {
    // Both are `Some` on every path that reaches here — the caller's guard
    // proved the spike, and a spike turn's `Duplex` always carries the run
    // interval — but a hold that quietly answered at once is a better failure
    // than a panic in a probe.
    let Some(run) = duplex.beat.as_mut() else { return Held::Elapsed };
    let Some(spike) = duplex.spike.as_ref() else { return Held::Elapsed };

    hold(spike, run, exec, &duplex.answers, cancel).await
}

async fn hold(
    spike: &Spike,
    run: &mut Interval,
    exec: Option<u32>,
    answers: &mpsc::UnboundedSender<Result<Vec<u8>, Infallible>>,
    cancel: &CancellationToken,
) -> Held {
    let started = Instant::now();
    let deadline = started + spike.delay;
    let mut exec_beat = spike.exec_heartbeat.then(|| beats(EXEC_HEARTBEAT));
    tracing::debug!(
        provider = ID,
        exec,
        delay_secs = spike.delay.as_secs(),
        exec_heartbeat = spike.exec_heartbeat,
        "cursor spike: holding the probe's exec open"
    );

    loop {
        let message = tokio::select! {
            biased;
            () = cancel.cancelled() => return Held::Cancelled,
            () = sleep_until(deadline) => {
                tracing::debug!(
                    provider = ID,
                    exec,
                    elapsed_ms = started.elapsed().as_millis(),
                    "cursor spike: the hold elapsed"
                );
                return Held::Elapsed;
            }
            () = beat(Some(run)) => {
                tracing::debug!(
                    provider = ID,
                    elapsed_ms = started.elapsed().as_millis(),
                    "cursor spike: run heartbeat, mid-hold"
                );
                run_heartbeat()
            }
            () = beat(exec_beat.as_mut()) => {
                tracing::debug!(
                    provider = ID,
                    exec,
                    elapsed_ms = started.elapsed().as_millis(),
                    "cursor spike: exec heartbeat"
                );
                exec_heartbeat(exec)
            }
        };

        if answers.unbounded_send(Ok(connect::envelope(&message))).is_err() {
            return Held::Closed;
        }
    }
}

/// The run-level interval a spike turn beats on.
pub(super) fn run_beats() -> Interval {
    beats(RUN_HEARTBEAT)
}

/// An interval whose first tick is one period out rather than immediate — an
/// interval that fired at zero would put a heartbeat ahead of the run
/// request's own answer for no reason.
fn beats(period: Duration) -> Interval {
    let mut interval = interval_at(Instant::now() + period, period);
    // A tick missed because the fold was busy decoding is a tick to skip, not
    // one to fire immediately afterwards: what is being measured is a cadence,
    // and a burst would misreport it.
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

/// Waits for `interval`'s next tick, or forever when there is none.
///
/// The `None` arm is what lets one `select!` serve a spike run and an ordinary
/// one without a second copy of the loop: a branch that never completes is a
/// branch that is not there.
pub(super) async fn beat(interval: Option<&mut Interval>) {
    match interval {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// The run-level liveness ping, `ClientMessage.client_heartbeat = 7`.
pub(super) fn run_heartbeat() -> Vec<u8> {
    proto::ClientMessage {
        client_heartbeat: buffa::MessageField::some(proto::ClientHeartbeat::default()),
        ..Default::default()
    }
    .encode_to_vec()
}

/// The exec-level liveness ping, `ExecControl.heartbeat = 3`, keyed on the
/// held exec's own id.
pub(super) fn exec_heartbeat(exec: Option<u32>) -> Vec<u8> {
    proto::ClientMessage {
        exec_control: buffa::MessageField::some(proto::ExecControl {
            heartbeat: buffa::MessageField::some(proto::ExecHeartbeat {
                id: exec,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
    .encode_to_vec()
}

/// The stream close that ends a served exec, byte for byte the one a refusal
/// sends.
fn closed(exec: Option<u32>) -> Vec<u8> {
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
    .encode_to_vec()
}

/// Writes down one exec as it arrives: its kind, its number, and — for an MCP
/// call — which members of `McpArgs` the server filled.
///
/// **Names and flags only.** `McpArgs.args = 2` is the model's argument
/// values and is not modelled at all, so there is nothing here that could
/// carry one; the string members are reported as present or absent rather
/// than by value, because a `tool_call_id` is enough to correlate a call in a
/// log and the identifiers echo a declaration this file already states.
pub(super) fn log_exec(exec: &proto::ExecRequest) {
    let (kind, number) = shape(exec);
    let Some(args) = exec.mcp_args.as_option() else {
        tracing::debug!(provider = ID, exec = exec.id, kind, number, "cursor spike: exec arrived");
        return;
    };

    tracing::debug!(
        provider = ID,
        exec = exec.id,
        kind,
        number,
        // Whether the name reaching the model is the one that was declared is
        // the whole of measurement (a); the value is this file's own constant,
        // so it is safe to say which of the two it matched.
        ganja_ping = args.name.as_deref() == Some(TOOL_NAME),
        tool_call_id = args.tool_call_id.is_some(),
        provider_identifier = args.provider_identifier.is_some(),
        tool_name = args.tool_name.is_some(),
        server_identifier = args.server_identifier.is_some(),
        smart_mode_approval_only = args.smart_mode_approval_only,
        skip_approval = args.skip_approval,
        others = ?args.__buffa_unknown_fields.iter().map(|field| field.number).collect::<Vec<_>>(),
        "cursor spike: mcp exec arrived"
    );
}

/// An exec's kind name and field number.
///
/// Deliberately its own table rather than a reach into `decode`'s: this file
/// is deleted whole, and a spike that had edited the shipped classifier to
/// hand back one more value would leave a seam behind when it went. The
/// numbers are `cursor.proto`'s own `ExecRequest`, which is where they are
/// cited.
fn shape(exec: &proto::ExecRequest) -> (&'static str, Option<u32>) {
    let modelled = [
        (exec.shell_args.is_set(), "shell_args", 2),
        (exec.write_args.is_set(), "write_args", 3),
        (exec.delete_args.is_set(), "delete_args", 4),
        (exec.grep_args.is_set(), "grep_args", 5),
        (exec.read_args.is_set(), "read_args", 7),
        (exec.ls_args.is_set(), "ls_args", 8),
        (exec.request_context_args.is_set(), "request_context_args", 10),
        (exec.mcp_args.is_set(), "mcp_args", 11),
        (exec.shell_stream_args.is_set(), "shell_stream_args", 14),
        (exec.fetch_args.is_set(), "fetch_args", 20),
        (exec.redacted_read_args.is_set(), "redacted_read_args", 29),
    ];
    if let Some((_, kind, number)) = modelled.into_iter().find(|(set, _, _)| *set) {
        return (kind, Some(number));
    }

    // span_context = 19 rides beside the oneof without being a kind, exactly
    // as `decode::exec_kind` reads it.
    (
        "unmodelled",
        exec.__buffa_unknown_fields.iter().map(|field| field.number).find(|number| *number != 19),
    )
}

/// The tool's schema as a value, parsed from the one string both encodings
/// are derived from.
fn schema() -> serde_json::Value {
    serde_json::from_str(SCHEMA_JSON).expect("this file's own schema literal is JSON")
}

/// `serde_json`'s value as `google.protobuf.Value`'s flattened shape.
///
/// Total over the input on purpose: an encoder that silently dropped a
/// variant would make a malformed declaration look like a server-side
/// refusal, and telling those two apart is the whole of measurement (e).
pub(super) fn json_value(value: &serde_json::Value) -> proto::JsonValue {
    let mut encoded = proto::JsonValue::default();
    match value {
        // `google.protobuf.NullValue` has exactly one member, `0`; sending the
        // field with that value is how the well-known type spells a null.
        serde_json::Value::Null => encoded.null_value = Some(0),
        serde_json::Value::Bool(value) => encoded.bool_value = Some(*value),
        // `as_f64` returns `None` only under serde_json's `arbitrary_precision`
        // feature, which this workspace does not enable; a NaN would still be
        // a number on the wire rather than a dropped field.
        serde_json::Value::Number(value) => {
            encoded.number_value = Some(value.as_f64().unwrap_or(f64::NAN));
        }
        serde_json::Value::String(value) => encoded.string_value = Some(value.clone()),
        serde_json::Value::Array(items) => {
            encoded.list_value = buffa::MessageField::some(proto::JsonList {
                values: items.iter().map(json_value).collect(),
                ..Default::default()
            });
        }
        serde_json::Value::Object(members) => {
            encoded.struct_value = buffa::MessageField::some(proto::JsonStruct {
                fields: members
                    .iter()
                    .map(|(key, value)| proto::JsonField {
                        key: Some(key.clone()),
                        value: buffa::MessageField::some(json_value(value)),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            });
        }
    }

    encoded
}

/// A flag read as a boolean, refusing anything that is neither.
///
/// # Errors
///
/// Returns [`ProviderError::Transport`] for a value outside
/// `1`/`true`/`0`/`false`, naming the variable and what it holds.
fn boolean(env: &dyn Fn(&str) -> Option<String>, variable: &str) -> Result<bool, ProviderError> {
    match env(variable) {
        None => Ok(false),
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "true" => Ok(true),
            "0" | "false" => Ok(false),
            _ => Err(refused(variable, &value, "`1`, `true`, `0` or `false`")),
        },
    }
}

/// The refusal a malformed flag earns: what was set, to what, and what would
/// have been read.
fn refused(variable: &str, value: &str, expected: &str) -> ProviderError {
    ProviderError::Transport(format!(
        "{variable} is set to {value:?}, and the cursor spike reads only {expected}"
    ))
}

#[cfg(test)]
#[path = "spike_tests.rs"]
mod tests;
