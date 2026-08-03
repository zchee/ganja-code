//! OpenAI-compatible chat completions, streamed.
//!
//! Spec: `POST {base}/chat/completions` with `stream: true`, authenticated by a
//! bearer token. The base URL is configurable because the shape is a de-facto
//! standard: the same code drives OpenAI, a local llama.cpp server, an
//! OpenRouter key, or anything else that copied the schema.
//!
//! Unlike Anthropic, the frames are unnamed — every one is a `data:` line
//! holding a chunk object, and the stream ends with the literal `[DONE]`.

use std::{collections::HashMap, fmt};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    protocol::{FinishReason, Part, Role, Usage},
    provider::{
        ApiKey, ChatRequest, Mapper, Provider, ProviderError, ProviderEvent, open,
        require_credential, setting, sse::Frame,
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
    key: ApiKey,
    base_url: String,
}

impl fmt::Debug for OpenAiProvider {
    /// Renders without the credential, so that logging a provider — or a
    /// `tracing` field holding one — cannot leak it.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("key", &self.key)
            .field("base_url", &self.base_url)
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
    pub fn new(key: impl Into<String>) -> Result<Self, ProviderError> {
        let key = ApiKey::new(key)
            .ok_or_else(|| ProviderError::Auth(format!("{API_KEY_ENV} is empty")))?;

        Ok(Self {
            client: client()?,
            key,
            base_url: DEFAULT_BASE_URL.to_owned(),
        })
    }

    /// Builds a provider from [`API_KEY_ENV`] and [`BASE_URL_ENV`].
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Auth`] when the credential is missing, so that
    /// a misconfigured session dies at startup rather than at the first prompt.
    pub fn from_env() -> Result<Self, ProviderError> {
        Ok(Self {
            client: client()?,
            key: require_credential(ID, API_KEY_ENV)?,
            base_url: setting(BASE_URL_ENV).unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
        })
    }

    /// Points the provider at `base_url` instead of [`DEFAULT_BASE_URL`], which
    /// is what makes this provider work against a compatible endpoint.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

/// Builds the shared HTTP client.
fn client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .build()
        .map_err(|error| ProviderError::Transport(format!("no HTTP client: {error}")))
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
        let body = Body::new(&request);
        let built = self
            .client
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .bearer_auth(self.key.expose())
            .json(&body)
            .build()
            .map_err(|error| {
                ProviderError::Transport(self.key.redact(&format!("malformed request: {error}")))
            })?;

        open(&self.client, built, &self.key, cancel, Mapping::default()).await
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
}

/// Opt-in for the trailing usage chunk.
#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// One message as chat completions spells it.
#[derive(Debug, Serialize)]
struct Turn<'a> {
    role: &'static str,
    content: &'a str,
}

impl<'a> Body<'a> {
    fn new(request: &'a ChatRequest) -> Self {
        // Chat completions has no `system` field: the prompt is the first
        // message, with its own role.
        let system = request.system.as_deref().map(|content| Turn {
            role: "system",
            content,
        });

        Self {
            model: &request.model,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            messages: system
                .into_iter()
                .chain(request.messages.iter().filter_map(|message| {
                    let content = message.parts.first().and_then(Part::as_text)?;
                    (!content.trim().is_empty()).then_some(Turn {
                        role: match message.role {
                            Role::User => "user",
                            Role::Assistant => "assistant",
                        },
                        content,
                    })
                }))
                .collect(),
        }
    }
}

/// Accumulates what the chunks so far said.
#[derive(Debug, Default)]
struct Mapping {
    usage: Usage,
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
            ("prompt_tokens", &mut self.usage.input_tokens),
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
    use tokio_util::sync::CancellationToken;

    use super::{Body, Mapping, OpenAiProvider};
    use crate::{
        protocol::{FinishReason, Message, Part, Usage},
        provider::{ChatRequest, ProviderError, ProviderEvent, replay},
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
                    input_tokens: 42,
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
            model: "gpt-test".to_owned(),
            system: Some("be brief".to_owned()),
            messages: vec![Message::user("hello"), empty, Message::user("again")],
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
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("hi")],
        };
        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["messages"],
            serde_json::json!([{"role": "user", "content": "hi"}])
        );
    }

    #[test]
    fn a_provider_never_renders_its_credential() {
        let provider = OpenAiProvider::new("sk-test-canary-XYZ")
            .expect("an HTTP client builds")
            .with_base_url("https://example.invalid/v1");

        let rendered = format!("{provider:?}");

        assert!(
            !rendered.contains("sk-test-canary-XYZ"),
            "a provider leaked its key: {rendered}"
        );
        assert!(rendered.contains("[redacted]"), "got {rendered}");
    }

    #[test]
    fn a_blank_credential_is_refused() {
        assert!(OpenAiProvider::new("\t").is_err());
    }
}
