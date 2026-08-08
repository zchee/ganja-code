//! The cursor wire: cursor's agent backend over the Connect protocol.
//!
//! Spec: `.omc/research/cursor/spike-wire-facts.md`, the record of a live
//! probe against `api2.cursor.sh` — this wire has no upstream TypeScript to
//! port, so the recorded wire facts are its specification the way an
//! upstream file is every other wire's. What they pin: the dialect is
//! Connect (bare `application/proto` on unary RPCs, the enveloped
//! `application/connect+proto` on streaming ones), the failure of a stream
//! arrives as an in-body EndStream frame rather than HTTP/2 trailers, and
//! bare gRPC never reaches the application at all. The framing is
//! hand-written in [`connect`] over the same `reqwest`/rustls stack every
//! other wire sends with — small enough to own, and the unary path needs no
//! framing at all.
//!
//! The messages are carried by [`buffa`] — Anthropic's pure-Rust, Apache-2.0
//! protobuf runtime, whose license matches this workspace's. It is
//! codegen-only by design, so the shapes live in `cursor.proto` (ganja's
//! own, authored against the recorded facts; its header says what was
//! derived from where) and the generated Rust is checked in under [`proto`],
//! regenerated and diffed by a drift test.
//!
//! **One-shot, for now.** A turn sends its single run request, reads the
//! whole response body, and hands back the events it contained; nothing is
//! surfaced until the server has finished talking. Incremental delivery,
//! cancellation that reaches mid-stream, the retry driver the other wires
//! ride, and the conversation-state machinery that carries history and tool
//! calls are all deliberately not here yet — the module boundary they land
//! behind is [`connect`]'s, and nothing about this shape is a contract.
//!
//! The provider rides the uncataloged tier, so a session must be told which
//! model to ask for; [`CursorWire::usable_models`] is the listing that says
//! what the seat may name. Construction reads nothing — grok's posture — and
//! the stored login is read per request, so a login that happens after a
//! session starts is picked up by its next request.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use futures::{StreamExt as _, stream, stream::BoxStream};
use tokio_util::sync::CancellationToken;

use crate::{
    auth::{self, RefreshOauth},
    provider::{
        ChatRequest, CredentialSource, Presented, Provider, ProviderError, ProviderEvent,
        check_base_url, client, shown_base_url,
    },
};

mod connect;
mod decode;
mod request;

/// The cursor wire's protobuf messages, generated from `cursor.proto` by
/// `buffa`'s codegen and checked in.
///
/// `@generated` — never edited by hand; `buf generate` rewrites it and the
/// drift test in this module's tests proves the checked-in copy still
/// matches the `.proto`.
///
/// The allow is for the generated decoders of fieldless messages, whose
/// unknown-field handling is a one-arm match: the codegen's own allow list
/// covers its view module but not these, and the alternative is hand-editing
/// a file whose whole contract is that nobody does.
#[allow(clippy::match_single_binding)]
pub mod proto {
    include!("cursor/ganja.cursor.v1.rs");
}

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects this
/// provider. [`auth::cursor::PROVIDER_ID`] rather than a second literal, for
/// grok's reason: a login writing under one name while a provider reads
/// under another fails as a storage bug and is debugged as one.
pub const ID: &str = auth::cursor::PROVIDER_ID;

/// Where cursor's agent backend lives, as the live probe reached it.
pub const DEFAULT_BASE_URL: &str = "https://api2.cursor.sh";

/// The model listing, the service's cheapest unary RPC.
const MODELS_PATH: &str = "/agent.v1.AgentService/GetUsableModels";

/// The chat turn, a streaming RPC.
const RUN_PATH: &str = "/agent.v1.AgentService/Run";

/// What a unary RPC carries: bare protobuf, both directions.
const UNARY_CONTENT_TYPE: &str = "application/proto";

/// What a streaming RPC carries: Connect-enveloped protobuf.
const STREAMING_CONTENT_TYPE: &str = "application/connect+proto";

/// The client the recorded requests identified as, live-confirmed accepted.
/// One constant so the day the server starts gating on it there is one
/// place to move.
const CLIENT_VERSION: &str = "cli-2026.01.09-231024f";

