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
//!
//! Three things a wrapper may declare about itself and nothing else may —
//! `AnthropicProvider::with_credential`, [`AnthropicProvider::with_base_url`]
//! and `AnthropicProvider::with_headers` — so an endpoint speaking this API
//! under a name a config chose is a wrapper rather than a fork. That is
//! [`super::openai`]'s seam set, completed here for [`super::compat`]'s sake.

use std::{borrow::Cow, collections::HashMap, fmt, sync::LazyLock};

use async_trait::async_trait;
use futures::stream::BoxStream;
use secrecy::SecretString;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    catalog,
    protocol::{FinishReason, Part, PartBody, Role, ToolState, Usage},
    provider::{
        ChatRequest, CredentialSource, Mapper, NO_RESULT, Presented, Provider, ProviderError,
        ProviderEvent, check_base_url, client, open, require_key, setting, shown_base_url,
        splice_effort,
        sse::Frame,
        steps,
        toolname::{ANTHROPIC_CAP, Aliases, alias},
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
///
/// The number is upstream's `OUTPUT_TOKEN_MAX` (`provider/transform.ts`), which
/// is deliberately below what the current models will generate — their own
/// limits are 64k and up. A ceiling that far under the model's is what keeps a
/// single runaway reply from spending a context window, and
/// `AnthropicProvider::max_tokens` lowers it further for any model whose own
/// limit is smaller.
pub const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// Arguments for a call the model never finished streaming.
///
/// `input` is required on a `tool_use` block, and a [`ToolState::Pending`] part
/// has none to give: an empty object is the honest spelling of "the model was
/// still saying".
static NO_INPUT: LazyLock<Value> = LazyLock::new(|| Value::Object(serde_json::Map::new()));

/// Streams replies from the Anthropic Messages API.
pub struct AnthropicProvider {
    client: reqwest::Client,
    credential: CredentialSource,
    base_url: String,
    max_tokens: u32,
    /// What every request carries besides `x-api-key` and the pinned API
    /// version.
    ///
    /// Empty for Anthropic itself, and for every gateway that copied the
    /// Messages schema and asks for nothing but the key. What fills it is a
    /// config-named endpoint that wants one — a routing header a proxy
    /// dispatches on, or the beta opt-in some deployments require — declared
    /// through [`with_headers`](Self::with_headers). Held rather than passed
    /// per request for [`super::openai::OpenAiProvider`]'s reason: headers
    /// describe the endpoint, which is fixed when the provider is built, and
    /// never the credential, which is not.
    headers: reqwest::header::HeaderMap,
    /// What this endpoint last said was left of the account's budget
    /// (**D484**). Held by the wire rather than by a session because that is
    /// what the headers measure; the parsing is [`super::rate`]'s, shared with
    /// every other wire, so nothing about this vendor's spelling is repeated
    /// here.
    rates: super::RateWindows,
}

impl fmt::Debug for AnthropicProvider {
    /// Renders without the credential, so that logging a provider — or a
    /// `tracing` field holding one — cannot leak it.
    ///
    /// That includes the base URL, which is allowed to carry a credential in
    /// its userinfo and is not exempt from the rule just because the credential
    /// arrived as configuration — and `headers`, which is
    /// somewhere a configured endpoint's token fits and which is therefore
    /// rendered no more than the key is.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("credential", &self.credential)
            .field("base_url", &shown_base_url(&self.base_url))
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
    /// through — see [`super::compat`], whose endpoint speaks this API under a
    /// name a config chose. The counterpart of
    /// [`OpenAiProvider::with_credential`](super::openai::OpenAiProvider::with_credential),
    /// and module-internal for its reason: what a caller outside this module
    /// picks between is providers rather than credential sources, so the one
    /// caller that assembles a provider out of parts — [`super::compat`] — is
    /// the only one here.
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
            max_tokens: DEFAULT_MAX_TOKENS,
            headers: reqwest::header::HeaderMap::new(),
            rates: super::RateWindows::default(),
        })
    }

    /// Points the provider at `base_url` instead of [`DEFAULT_BASE_URL`].
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Puts `headers` on every request this provider sends.
    ///
    /// The third thing a wrapper declares about itself, beside
    /// [`with_credential`](Self::with_credential) and the base URL — see
    /// [`headers`](Self::headers) for what fills it and why nothing else does.
    /// Crate-internal for the same reason those are.
    #[must_use]
    pub(super) fn with_headers(mut self, headers: reqwest::header::HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    /// The reply cap a request for `model` may ask for.
    ///
    /// The configured ceiling, lowered to whatever the catalog says the model
    /// will generate in one reply: asking a model for more than its own limit
    /// is a 400 rather than a longer answer. A model the table does not know
    /// keeps the configured value, because there is nothing truer to say about
    /// it. This is upstream's `maxOutputTokens` —
    /// `min(model.limit.output, OUTPUT_TOKEN_MAX)` — with
    /// [`DEFAULT_MAX_TOKENS`] as the second term.
    fn max_tokens(&self, model: &str) -> u32 {
        catalog::model(model)
            .and_then(|info| u32::try_from(info.max_output).ok())
            .map_or(self.max_tokens, |ceiling| ceiling.min(self.max_tokens))
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        ID
    }

    /// The media types the Messages API documents for `image` source blocks,
    /// plus the PDF its `document` block carries. Everything else — including
    /// `image/avif`, which the attachment allowlist names but no block here
    /// accepts — degrades to text at the engine rather than being sent as a
    /// block the API would refuse.
    fn accepts_attachment(&self, mime: &str) -> bool {
        matches!(
            mime,
            "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "application/pdf"
        )
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // Checked here as well as at startup because `with_base_url` can point
        // a provider anywhere, and this is the last moment before the key goes
        // on the wire.
        check_base_url(&self.base_url)?;

        // Resolved before the request is built rather than captured at
        // construction, for the reason `openai.rs` gives: a key resolves to
        // itself and pays nothing, and the seam is what lets a wrapper name
        // another source without forking the wire.
        let presented = self.credential.presented().await?;
        // The effort's options go under the wire's own fields, so a catalog
        // row can add `thinking` but can never unmake `model` or `max_tokens`.
        let own = Body::new(&request, self.max_tokens(&request.model));
        let body = splice_effort(&request.effort_options, &own);
        // Built from the same roster the body just advertised, so the decoder
        // reads back exactly what this request offered. Cloned per attempt
        // because `open` may call the factory again on a retry.
        let aliases = Aliases::of(&request.tools, ANTHROPIC_CAP);
        let built = self
            .client
            .post(format!(
                "{}/v1/messages",
                self.base_url.trim_end_matches('/')
            ))
            .header("x-api-key", presented.expose())
            .header("anthropic-version", API_VERSION)
            // After the credential, and never carrying one: these describe the
            // endpoint, and a secret put here would travel outside the
            // redaction `presented` is the single source of.
            .headers(self.headers.clone())
            .json(&body)
            .build()
            .map_err(|error| {
                ProviderError::Transport(presented.redact(&format!("malformed request: {error}")))
            })?;

        tracing::debug!(
            provider = ID,
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
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<Turn<'a>>,
    /// Omitted rather than sent empty. A turn with nothing to offer the model
    /// is the ordinary case for a session with no registry, and `"tools": []`
    /// is a field several compatible endpoints treat differently from its
    /// absence.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolSpec<'a>>,
}

/// One tool as the Messages API advertises it.
#[derive(Debug, Serialize)]
struct ToolSpec<'a> {
    /// The name the model is told, which is the registry's own unless that one
    /// is outside this API's `^[a-zA-Z0-9_-]{1,128}$` — see [`alias`].
    name: Cow<'a, str>,
    description: &'a str,
    input_schema: &'a Value,
}

/// One message as the Messages API spells it.
#[derive(Debug, Serialize)]
struct Turn<'a> {
    role: &'static str,
    content: Content<'a>,
}

