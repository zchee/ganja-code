//! ChatGPT's Responses API, streamed — the wire a subscription answers on.
//!
//! Spec: upstream `packages/core/src/plugin/provider/openai.ts:182-187`, which
//! sends **every** OpenAI model through `sdk.responses(...)` and disables
//! `gpt-5-chat-latest` at `:161-174` with the reason written on it — that alias
//! is chat-completions-only, so it is the one model the routing cannot serve.
//! What such a request actually looks like on the wire is
//! `packages/opencode/src/plugin/openai/codex.ts:341-426`, the fetch override
//! that authenticates it and decides where it goes, cross-checked against
//! `@ai-sdk/openai@3.0.84`'s `src/responses/*` for the body and the frames.
//!
//! **This is a second request/response mapping, and that is the point.** The
//! sibling [`super::grok`] is a base URL and a credential source over
//! [`super::openai`] because xAI's endpoint speaks that API; the Responses API
//! does not. It carries *items* rather than chat messages, names every frame,
//! and reports a tool call across three event types instead of one. So the
//! encoder and the mapper are here, and everything else — the client, the
//! endpoint check, the retry driver, the frame splitter — is still `mod.rs`'s.
//!
//! # Where a subscription request goes
//!
//! [`DEFAULT_BASE_URL`] is `https://chatgpt.com/backend-api/codex`, not
//! `https://api.openai.com/v1`. That is `codex.ts:12`'s `CODEX_API_ENDPOINT`,
//! and `codex.ts:410-418` rewrites *every* `/v1/responses` or
//! `/chat/completions` URL to it whenever the credential is an OAuth one. The
//! reason it is not an implementation detail of upstream's fetch layer: a
//! ChatGPT access token is minted for that backend, and the platform API at
//! `api.openai.com` refuses it. A session with an API key still goes to
//! `api.openai.com` — through [`super::openai`], which is the other half of the
//! dispatch in [`super::select`].
//!
//! [`BASE_URL_ENV`](super::openai::BASE_URL_ENV) overrides it, under the same
//! `check_base_url` rule the sibling is held to, which is what points this
//! provider at a loopback socket for a test.
//!
//! # What the credential is
//!
//! An access token that expires and rotates, resolved **per request** through
//! the seam [`super::grok`] uses — `codex.ts:353` re-reads it on every call for
//! exactly that reason. Nothing is captured at construction, so a login that
//! happened after this session started, and a renewal another turn performed,
//! are both picked up by the next request rather than the next process.

use std::{borrow::Cow, collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    auth::{self, RefreshOauth},
    protocol::{FinishReason, Part, PartBody, Role, ToolState, Usage},
    provider::{
        ChatRequest, Credential, Mapper, Provider, ProviderError, ProviderEvent, Resolved,
        check_base_url, client, open,
        openai::{self, arguments, result},
        setting, shown_base_url,
        sse::Frame,
        steps,
    },
    tool::ToolDefinition,
};

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this provider.
///
/// The same one [`super::openai`] answers to, because it is the same vendor
/// serving the same models at the same prices — only the wire and the
/// credential differ, and neither of those is what a catalog row is keyed by.
pub const ID: &str = openai::ID;

/// Where a ChatGPT subscription's requests go (`codex.ts:12`).
///
/// The path this provider appends is `/responses`, so the whole URL is
/// `codex.ts`'s `CODEX_API_ENDPOINT` exactly.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Which of a person's ChatGPT accounts to bill (`codex.ts:406-408`).
const ACCOUNT_HEADER: &str = "chatgpt-account-id";

/// Who the backend is told is asking (`codex.ts:551`).
///
/// **Deliberately still `opencode`**, for the reason
/// [`auth::openai`]'s own originator is: the access token was minted against a
/// client registration that belongs to that project, and a value the
/// registration has never been sent is a rejection nothing here could test for.
const ORIGINATOR_HEADER: &str = "originator";

/// The value [`ORIGINATOR_HEADER`] carries.
const ORIGINATOR: &str = "opencode";

/// Opts the request into the Responses surface the Codex CLI talks to.
///
/// **Not from the pin.** Upstream sends `openai-beta` only as the websocket
/// protocol header (`plugin/openai/ws.ts:80`), a different value on a transport
/// ganja does not have; this is the header the Codex CLI sends on its own HTTP
/// requests to the same endpoint. It is additive — the backend serves the same
/// stream without it — and it is here so that ganja's request differs from that
/// CLI's in as few ways as possible.
const BETA_HEADER: &str = "openai-beta";

/// The value [`BETA_HEADER`] carries.
const BETA: &str = "responses=experimental";

/// Streams replies from ChatGPT's Responses API.
pub struct ResponsesProvider {
    client: reqwest::Client,
    credential: Credential,
    base_url: String,
}

impl fmt::Debug for ResponsesProvider {
    /// Renders without the credential, the way every provider here does. The
    /// base URL goes through [`shown_base_url`] for the same reason the
    /// sibling's does: it is overridable configuration, and configuration is
    /// allowed to carry a secret in its userinfo.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesProvider")
            .field("credential", &self.credential)
            .field("base_url", &shown_base_url(&self.base_url))
            .finish()
    }
}

impl ResponsesProvider {
    /// The provider against ChatGPT's own backend, or wherever
    /// [`BASE_URL_ENV`](openai::BASE_URL_ENV) points.
    ///
    /// Nothing is read from the credential store here — see the module's note
    /// on why the store is consulted per request instead.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be built,
    /// or when [`BASE_URL_ENV`](openai::BASE_URL_ENV) names an endpoint an
    /// access token may not travel to — so that a misconfigured session dies at
    /// startup rather than at the first prompt.
    pub fn from_stored() -> Result<Self, ProviderError> {
        let base_url = setting(openai::BASE_URL_ENV).unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        let login = auth::openai::Login::new().map_err(|error| {
            // `Login::new` fails only where `client()` does, and for the same
            // reason, so it is classified the same way: nothing was refused.
            ProviderError::Transport(error.to_string())
        })?;

        Self::at(base_url, Arc::new(login))
    }