/// Longest error body kept for a status message. The shared trimming lives
/// in the retry driver this wire does not ride yet; the number is kept equal
/// so adopting the driver later changes no message.
const BODY_LIMIT: usize = 400;

/// The identity `GANJA_PROVIDER=cursor` selects.
///
/// Fieldless because the selection layer builds it as a bare name:
/// construction reads nothing, and every request builds a [`CursorWire`]
/// from the stored login at the moment it is needed. Fieldless is also what
/// keeps its `Debug` trivially clean; the wire's own `Debug` is where the
/// no-secrets posture is held.
#[derive(Debug, Default)]
pub struct CursorProvider;

#[async_trait]
impl Provider for CursorProvider {
    fn id(&self) -> &str {
        ID
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        CursorWire::from_stored()?.stream(request, cancel).await
    }
}

/// The wire itself: an endpoint, a client, and the credential source every
/// request resolves afresh.
///
/// Split from [`CursorProvider`] so a test can point it at a loopback
/// socket; the unit struct above is the one the selection layer names.
pub struct CursorWire {
    client: reqwest::Client,
    base_url: String,
    credential: CredentialSource,
}

impl fmt::Debug for CursorWire {
    /// Renders which endpoint and which kind of credential, never the
    /// credential — the same rule every other wire's `Debug` holds.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CursorWire")
            .field("credential", &self.credential)
            .field("base_url", &shown_base_url(&self.base_url))
            .finish()
    }
}

impl CursorWire {
    /// The wire against cursor's own endpoint, for a session that has a
    /// login to run as.
    ///
    /// The store is asked one question — is there a credential at all — and
    /// the answer is discarded once counted; the token a request carries is
    /// still resolved per request, through the shared refresher. Grok's
    /// posture, for grok's reasons: refusing here is what puts the "log in
    /// first" message ahead of a turn instead of inside one.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Auth`] when no cursor credential is stored —
    /// naming `ganja auth login cursor`, which is the repair — or when the
    /// store exists and could not be read, which a login does not fix and
    /// the store's own message describes. Returns
    /// [`ProviderError::Transport`] when no HTTP client can be built.
    pub fn from_stored() -> Result<Self, ProviderError> {
        let stored = auth::storage_key(ID);
        let listed =
            auth::list_providers().map_err(|error| ProviderError::Auth(error.to_string()))?;
        if !listed.iter().any(|entry| entry.provider_id == stored) {
            return Err(ProviderError::Auth(format!(
                "no {ID} credential is stored; run `ganja auth login {ID}`"
            )));
        }

        let refresh = auth::cursor::Refresh::new()
            .map_err(|error| ProviderError::Transport(error.to_string()))?;

        Self::at(DEFAULT_BASE_URL, Arc::new(refresh))
    }

    /// The same wire against an endpoint of the caller's choosing, which is
    /// how a test drives it against a loopback socket.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transport`] when no HTTP client can be
    /// built, or when `base_url` is somewhere an access token may not travel
    /// — the rule every other provider's endpoint is held to.
    pub fn at(
        base_url: impl Into<String>,
        refresh: Arc<dyn RefreshOauth>,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.into();
        check_base_url(&base_url)?;

        Ok(Self {
            client: client()?,
            base_url,
            credential: CredentialSource::Oauth {
                provider_id: ID,
                refresh,
            },
        })
    }

    /// The models the stored login may name, from the live listing.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] as [`stream`](Self::stream) classifies
    /// them: `Auth` for a credential problem, `Transport` when nothing
    /// answered, `Status` when the server refused, `Parse` when the answer
    /// could not be read.
    pub async fn usable_models(&self) -> Result<Vec<proto::ModelEntry>, ProviderError> {
        // The request message has no fields, and an empty message encodes to
        // no bytes at all — the zero-byte body the live probe was answered
        // on.
        let body = self.post(MODELS_PATH, false, Vec::new()).await?;

        decode::model_list(&body)
    }

    /// One turn: the run request out, the whole exchange back in, as the
    /// events it contained.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the turn cannot start or the exchange
    /// failed before anything streamed; a failure after visible text arrives
    /// as [`ProviderEvent::Failed`] inside the stream instead.
    pub async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let message = request::run_message(&request)?;
        let body = match self.post(RUN_PATH, true, connect::envelope(&message)).await {
            Ok(body) => body,
            // A turn the user already left is not a failed one.
            Err(_) if cancel.is_cancelled() => return Ok(stream::empty().boxed()),
            Err(error) => return Err(error),
        };

