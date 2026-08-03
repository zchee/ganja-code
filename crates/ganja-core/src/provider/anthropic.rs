//! The Anthropic Messages API, streamed.
//!
//! Spec: `POST {base}/v1/messages` with `stream: true`, authenticated by an
//! `x-api-key` header and pinned to a dated API version. The response is an
//! event stream whose frames are named, so the mapping below is a match on
//! [`Frame::event`](super::sse::Frame::event) rather than on a payload field.
//!
//! Unknown event types are logged and skipped. Anthropic adds frame types
//! between API versions, and a turn that panicked or failed on one would make
//! every future addition a breaking change.

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
pub const ID: &str = "anthropic";

/// Environment variable carrying the credential.
pub const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Environment variable pointing the provider at a different endpoint, which is
/// what a proxy or a gateway needs.
pub const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// Where the Messages API lives.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// API version this build's request and response shapes were written against.
pub const API_VERSION: &str = "2023-06-01";

/// Reply length cap sent with every request.
///
/// The API requires the field and has no "as much as the model allows"
/// sentinel, so this is a ceiling rather than a target: the model stops when it
/// is done, and `stop_reason: max_tokens` is what a truncated reply looks like.
pub const DEFAULT_MAX_TOKENS: u32 = 8_192;

/// Streams replies from the Anthropic Messages API.
pub struct AnthropicProvider {
    client: reqwest::Client,
    key: ApiKey,
    base_url: String,
    max_tokens: u32,
}

impl fmt::Debug for AnthropicProvider {
    /// Renders without the credential, so that logging a provider — or a
    /// `tracing` field holding one — cannot leak it.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("key", &self.key)
            .field("base_url", &self.base_url)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

impl AnthropicProvider {
    /// Builds a provider that authenticates with `key`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be built,
    /// which in practice means the TLS backend failed to initialize.
    pub fn new(key: impl Into<String>) -> Result<Self, ProviderError> {
        let key = ApiKey::new(key)
            .ok_or_else(|| ProviderError::Auth(format!("{API_KEY_ENV} is empty")))?;

        Ok(Self {
            client: client()?,
            key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
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
            max_tokens: DEFAULT_MAX_TOKENS,
        })
    }

    /// Points the provider at `base_url` instead of [`DEFAULT_BASE_URL`].
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Caps replies at `max_tokens` instead of [`DEFAULT_MAX_TOKENS`].
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
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
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        ID
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let body = Body::new(&request, self.max_tokens);
        let built = self
            .client
            .post(format!(
                "{}/v1/messages",
                self.base_url.trim_end_matches('/')
            ))
            .header("x-api-key", self.key.expose())
            .header("anthropic-version", API_VERSION)
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
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<Turn<'a>>,
}

/// One message as the Messages API spells it.
#[derive(Debug, Serialize)]
struct Turn<'a> {
    role: &'static str,
    content: &'a str,
}

impl<'a> Body<'a> {
    fn new(request: &'a ChatRequest, max_tokens: u32) -> Self {
        Self {
            model: &request.model,
            max_tokens,
            stream: true,
            system: request.system.as_deref(),
            messages: request
                .messages
                .iter()
                .filter_map(|message| {
                    // The API rejects an empty content string, and an assistant
                    // turn that failed before its first fragment produces one.
                    let content = message.parts.first().and_then(Part::as_text)?;
                    (!content.trim().is_empty()).then_some(Turn {
                        role: match message.role {
                            Role::User => "user",
                            Role::Assistant => "assistant",
                        },
                        content,
                    })
                })
                .collect(),
        }
    }
}

/// Accumulates what the frames so far said.
#[derive(Debug, Default)]
struct Mapping {
    usage: Usage,
    /// Identifiers of the tool blocks still open, by content-block index.
    tools: HashMap<u64, String>,
    /// Set once the model said why it stopped.
    stopped: bool,
}