    /// The same provider against endpoints of the caller's choosing, which is
    /// how a test drives it against a loopback socket.
    ///
    /// `refresh` is the endpoint half of a renewal — [`auth::openai::Login`]
    /// for a token endpoint that is not ChatGPT's. The rest of a renewal
    /// belongs to [`auth::Refresher`] and is not the caller's to choose.
    ///
    /// # Errors
    ///
    /// As [`from_stored`](Self::from_stored).
    pub fn at(
        base_url: impl Into<String>,
        refresh: Arc<dyn RefreshOauth>,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        check_base_url(&base_url)?;

        Ok(Self {
            client: client()?,
            credential: Credential::Oauth {
                provider_id: ID,
                refresh,
            },
            base_url,
        })
    }

    /// Builds the request one turn sends, given the credential it resolved.
    ///
    /// Split out from [`Provider::stream`] so that the header set — which is
    /// the whole difference between a request this backend serves and one it
    /// refuses — is provable without a socket.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when the URL or a header value will
    /// not build, with the access token scrubbed out of the message.
    fn request(
        &self,
        resolved: &Resolved,
        request: &ChatRequest,
    ) -> Result<reqwest::Request, ProviderError> {
        let mut built = self
            .client
            .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
            .bearer_auth(resolved.presented.expose())
            .header(ORIGINATOR_HEADER, ORIGINATOR)
            .header(BETA_HEADER, BETA)
            .header(
                reqwest::header::USER_AGENT,
                auth::device::UPSTREAM_USER_AGENT,
            );

        // Absent where the credential names no account: most people have
        // exactly one, and `auth::openai` treats a token with no such claim as
        // a login that worked rather than as a failure.
        if let Some(account_id) = &resolved.account_id {
            built = built.header(ACCOUNT_HEADER, account_id);
        }

        built.json(&Body::new(request)).build().map_err(|error| {
            ProviderError::Transport(
                resolved
                    .presented
                    .redact(&format!("malformed request: {error}")),
            )
        })
    }
}

#[async_trait]
impl Provider for ResponsesProvider {
    fn id(&self) -> &str {
        ID
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // Checked here as well as at construction because this is the last
        // moment before the access token goes on the wire.
        check_base_url(&self.base_url)?;

        // Resolved before the request is built, never captured at construction:
        // the token expires under a long session, and one renewed a moment ago
        // by another turn is the one this request should carry.
        let resolved = self.credential.resolved().await?;
        let built = self.request(&resolved, &request)?;

        open(
            &self.client,
            built,
            &resolved.presented,
            cancel,
            Mapping::default(),
        )
        .await
        .map_err(reauth)
    }
}

/// Says what a refused credential needs, rather than only what happened.
///
/// A `401` or `403` here is the backend rejecting the stored access token, and
/// the only thing that fixes it is a new login. The status alone reaches a
/// status bar as a number, so the command goes in the message beside it. The
/// classification changes nothing the retry driver does: neither status is in
/// [`RETRYABLE_STATUS`](super::retry::RETRYABLE_STATUS), and
/// [`ProviderError::Auth`] is not retryable either.
fn reauth(error: ProviderError) -> ProviderError {
    match error {
        ProviderError::Status {
            status: status @ (401 | 403),
            message,
        } => ProviderError::Auth(format!(
            "the ChatGPT endpoint refused the stored credential (HTTP {status}): \
             {message}; run `ganja auth login {ID}`"
        )),
        other => other,
    }
}

/// The JSON a request carries.
#[derive(Debug, Serialize)]
struct Body<'a> {
    model: &'a str,
    stream: bool,
    /// The system prompt.
    ///
    /// **A divergence, deliberately.** `@ai-sdk/openai` pushes it as an input
    /// item whose role is `system` for an ordinary model and `developer` for a
    /// reasoning one (`openai-responses-language-model.ts`, `systemMessageMode`).
    /// Which of the two a model wants is a per-model capability flag ganja's
    /// catalog does not carry, and guessing it wrong puts the whole system
    /// prompt in the wrong register. `instructions` is the Responses API's own
    /// field for the same text, means the same thing to either kind of model,
    /// and is what the Codex CLI sends — so it is one shape rather than a
    /// coin-flip between two.
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    /// The conversation, as items rather than as messages.
    input: Vec<Item<'a>>,
    /// Omitted rather than sent empty, the way the sibling omits it: a turn
    /// with nothing to offer is the ordinary case for a session with no
    /// registry.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolSpec<'a>>,
}

/// One tool as the Responses API advertises it.
///
/// Flatter than chat completions', which nests the same four fields under
/// `function` (`openai-responses-prepare-tools.ts`, `prepareFunctionTool`).
#[derive(Debug, Serialize)]
struct ToolSpec<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    /// The argument schema, which this API names `parameters` as well.
    parameters: &'a Value,
}

/// One entry in a request's `input`.
///
/// Untagged because the three shapes are told apart by the fields they carry —
/// a `role` for something that was said, a `type` for a call or its result —
/// which is how `convert-to-openai-responses-input.ts` builds them.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Item<'a> {
    /// Something a person or the model said.
    Said {
        role: &'static str,
        content: Vec<Block<'a>>,
    },
    /// A call the model made (`convert-to-openai-responses-input.ts:3338-3344`).
    ///
    /// Its own item rather than a field on the message that made it, which is
    /// the shape difference this whole encoder exists for.
    Called {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: &'a str,
        name: &'a str,
        /// The arguments as a JSON *string*, which is how this API carries them
        /// too — the model streams them as text.
        arguments: String,
    },
    /// What that call produced (`convert-to-openai-responses-input.ts:3740-3743`).
    Answered {
        #[serde(rename = "type")]
        kind: &'static str,
        call_id: &'a str,
        output: &'a str,
    },
}

