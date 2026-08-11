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
        splice_effort, sse::Frame, steps,
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

        open(&self.client, built, &presented, cancel, Mapping::default()).await
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
    name: &'a str,
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
    name: &'a str,
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
                        name: &tool.name,
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
                        name: tool,
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
            // completions has no item for one; see the same arm in
            // `anthropic.rs`.
            PartBody::File { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
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
        ToolState::Pending => return "{}".to_owned(),
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
        ToolState::Pending | ToolState::Running { .. } => NO_RESULT,
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
                        name: function["name"].as_str().unwrap_or_default().to_owned(),
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
fn failure(error: &Value) -> ProviderError {
    let message = error["message"]
        .as_str()
        .unwrap_or("the provider reported an error")
        .to_owned();

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
mod tests {
    use futures::StreamExt as _;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::{Body, Mapping, NO_RESULT, OpenAiProvider};
    use crate::{
        catalog,
        protocol::{FinishReason, Message, Part, PartBody, PartId, ToolState, Usage},
        provider::{ChatRequest, ProviderError, ProviderEvent, replay, splice_effort},
        tool::ToolDefinition,
    };

    /// Runs a recorded transcript through the real splitter and mapper.
    async fn events(transcript: &'static str) -> Vec<ProviderEvent> {
        replay(transcript, CancellationToken::new(), Mapping::default())
            .collect()
            .await
    }

    /// The reply text a transcript streams.
    fn text(events: &[ProviderEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_happy_path_transcript_maps_to_text_reasoning_and_a_bill() {
        let seen = events(include_str!("../../tests/fixtures/openai_happy_path.sse")).await;

        assert_eq!(text(&seen), "Hello, world!");
        assert!(
            seen.contains(&ProviderEvent::ReasoningDelta(
                "A greeting is enough.".to_owned()
            )),
            "reasoning_content should not be dropped, got {seen:?}"
        );
        assert_eq!(
            &seen[seen.len() - 2..],
            &[
                ProviderEvent::Usage(Usage {
                    // The chunk says `prompt_tokens: 42` with 16 of them
                    // cached, and `Usage` keeps its counters disjoint, so the
                    // fresh half of that prompt is 26.
                    input_tokens: 26,
                    output_tokens: 9,
                    reasoning_tokens: 4,
                    cache_read_tokens: 16,
                    cache_write_tokens: 0,
                }),
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            "the trailing usage chunk should be reported before the finish, got {seen:?}"
        );
    }

    /// This API reports the whole prompt as `prompt_tokens` and then says how
    /// much of it the cache served; [`Usage`] keeps the two apart so each can be
    /// billed at its own rate, a cache read costing a fraction of fresh input.
    /// Handing both counts through unchanged bills the cached half twice.
    #[tokio::test]
    async fn a_cached_prompt_reports_only_its_fresh_half_as_input() {
        let cases = [
            (
                "a cached prompt bills only what the cache did not serve",
                concat!(
                    r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}],"#,
                    r#""usage":{"prompt_tokens":1000,"completion_tokens":20,"#,
                    r#""prompt_tokens_details":{"cached_tokens":800}}}"#,
                    "\n\ndata: [DONE]\n\n",
                ),
                Usage {
                    input_tokens: 200,
                    output_tokens: 20,
                    cache_read_tokens: 800,
                    ..Usage::default()
                },
            ),
            (
                "an endpoint claiming more cached tokens than prompt tokens reads as \
                 nothing fresh rather than wrapping into a bill nobody owes",
                concat!(
                    r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}],"#,
                    r#""usage":{"prompt_tokens":100,"completion_tokens":5,"#,
                    r#""prompt_tokens_details":{"cached_tokens":900}}}"#,
                    "\n\ndata: [DONE]\n\n",
                ),
                Usage {
                    input_tokens: 0,
                    output_tokens: 5,
                    cache_read_tokens: 900,
                    ..Usage::default()
                },
            ),
            (
                "a prompt nothing was cached for is fresh in full",
                concat!(
                    r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}],"#,
                    r#""usage":{"prompt_tokens":1000,"completion_tokens":20}}"#,
                    "\n\ndata: [DONE]\n\n",
                ),
                Usage {
                    input_tokens: 1_000,
                    output_tokens: 20,
                    ..Usage::default()
                },
            ),
        ];

        for (name, transcript, expected) in cases {
            let seen = events(transcript).await;

            assert!(
                seen.contains(&ProviderEvent::Usage(expected)),
                "{name}: got {seen:?}"
            );
        }
    }

    /// The bill the corrected counts actually produce. Priced apart, a heavily
    /// cached prompt costs a fraction of what the same tokens would fresh —
    /// which is exactly the difference double-counting used to erase.
    #[test]
    fn a_cached_prompt_is_billed_once_rather_than_twice() {
        let model = catalog::model("gpt-5.6").expect("the catalog knows the model");
        let corrected = Usage {
            input_tokens: 200_000,
            output_tokens: 0,
            cache_read_tokens: 800_000,
            ..Usage::default()
        };
        // What the same response cost before the cached tokens came back out of
        // `prompt_tokens`: the whole million counted as fresh input *and* the
        // cached 800k counted again beside it.
        let doubled = Usage {
            input_tokens: 1_000_000,
            ..corrected
        };

        let billed = catalog::cost(&corrected, &model).total_usd;
        let expected = model.pricing.input * 0.2 + model.pricing.cache_read * 0.8;

        assert!(
            (billed - expected).abs() < 1e-9,
            "200k fresh at ${}/Mtok plus 800k cached at ${}/Mtok is ${expected}, got ${billed}",
            model.pricing.input,
            model.pricing.cache_read,
        );
        assert!(
            catalog::cost(&doubled, &model).total_usd > billed * 3.0,
            "the old counts over-reported by more than a factor of three, which is \
             the size of the error this pins"
        );
    }

    #[tokio::test]
    async fn tool_calls_are_opened_filled_and_closed() {
        let seen = events(include_str!("../../tests/fixtures/openai_tool_calls.sse")).await;

        assert_eq!(text(&seen), "Reading the file first.");
        assert_eq!(
            seen.iter()
                .filter(|event| !matches!(
                    event,
                    ProviderEvent::TextDelta(_) | ProviderEvent::Usage(_)
                ))
                .collect::<Vec<_>>(),
            vec![
                &ProviderEvent::ToolCallStart {
                    id: "call_read".to_owned(),
                    name: "read".to_owned()
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_read".to_owned(),
                    json: "{\"file".to_owned()
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_read".to_owned(),
                    json: "Path\":\"src/main.rs\"}".to_owned()
                },
                &ProviderEvent::ToolCallStart {
                    id: "call_glob".to_owned(),
                    name: "glob".to_owned()
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_glob".to_owned(),
                    json: "{\"pattern\":\"**/*.rs\"}".to_owned()
                },
                // Chat completions has no per-call terminator, so both calls
                // close when the stream does, in index order.
                &ProviderEvent::ToolCallEnd {
                    id: "call_read".to_owned()
                },
                &ProviderEvent::ToolCallEnd {
                    id: "call_glob".to_owned()
                },
                &ProviderEvent::Finish(FinishReason::Completed),
            ]
        );
    }

    /// A model that talks while it calls, which chat completions carries as
    /// content and `tool_calls` in the same chunk. Neither may swallow the
    /// other, and a call's fragments have to find their way back to the call
    /// they belong to across everything in between.
    #[tokio::test]
    async fn text_and_a_fragmented_call_interleave_without_losing_either() {
        let seen = events(include_str!(
            "../../tests/fixtures/openai_tool_calls_interleaved.sse"
        ))
        .await;

        assert_eq!(text(&seen), "Reading the file first. Then the directory.");
        assert_eq!(
            seen.iter()
                .filter(|event| !matches!(event, ProviderEvent::TextDelta(_)))
                .collect::<Vec<_>>(),
            vec![
                &ProviderEvent::ToolCallStart {
                    id: "call_read".to_owned(),
                    name: "read".to_owned(),
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_read".to_owned(),
                    json: "{\"file".to_owned(),
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_read".to_owned(),
                    json: "Path\":\"src/".to_owned(),
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_read".to_owned(),
                    json: "main.rs\"}".to_owned(),
                },
                &ProviderEvent::ToolCallStart {
                    id: "call_glob".to_owned(),
                    name: "glob".to_owned(),
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_glob".to_owned(),
                    json: "{\"pattern\"".to_owned(),
                },
                &ProviderEvent::ToolCallDelta {
                    id: "call_glob".to_owned(),
                    json: ":\"**/*.rs\"}".to_owned(),
                },
                // Chat completions has no per-call terminator, so both calls
                // close when the stream does, in index order.
                &ProviderEvent::ToolCallEnd {
                    id: "call_read".to_owned(),
                },
                &ProviderEvent::ToolCallEnd {
                    id: "call_glob".to_owned(),
                },
                // 317 prompt tokens of which the cache served 256: 61 fresh.
                &ProviderEvent::Usage(Usage {
                    input_tokens: 61,
                    output_tokens: 58,
                    cache_read_tokens: 256,
                    ..Usage::default()
                }),
                &ProviderEvent::Finish(FinishReason::Completed),
            ],
            "got {seen:?}"
        );
    }

    /// A call is executed when its arguments end, so closing one whose
    /// arguments never arrived would run a tool on half a request. A stream
    /// that died mid-call has to end as a failure with the call still open —
    /// which for this API means the `[DONE]` that closes calls never came.
    #[tokio::test]
    async fn a_stream_that_dies_mid_call_never_closes_it() {
        let seen = events(include_str!(
            "../../tests/fixtures/openai_tool_call_cut_short.sse"
        ))
        .await;

        assert_eq!(text(&seen), "Let me read that file.");
        assert_eq!(
            seen.iter()
                .filter(|event| !matches!(event, ProviderEvent::TextDelta(_)))
                .collect::<Vec<_>>(),
            vec![
                &ProviderEvent::ToolCallStart {
                    id: "call_cut".to_owned(),
                    name: "read".to_owned(),
                },
                // The chunk the body was cut in half of never arrives: an
                // incomplete frame is not a frame.
                &ProviderEvent::ToolCallDelta {
                    id: "call_cut".to_owned(),
                    json: "{\"file".to_owned(),
                },
                &ProviderEvent::Failed(ProviderError::Transport(
                    "the response body ended before the model finished".to_owned()
                )),
            ],
            "got {seen:?}"
        );
    }

    /// Pins the choice: a chunk that will not parse ends the turn. Skipping it
    /// would drop reply text with nothing downstream able to tell that a gap
    /// exists, and a transcript with a silent hole in it is worse than one that
    /// says it broke.
    #[tokio::test]
    async fn a_malformed_chunk_ends_the_turn_rather_than_being_skipped() {
        let seen = events(include_str!(
            "../../tests/fixtures/openai_malformed_frame.sse"
        ))
        .await;

        assert_eq!(text(&seen), "Hello", "text before the break is kept");
        assert!(
            matches!(
                seen.last(),
                Some(ProviderEvent::Failed(ProviderError::Parse(_)))
            ),
            "got {seen:?}"
        );
        assert_eq!(
            seen.len(),
            2,
            "nothing after the broken chunk is read, got {seen:?}"
        );
    }

    #[tokio::test]
    async fn a_body_that_stops_mid_reply_fails_rather_than_completing() {
        let seen = events(include_str!("../../tests/fixtures/openai_truncated.sse")).await;

        assert_eq!(text(&seen), "The connection drops right");
        assert!(
            matches!(
                seen.last(),
                Some(ProviderEvent::Failed(ProviderError::Transport(_)))
            ),
            "a dropped connection must never read as a finished turn, got {seen:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_done_sentinel_after_a_finish_reason_still_completes() {
        // Plenty of compatible servers just close the socket. The model said it
        // stopped, so the reply is whole and only the sentinel went missing.
        let seen = events(concat!(
            r#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "hi");
        assert_eq!(
            seen.last(),
            Some(&ProviderEvent::Finish(FinishReason::Completed))
        );
    }

    #[tokio::test]
    async fn an_error_chunk_ends_the_turn_as_a_failure() {
        let seen = events(concat!(
            r#"data: {"choices":[{"index":0,"delta":{"content":"partial"}}]}"#,
            "\n\n",
            r#"data: {"error":{"message":"upstream capacity exceeded","type":"server_error"}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "partial");
        assert_eq!(
            seen.last(),
            Some(&ProviderEvent::Failed(ProviderError::Status {
                status: 500,
                message: "upstream capacity exceeded".to_owned(),
            }))
        );
    }

    #[tokio::test]
    async fn a_cancel_mid_transcript_ends_the_stream_without_a_verdict() {
        let cancel = CancellationToken::new();
        let mut stream = Box::pin(replay(
            include_str!("../../tests/fixtures/openai_happy_path.sse"),
            cancel.clone(),
            Mapping::default(),
        ));

        assert_eq!(
            stream.next().await,
            Some(ProviderEvent::ReasoningDelta(
                "A greeting is enough.".to_owned()
            ))
        );
        cancel.cancel();

        let rest: Vec<ProviderEvent> = stream.collect().await;
        assert!(rest.is_empty(), "a cancelled stream ends: {rest:?}");
    }

    #[test]
    fn the_system_prompt_becomes_the_first_message() {
        let mut empty = Message::assistant("gpt");
        empty.parts.push(Part::text(""));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: Some("be brief".to_owned()),
            messages: vec![Message::user("hello"), empty, Message::user("again")],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body,
            serde_json::json!({
                "model": "gpt-test",
                "stream": true,
                "stream_options": {"include_usage": true},
                "messages": [
                    {"role": "system", "content": "be brief"},
                    {"role": "user", "content": "hello"},
                    {"role": "user", "content": "again"},
                ],
            })
        );
    }

    #[test]
    fn a_request_without_a_system_prompt_starts_with_the_user() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
        };
        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["messages"],
            serde_json::json!([{"role": "user", "content": "hi"}])
        );
    }

    /// The splice order at this wire's send site: the map passes through
    /// verbatim — this wire maps none of its keys — and still loses every key
    /// the wire itself writes.
    #[test]
    fn an_effort_passes_through_but_cannot_claim_the_model() {
        let request = ChatRequest {
            effort_options: serde_json::json!({
                "reasoning_effort": "high",
                "model": "someone-elses",
                "stream": false,
            })
            .as_object()
            .cloned()
            .expect("the fixture options are an object"),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
        };

        let own = Body::new(&request);
        let body = serde_json::to_value(splice_effort(&request.effort_options, &own))
            .expect("a spliced body serializes");

        assert_eq!(
            body["reasoning_effort"],
            serde_json::json!("high"),
            "a key the wire does not write arrives verbatim"
        );
        assert_eq!(
            body["model"],
            serde_json::json!("gpt-test"),
            "a key the wire writes resolves to the wire"
        );
        assert_eq!(body["stream"], serde_json::json!(true));
    }

    /// A tool part carrying `state`, as an assistant message holds one.
    fn tool_part(call_id: &str, tool: &str, state: ToolState) -> Part {
        Part {
            id: PartId::ascending(),
            body: PartBody::Tool {
                call_id: call_id.to_owned(),
                tool: tool.to_owned(),
                state,
            },
        }
    }

    /// The transcript of a turn that read a file and failed to glob: a step
    /// marker, some text, one call that worked, one that did not, and the step
    /// marker that closed the request.
    fn a_turn_with_two_calls() -> Message {
        let mut assistant = Message::assistant("gpt-test");

        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });
        assistant.parts.push(Part::text("Reading the file first."));
        assistant.parts.push(tool_part(
            "call_read",
            "read",
            ToolState::Completed {
                input: json!({"filePath": "src/main.rs"}),
                output: "fn main() {}".to_owned(),
                title: "src/main.rs".to_owned(),
                metadata: json!({}),
                started: 1,
                completed: 2,
            },
        ));
        assistant.parts.push(tool_part(
            "call_glob",
            "glob",
            ToolState::Error {
                input: json!({"pattern": "**/*.rs"}),
                error: "no such directory".to_owned(),
                started: 3,
                completed: 4,
            },
        ));
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepFinish {
                usage: Usage::default(),
            },
        });

        assistant
    }

    /// A request offering `read`, which is what a session with a registry
    /// sends on every turn.
    fn a_tool() -> ToolDefinition {
        ToolDefinition {
            name: "read".to_owned(),
            description: "Reads a file from disk.".to_owned(),
            schema: json!({
                "type": "object",
                "properties": {"filePath": {"type": "string"}},
                "required": ["filePath"],
            }),
        }
    }

    #[test]
    fn a_request_advertises_the_tools_it_was_given() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs")],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "Reads a file from disk.",
                    "parameters": {
                        "type": "object",
                        "properties": {"filePath": {"type": "string"}},
                        "required": ["filePath"],
                    },
                },
            }]),
            "got {body}"
        );
    }

    /// A turn that called tools has to read back to the model the way it
    /// happened: the calls on the assistant message that made them, and each
    /// result as the `tool` message that answers it.
    #[test]
    fn a_finished_call_is_sent_back_as_a_call_and_a_tool_message() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![
                Message::user("read src/main.rs"),
                a_turn_with_two_calls(),
                Message::user("thanks"),
            ],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "read src/main.rs"},
                {"role": "assistant", "content": "Reading the file first.", "tool_calls": [
                    {
                        "id": "call_read",
                        "type": "function",
                        // Arguments travel as a string, not as an object: the
                        // model streams them as text and this API carries them
                        // the way it received them.
                        "function": {"name": "read", "arguments": r#"{"filePath":"src/main.rs"}"#},
                    },
                    {
                        "id": "call_glob",
                        "type": "function",
                        "function": {"name": "glob", "arguments": r#"{"pattern":"**/*.rs"}"#},
                    },
                ]},
                {"role": "tool", "content": "fn main() {}", "tool_call_id": "call_read"},
                // A failure has nowhere to be flagged here, so it travels as
                // the text the model reads.
                {"role": "tool", "content": "no such directory", "tool_call_id": "call_glob"},
                {"role": "user", "content": "thanks"},
            ]),
            "got {body}"
        );
    }

    /// The transcript of a turn that took two model requests: it read a file,
    /// read what came back, and only then said what it was going to do about
    /// it. Both steps are parts of one assistant message, which is what makes
    /// the boundary between them worth respecting.
    fn a_turn_of_two_steps() -> Message {
        let mut assistant = Message::assistant("gpt-test");

        for (text, call_id, tool, input, output) in [
            (
                "Reading.",
                "call_read",
                "read",
                json!({"filePath": "src/main.rs"}),
                "fn main() { let x = 1; }",
            ),
            (
                "Now editing.",
                "call_edit",
                "edit",
                json!({"filePath": "src/main.rs", "oldString": "1", "newString": "2"}),
                "1 replacement",
            ),
        ] {
            assistant.parts.push(Part {
                id: PartId::ascending(),
                body: PartBody::StepStart,
            });
            assistant.parts.push(Part::text(text));
            assistant.parts.push(tool_part(
                call_id,
                tool,
                ToolState::Completed {
                    input,
                    output: output.to_owned(),
                    title: "src/main.rs".to_owned(),
                    metadata: json!({}),
                    started: 1,
                    completed: 2,
                },
            ));
            assistant.parts.push(Part {
                id: PartId::ascending(),
                body: PartBody::StepFinish {
                    usage: Usage::default(),
                },
            });
        }

        assistant
    }

    /// A turn that took two model requests reads back as two of them. The API
    /// would accept one flattened message — both calls in one `tool_calls`
    /// array, then both `tool` messages — but it would join "Reading." and "Now
    /// editing." into a single content string sitting ahead of every result, so
    /// a model re-reading its own trace would find its reasoning shuffled out
    /// from under the evidence it reasoned from.
    #[test]
    fn a_two_step_turn_is_sent_back_one_message_pair_per_step() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("fix the bug"), a_turn_of_two_steps()],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "fix the bug"},
                {"role": "assistant", "content": "Reading.", "tool_calls": [{
                    "id": "call_read",
                    "type": "function",
                    "function": {"name": "read", "arguments": r#"{"filePath":"src/main.rs"}"#},
                }]},
                {
                    "role": "tool",
                    "content": "fn main() { let x = 1; }",
                    "tool_call_id": "call_read",
                },
                // The second step opens here, after its evidence rather than
                // before it.
                {"role": "assistant", "content": "Now editing.", "tool_calls": [{
                    "id": "call_edit",
                    "type": "function",
                    "function": {
                        "name": "edit",
                        "arguments":
                            r#"{"filePath":"src/main.rs","newString":"2","oldString":"1"}"#,
                    },
                }]},
                {"role": "tool", "content": "1 replacement", "tool_call_id": "call_edit"},
            ]),
            "got {body}"
        );

        // The property the shape above exists for, stated on its own so that a
        // future rearrangement of the messages cannot quietly lose it.
        let wire = body["messages"].to_string();
        let position = |needle: &str| wire.find(needle).expect("the wire holds {needle}");
        assert!(
            position("Now editing.") > position("fn main() { let x = 1; }"),
            "what the model said in the second step must read as having been \
             said after the first step's result came back: {wire}"
        );
    }

    /// Older stored transcripts and hand-built messages carry no step markers
    /// at all. There is one step in that case, not none: everything the message
    /// holds, encoded exactly as it was before turns were ever split.
    #[test]
    fn a_turn_without_step_markers_is_one_step() {
        let mut assistant = Message::assistant("gpt-test");
        assistant.parts.push(Part::text("Reading."));
        assistant.parts.push(tool_part(
            "call_read",
            "read",
            ToolState::Completed {
                input: json!({"filePath": "src/main.rs"}),
                output: "fn main() {}".to_owned(),
                title: "src/main.rs".to_owned(),
                metadata: json!({}),
                started: 1,
                completed: 2,
            },
        ));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("read it"), assistant],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "read it"},
                {"role": "assistant", "content": "Reading.", "tool_calls": [{
                    "id": "call_read",
                    "type": "function",
                    "function": {"name": "read", "arguments": r#"{"filePath":"src/main.rs"}"#},
                }]},
                {"role": "tool", "content": "fn main() {}", "tool_call_id": "call_read"},
            ]),
            "got {body}"
        );
    }

    /// A turn cancelled while a tool was running leaves a call nobody answered,
    /// and an assistant turn following an unanswered call is a request this API
    /// refuses. Dropping the call instead would leave the reply talking about
    /// one that is not there, so the pair is completed with a placeholder.
    #[test]
    fn a_call_that_never_finished_is_answered_rather_than_left_dangling() {
        for state in [
            ToolState::Pending,
            ToolState::Running {
                input: json!({"filePath": "src/main.rs"}),
                metadata: serde_json::Value::Null,
                started: 1,
            },
        ] {
            let running = matches!(state, ToolState::Running { .. });
            let mut assistant = Message::assistant("gpt-test");
            assistant.parts.push(tool_part("call_read", "read", state));

            let request = ChatRequest {
                effort_options: Default::default(),
                model: "gpt-test".to_owned(),
                system: None,
                messages: vec![Message::user("read src/main.rs"), assistant],
                tools: Vec::new(),
            };

            let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

            assert_eq!(
                body["messages"][1],
                json!({"role": "assistant", "tool_calls": [{
                    "id": "call_read",
                    "type": "function",
                    "function": {
                        "name": "read",
                        // A call the model never finished streaming has no
                        // arguments, and the field is required.
                        "arguments": if running { r#"{"filePath":"src/main.rs"}"# } else { "{}" },
                    },
                }]}),
                "a turn that only called a tool has no content to send: {body}"
            );
            assert_eq!(
                body["messages"][2],
                json!({
                    "role": "tool",
                    "content": NO_RESULT,
                    "tool_call_id": "call_read",
                }),
                "an unanswered call must not reach the API unanswered: {body}"
            );
        }
    }

    /// Step markers never travel as content of their own — they are this
    /// crate's bookkeeping — but they do decide where one message ends and the
    /// next begins, so text either side of one is two messages rather than one
    /// joined string. A message holding nothing but markers is not a message at
    /// all.
    #[test]
    fn a_step_marker_starts_a_new_message_rather_than_being_dropped() {
        let mut assistant = Message::assistant("gpt-test");
        assistant.parts.push(Part::text("Reading the file."));
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepFinish {
                usage: Usage::default(),
            },
        });
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });
        assistant
            .parts
            .push(Part::text("It holds a main function."));

        let mut markers_only = Message::assistant("gpt-test");
        markers_only.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("hi"), assistant, markers_only],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "Reading the file."},
                {"role": "assistant", "content": "It holds a main function."},
            ]),
            "got {body}"
        );
    }

    /// Fragments of one step's reply, on the other hand, are one message: this
    /// API has a single content string per message, and joining them without a
    /// separator would run the last word of one into the first of the next.
    #[test]
    fn text_fragments_within_one_step_are_joined_into_one_message() {
        let mut assistant = Message::assistant("gpt-test");
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });
        assistant.parts.push(Part::text("Reading the file."));
        assistant
            .parts
            .push(Part::text("It holds a main function."));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("hi"), assistant],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["messages"][1],
            json!({
                "role": "assistant",
                "content": "Reading the file.\nIt holds a main function.",
            }),
            "got {body}"
        );
    }

    /// Both credentials a provider holds: the key it was built with, and
    /// whatever the base URL carries — which for this provider is the common
    /// case, since pointing it at a gateway is the whole reason the base URL is
    /// configurable.
    #[test]
    fn a_provider_never_renders_its_credential() {
        // Both shapes `check_base_url` blesses. The loopback one is not
        // hypothetical: it is what a local inference server is reached on, and
        // what the integration suite points this provider at.
        let cases = [
            (
                "https://ganja:sk-url-canary-9999@gateway.invalid:8443/v1?token=sk-query-canary-7777",
                "gateway.invalid:8443",
            ),
            (
                "http://ganja:sk-url-canary-9999@127.0.0.1:8080",
                "127.0.0.1:8080",
            ),
        ];

        for (base_url, endpoint) in cases {
            let provider = OpenAiProvider::new("sk-test-canary-XYZ")
                .expect("an HTTP client builds")
                .with_base_url(base_url);

            let rendered = format!("{provider:?}");

            for secret in [
                "sk-test-canary-XYZ",
                "sk-url-canary-9999",
                "sk-query-canary-7777",
                "ganja:",
            ] {
                assert!(
                    !rendered.contains(secret),
                    "a provider leaked {secret}: {rendered}"
                );
            }
            assert!(rendered.contains("[redacted]"), "got {rendered}");
            assert!(
                rendered.contains(endpoint),
                "the endpoint should survive being made safe to print: {rendered}"
            );
        }
    }

    #[test]
    fn a_blank_credential_is_refused() {
        assert!(OpenAiProvider::new("\t").is_err());
    }
}