impl Mapper for Mapping {
    fn frame(&mut self, frame: &Frame, events: &mut Vec<ProviderEvent>) {
        let name = frame.event();

        // `ping` carries nothing and arrives often; skipping it before parsing
        // keeps the hot path off serde.
        if name == "ping" {
            return;
        }

        let data: Value = match serde_json::from_str(&frame.data) {
            Ok(data) => data,
            Err(error) => {
                // Every Anthropic frame is JSON and every one of them means
                // something, so skipping past a broken one would silently drop
                // part of the reply.
                events.push(ProviderEvent::Failed(ProviderError::Parse(format!(
                    "{name:?} frame: {error}"
                ))));
                return;
            }
        };

        match name {
            "message_start" => self.absorb(&data["message"]["usage"]),
            "content_block_start" => self.block_start(&data, events),
            "content_block_delta" => self.block_delta(&data, events),
            "content_block_stop" => {
                if let Some(id) = index(&data).and_then(|index| self.tools.remove(&index)) {
                    events.push(ProviderEvent::ToolCallEnd { id });
                }
            }
            "message_delta" => {
                self.absorb(&data["usage"]);
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.stopped = true;
                    tracing::debug!(stop_reason = reason, "the model stopped");
                }
            }
            "message_stop" => self.finish(events),
            "error" => events.push(ProviderEvent::Failed(failure(&data["error"]))),
            unknown => {
                tracing::debug!(event = unknown, "skipping an unfamiliar Anthropic frame");
            }
        }
    }

    fn truncated(&mut self, events: &mut Vec<ProviderEvent>) {
        // A body cut off after the model said why it stopped lost only the
        // terminator; one cut off before it lost reply text nobody can recover.
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
    /// Reads whichever token counts a `usage` object carries.
    ///
    /// Both `message_start` and `message_delta` carry one, and neither is
    /// complete: the first knows the prompt, the second the reply.
    fn absorb(&mut self, usage: &Value) {
        for (field, slot) in [
            ("input_tokens", &mut self.usage.input_tokens),
            ("output_tokens", &mut self.usage.output_tokens),
            ("cache_read_input_tokens", &mut self.usage.cache_read_tokens),
            (
                "cache_creation_input_tokens",
                &mut self.usage.cache_write_tokens,
            ),
        ] {
            if let Some(count) = usage[field].as_u64() {
                *slot = count;
            }
        }
    }

    fn block_start(&mut self, data: &Value, events: &mut Vec<ProviderEvent>) {
        let block = &data["content_block"];
        if block["type"] != "tool_use" {
            // Text and thinking blocks announce themselves too; their content
            // arrives as deltas, so there is nothing to report yet.
            return;
        }

        let id = block["id"].as_str().unwrap_or_default().to_owned();
        let name = block["name"].as_str().unwrap_or_default().to_owned();

        if let Some(index) = index(data) {
            self.tools.insert(index, id.clone());
        }
        events.push(ProviderEvent::ToolCallStart { id, name });
    }

    fn block_delta(&mut self, data: &Value, events: &mut Vec<ProviderEvent>) {
        let delta = &data["delta"];

        match delta["type"].as_str() {
            Some("text_delta") => {
                if let Some(text) = delta["text"].as_str() {
                    events.push(ProviderEvent::TextDelta(text.to_owned()));
                }
            }
            Some("thinking_delta") => {
                if let Some(thinking) = delta["thinking"].as_str() {
                    events.push(ProviderEvent::ReasoningDelta(thinking.to_owned()));
                }
            }
            Some("input_json_delta") => {
                let Some(id) = index(data).and_then(|index| self.tools.get(&index)) else {
                    tracing::debug!("arguments arrived for a tool block that never opened");
                    return;
                };

                events.push(ProviderEvent::ToolCallDelta {
                    id: id.clone(),
                    json: delta["partial_json"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
            // `signature_delta` and whatever comes next: the reply reads the
            // same without them.
            other => tracing::debug!(delta = other, "skipping an unfamiliar Anthropic delta"),
        }
    }

    /// Reports the bill and ends the turn.
    fn finish(&mut self, events: &mut Vec<ProviderEvent>) {
        // Anthropic closes every block before it stops, so this normally finds
        // nothing. It matters when the body was cut off after the stop reason:
        // P3 executes a call when its arguments end, and a call that never ends
        // is a tool that never runs. Ordered so a replay is deterministic.
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

/// The content-block index a frame addresses.
fn index(data: &Value) -> Option<u64> {
    data["index"].as_u64()
}

/// Turns an `error` frame into the failure the turn reports.
///
/// Anthropic names its error types rather than restating the HTTP status, so
/// the status is reconstructed from the name — which is what makes
/// [`ProviderError::is_retryable`] work on a mid-stream overload.
fn failure(error: &Value) -> ProviderError {
    let kind = error["type"].as_str().unwrap_or("api_error");
    let message = error["message"]
        .as_str()
        .unwrap_or("the provider reported an error")
        .to_owned();

    match kind {
        "invalid_request_error" => ProviderError::Status {
            status: 400,
            message,
        },
        "authentication_error" => ProviderError::Auth(message),
        "permission_error" => ProviderError::Status {
            status: 403,
            message,
        },
        "not_found_error" => ProviderError::Status {
            status: 404,
            message,
        },
        "request_too_large" => ProviderError::Status {
            status: 413,
            message,
        },
        "rate_limit_error" => ProviderError::Status {
            status: 429,
            message,
        },
        "overloaded_error" => ProviderError::Status {
            status: 529,
            message,
        },
        _ => ProviderError::Status {
            status: 500,
            message,
        },
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use tokio_util::sync::CancellationToken;

    use super::{AnthropicProvider, Body, DEFAULT_MAX_TOKENS, Mapping};
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
        let seen = events(include_str!(
            "../../tests/fixtures/anthropic_happy_path.sse"
        ))
        .await;

        assert_eq!(text(&seen), "Hello, world!");
        assert_eq!(
            seen.iter()
                .filter_map(|event| match event {
                    ProviderEvent::ReasoningDelta(delta) => Some(delta.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            "The user wants a greeting.",
            "a thinking block should become reasoning, not reply text"
        );
        assert_eq!(
            &seen[seen.len() - 2..],
            &[
                ProviderEvent::Usage(Usage {
                    input_tokens: 1_024,
                    output_tokens: 12,
                    reasoning_tokens: 0,
                    cache_read_tokens: 768,
                    cache_write_tokens: 256,
                }),
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            "the bill is reported before the finish, got {seen:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, ProviderEvent::Failed(_))),
            "ping, comments, signature deltas and an unknown event type are all skipped, \
             not fatal: {seen:?}"
        );
    }

    #[tokio::test]
    async fn an_error_frame_ends_the_turn_as_a_failure() {
        let seen = events(include_str!(
            "../../tests/fixtures/anthropic_mid_stream_error.sse"
        ))
        .await;

        assert_eq!(text(&seen), "Let me start by", "partial text is kept");
        assert_eq!(
            seen.last(),
            Some(&ProviderEvent::Failed(ProviderError::Status {
                status: 529,
                message: "Overloaded".to_owned(),
            })),
            "an overload should map to the status that makes it retryable, got {seen:?}"
        );
    }

    #[tokio::test]
    async fn tool_blocks_interleave_with_text_without_losing_either() {
        let seen = events(include_str!(
            "../../tests/fixtures/anthropic_tool_use_interleaved.sse"
        ))
        .await;

        assert_eq!(
            text(&seen),
            "Reading the file first. And listing the directory."
        );
        assert_eq!(
            seen.iter()
                .filter(|event| !matches!(event, ProviderEvent::TextDelta(_)))
                .collect::<Vec<_>>(),
            vec![
                &ProviderEvent::ToolCallStart {
                    id: "toolu_01Read".to_owned(),
                    name: "read".to_owned()
                },
                &ProviderEvent::ToolCallDelta {
                    id: "toolu_01Read".to_owned(),
                    json: "{\"file".to_owned()
                },
                &ProviderEvent::ToolCallDelta {
                    id: "toolu_01Read".to_owned(),
                    json: "Path\":\"src/main.rs\"}".to_owned()
                },
                &ProviderEvent::ToolCallEnd {
                    id: "toolu_01Read".to_owned()
                },
                &ProviderEvent::ToolCallStart {
                    id: "toolu_01Glob".to_owned(),
                    name: "glob".to_owned()
                },
                &ProviderEvent::ToolCallDelta {
                    id: "toolu_01Glob".to_owned(),
                    json: "{\"pattern\":\"**/*.rs\"}".to_owned()
                },
                &ProviderEvent::ToolCallEnd {
                    id: "toolu_01Glob".to_owned()
                },
                &ProviderEvent::Usage(Usage {
                    input_tokens: 211,
                    output_tokens: 94,
                    cache_read_tokens: 128,
                    ..Usage::default()
                }),
                &ProviderEvent::Finish(FinishReason::Completed),
            ],
            "every call should be opened, filled and closed exactly once"
        );
    }

    #[tokio::test]
    async fn a_body_that_stops_mid_reply_fails_rather_than_completing() {
        let seen = events(include_str!("../../tests/fixtures/anthropic_truncated.sse")).await;

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
    async fn a_malformed_frame_ends_the_turn_rather_than_silently_skipping_it() {
        let seen = replay(
            "event: message_start\ndata: {\"type\":\"message_start\"\n\n",
            CancellationToken::new(),
            Mapping::default(),
        )
        .collect::<Vec<_>>()
        .await;

        assert!(
            matches!(
                seen.as_slice(),
                [ProviderEvent::Failed(ProviderError::Parse(_))]
            ),
            "got {seen:?}"
        );
    }

    #[tokio::test]
    async fn a_cancel_mid_transcript_ends_the_stream_without_a_verdict() {
        let cancel = CancellationToken::new();
        let mut stream = Box::pin(replay(
            include_str!("../../tests/fixtures/anthropic_happy_path.sse"),
            cancel.clone(),
            Mapping::default(),
        ));

        assert_eq!(
            stream.next().await,
            Some(ProviderEvent::ReasoningDelta(
                "The user wants a greeting.".to_owned()
            ))
        );
        cancel.cancel();

        let rest: Vec<ProviderEvent> = stream.collect().await;
        assert!(
            rest.is_empty(),
            "a cancelled stream ends; the engine is what calls that Cancelled, and it \
             cannot if a Finish or a Failed arrives: {rest:?}"
        );
    }

    #[test]
    fn a_request_carries_the_transcript_and_the_system_prompt() {
        let mut empty = Message::assistant("claude");
        empty.parts.push(Part::text(""));

        let request = ChatRequest {
            model: "claude-test".to_owned(),
            system: Some("be brief".to_owned()),
            messages: vec![
                Message::user("hello"),
                Message::assistant("claude"),
                empty,
                Message::user("again"),
            ],
        };

        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(
            body,
            serde_json::json!({
                "model": "claude-test",
                "max_tokens": DEFAULT_MAX_TOKENS,
                "stream": true,
                "system": "be brief",
                "messages": [
                    {"role": "user", "content": "hello"},
                    {"role": "user", "content": "again"},
                ],
            }),
            "a message with nothing in it is not sent: the API rejects it"
        );
    }

    #[test]
    fn a_request_without_a_system_prompt_omits_the_field() {
        let request = ChatRequest {
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("hi")],
        };
        let body = serde_json::to_string(&Body::new(&request, 16)).expect("the body serializes");

        assert!(!body.contains("system"), "got {body}");
        assert!(body.contains(r#""max_tokens":16"#), "got {body}");
    }

    #[test]
    fn a_provider_never_renders_its_credential() {
        let provider = AnthropicProvider::new("sk-test-canary-XYZ")
            .expect("an HTTP client builds")
            .with_base_url("https://example.invalid");

        let rendered = format!("{provider:?}");

        assert!(
            !rendered.contains("sk-test-canary-XYZ"),
            "a provider leaked its key: {rendered}"
        );
        assert!(rendered.contains("[redacted]"), "got {rendered}");
    }

    #[test]
    fn a_blank_credential_is_refused() {
        assert!(AnthropicProvider::new("  ").is_err());
    }
}