/// One piece of a said item's content.
///
/// The kind differs by who said it: what reaches the model is `input_text`,
/// what the model said is `output_text`.
#[derive(Debug, Serialize)]
struct Block<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: Cow<'a, str>,
}

impl<'a> Body<'a> {
    /// Turns a request into the JSON the Responses API expects.
    ///
    /// # How a transcript becomes a request
    ///
    /// The same split into [`steps`] the sibling makes, and for the same
    /// reason — a call's result belongs after the step that made the call, not
    /// bundled with everything the turn ever said — but flattened differently:
    /// this API has no message that holds calls, so a step becomes up to three
    /// runs of items in order.
    ///
    /// - its text becomes one said item, `input_text` for a user and
    ///   `output_text` for the model;
    /// - each call becomes a `function_call` item;
    /// - each of those calls' results becomes a `function_call_output` item,
    ///   after all of them, because the API pairs them by `call_id` rather than
    ///   by position.
    ///
    /// A step carrying neither text nor calls contributes nothing, which is
    /// what the marker opening a turn and a turn that died before its first
    /// fragment both are.
    fn new(request: &'a ChatRequest) -> Self {
        let mut input: Vec<Item<'a>> = Vec::new();

        for message in &request.messages {
            let (role, block) = match message.role {
                Role::User => ("user", "input_text"),
                Role::Assistant => ("assistant", "output_text"),
            };

            for step in steps(&message.parts) {
                let (texts, calls) = split(step);

                if let Some(text) = texts {
                    input.push(Item::Said {
                        role,
                        content: vec![Block { kind: block, text }],
                    });
                }
                for part in &calls {
                    input.push(Item::Called {
                        kind: "function_call",
                        call_id: part.call_id,
                        name: part.tool,
                        arguments: arguments(part.state),
                    });
                }
                for part in &calls {
                    input.push(Item::Answered {
                        kind: "function_call_output",
                        call_id: part.call_id,
                        output: result(part.state),
                    });
                }
            }
        }

        Self {
            model: &request.model,
            stream: true,
            instructions: request.system.as_deref(),
            input,
            tools: request
                .tools
                .iter()
                .map(|tool: &ToolDefinition| ToolSpec {
                    kind: "function",
                    name: &tool.name,
                    description: &tool.description,
                    parameters: &tool.schema,
                })
                .collect(),
        }
    }
}

/// One call a step made, borrowed from the part that recorded it.
struct Made<'a> {
    call_id: &'a str,
    tool: &'a str,
    state: &'a ToolState,
}

/// Splits one step into the text it said and the calls it made.
fn split(parts: &[Part]) -> (Option<Cow<'_, str>>, Vec<Made<'_>>) {
    let mut texts: Vec<&str> = Vec::new();
    let mut calls = Vec::new();

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
            } => calls.push(Made {
                call_id,
                tool,
                state,
            }),
            // A mentioned file is a *reference*, resolved into a text block
            // before a request is built (`session::resolve_mentions`); see the
            // same arm in `openai.rs`. `StepFinish` carries a step's bill
            // rather than content, and `StepStart` was consumed as the boundary
            // this step was cut at.
            PartBody::File { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. } => {}
        }
    }

    let text = match texts.as_slice() {
        [] => None,
        [only] => Some(Cow::Borrowed(*only)),
        // One said item, one text block. Joining fragments without a separator
        // would run the last word of one into the first of the next.
        many => Some(Cow::Owned(many.join("\n"))),
    };

    (text, calls)
}

/// Accumulates what the frames so far said.
#[derive(Debug, Default)]
struct Mapping {
    usage: Usage,
    /// Call identifiers by the *item* id their argument deltas name.
    ///
    /// The two are different strings here, which is the trap this API sets:
    /// `response.output_item.added` carries both an item `id` and a `call_id`,
    /// the arguments arrive keyed by the item id
    /// (`response.function_call_arguments.delta`'s `item_id`), and the id a
    /// result has to quote back is the `call_id`. Keying tool events by the
    /// wrong one produces a transcript whose calls nothing answers.
    calls: HashMap<String, String>,
}

impl Mapper for Mapping {
    fn frame(&mut self, frame: &Frame, events: &mut Vec<ProviderEvent>) {
        // Some deployments close the stream with the chat-completions sentinel
        // as well as with `response.completed`. It is not JSON, so reading it
        // as a chunk would report a parse failure on a stream that ended
        // correctly.
        if frame.data.trim() == DONE {
            return;
        }

        let chunk: Value = match serde_json::from_str(&frame.data) {
            Ok(chunk) => chunk,
            Err(error) => {
                // Skipping would drop reply text without anything downstream
                // knowing a gap exists, which is worse than ending the turn
                // with a message that says so.
                events.push(ProviderEvent::Failed(ProviderError::Parse(format!(
                    "responses chunk: {error}"
                ))));
                return;
            }
        };

        // Every frame names itself twice — an `event:` line and a `type` field
        // — and this reads the field, the way `@ai-sdk/openai` does. A frame
        // arriving with only the `event:` line is a shape this build has never
        // measured; a frame arriving with only the field is what a proxy that
        // re-serializes the stream produces.
        match chunk["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => self.delta(&chunk, events, ProviderEvent::TextDelta),
            "response.reasoning_summary_text.delta" => {
                self.delta(&chunk, events, ProviderEvent::ReasoningDelta);
            }
            "response.output_item.added" => self.opened(&chunk["item"], events),
            "response.function_call_arguments.delta" => self.filled(&chunk, events),
            "response.output_item.done" => self.closed(&chunk["item"], events),
            "response.completed" | "response.incomplete" => {
                self.absorb(&chunk["response"]["usage"]);
                if let Some(reason) = chunk["response"]["incomplete_details"]["reason"].as_str() {
                    // No `FinishReason` says "stopped early but said something":
                    // the reply that arrived is whole as far as the loop is
                    // concerned, and the sibling logs its `finish_reason` the
                    // same way rather than inventing a verdict.
                    tracing::debug!(reason, "the model stopped before it was done");
                }

                events.push(ProviderEvent::Usage(self.usage));
                events.push(ProviderEvent::Finish(FinishReason::Completed));
            }
            "response.failed" => {
                events.push(ProviderEvent::Failed(failure(&chunk["response"]["error"])));
            }
            // A chunk-level error, which is how this API reports a failure that
            // happened after the status was already 200.
            "error" => events.push(ProviderEvent::Failed(failure(&chunk))),
            other => tracing::debug!(event = other, "an unmapped responses event"),
        }
    }
}