/// What a message carries.
///
/// The API accepts a bare string for a message that is only text, which is what
/// every message was before tools, and what most still are.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Content<'a> {
    /// The whole message is this text.
    Text(&'a str),
    /// Anything else: text beside calls, or a message of results.
    Blocks(Vec<Block<'a>>),
}

/// One content block.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block<'a> {
    /// Something the model said, or was told.
    Text {
        /// The text itself.
        text: &'a str,
    },
    /// An image the user attached, base64 the engine encoded at send time.
    Image {
        /// The payload, always base64 here.
        source: Source<'a>,
    },
    /// A PDF the user attached — this API's name for a file block.
    Document {
        /// The payload, always base64 here.
        source: Source<'a>,
    },
    /// A call the model made.
    ToolUse {
        /// The provider's identifier for the call, which its result names.
        id: &'a str,
        /// Tool that was called, under the same [`alias`] the model was
        /// originally offered it as — aliasing is deterministic, so replaying
        /// a transcript needs nothing remembered from the turn that made it.
        name: Cow<'a, str>,
        /// The arguments it was called with.
        input: &'a Value,
    },
    /// What a call produced, or why it produced nothing.
    ToolResult {
        /// The call this answers.
        tool_use_id: &'a str,
        /// The result, as the model sees it.
        content: &'a str,
        /// Sent only when the call failed; the API reads its absence as
        /// success.
        #[serde(skip_serializing_if = "succeeded")]
        is_error: bool,
    },
}

/// Whether a result block describes a call that worked.
fn succeeded(is_error: &bool) -> bool {
    !*is_error
}

/// The payload of an [`Block::Image`] or [`Block::Document`], in the one form
/// this build sends: base64 the engine encoded when it built the request.
///
/// The shape is the API's `{"type":"base64","media_type":…,"data":…}` source
/// object verbatim; the URL form the API also accepts stays unused because a
/// mention names a local file, and the whole point of the send-time read is
/// that its bytes travel rather than a path nobody else can follow.
#[derive(Debug, Serialize)]
struct Source<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: &'a str,
    data: &'a str,
}