        Ok(stream::iter(decode::exchange(&body)?).boxed())
    }

    /// Sends one RPC and returns its complete, successful body.
    ///
    /// The header set is the recorded one, verbatim; the split between the
    /// two content types — and `connect-protocol-version` on the streaming
    /// RPC only — is exactly what the live probe measured the server
    /// enforcing.
    async fn post(
        &self,
        path: &str,
        streaming: bool,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, ProviderError> {
        let presented = self.credential.presented().await?;

        let mut sent = self
            .client
            .post(format!("{}{path}", self.base_url))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", presented.expose()),
            )
            .header("x-cursor-client-version", CLIENT_VERSION)
            .header("x-cursor-client-type", "cli")
            .header("x-ghost-mode", "true")
            .header("x-request-id", request::fresh_id()?)
            // Meaningful only to a server that would send trailers — this
            // one does not — but the recorded client sends it and this build
            // identifies as that client.
            .header(reqwest::header::TE, "trailers")
            .header(
                reqwest::header::CONTENT_TYPE,
                if streaming {
                    STREAMING_CONTENT_TYPE
                } else {
                    UNARY_CONTENT_TYPE
                },
            );
        if streaming {
            sent = sent.header("connect-protocol-version", "1");
        }

        let response = sent.body(body).send().await.map_err(transport)?;
        let status = response.status();
        if !status.is_success() {
            let echoed = response.text().await.unwrap_or_default();
            return Err(refused(status.as_u16(), &echoed, &presented));
        }

        Ok(response.bytes().await.map_err(transport)?.to_vec())
    }
}

/// Describes a transport failure, following the cause chain the way the
/// retry driver's twin does — `reqwest::Error` alone says only that a
/// request failed, never why, and the URL is dropped first because a base
/// URL may carry credentials. Local to this wire only until it rides the
/// shared driver, which owns the shared spelling.
fn transport(error: reqwest::Error) -> ProviderError {
    use std::fmt::Write as _;

    let error = error.without_url();
    let mut message = error.to_string();
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&error);
    while let Some(cause) = source {
        let _ = write!(message, ": {cause}");
        source = cause.source();
    }

    ProviderError::Transport(message)
}