impl Mapping {
    /// Maps a `delta` field onto `make`, dropping an empty one.
    fn delta(
        &mut self,
        chunk: &Value,
        events: &mut Vec<ProviderEvent>,
        make: fn(String) -> ProviderEvent,
    ) {
        if let Some(delta) = chunk["delta"].as_str()
            && !delta.is_empty()
        {
            events.push(make(delta.to_owned()));
        }
    }

    /// Opens a call the model started making.
    ///
    /// Items of every other kind — a message, a block of reasoning, a
    /// server-side tool this build never offered — are the stream announcing
    /// structure rather than content, and produce nothing.
    fn opened(&mut self, item: &Value, events: &mut Vec<ProviderEvent>) {
        if item["type"].as_str() != Some(FUNCTION_CALL) {
            return;
        }

        let (Some(item_id), Some(call_id)) = (item["id"].as_str(), item["call_id"].as_str()) else {
            tracing::debug!("a function call arrived without the ids that correlate it");
            return;
        };

        self.calls.insert(item_id.to_owned(), call_id.to_owned());
        events.push(ProviderEvent::ToolCallStart {
            id: call_id.to_owned(),
            name: item["name"].as_str().unwrap_or_default().to_owned(),
        });
    }

    /// Appends a fragment of a call's arguments.
    fn filled(&mut self, chunk: &Value, events: &mut Vec<ProviderEvent>) {
        let Some(id) = chunk["item_id"]
            .as_str()
            .and_then(|item_id| self.calls.get(item_id))
        else {
            tracing::debug!("arguments arrived for a call that was never opened");
            return;
        };

        if let Some(json) = chunk["delta"].as_str()
            && !json.is_empty()
        {
            events.push(ProviderEvent::ToolCallDelta {
                id: id.clone(),
                json: json.to_owned(),
            });
        }
    }

    /// Closes a call whose arguments are complete.
    ///
    /// A call is executed when it closes, so a stream that died mid-call must
    /// never reach here — which it cannot, because this event is the API's own
    /// terminator for one and an incomplete frame is not a frame.
    fn closed(&mut self, item: &Value, events: &mut Vec<ProviderEvent>) {
        if item["type"].as_str() != Some(FUNCTION_CALL) {
            return;
        }

        if let Some(id) = item["id"].as_str().and_then(|item_id| {
            // Removed rather than read: the item is done, and leaving it would
            // let a later frame quoting a reused id reopen a closed call.
            self.calls.remove(item_id)
        }) {
            events.push(ProviderEvent::ToolCallEnd { id });
        }
    }

    /// Reads the usage the terminal frame carries.
    ///
    /// This API reports `input_tokens` as the whole prompt — cache reads and
    /// cache writes included — while [`Usage`]'s input counters are disjoint so
    /// that each can be billed at its own rate. So the two cached counts come
    /// back out of it, exactly as `convert-openai-responses-usage.ts` derives
    /// its `noCache`. Without this a cached session reports its prompt twice
    /// and is priced several times over.
    ///
    /// `output_tokens` is *not* reduced by the reasoning count: [`Usage`]
    /// documents `reasoning_tokens` as a subset of `output_tokens` rather than
    /// a count beside it, and nothing prices it separately.
    ///
    /// Saturating rather than wrapping: an endpoint claiming more cached tokens
    /// than prompt tokens must read as nothing fresh, not as a bill for
    /// eighteen quintillion tokens.
    fn absorb(&mut self, usage: &Value) {
        let input = &usage["input_tokens_details"];
        let output = &usage["output_tokens_details"];

        self.usage.cache_read_tokens = input["cached_tokens"].as_u64().unwrap_or_default();
        self.usage.cache_write_tokens = input["cache_write_tokens"].as_u64().unwrap_or_default();
        self.usage.reasoning_tokens = output["reasoning_tokens"].as_u64().unwrap_or_default();
        self.usage.output_tokens = usage["output_tokens"].as_u64().unwrap_or_default();
        self.usage.input_tokens = usage["input_tokens"]
            .as_u64()
            .unwrap_or_default()
            .saturating_sub(self.usage.cache_read_tokens)
            .saturating_sub(self.usage.cache_write_tokens);
    }
}

/// The item kind a tool call arrives as.
const FUNCTION_CALL: &str = "function_call";

/// The chat-completions sentinel, which some deployments send here too.
const DONE: &str = "[DONE]";