impl<'a> Source<'a> {
    /// A base64 source carrying `data` as `media_type`.
    fn base64(media_type: &'a str, data: &'a str) -> Self {
        Self {
            kind: "base64",
            media_type,
            data,
        }
    }
}

impl<'a> Body<'a> {
    /// Turns a request into the JSON the Messages API expects.
    ///
    /// # How a transcript becomes a request
    ///
    /// Canonical [`Message`](crate::protocol::Message)s carry ordered parts and
    /// hold a call beside its result; the API carries ordered content blocks
    /// and puts the result in the *next* message, with the user role. So each
    /// of a message's [`steps`] becomes up to two:
    ///
    /// - its text parts become `text` blocks and its tool parts `tool_use`
    ///   blocks, in the order the parts are in, so the model reads its own
    ///   reply back in the order it produced it;
    /// - every one of that step's calls contributes a `tool_result` block to a
    ///   single user message placed immediately after, because the API answers
    ///   a message's calls in the message that follows it and rejects a
    ///   `tool_result` naming a call it cannot see. All of one step's results
    ///   share that one user message, which is what "immediately after" leaves
    ///   room for.
    ///
    /// Step markers themselves serialize to nothing: they say where the split
    /// falls, not anything the model said.
    ///
    /// A step that contributes no blocks is dropped rather than sent empty: the
    /// API rejects empty content, and both the marker that opens a turn and an
    /// assistant turn that failed before its first fragment are exactly that.
    ///
    /// Two *steps of one message* that end up with the same role are merged
    /// back into a single message, which is what keeps splitting a turn up from
    /// ever emitting a transcript whose roles fail to alternate. Steps normally
    /// alternate on their own — every step but a turn's last one ends in calls,
    /// and those calls' results are a user message in between — so this catches
    /// only the interrupted shapes that do not, which before this split were
    /// one message anyway. Upstream arrives here from the other direction: the
    /// AI SDK's Anthropic provider regroups the per-step messages it is handed
    /// by role (`groupIntoBlocks`) before it builds a request.
    fn new(request: &'a ChatRequest, max_tokens: u32) -> Self {
        let mut turns: Vec<(&'static str, Vec<Block<'a>>)> =
            Vec::with_capacity(request.messages.len());

        for message in &request.messages {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            // Where this message's own steps start, so that merging stays
            // inside it: two adjacent canonical messages are two things the
            // transcript holds apart, whatever their roles.
            let first = turns.len();

            for step in steps(&message.parts) {
                let (blocks, results) = split(step);
                if blocks.is_empty() {
                    continue;
                }

                merge(&mut turns, first, role, blocks);
                if !results.is_empty() {
                    merge(&mut turns, first, "user", results);
                }
            }
        }

        Self {
            model: &request.model,
            max_tokens,
            stream: true,
            system: request.system.as_deref(),
            messages: turns
                .into_iter()
                .map(|(role, blocks)| Turn {
                    role,
                    content: content(blocks),
                })
                .collect(),
            tools: request
                .tools
                .iter()
                .map(|tool| ToolSpec {
                    name: alias(&tool.name, ANTHROPIC_CAP),
                    description: &tool.description,
                    input_schema: &tool.schema,
                })
                .collect(),
        }
    }
}

/// Adds `blocks` to the transcript, extending the message before them when that
/// one carries `role` too and belongs to the same canonical message —
/// everything from index `first` on.
fn merge<'a>(
    turns: &mut Vec<(&'static str, Vec<Block<'a>>)>,
    first: usize,
    role: &'static str,
    blocks: Vec<Block<'a>>,
) {
    if turns.len() > first
        && let Some((last, held)) = turns.last_mut()
        && *last == role
    {
        held.extend(blocks);
        return;
    }

    turns.push((role, blocks));
}

