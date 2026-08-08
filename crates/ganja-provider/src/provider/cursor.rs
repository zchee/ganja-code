//! The cursor wire: still a stub that refuses by name, with its protobuf
//! message runtime now admitted beneath it.
//!
//! Cursor's agent backend speaks gRPC/protobuf. The messages are carried by
//! [`buffa`] — Anthropic's pure-Rust, Apache-2.0 protobuf runtime, whose
//! license matches this workspace's. It is codegen-only by design, so the
//! shapes live in `cursor.proto` (ganja's own, authored against the spike's
//! recorded wire facts) and the generated Rust is checked in under
//! [`proto`], regenerated and diffed by a drift test. What is admitted in this
//! landing is the message runtime alone: the *transport* that carries these
//! bytes — a Connect/gRPC dialect the live spike will name — is chosen after
//! the spike, not here.
//!
//! The behavior has not moved yet. `GANJA_PROVIDER=cursor` still builds a
//! session cheaply — grok's construction posture, nothing read at construction
//! — and the first request is still refused with [`REFUSAL`], because the
//! request path lands in a later wave. It rides the uncataloged tier, so it
//! must be told which model to ask for, exactly like a config-declared
//! endpoint.

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use super::{ChatRequest, Provider, ProviderError, ProviderEvent};

/// The cursor wire's protobuf messages, generated from `cursor.proto` by
/// `buffa`'s codegen and checked in.
///
/// `@generated` — never edited by hand; `buf generate` rewrites it and the
/// drift test in this module's tests proves the checked-in copy still matches
/// the `.proto`. A scaffold this landing: it carries one placeholder message,
/// replaced by the recorded request/response pair in the wire wave.
pub mod proto {
    include!("cursor/ganja.cursor.v1.rs");
}

/// Value of [`PROVIDER_ENV`](super::PROVIDER_ENV) that selects the stub.
pub const ID: &str = "cursor";

/// What every request is answered with until the real wire lands.
///
/// Model-facing *and* user-facing — a headless run prints it, a TUI session
/// shows it in the status bar — so it says what to do next rather than only
/// what is missing.
pub const REFUSAL: &str = "cursor support is a stub: this build ships no cursor wire yet, \
     and nothing was sent. Select another provider, or reach an \
     openai-compatible endpoint through the config's `provider` table.";

/// The stub. Holds nothing, so its `Debug` can leak nothing.
#[derive(Debug, Default)]
pub struct CursorProvider;

#[async_trait]
impl Provider for CursorProvider {
    fn id(&self) -> &str {
        ID
    }

    /// Refuses before anything exists to cancel or to stream.
    ///
    /// `Transport` is the honest variant of the four: the request did not
    /// complete and no provider answered — `Auth` would send somebody to log
    /// in to a wire that does not exist. Revisit the taxonomy with the real
    /// wire, not before.
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        Err(ProviderError::Transport(REFUSAL.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::{super::PROVIDERS, *};

    /// The smallest request there is; the stub must refuse it without reading
    /// it.
    fn request() -> ChatRequest {
        ChatRequest {
            model: "still-imaginary".to_owned(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn the_stub_refuses_every_request_naming_the_deferral() {
        let refusal = CursorProvider
            .stream(request(), CancellationToken::new())
            .await
            .err()
            .expect("a stub that streamed something would be claiming a wire it does not have");

        let rendered = refusal.to_string();
        assert!(rendered.contains("stub"), "{rendered}");
        assert!(
            rendered.contains("`provider` table"),
            "the refusal has to say what to do instead, not only what is missing: {rendered}"
        );
    }

    #[test]
    fn the_identity_answered_is_the_one_the_shipped_list_carries() {
        assert_eq!(CursorProvider.id(), ID);
        assert!(
            PROVIDERS.contains(&ID),
            "a stub outside the shipped list would be selectable by nobody"
        );
    }

    /// Fieldless is the mechanism, not the aspiration: the day this struct
    /// grows a credential or a channel, this test is the reminder that its
    /// `Debug` is part of the no-secrets surface every other provider holds.
    #[test]
    fn the_debug_rendering_is_the_bare_name() {
        assert_eq!(format!("{CursorProvider:?}"), "CursorProvider");
    }

    /// The admitted runtime is real, not merely named: a generated message
    /// round-trips through `buffa`'s encode/decode. This is what makes the
    /// dependency reach the lock (so `cargo deny` audits its license) and the
    /// live spike run against the same version the wire will.
    #[test]
    fn the_admitted_protobuf_runtime_round_trips_a_generated_message() {
        use buffa::Message as _;

        let probe = super::proto::Probe::default().with_model("cursor-model-x");
        let bytes = probe.encode_to_vec();
        let decoded = super::proto::Probe::decode_from_slice(&bytes)
            .expect("a message buffa encoded decodes");

        assert_eq!(decoded.model.as_deref(), Some("cursor-model-x"));
    }

    /// The checked-in generated code still matches its `.proto`: regenerating
    /// with the same remote plugin must produce byte-identical output. A drift
    /// here means somebody edited the `@generated` file by hand or changed the
    /// `.proto` without regenerating — either way the source of truth and the
    /// compiled code have diverged.
    ///
    /// Skipped rather than failed when `buf` is absent: the drift check is a
    /// developer-machine guard, and the workspace deliberately keeps `buf`
    /// and `protoc` out of CI (the generated code is checked in for exactly
    /// that reason). CI proves the code compiles and round-trips; this proves
    /// it was not hand-edited, on a machine that can regenerate.
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