/// Turns an error object into the failure the turn reports.
///
/// The status was 200 by the time any of these arrived and this API's `code` is
/// a slug rather than a number, so `500` is the truest thing there is to say —
/// the same reading the sibling's error chunks get.
fn failure(error: &Value) -> ProviderError {
    ProviderError::Status {
        status: 500,
        message: error["message"]
            .as_str()
            .unwrap_or("the provider reported an error")
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt as _;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::{
        ACCOUNT_HEADER, BETA, BETA_HEADER, Body, DEFAULT_BASE_URL, ID, Mapping, ORIGINATOR,
        ORIGINATOR_HEADER, ResponsesProvider, reauth,
    };
    use crate::{
        auth::{self, AuthError, OauthCredential, RefreshOauth},
        catalog,
        protocol::{FinishReason, Message, Part, PartBody, PartId, ToolState, Usage},
        provider::{
            ChatRequest, PROVIDERS, Presented, ProviderError, ProviderEvent, Resolved,
            openai::{self, NO_RESULT},
            replay,
        },
        tool::ToolDefinition,
    };

    /// A token no other value in this module could be mistaken for.
    const ACCESS: &str = "at-responses-canary-7717";

    /// The account the credential names.
    const ACCOUNT: &str = "acct_2f7QpL9";

    /// A renewal that must never run, for the cases about construction rather
    /// than about a token endpoint.
    struct NeverRenews;

    #[async_trait::async_trait]
    impl RefreshOauth for NeverRenews {
        async fn refresh(
            &self,
            provider_id: &str,
            _credential: &OauthCredential,
        ) -> Result<OauthCredential, AuthError> {
            panic!("{provider_id} was renewed by a test that only builds a provider");
        }
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

    /// A resolved credential, the way one reaches [`ResponsesProvider::request`].
    fn resolved(account_id: Option<&str>) -> Resolved {
        Resolved {
            presented: Presented::new(ACCESS).expect("a non-blank token"),
            account_id: account_id.map(str::to_owned),
        }
    }

    /// A provider pointed somewhere a token may travel.
    fn provider() -> ResponsesProvider {
        ResponsesProvider::at(
            "http://127.0.0.1:8080/backend-api/codex",
            Arc::new(NeverRenews),
        )
        .expect("loopback may carry a token")
    }

    /// One turn's worth of request.
    fn ask() -> ChatRequest {
        ChatRequest {
            model: "gpt-5.6".to_owned(),
            system: None,
            messages: vec![Message::user("hello")],
            tools: Vec::new(),
        }
    }

    #[test]
    fn the_subscription_wire_is_the_same_vendor_as_the_key_one() {
        assert_eq!(ID, openai::ID, "one provider id, or a turn is priced wrong");
        assert!(
            PROVIDERS.contains(&ID),
            "a provider nothing can select is a provider nobody has"
        );
        assert_eq!(ID, auth::openai::PROVIDER_ID, "and one credential to read");
        assert_eq!(
            format!("{DEFAULT_BASE_URL}/responses"),
            "https://chatgpt.com/backend-api/codex/responses",
            "codex.ts:12 — a ChatGPT token is minted for this backend and \
             api.openai.com refuses it"
        );
    }

    /// Every header the backend uses to decide whether to serve a request at
    /// all. Dropping any one of them is a turn that fails in production and
    /// nowhere else, which is why this is asserted on the request rather than
    /// on the code that builds it.
    #[test]
    fn every_request_names_the_account_the_originator_and_the_agent() {
        let built = provider()
            .request(&resolved(Some(ACCOUNT)), &ask())
            .expect("the request builds");
        let headers = built.headers();
        let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());

        assert_eq!(
            built.url().as_str(),
            "http://127.0.0.1:8080/backend-api/codex/responses"
        );
        assert_eq!(
            header("authorization"),
            Some(format!("Bearer {ACCESS}")).as_deref()
        );
        assert_eq!(
            header(ACCOUNT_HEADER),
            Some(ACCOUNT),
            "codex.ts:406-408 — without it the backend cannot tell which of a \
             person's accounts to serve"
        );
        assert_eq!(header(ORIGINATOR_HEADER), Some(ORIGINATOR));
        assert_eq!(header(BETA_HEADER), Some(BETA));
        assert_eq!(
            header("user-agent"),
            Some(auth::device::UPSTREAM_USER_AGENT),
            "one User-Agent for every request this build makes against \
             somebody else's client registration"
        );

        // A credential naming no account still makes a request: most people
        // have exactly one, and `auth::openai` reads a token with no such claim
        // as a login that worked.
        let anonymous = provider()
            .request(&resolved(None), &ask())
            .expect("the request builds");
        assert!(
            !anonymous.headers().contains_key(ACCOUNT_HEADER),
            "an account nobody named must not travel as an empty string"
        );
    }

    /// The endpoint is not exempt from the rule every other provider's is held
    /// to just because the credential arrived as a token rather than as a key.
    #[test]
    fn an_access_token_may_not_be_sent_anywhere_a_key_could_not_be() {
        let refused = ResponsesProvider::at(
            "http://chatgpt.com/backend-api/codex",
            Arc::new(NeverRenews),
        )
        .expect_err("plain http to a public host puts the token on the wire in the clear");

        assert!(
            matches!(refused, ProviderError::Transport(_)),
            "{refused:?}"
        );
        assert!(
            ResponsesProvider::at("http://127.0.0.1:8080", Arc::new(NeverRenews)).is_ok(),
            "loopback never reaches a network, which is what a test depends on"
        );
    }

    #[test]
    fn a_provider_never_renders_its_credential() {
        let provider = ResponsesProvider::at(
            "http://ganja:at-url-canary-9999@127.0.0.1:8080/backend-api/codex",
            Arc::new(NeverRenews),
        )
        .expect("loopback may carry a token");
        let rendered = format!("{provider:?}");

        assert!(
            !rendered.contains("at-url-canary-9999") && !rendered.contains("ganja:"),
            "a provider leaked its endpoint's userinfo: {rendered}"
        );
        assert!(
            rendered.contains("Oauth") && rendered.contains(ID),
            "a provider renders as which provider it is: {rendered}"
        );
        assert!(
            rendered.contains("127.0.0.1:8080"),
            "the endpoint is what tells one provider from another: {rendered}"
        );
    }

    /// The other half of the obligation `catalog`'s own table test states: a
    /// provider a session can select has to be one the catalog can size and
    /// price.
    #[test]
    fn a_subscription_session_that_names_no_model_gets_one_the_catalog_can_price() {
        let id = catalog::default_model(ID).expect("openai has a pinned default");
        let info = catalog::model(id).expect("the default is in the table");

        assert_eq!(info.provider_id, ID);
        assert!(info.context_window > 0 && info.max_output > 0);
    }

    #[test]
    fn a_refused_credential_says_which_login_repairs_it() {
        for status in [401, 403] {
            let named = reauth(ProviderError::Status {
                status,
                message: "invalid token".to_owned(),
            });

            assert!(matches!(named, ProviderError::Auth(_)), "{named:?}");
            assert!(
                format!("{named}").contains("ganja auth login openai"),
                "the message is what a status bar shows: {named}"
            );
            assert!(
                !named.is_retryable(),
                "retrying a refused token is a storm against an identity provider"
            );
        }

        // Everything else is left as it was: a rate limit is not a login.
        let limited = reauth(ProviderError::Status {
            status: 429,
            message: "slow down".to_owned(),
        });
        assert!(
            matches!(limited, ProviderError::Status { status: 429, .. }) && limited.is_retryable(),
            "{limited:?}"
        );
    }

    #[test]
    fn the_system_prompt_travels_as_instructions_and_the_turn_as_items() {
        let mut empty = Message::assistant("gpt");
        empty.parts.push(Part::text(""));

        let request = ChatRequest {
            model: "gpt-test".to_owned(),
            system: Some("be brief".to_owned()),
            messages: vec![Message::user("hello"), empty, Message::user("again")],
            tools: Vec::new(),
        };

        assert_eq!(
            serde_json::to_value(Body::new(&request)).expect("the body serializes"),
            json!({
                "model": "gpt-test",
                "stream": true,
                "instructions": "be brief",
                "input": [
                    {"role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                    {"role": "user", "content": [{"type": "input_text", "text": "again"}]},
                ],
            })
        );
    }

    #[test]
    fn a_request_advertises_the_tools_it_was_given() {
        let request = ChatRequest {
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs")],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["tools"],
            json!([{
                // Flatter than chat completions', which nests these under
                // `function` — a tool advertised in the sibling's shape is one
                // this API ignores, so the model would be offered nothing.
                "type": "function",
                "name": "read",
                "description": "Reads a file from disk.",
                "parameters": {
                    "type": "object",
                    "properties": {"filePath": {"type": "string"}},
                    "required": ["filePath"],
                },
            }]),
            "got {body}"
        );
    }

    /// A request offering `read`, which is what a session with a registry sends
    /// on every turn.
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

    /// A call that ran and what it produced.
    fn completed(input: serde_json::Value, output: &str) -> ToolState {
        ToolState::Completed {
            input,
            output: output.to_owned(),
            title: "src/main.rs".to_owned(),
            metadata: json!({}),
            started: 1,
            completed: 2,
        }
    }

    /// A turn that called tools reads back as items: what the model said, the
    /// calls beside it, then every result — each its own entry, because this
    /// API has no message that holds a call.
    #[test]
    fn a_finished_call_is_sent_back_as_a_call_item_and_an_output_item() {
        let mut assistant = Message::assistant("gpt-test");
        assistant.parts.push(Part {
            id: PartId::ascending(),
            body: PartBody::StepStart,
        });
        assistant.parts.push(Part::text("Reading the file first."));
        assistant.parts.push(tool_part(
            "call_read",
            "read",
            completed(json!({"filePath": "src/main.rs"}), "fn main() {}"),
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

        let request = ChatRequest {
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![
                Message::user("read src/main.rs"),
                assistant,
                Message::user("thanks"),
            ],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": [{"type": "input_text", "text": "read src/main.rs"}]},
                {
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Reading the file first."}],
                },
                {
                    "type": "function_call",
                    "call_id": "call_read",
                    "name": "read",
                    // Arguments travel as a string here too: the model streams
                    // them as text and the API carries them as it got them.
                    "arguments": r#"{"filePath":"src/main.rs"}"#,
                },
                {
                    "type": "function_call",
                    "call_id": "call_glob",
                    "name": "glob",
                    "arguments": r#"{"pattern":"**/*.rs"}"#,
                },
                {"type": "function_call_output", "call_id": "call_read", "output": "fn main() {}"},
                // A failure has nowhere to be flagged here either, so it
                // travels as the text the model reads.
                {
                    "type": "function_call_output",
                    "call_id": "call_glob",
                    "output": "no such directory",
                },
                {"role": "user", "content": [{"type": "input_text", "text": "thanks"}]},
            ]),
            "got {body}"
        );
    }

    /// A turn that took two model requests reads back as two of them. The API
    /// would accept one flattened run, but it would put everything the model
    /// said ahead of every result, so a model re-reading its own trace would
    /// find its reasoning shuffled out from under the evidence it reasoned
    /// from.
    #[test]
    fn a_two_step_turn_is_sent_back_one_group_per_step() {
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
            assistant
                .parts
                .push(tool_part(call_id, tool, completed(input, output)));
        }

        let request = ChatRequest {
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("fix the bug"), assistant],
            tools: vec![a_tool()],
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");
        let wire = body["input"].to_string();
        let position = |needle: &str| wire.find(needle).expect("the wire holds it");

        assert!(
            position("Now editing.") > position("fn main() { let x = 1; }"),
            "what the model said in the second step must read as having been \
             said after the first step's result came back: {wire}"
        );
    }

    /// A turn cancelled while a tool was running leaves a call nobody answered,
    /// and this API pairs a call with its output by `call_id`: a call with no
    /// output is one the model is still waiting on. Dropping the call instead
    /// would leave the reply talking about one that is not there.
    #[test]
    fn a_call_that_never_finished_is_answered_rather_than_left_dangling() {
        let mut assistant = Message::assistant("gpt-test");
        assistant
            .parts
            .push(tool_part("call_read", "read", ToolState::Pending));

        let request = ChatRequest {
            model: "gpt-test".to_owned(),
            system: None,
            messages: vec![Message::user("read src/main.rs"), assistant],
            tools: Vec::new(),
        };

        let body = serde_json::to_value(Body::new(&request)).expect("the body serializes");

        assert_eq!(
            body["input"][2],
            json!({
                "type": "function_call_output",
                "call_id": "call_read",
                // One spelling across both wires: the sibling's, imported.
                "output": NO_RESULT,
            }),
            "an unanswered call must not reach the API unanswered: {body}"
        );
    }

    /// The happy path: text, a summarized thought, and the bill the terminal
    /// frame carries.
    #[tokio::test]
    async fn a_happy_path_transcript_maps_to_text_reasoning_and_a_bill() {
        let seen = events(concat!(
            "event: response.created\n",
            r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6"}}"#,
            "\n\n",
            "event: response.output_item.added\n",
            r#"data: {"type":"response.output_item.added","output_index":0,"#,
            r#""item":{"type":"reasoning","id":"rs_1"}}"#,
            "\n\n",
            "event: response.reasoning_summary_text.delta\n",
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","#,
            r#""summary_index":0,"delta":"A greeting is enough."}"#,
            "\n\n",
            "event: response.output_item.added\n",
            r#"data: {"type":"response.output_item.added","output_index":1,"#,
            r#""item":{"type":"message","id":"msg_1"}}"#,
            "\n\n",
            "event: response.output_text.delta\n",
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"Hello, "}"#,
            "\n\n",
            "event: response.output_text.delta\n",
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"world!"}"#,
            "\n\n",
            "event: response.completed\n",
            r#"data: {"type":"response.completed","response":{"usage":{"#,
            r#""input_tokens":42,"input_tokens_details":{"cached_tokens":16},"#,
            r#""output_tokens":9,"output_tokens_details":{"reasoning_tokens":4}}}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "Hello, world!");
        assert!(
            seen.contains(&ProviderEvent::ReasoningDelta(
                "A greeting is enough.".to_owned()
            )),
            "a summarized thought should not be dropped, got {seen:?}"
        );
        assert_eq!(
            &seen[seen.len() - 2..],
            &[
                ProviderEvent::Usage(Usage {
                    // 42 prompt tokens of which the cache served 16: 26 fresh.
                    input_tokens: 26,
                    output_tokens: 9,
                    reasoning_tokens: 4,
                    cache_read_tokens: 16,
                    cache_write_tokens: 0,
                }),
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            "got {seen:?}"
        );
    }

    /// This API reports the whole prompt as `input_tokens` and then says how
    /// much of it the cache served and wrote; [`Usage`] keeps the three apart so
    /// each can be billed at its own rate, a cache read costing a fraction of
    /// fresh input. Handing every count through unchanged bills the cached part
    /// twice.
    #[tokio::test]
    async fn a_cached_prompt_reports_only_its_fresh_half_as_input() {
        let cases = [
            (
                "a cached prompt bills only what the cache did not serve",
                concat!(
                    r#"data: {"type":"response.completed","response":{"usage":{"#,
                    r#""input_tokens":1000,"input_tokens_details":{"cached_tokens":800},"#,
                    r#""output_tokens":20}}}"#,
                    "\n\n",
                ),
                Usage {
                    input_tokens: 200,
                    output_tokens: 20,
                    cache_read_tokens: 800,
                    ..Usage::default()
                },
            ),
            (
                "a written cache entry is not fresh input either",
                concat!(
                    r#"data: {"type":"response.completed","response":{"usage":{"#,
                    r#""input_tokens":1000,"input_tokens_details":{"cached_tokens":600,"#,
                    r#""cache_write_tokens":300},"output_tokens":20}}}"#,
                    "\n\n",
                ),
                Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_tokens: 600,
                    cache_write_tokens: 300,
                    ..Usage::default()
                },
            ),
            (
                "an endpoint claiming more cached tokens than prompt tokens reads as \
                 nothing fresh rather than wrapping into a bill nobody owes",
                concat!(
                    r#"data: {"type":"response.completed","response":{"usage":{"#,
                    r#""input_tokens":100,"input_tokens_details":{"cached_tokens":900},"#,
                    r#""output_tokens":5}}}"#,
                    "\n\n",
                ),
                Usage {
                    input_tokens: 0,
                    output_tokens: 5,
                    cache_read_tokens: 900,
                    ..Usage::default()
                },
            ),
            (
                "a prompt nothing was cached for is fresh in full, thinking included \
                 in the output rather than counted beside it",
                concat!(
                    r#"data: {"type":"response.completed","response":{"usage":{"#,
                    r#""input_tokens":1000,"output_tokens":120,"#,
                    r#""output_tokens_details":{"reasoning_tokens":100}}}}"#,
                    "\n\n",
                ),
                Usage {
                    input_tokens: 1_000,
                    output_tokens: 120,
                    reasoning_tokens: 100,
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
    /// which is exactly the difference double-counting erases.
    #[test]
    fn a_cached_prompt_is_billed_once_rather_than_twice() {
        let model = catalog::model("gpt-5.6").expect("the catalog knows the model");
        let corrected = Usage {
            input_tokens: 200_000,
            cache_read_tokens: 800_000,
            ..Usage::default()
        };
        let doubled = Usage {
            input_tokens: 1_000_000,
            ..corrected
        };

        assert!(
            catalog::cost(&doubled, &model).total_usd
                > catalog::cost(&corrected, &model).total_usd * 3.0,
            "the uncorrected counts over-report by more than a factor of three, \
             which is the size of the error this pins"
        );
    }

    /// A call arrives across three event types, and the id that correlates them
    /// is not the id its result has to quote back.
    #[tokio::test]
    async fn tool_calls_are_opened_filled_and_closed() {
        let seen = events(concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","#,
            r#""delta":"Reading the file first."}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"#,
            r#""type":"function_call","id":"fc_1","call_id":"call_read","name":"read","#,
            r#""arguments":""}}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","#,
            r#""output_index":1,"delta":"{\"file"}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","#,
            r#""output_index":1,"delta":"Path\":\"src/main.rs\"}"}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"#,
            r#""type":"function_call","id":"fc_1","call_id":"call_read","name":"read","#,
            r#""arguments":"{\"filePath\":\"src/main.rs\"}"}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{"usage":{"#,
            r#""input_tokens":10,"output_tokens":5}}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "Reading the file first.");
        assert_eq!(
            seen.iter()
                .filter(|event| !matches!(
                    event,
                    ProviderEvent::TextDelta(_) | ProviderEvent::Usage(_)
                ))
                .collect::<Vec<_>>(),
            vec![
                // The `call_id`, never the item id the deltas were keyed by:
                // this is the string a `function_call_output` has to quote.
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
                    json: "Path\":\"src/main.rs\"}".to_owned(),
                },
                // This API *does* terminate a call, unlike chat completions.
                &ProviderEvent::ToolCallEnd {
                    id: "call_read".to_owned(),
                },
                &ProviderEvent::Finish(FinishReason::Completed),
            ],
            "got {seen:?}"
        );
    }

    /// The SSE decoder must tolerate anything: this stream carries a dozen
    /// event types this build has no use for, and several more the API has not
    /// invented yet.
    #[tokio::test]
    async fn an_unmapped_event_is_skipped_rather_than_ending_the_turn() {
        let seen = events(concat!(
            r#"data: {"type":"response.in_progress","response":{"id":"resp_1"}}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"#,
            r#""type":"web_search_call","id":"ws_1","status":"in_progress"}}"#,
            "\n\n",
            r#"data: {"type":"response.something.nobody.has.written.yet","delta":"x"}"#,
            "\n\n",
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"hi"}"#,
            "\n\n",
            "data: [DONE]\n\n",
            r#"data: {"type":"response.completed","response":{"usage":{"#,
            r#""input_tokens":1,"output_tokens":1}}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "hi");
        assert_eq!(
            seen.last(),
            Some(&ProviderEvent::Finish(FinishReason::Completed)),
            "an unknown event is a log line, and `[DONE]` is not a parse \
             failure: {seen:?}"
        );
    }

    #[tokio::test]
    async fn a_body_that_stops_mid_reply_fails_rather_than_completing() {
        let seen = events(concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","#,
            r#""delta":"The connection drops right"}"#,
            "\n\n",
        ))
        .await;

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
    async fn a_malformed_chunk_ends_the_turn_rather_than_being_skipped() {
        let seen = events(concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"Hello"}"#,
            "\n\n",
            "data: {\"type\": not json at all\n\n",
            r#"data: {"type":"response.output_text.delta","item_id":"m","delta":" there"}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "Hello", "text before the break is kept");
        assert_eq!(
            seen.len(),
            2,
            "nothing after the broken chunk is read, got {seen:?}"
        );
        assert!(
            matches!(
                seen.last(),
                Some(ProviderEvent::Failed(ProviderError::Parse(_)))
            ),
            "got {seen:?}"
        );
    }

    /// This API has two ways of saying a turn broke after the status was
    /// already 200, and neither may read as a model that finished talking.
    #[tokio::test]
    async fn a_failed_response_and_an_error_chunk_both_end_the_turn_as_failures() {
        for transcript in [
            concat!(
                r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"partial"}"#,
                "\n\n",
                r#"data: {"type":"response.failed","sequence_number":9,"response":{"#,
                r#""error":{"code":"server_error","message":"upstream capacity exceeded"}}}"#,
                "\n\n",
            ),
            concat!(
                r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"partial"}"#,
                "\n\n",
                r#"data: {"type":"error","sequence_number":9,"code":"server_error","#,
                r#""message":"upstream capacity exceeded"}"#,
                "\n\n",
            ),
        ] {
            let seen = events(transcript).await;

            assert_eq!(text(&seen), "partial");
            assert_eq!(
                seen.last(),
                Some(&ProviderEvent::Failed(ProviderError::Status {
                    status: 500,
                    message: "upstream capacity exceeded".to_owned(),
                })),
                "got {seen:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_cancel_mid_transcript_ends_the_stream_without_a_verdict() {
        let cancel = CancellationToken::new();
        let mut stream = Box::pin(replay(
            concat!(
                r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"one"}"#,
                "\n\n",
                r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"two"}"#,
                "\n\n",
            ),
            cancel.clone(),
            Mapping::default(),
        ));

        assert_eq!(
            stream.next().await,
            Some(ProviderEvent::TextDelta("one".to_owned()))
        );
        cancel.cancel();

        let rest: Vec<ProviderEvent> = stream.collect().await;
        assert!(rest.is_empty(), "a cancelled stream ends: {rest:?}");
    }

    #[tokio::test]
    async fn a_turn_that_stopped_early_still_reports_what_it_spent() {
        let seen = events(concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"as far as"}"#,
            "\n\n",
            r#"data: {"type":"response.incomplete","response":{"#,
            r#""incomplete_details":{"reason":"max_output_tokens"},"#,
            r#""usage":{"input_tokens":10,"output_tokens":128}}}"#,
            "\n\n",
        ))
        .await;

        assert_eq!(text(&seen), "as far as");
        assert_eq!(
            &seen[seen.len() - 2..],
            &[
                ProviderEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 128,
                    ..Usage::default()
                }),
                // A reply that stopped at the output ceiling is still a reply,
                // and the loop has no verdict between "done" and "broke".
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            "got {seen:?}"
        );
    }
}