/// Splits one step into the blocks it sends and the results that answer it.
fn split(parts: &[Part]) -> (Vec<Block<'_>>, Vec<Block<'_>>) {
    let mut blocks = Vec::with_capacity(parts.len());
    let mut results = Vec::new();

    for part in parts {
        match &part.body {
            PartBody::Text { text } => {
                if !text.trim().is_empty() {
                    blocks.push(Block::Text { text });
                }
            }
            PartBody::Tool {
                call_id,
                tool,
                state,
            } => {
                blocks.push(Block::ToolUse {
                    id: call_id,
                    name: alias(tool, ANTHROPIC_CAP),
                    input: input(state),
                });

                let (content, is_error) = result(state);
                results.push(Block::ToolResult {
                    tool_use_id: call_id,
                    content,
                    is_error,
                });
            }
            // A binary attachment the engine read at send time: its base64
            // rides the request's own copy of the file part, and only for a
            // mime `accepts_attachment` said yes to, so the match below is by
            // payload shape rather than by allowlist.
            PartBody::File {
                mime,
                content: Some(content),
                ..
            } => {
                let source = Source::base64(mime, content);
                blocks.push(if mime == "application/pdf" {
                    Block::Document { source }
                } else {
                    Block::Image { source }
                });
            }
            // A mentioned *text* file is a reference, and the reference is
            // resolved into a text block before a request is built
            // (`session::resolve_mentions`). One arriving here with no content
            // would be a request built past that resolution, and sending a
            // path the model cannot follow would read as content it could.
            //
            // `StepFinish` carries a step's bill rather than content, and
            // `StepStart` was consumed as the boundary this step was cut at.
            //
            // A `Reasoning` part is another wire's sealed state — this API's
            // own equivalent is a `thinking` block with a signature, which
            // this build does not port — and handing an opaque blob to the
            // provider that did not seal it is not a thing to attempt.
            //
            // `ReasoningText` is thinking this build renders rather than
            // replays: the sealed half above is what a provider asked to have
            // handed back, and sending the readable half beside it would be
            // sending the same thought twice, once in a form nothing asked
            // for.
            PartBody::File { content: None, .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
            | PartBody::ReasoningText { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Reasoning { .. } => {}
        }
    }

    (blocks, results)
}

/// Spells `blocks` the shortest way the API accepts them.
fn content(blocks: Vec<Block<'_>>) -> Content<'_> {
    if let [Block::Text { text }] = blocks.as_slice() {
        return Content::Text(text);
    }

    Content::Blocks(blocks)
}

/// The arguments a call ran with, or [`NO_INPUT`] when it never got that far.
fn input(state: &ToolState) -> &Value {
    match state {
        ToolState::Pending => &NO_INPUT,
        ToolState::Running { input, .. }
        | ToolState::Completed { input, .. }
        | ToolState::Error { input, .. } => input,
    }
}

/// What a call produced and whether that counts as a failure.
fn result(state: &ToolState) -> (&str, bool) {
    match state {
        ToolState::Completed { output, .. } => (output, false),
        ToolState::Error { error, .. } => (error, true),
        // See [`NO_RESULT`]: the turn that made this call died before the tool
        // answered, and an unanswered call is a request the API refuses.
        ToolState::Pending | ToolState::Running { .. } => (NO_RESULT, true),
    }
}

/// Accumulates what the frames so far said.
#[derive(Debug, Default)]
struct Mapping {
    usage: Usage,
    /// Identifiers of the tool blocks still open, by content-block index.
    tools: HashMap<u64, String>,
    /// What this request's advertised names map back to, empty for the
    /// ordinary roster whose names this API already accepts.
    aliases: Aliases,
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
        // Back through this request's own map: what the engine executes, what
        // the permission rules match and what the transcript records is the
        // registry name, never the one the wire had to advertise.
        let name = self
            .aliases
            .original(block["name"].as_str().unwrap_or_default().to_owned());

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
        // the engine executes a call when its arguments end, and a call that
        // never ends is a tool that never runs. Ordered so a replay is
        // deterministic.
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
    // Not logged here: the failure is warned once, redacted, at
    // `provider::shielded`, the seam that holds the credential to mask with.
    let message = super::reported(error);

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
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::{
        ANTHROPIC_CAP, Aliases, AnthropicProvider, Body, DEFAULT_MAX_TOKENS, Frame, ID,
        Mapper as _, Mapping, NO_RESULT, alias,
    };
    use crate::{
        catalog,
        protocol::{FinishReason, Message, Part, PartBody, PartId, ToolState, Usage},
        provider::{
            ChatRequest, PROVIDERS, Provider as _, ProviderError, ProviderEvent, replay,
            splice_effort,
        },
        tool::ToolDefinition,
    };

    /// The obligation `catalog`'s own table test states per *tier* and each
    /// wire states for itself: a provider a session can select has to be one
    /// the catalog can size and price, or the first turn has no model to ask
    /// for and no cost to report.
    ///
    /// Named per provider rather than derived, because the tier predicate
    /// deliberately excuses a provider with no rows — a wire that lost its
    /// rows would pass there and has to fail here.
    #[test]
    fn an_anthropic_session_that_names_no_model_gets_one_the_catalog_can_price() {
        assert!(PROVIDERS.contains(&ID));

        let id = catalog::default_model(ID).expect("anthropic has a pinned default");
        let info = catalog::model(id).expect("the default is in the table");

        assert_eq!(info.provider_id, ID);
        assert!(info.context_window > 0 && info.max_output > 0);
        assert!(
            info.pricing.input > 0.0 && info.pricing.output > 0.0,
            "a priced provider with a free row is a row nobody filled in"
        );
    }

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

    /// A call is executed when its arguments end, so closing one whose
    /// arguments never arrived would run a tool on half a request. A stream
    /// that died mid-call has to end as a failure with the call still open.
    #[tokio::test]
    async fn a_stream_that_dies_mid_call_never_closes_it() {
        let seen = events(include_str!(
            "../../tests/fixtures/anthropic_tool_call_cut_short.sse"
        ))
        .await;

        assert_eq!(text(&seen), "Let me read that file.");
        assert_eq!(
            seen.iter()
                .filter(|event| !matches!(event, ProviderEvent::TextDelta(_)))
                .collect::<Vec<_>>(),
            vec![
                &ProviderEvent::ToolCallStart {
                    id: "toolu_01Cut".to_owned(),
                    name: "read".to_owned(),
                },
                // The fragment the body was cut in half of never arrives: an
                // incomplete frame is not a frame.
                &ProviderEvent::ToolCallDelta {
                    id: "toolu_01Cut".to_owned(),
                    json: "{\"file".to_owned(),
                },
                &ProviderEvent::Failed(ProviderError::Transport(
                    "the response body ended before the model finished".to_owned()
                )),
            ],
            "got {seen:?}"
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
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: Some("be brief".to_owned()),
            tools: Vec::new(),
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

    /// Readable thinking is display-only, and this is the wire where that
    /// stops being prose and starts being a test (bead `pwe`).
    ///
    /// The invariant was held by one arm of one match — move
    /// `PartBody::ReasoningText` up into the text arm and every later request
    /// silently starts carrying the model's scratch paper as if the model had
    /// said it. The transcript here is what a real reasoning turn leaves
    /// behind: a thought, a reply, and the sealed state this API has no item
    /// for either.
    #[test]
    fn a_transcript_held_thought_is_absent_from_the_body_this_wire_sends() {
        const THOUGHT: &str = "the-user-is-probably-testing-me";

        let mut turn = Message::assistant("claude");
        turn.parts.push(Part::reasoning_text(THOUGHT));
        turn.parts.push(Part::text("Hello!"));
        turn.parts.push(Part::reasoning(
            "anthropic",
            "rs_1",
            Some("sealed-blob-0001".to_owned()),
        ));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: None,
            tools: Vec::new(),
            messages: vec![Message::user("hi"), turn, Message::user("again")],
        };
        let body = serde_json::to_string(&Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert!(
            !body.contains(THOUGHT),
            "the thought reached the wire; nothing sends readable reasoning: {body}"
        );
        assert!(
            !body.contains("sealed-blob-0001"),
            "this API's own thinking block is not ported, so a foreign wire's \
             sealed state must not travel either: {body}"
        );
        assert!(
            body.contains("Hello!"),
            "the reply still has to be sent — an assertion that passed by \
             encoding nothing would prove nothing: {body}"
        );
    }

    #[test]
    fn a_request_without_a_system_prompt_omits_the_field() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
        };
        let body = serde_json::to_string(&Body::new(&request, 16)).expect("the body serializes");

        assert!(!body.contains("system"), "got {body}");
        assert!(body.contains(r#""max_tokens":16"#), "got {body}");
    }

    /// The splice order at this wire's send site: an effort adds what the body
    /// does not carry — `thinking` is the catalog's use of it — and loses every
    /// key the wire itself writes, because the wire's fields are what make the
    /// request one the Messages API accepts.
    #[test]
    fn an_effort_adds_thinking_but_cannot_claim_max_tokens() {
        let request = ChatRequest {
            effort_options: serde_json::json!({
                "thinking": {"type": "enabled", "budget_tokens": 16000},
                "max_tokens": 1,
            })
            .as_object()
            .cloned()
            .expect("the fixture options are an object"),
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
        };

        let own = Body::new(&request, DEFAULT_MAX_TOKENS);
        let body = serde_json::to_value(splice_effort(&request.effort_options, &own))
            .expect("a spliced body serializes");

        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "enabled", "budget_tokens": 16000}),
            "a key the wire does not write arrives verbatim"
        );
        assert_eq!(
            body["max_tokens"],
            serde_json::json!(DEFAULT_MAX_TOKENS),
            "a key the wire writes resolves to the wire"
        );
        assert_eq!(body["model"], serde_json::json!("claude-test"));
    }

    /// The exact source-block shapes the Messages API documents, pinned so a
    /// drift in the encoder is a red test rather than a 400 from the vendor.
    /// An image rides an `image` block and a PDF a `document` block, both with
    /// the base64 the engine encoded at send time; a file part that carries no
    /// content is a reference the engine already resolved, and encodes nothing.
    #[test]
    fn an_attachment_becomes_the_source_block_its_mime_names() {
        let mut user = Message::user("what are these");
        user.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::File {
                path: "shot.png".to_owned(),
                mime: "image/png".to_owned(),
                start: None,
                end: None,
                content: Some("aW1n".to_owned()),
            },
        });
        user.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::File {
                path: "paper.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                start: None,
                end: None,
                content: Some("cGRm".to_owned()),
            },
        });
        user.parts.push(Part::file("notes.md", "text/plain"));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![user],
            tools: Vec::new(),
        };
        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(
            body["messages"],
            serde_json::json!([{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what are these"},
                    {
                        "type": "image",
                        "source": {"type": "base64", "media_type": "image/png", "data": "aW1n"},
                    },
                    {
                        "type": "document",
                        "source": {
                            "type": "base64",
                            "media_type": "application/pdf",
                            "data": "cGRm",
                        },
                    },
                ],
            }]),
            "got {body}"
        );
    }

    /// What the engine consults before it fills a file part in: the mimes the
    /// Messages API documents, and nothing else — `image/avif` is on the
    /// attachment allowlist and still degrades, because sending a block the
    /// vendor does not document is guessing.
    #[test]
    fn the_wire_accepts_the_mimes_the_api_documents_and_no_others() {
        let provider = AnthropicProvider::new("sk-test-canary-XYZ").expect("a client builds");

        for mime in [
            "image/jpeg",
            "image/png",
            "image/gif",
            "image/webp",
            "application/pdf",
        ] {
            assert!(provider.accepts_attachment(mime), "{mime} is documented");
        }
        for mime in ["image/avif", "image/svg+xml", "text/plain", "video/mp4"] {
            assert!(!provider.accepts_attachment(mime), "{mime} degrades");
        }
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
        let mut assistant = Message::assistant("claude-test");

        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });
        assistant.parts.push(Part::text("Reading the file first."));
        assistant.parts.push(tool_part(
            "toolu_01Read",
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
            "toolu_01Glob",
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

    /// The live field failure the alias exists for: a plugin-contributed MCP
    /// server arrives namespaced `plugin:<name>:<server>` (**D473**), so its
    /// tools are named like this — 69 characters, with colons besides. This
    /// API's own cap is 128, so what it refuses here is the alphabet rather
    /// than the length, and the alias must not truncate what fits.
    const REFUSED_NAME: &str =
        "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research_result";

    /// What [`REFUSED_NAME`] is advertised as under this API's wider cap: the same
    /// name with its colons scrubbed, and nothing else.
    const ADVERTISED: &str =
        "mcp__plugin_mcp-gemini-search_mcp-gemini-search__deep_research_result";

    /// [`a_tool`] under the name that got a live turn killed.
    fn a_refused_tool() -> ToolDefinition {
        ToolDefinition {
            name: REFUSED_NAME.to_owned(),
            ..a_tool()
        }
    }

    #[test]
    fn a_tool_name_this_api_refuses_is_advertised_under_a_conforming_alias() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("research it")],
            tools: vec![a_refused_tool()],
        };

        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(body["tools"][0]["name"], ADVERTISED, "got {body}");
        assert_eq!(
            alias(REFUSED_NAME, ANTHROPIC_CAP),
            ADVERTISED,
            "128 characters is room enough that nothing is cut"
        );
    }

    /// The other half of the same seam. What the engine executes, what the
    /// permission rules match and what the transcript records is the registry
    /// name, so an alias the model calls back has to be undone before the
    /// event leaves the wire.
    #[test]
    fn a_call_answering_with_the_alias_comes_back_out_under_the_registry_name() {
        let tools = vec![a_refused_tool()];
        let mut mapping = Mapping {
            aliases: Aliases::of(&tools, ANTHROPIC_CAP),
            ..Mapping::default()
        };
        let mut seen = Vec::new();

        mapping.frame(
            &Frame {
                event: Some("content_block_start".to_owned()),
                data: json!({
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_01Research",
                        "name": ADVERTISED,
                        "input": {},
                    },
                })
                .to_string(),
            },
            &mut seen,
        );

        assert_eq!(
            seen,
            vec![ProviderEvent::ToolCallStart {
                id: "toolu_01Research".to_owned(),
                name: REFUSED_NAME.to_owned(),
            }],
            "got {seen:?}"
        );
    }

    /// A call replayed on a later request has to name what that request's own
    /// roster named, or the model is handed a trace citing a tool it was never
    /// offered. Aliasing is deterministic, so both sides recompute it rather
    /// than remembering anything across turns.
    #[test]
    fn a_completed_call_replays_under_the_same_alias_the_roster_advertises() {
        let mut assistant = Message::assistant("claude-test");
        assistant.parts.push(tool_part(
            "toolu_01Research",
            REFUSED_NAME,
            ToolState::Completed {
                input: json!({"filePath": "src/main.rs"}),
                output: "a report".to_owned(),
                title: "deep research".to_owned(),
                metadata: json!({}),
                started: 1,
                completed: 2,
            },
        ));

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("research it"), assistant],
            tools: vec![a_refused_tool()],
        };

        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(
            body["messages"][1]["content"][0]["name"], ADVERTISED,
            "the replayed call has to name exactly what the roster named: {body}"
        );
        assert_eq!(body["tools"][0]["name"], ADVERTISED, "got {body}");
    }

    #[test]
    fn a_request_advertises_the_tools_it_was_given() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs")],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(
            body["tools"],
            json!([{
                "name": "read",
                "description": "Reads a file from disk.",
                "input_schema": {
                    "type": "object",
                    "properties": {"filePath": {"type": "string"}},
                    "required": ["filePath"],
                },
            }]),
            "got {body}"
        );
    }

    /// A turn that called tools has to read back to the model the way it
    /// happened: the calls in the assistant message that made them, and their
    /// results in the user message that follows, which is the only place the
    /// API accepts them.
    #[test]
    fn a_finished_call_is_sent_back_as_a_use_and_a_result() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: Some("be brief".to_owned()),
            messages: vec![
                Message::user("read src/main.rs"),
                a_turn_with_two_calls(),
                Message::user("thanks"),
            ],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "read src/main.rs"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Reading the file first."},
                    {
                        "type": "tool_use",
                        "id": "toolu_01Read",
                        "name": "read",
                        "input": {"filePath": "src/main.rs"},
                    },
                    {
                        "type": "tool_use",
                        "id": "toolu_01Glob",
                        "name": "glob",
                        "input": {"pattern": "**/*.rs"},
                    },
                ]},
                // One message for both results, because the API answers a
                // message's calls in the message that follows it.
                {"role": "user", "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_01Read",
                        "content": "fn main() {}",
                    },
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_01Glob",
                        "content": "no such directory",
                        "is_error": true,
                    },
                ]},
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
        let mut assistant = Message::assistant("claude-test");

        for (text, call_id, tool, input, output) in [
            (
                "Reading.",
                "toolu_01Read",
                "read",
                json!({"filePath": "src/main.rs"}),
                "fn main() { let x = 1; }",
            ),
            (
                "Now editing.",
                "toolu_01Edit",
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
    /// would accept one flattened message — both calls, then both results, is
    /// indistinguishable from parallel tool use — but it would put "Now
    /// editing." *before* the read it was said in response to, and a model
    /// re-reading its own trace would find its reasoning shuffled out from
    /// under the evidence it reasoned from.
    #[test]
    fn a_two_step_turn_is_sent_back_one_message_pair_per_step() {
        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("fix the bug"), a_turn_of_two_steps()],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "fix the bug"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Reading."},
                    {
                        "type": "tool_use",
                        "id": "toolu_01Read",
                        "name": "read",
                        "input": {"filePath": "src/main.rs"},
                    },
                ]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01Read",
                    "content": "fn main() { let x = 1; }",
                }]},
                // The second step opens here, after its evidence rather than
                // before it.
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Now editing."},
                    {
                        "type": "tool_use",
                        "id": "toolu_01Edit",
                        "name": "edit",
                        "input": {
                            "filePath": "src/main.rs",
                            "oldString": "1",
                            "newString": "2",
                        },
                    },
                ]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01Edit",
                    "content": "1 replacement",
                }]},
            ]),
            "got {body}"
        );

        // The property the shape above exists for, stated on its own so that a
        // future rearrangement of the blocks cannot quietly lose it.
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
        let mut assistant = Message::assistant("claude-test");
        assistant.parts.push(Part::text("Reading."));
        assistant.parts.push(tool_part(
            "toolu_01Read",
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
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("read it"), assistant],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "read it"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Reading."},
                    {
                        "type": "tool_use",
                        "id": "toolu_01Read",
                        "name": "read",
                        "input": {"filePath": "src/main.rs"},
                    },
                ]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01Read",
                    "content": "fn main() {}",
                }]},
            ]),
            "got {body}"
        );
    }

    /// Splitting a turn must never produce two messages in a row with the same
    /// role: this API refuses a transcript whose roles do not alternate. Steps
    /// alternate on their own whenever each ends in calls, so what is left is
    /// the interrupted shape — a step that said something and called nothing,
    /// with another step behind it — which was one message before the split and
    /// stays one after it.
    #[test]
    fn two_steps_that_called_nothing_stay_one_message() {
        let mut assistant = Message::assistant("claude-test");
        for text in ["Thinking about it.", "Here is the answer."] {
            assistant.parts.push(Part {
                id: PartId::ascending(),
                body: PartBody::StepStart,
            });
            assistant.parts.push(Part::text(text));
        }

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("hi"), assistant, Message::user("thanks")],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Thinking about it."},
                    {"type": "text", "text": "Here is the answer."},
                ]},
                // And a message of its own is still a message of its own:
                // merging stops at the edge of the one it started in.
                {"role": "user", "content": "thanks"},
            ]),
            "got {body}"
        );
    }

    /// A turn cancelled while a tool was running leaves a call nobody answered.
    /// Sending it as it stands is a request the API refuses outright, and
    /// dropping it leaves the reply talking about a call that is not there, so
    /// the pair is completed with a placeholder.
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
            let mut assistant = Message::assistant("claude-test");
            assistant
                .parts
                .push(tool_part("toolu_01Read", "read", state));

            let request = ChatRequest {
                effort_options: Default::default(),
                model: "claude-test".to_owned(),
                system: None,
                messages: vec![Message::user("read src/main.rs"), assistant],
                tools: Vec::new(),
            };

            let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
                .expect("the body serializes");

            assert_eq!(
                body["messages"][1],
                json!({"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_01Read",
                    "name": "read",
                    // A call the model never finished streaming has no
                    // arguments, and the field is required.
                    "input": if running { json!({"filePath": "src/main.rs"}) } else { json!({}) },
                }]}),
                "got {body}"
            );
            assert_eq!(
                body["messages"][2],
                json!({"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01Read",
                    "content": NO_RESULT,
                    "is_error": true,
                }]}),
                "an unanswered call must not reach the API unanswered: {body}"
            );
        }
    }

    /// Step markers are this crate's bookkeeping — where one model request
    /// ended and the next began — rather than anything the model said, so a
    /// message of nothing else is not a message at all.
    #[test]
    fn step_markers_are_not_sent() {
        let mut assistant = Message::assistant("claude-test");
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepFinish {
                usage: Usage::default(),
            },
        });

        let request = ChatRequest {
            effort_options: Default::default(),
            model: "claude-test".to_owned(),
            system: None,
            messages: vec![Message::user("hi"), assistant],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request, DEFAULT_MAX_TOKENS))
            .expect("the body serializes");

        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "hi"}]),
            "got {body}"
        );
    }

    /// Asking a model for more than it will generate is a 400 rather than a
    /// longer reply, and the catalog is what knows the difference. Whichever of
    /// the two ceilings is lower wins, in both directions.
    #[test]
    fn the_reply_cap_is_the_lower_of_the_catalog_and_the_configuration() {
        let provider = AnthropicProvider::new("sk-test-canary-XYZ").expect("a client builds");

        assert_eq!(
            provider.max_tokens("claude-test"),
            DEFAULT_MAX_TOKENS,
            "a model the table does not know keeps the configured ceiling"
        );
        assert_eq!(
            provider.max_tokens("claude-sonnet-5"),
            DEFAULT_MAX_TOKENS,
            "a model that will generate more than the cap is still capped: \
             sonnet's own limit is 128k"
        );

        let mut modest = AnthropicProvider::new("sk-test-canary-XYZ").expect("a client builds");
        modest.max_tokens = 4_096;

        assert_eq!(
            modest.max_tokens("claude-sonnet-5"),
            4_096,
            "a caller asking for less than the cap gets what it asked for"
        );

        let mut generous = provider;
        generous.max_tokens = 200_000;

        assert_eq!(
            generous.max_tokens("claude-haiku-4-5"),
            64_000,
            "the model's own limit is the ceiling once it is the smaller one"
        );
        assert_eq!(
            generous.max_tokens("claude-test"),
            200_000,
            "and an unknown model is still asked for what the caller configured"
        );
    }

    /// Both credentials a provider holds: the key it was built with, and
    /// whatever the base URL carries. The second is configuration rather than
    /// something this build asked for, which makes it easy to forget and no
    /// less of a secret. `Debug` is the whole surface — there is no `Display`
    /// for a provider — and it is what every `tracing` field holding one
    /// renders through.
    #[test]
    fn a_provider_never_renders_its_credential() {
        // Both shapes `check_base_url` blesses: a gateway reached over https,
        // and the loopback endpoint the integration suite itself points at —
        // which is where a userinfo-bearing base URL actually shows up today.
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
            let provider = AnthropicProvider::new("sk-test-canary-XYZ")
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
            // Still worth reading: which endpoint this provider is pointed at
            // is the first thing anyone debugging one wants to know.
            assert!(
                rendered.contains(endpoint),
                "the endpoint should survive being made safe to print: {rendered}"
            );
        }
    }

    #[test]
    fn a_blank_credential_is_refused() {
        assert!(AnthropicProvider::new("  ").is_err());
    }

    #[tokio::test]
    async fn a_request_that_cannot_be_built_reports_why_without_the_endpoint() {
        // A newline cannot go in a header value, so the request fails to build
        // after `check_base_url` has passed. Nothing strips the base URL out of
        // the message: a builder error carries no URL, and a `reqwest::Error`
        // that does carry one renders it with its credentials already removed.
        // Both are the dependency's behaviour rather than this crate's, which
        // is why they are worth holding here.
        let provider = AnthropicProvider::new("sk-test-canary-XYZ\nnewline")
            .expect("an HTTP client builds")
            .with_base_url(
                "https://ganja:sk-url-canary-9999@gateway.invalid:8443/v1?token=sk-query-canary-7777",
            );

        let opened = provider
            .stream(
                ChatRequest {
                    effort_options: Default::default(),
                    model: "claude-sonnet-5".to_owned(),
                    system: None,
                    messages: vec![Message::user("hi")],
                    tools: Vec::new(),
                },
                CancellationToken::new(),
            )
            .await;

        // A stream is not `Debug`, so this cannot go through `expect_err`.
        let Err(error) = opened else {
            panic!("a header value with a newline cannot be built");
        };

        let rendered = format!("{error} / {error:?}");
        for secret in [
            "sk-test-canary-XYZ",
            "sk-url-canary-9999",
            "sk-query-canary-7777",
            "ganja:",
        ] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        }
        assert!(
            rendered.contains("malformed request"),
            "the failure should still say what happened: {rendered}"
        );
    }
}