/// A non-2xx answer, trimmed to what a status bar can hold and scrubbed of
/// the credential — a server echoing the request it refused is the leak this
/// exists to stop. The retry driver's twin, local for [`transport`]'s
/// reason.
fn refused(status: u16, body: &str, presented: &Presented) -> ProviderError {
    let trimmed = body.trim();
    let message = if trimmed.is_empty() {
        "no error body".to_owned()
    } else {
        let shortened = match trimmed.char_indices().nth(BODY_LIMIT) {
            Some((cut, _)) => format!("{}…", &trimmed[..cut]),
            None => trimmed.to_owned(),
        };
        presented.redact(&shortened)
    };

    ProviderError::Status { status, message }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        super::PROVIDERS, CursorProvider, CursorWire, DEFAULT_BASE_URL, ID, Provider as _,
        ProviderError, refused,
    };
    use crate::{
        auth::{self, AuthError, OauthCredential, RefreshOauth},
        provider::Presented,
    };

    /// A renewal that must never run, for the cases that are about
    /// construction rather than about a token endpoint.
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

    #[test]
    fn ganja_calls_it_cursor_everywhere_the_wire_can_see() {
        assert_eq!(CursorProvider.id(), ID);
        assert_eq!(ID, "cursor");
        assert_eq!(
            ID,
            auth::cursor::PROVIDER_ID,
            "one constant, or a login stores under a name the provider does not read"
        );
        assert!(
            PROVIDERS.contains(&ID),
            "a provider nothing can select is a provider nobody has"
        );
    }

    #[test]
    fn the_endpoint_is_cursors_own_and_the_debug_holds_no_secret() {
        assert_eq!(DEFAULT_BASE_URL, "https://api2.cursor.sh");

        // Built through `at` at the same constant `from_stored` passes,
        // because `from_stored` reads whatever credential store the machine
        // running this suite really holds.
        let wire =
            CursorWire::at(DEFAULT_BASE_URL, Arc::new(NeverRenews)).expect("a client builds");
        let rendered = format!("{wire:?}");
        assert!(
            rendered.contains("Oauth") && rendered.contains("cursor"),
            "{rendered}"
        );
        assert!(
            rendered.contains("https://api2.cursor.sh"),
            "the endpoint is what tells one wire from another: {rendered}"
        );

        // The selectable identity stays a bare name: nothing to leak, and
        // nothing read at construction.
        assert_eq!(format!("{CursorProvider:?}"), "CursorProvider");
    }

    /// The endpoint is not exempt from the rule every other base URL is held
    /// to just because the credential arrived as a token rather than a key.
    #[test]
    fn an_access_token_may_not_be_sent_anywhere_a_key_could_not_be() {
        let refused = CursorWire::at("http://api2.cursor.sh", Arc::new(NeverRenews))
            .expect_err("plain http to a public host puts the token on the wire in the clear");
        assert!(
            matches!(refused, ProviderError::Transport(_)),
            "{refused:?}"
        );
        assert!(
            CursorWire::at("http://127.0.0.1:4096", Arc::new(NeverRenews)).is_ok(),
            "loopback never reaches a network, which is what a test depends on"
        );
    }

    /// A refused response may quote the request it refused, and the request
    /// carried the token.
    #[test]
    fn a_refusal_echoing_the_credential_is_scrubbed_before_anyone_reads_it() {
        let presented = Presented::new("at-canary-FFFF").expect("a non-blank credential");
        let error = refused(401, "bad token at-canary-FFFF, go away", &presented);

        let rendered = error.to_string();
        assert!(!rendered.contains("at-canary-FFFF"), "{rendered}");
        assert!(rendered.contains("[redacted]"), "{rendered}");
        assert!(rendered.contains("401"), "{rendered}");
    }

    /// The admitted runtime is real, not merely named: a generated message
    /// round-trips through `buffa`'s encode/decode. This is what makes the
    /// dependency reach the lock (so `cargo deny` audits its license) and
    /// the live listing decode against the same version.
    #[test]
    fn the_admitted_protobuf_runtime_round_trips_a_generated_message() {
        use buffa::Message as _;

        let entry = super::proto::ModelEntry::default()
            .with_model_id("gpt-5.3-codex")
            .with_display_name("Codex 5.3");
        let bytes = entry.encode_to_vec();
        let decoded = super::proto::ModelEntry::decode_from_slice(&bytes)
            .expect("a message buffa encoded decodes");

        assert_eq!(decoded.model_id.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(decoded.display_name.as_deref(), Some("Codex 5.3"));
    }

    /// The checked-in generated code still matches its `.proto`:
    /// regenerating with the same remote plugin must produce byte-identical
    /// output. A drift here means somebody edited the `@generated` file by
    /// hand or changed the `.proto` without regenerating — either way the
    /// source of truth and the compiled code have diverged.
    ///
    /// Skipped rather than failed when `buf` is absent: the drift check is a
    /// developer-machine guard, and the workspace deliberately keeps `buf`
    /// and `protoc` out of CI (the generated code is checked in for exactly
    /// that reason). CI proves the code compiles and round-trips; this
    /// proves it was not hand-edited, on a machine that can regenerate.
    #[test]
    fn the_checked_in_generated_code_matches_the_proto() {
        use std::process::Command;

        let crate_dir = env!("CARGO_MANIFEST_DIR");
        if Command::new("buf").arg("--version").output().is_err() {
            eprintln!("skipping the proto drift check: `buf` is not on PATH");
            return;
        }

        let generated =
            std::path::Path::new(crate_dir).join("src/provider/cursor/ganja.cursor.v1.rs");
        let before = std::fs::read_to_string(&generated).expect("the generated file is present");

        let status = Command::new("buf")
            .arg("generate")
            .current_dir(crate_dir)
            .status()
            .expect("buf generate runs");
        assert!(status.success(), "buf generate failed");

        let after = std::fs::read_to_string(&generated).expect("the generated file is present");
        assert_eq!(
            before, after,
            "the checked-in cursor protobuf code has drifted from cursor.proto; \
             run `buf generate` in crates/ganja-provider and commit the result"
        );
    }
}
