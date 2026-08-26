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
            //
            // A `Peer` part is rendered into the user turn at request
            // assembly (D495); a wire never encodes one as its own message,
            // because no vendor has a role for "somebody else's agent".
            PartBody::File { content: None, .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. }
            | PartBody::ReasoningText { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Peer { .. }
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
        ToolState::Pending { .. } => &NO_INPUT,
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
        ToolState::Pending { .. } | ToolState::Running { .. } => (NO_RESULT, true),
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
#[path = "anthropic_tests.rs"]
mod tests;
