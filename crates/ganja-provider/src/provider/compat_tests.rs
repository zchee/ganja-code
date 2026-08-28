use super::{CompatProvider, Dialect};
use crate::catalog;
use crate::provider::{CredentialSource, PROVIDERS, Presented, Provider as _, ProviderError};

/// A credential that must never be rendered by anything here.
const CANARY: &str = "sk-test-canary-XYZ";

fn built(dialect: Dialect, base_url: &str) -> Result<CompatProvider, ProviderError> {
    CompatProvider::new(
        "local-llama",
        dialect,
        base_url,
        CredentialSource::Key(Presented::new(CANARY).expect("a non-blank key")),
        reqwest::header::HeaderMap::new(),
    )
}

/// The whole reason this is a newtype rather than a flag: a turn is priced,
/// gated and disclosed by the name its provider reports.
#[test]
fn a_config_named_provider_answers_to_the_name_its_entry_was_written_under() {
    for dialect in
        [Dialect::OpenaiChatCompletions, Dialect::OpenaiResponses, Dialect::AnthropicMessages]
    {
        let provider = built(dialect, "http://127.0.0.1:8080/v1").expect("a client builds");

        assert_eq!(provider.id(), "local-llama", "{dialect:?} reported the wire it borrows");
        assert!(
            !PROVIDERS.contains(&provider.id()),
            "the config tier is what makes this selectable, not the builtin list"
        );
        assert!(
            !catalog::carries(provider.id()),
            "an endpoint a person named is one no published table knows"
        );
    }
}

/// The endpoint is not exempt from the rule every other provider's is held
/// to just because it arrived as configuration — and neither the key nor
/// the URL may reach a rendering, which is what a `tracing` field holding
/// a provider becomes.
#[test]
fn a_configured_endpoint_may_carry_a_key_only_where_a_builtin_one_could() {
    for dialect in
        [Dialect::OpenaiChatCompletions, Dialect::OpenaiResponses, Dialect::AnthropicMessages]
    {
        let refused = built(dialect, "http://gateway.example/v1")
            .expect_err("plain http to a public host puts the key on the wire in the clear");
        assert!(matches!(refused, ProviderError::Transport(_)), "{dialect:?}: {refused:?}");

        let provider = built(dialect, "https://ganja:secret@gateway.example/v1")
            .expect("https is where a key may travel");
        let rendered = format!("{provider:?}");
        assert!(
            rendered.contains("local-llama") && rendered.contains("gateway.example"),
            "a provider renders as which one it is and where it points: {rendered}"
        );
        assert!(
            !rendered.contains(CANARY) && !rendered.contains("secret"),
            "the credential — in the header or in the userinfo — reached a \
                 rendering: {rendered}"
        );
    }
}

/// The three words a config file may spell, held to the spelling `serde`
/// derives, because a fourth value here would be a fourth request/response
/// mapping rather than another endpoint.
#[test]
fn the_dialects_are_spelled_the_way_a_config_file_spells_them() {
    for (dialect, spelled) in [
        (Dialect::OpenaiChatCompletions, "openai-chat-completions"),
        (Dialect::OpenaiResponses, "openai-responses"),
        (Dialect::AnthropicMessages, "anthropic-messages"),
    ] {
        // Through the derive, which is the whole mechanism: a dialect is
        // read from a config file and never written back, so this
        // direction is the only one there is.
        assert_eq!(
            serde_json::from_value::<Dialect>(serde_json::json!(spelled))
                .expect("the word a config file writes"),
            dialect
        );
    }
    assert!(
        serde_json::from_value::<Dialect>(serde_json::json!("anthropic")).is_err(),
        "a dialect nothing implements is refused rather than guessed at"
    );
}
