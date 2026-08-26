//! OpenAI-compatible chat completions, streamed.
//!
//! Spec: `POST {base}/chat/completions` with `stream: true`, authenticated by a
//! bearer token. The base URL is configurable because the shape is a de-facto
//! standard: the same code drives a local llama.cpp server, an OpenRouter key,
//! or anything else that copied the schema.
//!
//! **Not, any longer, the vendor whose schema it is.** A `GANJA_PROVIDER=openai`
//! session speaks [`super::responses`] on either credential, because upstream's
//! plugin routes every model of that vendor through the Responses API without
//! consulting the credential at all (`plugin/provider/openai.ts:185`). What
//! keeps this module is that two providers *are* this wire under another name —
//! [`super::grok`] and [`super::copilot`] — and that the shape is what any
//! other compatible endpoint would want. [`ID`], [`API_KEY_ENV`] and
//! [`BASE_URL_ENV`] still live here because they are the vendor's names and
//! [`super::responses`] reads them from here.
//!
//! Unlike Anthropic, the frames are unnamed — every one is a `data:` line
//! holding a chunk object, and the stream ends with the literal `[DONE]`.

use std::{borrow::Cow, collections::HashMap, fmt};

use async_trait::async_trait;
use futures::stream::BoxStream;
use secrecy::SecretString;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    protocol::{FinishReason, Part, PartBody, Role, ToolState, Usage},
    provider::{
        ChatRequest, CredentialSource, Mapper, NO_RESULT, Presented, Provider, ProviderError,
        ProviderEvent, check_base_url, client, open, require_key, setting, shown_base_url,
        splice_effort,
        sse::Frame,
        steps,
        toolname::{Aliases, OPENAI_CAP, alias},
    },
};

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this provider.
pub const ID: &str = "openai";

/// Environment variable carrying the credential.
pub const API_KEY_ENV: &str = "OPENAI_API_KEY";

/// Environment variable pointing the provider at a compatible endpoint. This is
/// how a session reaches anything that is not OpenAI itself.
pub const BASE_URL_ENV: &str = "OPENAI_BASE_URL";

/// Where OpenAI's own API lives.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// The frame that ends a chat-completions stream.
const DONE: &str = "[DONE]";

/// Streams replies from an OpenAI-compatible chat completions endpoint.
pub struct OpenAiProvider {
    client: reqwest::Client,
    credential: CredentialSource,
    base_url: String,
    /// What every request carries besides the bearer token.
    ///
    /// Empty for OpenAI itself and for every gateway that copied its schema —
    /// the credential is the whole of what those endpoints ask for. What fills
    /// it is a provider that *is* this wire under another name and whose
    /// endpoint decides what to serve by more than the token: see
    /// [`super::copilot`], which `api.githubcopilot.com` refuses without four
    /// of them. Held rather than passed per request because they describe the
    /// endpoint, which is fixed when the provider is built, and never the
    /// credential, which is not.
    headers: reqwest::header::HeaderMap,
    /// What this endpoint last said was left of the account's budget
    /// (**D484**), parsed by the shared table in [`super::rate`] — every
    /// endpoint riding this wire spells its buckets the same `x-ratelimit-*`
    /// way, and one that spells them no way at all simply meters nothing.
    rates: super::RateWindows,
}

impl fmt::Debug for OpenAiProvider {
    /// Renders without the credential, so that logging a provider — or a
    /// `tracing` field holding one — cannot leak it.
    ///
    /// That includes the base URL, which is allowed to carry a credential in
    /// its userinfo and is not exempt from the rule just because the credential
    /// arrived as configuration.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("credential", &self.credential)
            .field("base_url", &shown_base_url(&self.base_url))
            .finish()
    }
}

impl OpenAiProvider {
    /// Builds a provider that authenticates with `key`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Auth`] for a blank credential and
    /// [`ProviderError::Transport`] when no HTTP client can be built.
    pub fn new(key: impl Into<SecretString>) -> Result<Self, ProviderError> {
        let key = Presented::new(key)
            .ok_or_else(|| ProviderError::Auth(format!("{API_KEY_ENV} is empty")))?;

        Self::with_credential(CredentialSource::Key(key), DEFAULT_BASE_URL)
    }

    /// Builds a provider from [`API_KEY_ENV`] and [`BASE_URL_ENV`].
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Auth`] when the credential is missing and
    /// [`ProviderError::Transport`] when [`BASE_URL_ENV`] names an endpoint the
    /// key cannot safely be sent to, so that a misconfigured session dies at
    /// startup rather than at the first prompt.
    pub fn from_env() -> Result<Self, ProviderError> {
        let base_url = setting(BASE_URL_ENV).unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        check_base_url(&base_url)?;

        Self::with_credential(
            CredentialSource::Key(require_key(ID, API_KEY_ENV)?),
            base_url,
        )
    }

    /// Builds a provider that authenticates however `credential` says.
    ///
    /// The seam a provider which is this wire under another name is built
    /// through — see [`super::grok`], whose endpoint speaks this API and whose
    /// credential is an OAuth access token rather than a key. Module-internal
    /// because what a caller outside this module picks between is providers,
    /// not credential sources.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be built.
    pub(super) fn with_credential(
        credential: CredentialSource,
        base_url: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            client: client()?,
            credential,
            base_url: base_url.into(),
            headers: reqwest::header::HeaderMap::new(),
            rates: super::RateWindows::default(),
        })
    }

    /// Points the provider at `base_url` instead of [`DEFAULT_BASE_URL`], which
    /// is what makes this provider work against a compatible endpoint.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Puts `headers` on every request this provider sends.
    ///
    /// The third and last thing a wrapper provider declares about itself, beside
    /// [`with_credential`](Self::with_credential) and the base URL — see
    /// [`headers`](Self::headers) for what fills it and why nothing else does.
    /// Crate-internal for [`with_credential`](Self::with_credential)'s reason:
    /// what a caller outside this module picks between is providers.
    #[must_use]
    pub(super) fn with_headers(mut self, headers: reqwest::header::HeaderMap) -> Self {
        self.headers = headers;
        self
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        ID
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // Checked here as well as at startup because `with_base_url` can point
        // a provider anywhere, and this is the last moment before the credential
        // goes on the wire.
        check_base_url(&self.base_url)?;

        // Resolved before the request is built rather than captured at
        // construction: an access token expires under a long session, and one
        // renewed a moment ago by another turn is the one this request should
        // carry. A key resolves to itself, so the ordinary case pays nothing.
        let presented = self.credential.presented().await?;
        // The effort's options pass through under the wire's own fields —
        // this wire maps none of them, and a collision resolves to the wire.
        let own = Body::new(&request);
        let body = splice_effort(&request.effort_options, &own);
        // Built from the same roster the body just advertised, so the decoder
        // reads back exactly what this request offered. Cloned per attempt
        // because `open` may call the factory again on a retry.
        let aliases = Aliases::of(&request.tools, OPENAI_CAP);
        let built = self
            .client
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .bearer_auth(presented.expose())
            // After the bearer, and never carrying one: these describe the
            // endpoint, and a credential put here would travel outside the
            // redaction `presented` is the single source of.
            .headers(self.headers.clone())
            .json(&body)
            .build()
            .map_err(|error| {
                ProviderError::Transport(presented.redact(&format!("malformed request: {error}")))
            })?;

        // `wire` and not `provider`: grok, github-copilot and every configured
        // chat-completions endpoint delegate their whole `stream` to this one,
        // so the name a session runs under is not knowable here. The endpoint
        // beside it is what tells those sessions apart, which is the fact a
        // reader of the log actually needs.
        tracing::debug!(
            wire = ID,
            model = request.model,
            endpoint = super::endpoint(built.url(), &self.base_url),
            "requesting a turn"
        );

        open(
            move || Mapping {
                aliases: aliases.clone(),
                ..Mapping::default()
            },
            &self.client,
            built,
            &self.base_url,
            &presented,
            &self.rates,
            cancel,
        )
        .await
    }

    fn rate_windows(&self) -> Vec<super::RateWindow> {
        self.rates.latest()
    }

    /// The plan half of the same store (**D485**).
    fn plan_windows(&self) -> Vec<super::PlanWindow> {
        self.rates.latest_plans()
    }
}

/// The JSON a request carries.
#[derive(Debug, Serialize)]
struct Body<'a> {
    model: &'a str,
    stream: bool,
    /// Without this the stream reports no token counts at all.
    stream_options: StreamOptions,
    messages: Vec<Turn<'a>>,
    /// Omitted rather than sent empty. A turn with nothing to offer the model
    /// is the ordinary case for a session with no registry, and plenty of
    /// compatible endpoints reject `"tools": []` outright.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolSpec<'a>>,
}

/// Opt-in for the trailing usage chunk.
#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// One tool as chat completions advertises it.
#[derive(Debug, Serialize)]
struct ToolSpec<'a> {
    /// Only `function` exists today; the field is what makes room for the rest.
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionSpec<'a>,
}

/// The half of a tool advertisement that describes the function.
#[derive(Debug, Serialize)]
struct FunctionSpec<'a> {
    /// The name the model is told, which is the registry's own unless that one
    /// is outside this API's `^[a-zA-Z0-9_-]{1,64}$` — see [`alias`].
    name: Cow<'a, str>,
    description: &'a str,
    /// The argument schema, which this API names `parameters`.
    parameters: &'a Value,
}

/// One message as chat completions spells it.
#[derive(Debug, Serialize)]
struct Turn<'a> {
    role: &'static str,
    /// Absent on an assistant message that only called tools, which is how the
    /// API spells a turn that said nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Cow<'a, str>>,
    /// The calls an assistant message made.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<Call<'a>>,
    /// Set on a `tool` message, naming the call it answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

/// One call an assistant message made.
#[derive(Debug, Serialize)]
struct Call<'a> {
    /// The provider's identifier for the call, which its result names.
    id: &'a str,
    /// Only `function` exists today.
    #[serde(rename = "type")]
    kind: &'static str,
    function: CallFunction<'a>,
}

/// The half of a call that says what was called, and how.
#[derive(Debug, Serialize)]
struct CallFunction<'a> {
    /// Under the same [`alias`] the model was originally offered the tool as —
    /// aliasing is deterministic, so replaying a transcript needs nothing
    /// remembered from the turn that made the call.
    name: Cow<'a, str>,
    /// The arguments as a JSON *string*, which is how this API carries them —
    /// the model streams them as text, and a server is free to hand back
    /// whatever it was given, valid JSON or not.
    arguments: String,
}

impl<'a> Turn<'a> {
    /// A message that is only text.
    fn said(role: &'static str, content: Cow<'a, str>) -> Self {
        Self {
            role,
            content: Some(content),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// One call's result, which this API carries as a message of its own.
    fn answered(tool_call_id: &'a str, content: &'a str) -> Self {
        Self {
            role: "tool",
            content: Some(Cow::Borrowed(content)),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id),
        }
    }
}

impl<'a> Body<'a> {
    /// Turns a request into the JSON chat completions expects.
    ///
    /// # How a transcript becomes a request
    ///
    /// Canonical [`Message`](crate::protocol::Message)s carry ordered parts and
    /// hold a call beside its result; this API carries one content string per
    /// message, calls in a `tool_calls` array beside it, and each result as its
    /// own `tool` message. So each of a message's [`steps`] becomes one message
    /// plus one per call:
    ///
    /// - its text parts become the content and its tool parts the `tool_calls`
    ///   entries;
    /// - every one of that step's calls is answered by a `tool` message placed
    ///   immediately after, in call order, because the API refuses an assistant
    ///   turn that follows an unanswered call.
    ///
    /// Step markers themselves serialize to nothing: they say where the split
    /// falls, not anything the model said.
    ///
    /// A step that would carry neither content nor calls is dropped rather than
    /// sent empty, which is what both the marker that opens a turn and an
    /// assistant turn that failed before its first fragment are.
    fn new(request: &'a ChatRequest) -> Self {
        // Chat completions has no `system` field: the prompt is the first
        // message, with its own role.
        let mut messages: Vec<Turn<'a>> = request
            .system
            .as_deref()
            .map(|content| Turn::said("system", Cow::Borrowed(content)))
            .into_iter()
            .collect();

        for message in &request.messages {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };

            for step in steps(&message.parts) {
                let (content, calls, results) = split(step);
                if content.is_none() && calls.is_empty() {
                    continue;
                }

                messages.push(Turn {
                    role,
                    content,
                    tool_calls: calls,
                    tool_call_id: None,
                });
                messages.extend(results);
            }
        }

        Self {
            model: &request.model,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            messages,
            tools: request
                .tools
                .iter()
                .map(|tool| ToolSpec {
                    kind: "function",
                    function: FunctionSpec {
                        name: alias(&tool.name, OPENAI_CAP),
                        description: &tool.description,
                        parameters: &tool.schema,
                    },
                })
                .collect(),
        }
    }
}

/// Splits one step into its content, the calls it made, and the messages that
/// answer them.
fn split(parts: &[Part]) -> (Option<Cow<'_, str>>, Vec<Call<'_>>, Vec<Turn<'_>>) {
    let mut texts: Vec<&str> = Vec::new();
    let mut calls = Vec::new();
    let mut results = Vec::new();

    for part in parts {
        match &part.body {
            PartBody::Text { text } => {
                if !text.trim().is_empty() {
                    texts.push(text);
                }
            }
            PartBody::Tool {
                call_id,
                tool,
                state,
            } => {
                calls.push(Call {
                    id: call_id,
                    kind: "function",
                    function: CallFunction {
                        name: alias(tool, OPENAI_CAP),
                        arguments: arguments(state),
                    },
                });
                results.push(Turn::answered(call_id, result(state)));
            }
            // A mentioned file is a *reference*, resolved into a text block
            // before a request is built (`session::resolve_mentions`). This
            // wire declines every binary mime (`accepts_attachment`'s
            // default), so a file part here never carries content — the
            // engine degraded it to text before the request was built.
            //
            // `StepFinish` carries a step's bill rather than content, and
            // `StepStart` was consumed as the boundary this step was cut at.
            //
            // Sealed reasoning belongs to the wire that sealed it, and chat
            // completions has no item for one; readable thinking is rendered
            // rather than replayed. A `Peer` part is rendered into the user
            // turn at request assembly (D495) and never encoded here as a
            // message of its own. See the same arm in `anthropic.rs`.
            PartBody::File { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
            | PartBody::ReasoningText { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Peer { .. }
            | PartBody::Reasoning { .. } => {}
        }
    }

    let content = match texts.as_slice() {
        [] => None,
        [only] => Some(Cow::Borrowed(*only)),
        // One message, one content string. Within a step that means fragments
        // of one reply the calls interrupted, and joining them without a
        // separator would run the last word of one into the first of the next.
        many => Some(Cow::Owned(many.join("\n"))),
    };

    (content, calls, results)
}

/// The arguments a call ran with, as the JSON string this API carries.
///
/// A call the model never finished streaming has none; an empty object is the
/// honest spelling of "the model was still saying", and the field is required.
///
/// Shared with [`super::responses`]: both wires carry arguments as a string,
/// because both receive them as one.
pub(super) fn arguments(state: &ToolState) -> String {
    let input = match state {
        ToolState::Pending { .. } => return "{}".to_owned(),
        ToolState::Running { input, .. }
        | ToolState::Completed { input, .. }
        | ToolState::Error { input, .. } => input,
    };

    serde_json::to_string(input).expect("a serde_json::Value always serializes")
}

/// What a call produced, or why it produced nothing.
///
/// This API has no error flag on a result, so a failure travels as the text the
/// model reads — which is what [`ToolState::Error`] holds anyway. Shared with
/// [`super::responses`], whose `function_call_output` has no flag either.
pub(super) fn result(state: &ToolState) -> &str {
    match state {
        ToolState::Completed { output, .. } => output,
        ToolState::Error { error, .. } => error,
        // See [`NO_RESULT`]: the turn that made this call died before the tool
        // answered, and an unanswered call is a request the API refuses.
        ToolState::Pending { .. } | ToolState::Running { .. } => NO_RESULT,
    }
}

/// Accumulates what the chunks so far said.
#[derive(Debug, Default)]
struct Mapping {
    usage: Usage,
    /// `prompt_tokens` as reported, before the cached count is taken back out
    /// of it.
    ///
    /// This API's `prompt_tokens` is the whole prompt *including* whatever
    /// `prompt_tokens_details.cached_tokens` was served from the cache, while
    /// [`Usage`]'s five counters are disjoint. Held raw and reconciled once the
    /// stream ends, so that the two counts need not have arrived together.
    prompt_tokens: u64,
    /// Identifiers of the tool calls seen so far, by their position in the
    /// chunk's `tool_calls` array — which is the only thing that correlates
    /// arguments with the call they belong to.
    tools: HashMap<u64, String>,
    /// What this request's advertised names map back to, empty for the
    /// ordinary roster whose names this API already accepts.
    aliases: Aliases,
    /// Set once a choice reported why it stopped.
    stopped: bool,
}

impl Mapper for Mapping {
    fn frame(&mut self, frame: &Frame, events: &mut Vec<ProviderEvent>) {
        if frame.data.trim() == DONE {
            self.finish(events);
            return;
        }

        let chunk: Value = match serde_json::from_str(&frame.data) {
            Ok(chunk) => chunk,
            Err(error) => {
                // Skipping would drop reply text without anything downstream
                // knowing a gap exists, which is worse than ending the turn
                // with a message that says so.
                events.push(ProviderEvent::Failed(ProviderError::Parse(format!(
                    "chat completion chunk: {error}"
                ))));
                return;
            }
        };

        // Compatible endpoints report a mid-stream failure as a chunk rather
        // than as a status, because the status was already 200.
        if !chunk["error"].is_null() {
            events.push(ProviderEvent::Failed(failure(&chunk["error"])));
            return;
        }

        self.absorb(&chunk["usage"]);

        let choice = &chunk["choices"][0];
        if choice.is_null() {
            // The trailing usage-only chunk has no choices at all.
            return;
        }

        self.delta(&choice["delta"], events);

        if let Some(reason) = choice["finish_reason"].as_str() {
            self.stopped = true;
            tracing::debug!(finish_reason = reason, "the model stopped");
        }
    }

    fn truncated(&mut self, events: &mut Vec<ProviderEvent>) {
        // Plenty of compatible servers close the body instead of sending
        // `[DONE]`. Once a choice has reported a finish reason the reply is
        // whole, and only the sentinel went missing.
        if self.stopped {
            self.finish(events);
            return;
        }

        events.push(ProviderEvent::Failed(ProviderError::Transport(
            "the response body ended before the model finished".to_owned(),
        )));
    }
}

impl Mapping {
    /// Reads the trailing usage chunk, which is the only one that has one.
    fn absorb(&mut self, usage: &Value) {
        for (pointer, slot) in [
            ("prompt_tokens", &mut self.prompt_tokens),
            ("completion_tokens", &mut self.usage.output_tokens),
        ] {
            if let Some(count) = usage[pointer].as_u64() {
                *slot = count;
            }
        }

        if let Some(cached) = usage["prompt_tokens_details"]["cached_tokens"].as_u64() {
            self.usage.cache_read_tokens = cached;
        }
        if let Some(reasoning) = usage["completion_tokens_details"]["reasoning_tokens"].as_u64() {
            self.usage.reasoning_tokens = reasoning;
        }
    }

    /// Maps one choice's delta.
    fn delta(&mut self, delta: &Value, events: &mut Vec<ProviderEvent>) {
        if let Some(text) = delta["content"].as_str()
            && !text.is_empty()
        {
            events.push(ProviderEvent::TextDelta(text.to_owned()));
        }

        // Not in the OpenAI schema; DeepSeek and the reasoning models behind
        // several gateways send it, and dropping it would lose the thinking.
        if let Some(reasoning) = delta["reasoning_content"].as_str()
            && !reasoning.is_empty()
        {
            events.push(ProviderEvent::ReasoningDelta(reasoning.to_owned()));
        }

        let Some(calls) = delta["tool_calls"].as_array() else {
            return;
        };

        for call in calls {
            let index = call["index"].as_u64().unwrap_or_default();
            let function = &call["function"];

            let id = match self.tools.get(&index) {
                Some(id) => id.clone(),
                None => {
                    // The identifier and the name arrive once, on the chunk
                    // that opens the call; everything after it is arguments.
                    // A server that sends neither still gets a stable id.
                    let id = call["id"]
                        .as_str()
                        .map_or_else(|| format!("call_{index}"), str::to_owned);
                    self.tools.insert(index, id.clone());

                    events.push(ProviderEvent::ToolCallStart {
                        id: id.clone(),
                        // Back through this request's own map: what the engine
                        // executes, what the permission rules match and what
                        // the transcript records is the registry name, never
                        // the one the wire had to advertise.
                        name: self
                            .aliases
                            .original(function["name"].as_str().unwrap_or_default().to_owned()),
                    });
                    id
                }
            };

            if let Some(json) = function["arguments"].as_str()
                && !json.is_empty()
            {
                events.push(ProviderEvent::ToolCallDelta {
                    id,
                    json: json.to_owned(),
                });
            }
        }
    }

    /// Closes any open tool calls, reports the bill, and ends the turn.
    fn finish(&mut self, events: &mut Vec<ProviderEvent>) {
        // Chat completions has no per-call terminator: a call is complete when
        // the stream is. Ordered so that a replay is deterministic.
        let mut open: Vec<(u64, String)> = self.tools.drain().collect();
        open.sort_unstable();
        events.extend(
            open.into_iter()
                .map(|(_index, id)| ProviderEvent::ToolCallEnd { id }),
        );

        // `prompt_tokens` counts the cached tokens as well, and `Usage` keeps
        // its five counters disjoint so that each can be billed at its own
        // rate, so the cached ones come back out here. Without this a cached
        // session reports its prompt twice and is priced several times over —
        // a cache read costs a tenth of what fresh input does.
        //
        // Upstream reaches the same shape twice over: `packages/core`'s
        // OpenAI-compatible model derives a `noCache` count as
        // `promptTokens - cachedTokens`, and `session.ts`'s `getUsage` subtracts
        // both cache counters from `inputTokens` before pricing anything.
        //
        // Saturating rather than wrapping: an endpoint claiming more cached
        // tokens than prompt tokens must read as nothing fresh, not as a bill
        // for eighteen quintillion tokens.
        self.usage.input_tokens = self
            .prompt_tokens
            .saturating_sub(self.usage.cache_read_tokens);

        events.push(ProviderEvent::Usage(self.usage));
        events.push(ProviderEvent::Finish(FinishReason::Completed));
    }
}

/// Turns an error object into the failure the turn reports.
///
/// What the object said is [`super::reported`]'s business, so that a body
/// carrying a `code` and no `message` — which on this wire is the ordinary
/// shape of a gateway's error chunk — stops reading as a body that carried
/// nothing.
fn failure(error: &Value) -> ProviderError {
    // Not logged here: the failure is warned once, redacted, at
    // `provider::shielded`, the seam that holds the credential to mask with.
    let message = super::reported(error);

    match error["code"]
        .as_u64()
        .and_then(|code| u16::try_from(code).ok())
    {
        Some(status) => ProviderError::Status { status, message },
        // `code` is usually a slug rather than a number, and the HTTP status
        // was 200 by the time this arrived, so there is nothing truer to say.
        None => ProviderError::Status {
            status: 500,
            message,
        },
    }
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
