use super::{
    API_KEY_ENV, CHAT, Dialect, GO_BASE_URL, GO_ID, GOOGLE, MESSAGES, OpencodeProvider, RESPONSES,
    ZEN_BASE_URL, ZEN_ID, messages_base,
};
use crate::{
    auth,
    provider::{PROVIDERS, Provider as _, ProviderError, responses::Backend},
};

/// A key no other value in this module could be mistaken for.
const KEY: &str = "sk-zen-canary-5150";

fn gateway(id: &'static str) -> OpencodeProvider {
    OpencodeProvider::at(id, "http://127.0.0.1:8080/zen/v1", KEY).expect("loopback may carry a key")
}

#[test]
fn two_ids_one_variable_and_both_spelled_the_way_the_catalog_files_them() {
    assert_eq!(ZEN_ID, "opencode");
    assert_eq!(GO_ID, "opencode-go");
    assert_eq!(ZEN_BASE_URL, "https://opencode.ai/zen/v1");
    assert_eq!(GO_BASE_URL, "https://opencode.ai/zen/go/v1");

    for id in [ZEN_ID, GO_ID] {
        assert!(
            PROVIDERS.contains(&id),
            "{id} ships, or nothing can select it"
        );
        assert_eq!(
            auth::key_var(id),
            Some(API_KEY_ENV),
            "both ids read the one variable the catalog names for each"
        );
        assert_eq!(
            auth::storage_key(id),
            id,
            "stored under its own name, as the vendor's own client does — an \
                 alias invented here would hide the credential from it"
        );
    }

    assert_eq!(gateway(ZEN_ID).id(), ZEN_ID);
    assert_eq!(gateway(GO_ID).id(), GO_ID, "not the wire it borrows");
}

/// The allowlist, and the two refusals that are the point of having one.
#[test]
fn a_transport_this_build_has_no_wire_for_is_refused_by_name() {
    assert_eq!(Dialect::of(None, ZEN_ID, "glm-5"), Ok(Dialect::Chat));
    assert_eq!(Dialect::of(Some(CHAT), ZEN_ID, "glm-5"), Ok(Dialect::Chat));
    assert_eq!(
        Dialect::of(Some(RESPONSES), ZEN_ID, "gpt-5.6-luna"),
        Ok(Dialect::Responses)
    );
    assert_eq!(
        Dialect::of(Some(MESSAGES), ZEN_ID, "qwen3.6-plus"),
        Ok(Dialect::Messages)
    );

    let refused = Dialect::of(Some(GOOGLE), ZEN_ID, "gemini-3-pro")
        .expect_err("this build has no Google wire and will not invent one");
    let ProviderError::Transport(message) = refused else {
        panic!("a request declined before it is made is a transport refusal");
    };
    assert!(message.contains("gemini-3-pro"), "{message}");
    assert!(
        message.contains(GOOGLE),
        "the refusal names the transport, or nobody can act on it: {message}"
    );
    assert!(
        message.contains("the vendor's own client"),
        "parity, not a gap — say so: {message}"
    );

    let unknown = Dialect::of(Some("@ai-sdk/something-new"), GO_ID, "future-model")
        .expect_err("an allowlist, not a fallthrough");
    assert!(
        matches!(unknown, ProviderError::Transport(ref m) if m.contains("@ai-sdk/something-new")),
        "{unknown:?}"
    );
}

/// Whichever wire a turn lands on, the credential is not something any of
/// them renders.
///
/// The *header* each dialect presents it under is the probe's hardest-won
/// fact and cannot be proved here: it depends on the catalog, and
/// installing one is process-wide state. `tests/opencode_dialects.rs` is
/// where all three are driven against a real socket and their headers read
/// off the requests that actually arrived.
#[test]
fn no_wire_behind_the_gateway_renders_the_key_it_holds() {
    let provider = gateway(ZEN_ID);

    let rendered = format!(
        "{:?}{:?}{:?}",
        provider.chat, provider.responses, provider.messages
    );
    assert!(!rendered.contains(KEY), "{rendered}");
    assert!(
        rendered.contains("Key"),
        "each wire still says what kind of credential it holds: {rendered}"
    );
}

/// The Responses rows here take openrouter's posture, and for openrouter's
/// reason: this vendor documents nothing about the sealed-reasoning
/// pairing, so nothing is asked for and nothing is replayed.
#[test]
fn the_gateways_responses_rows_guess_nothing_about_sealed_reasoning() {
    for id in [ZEN_ID, GO_ID] {
        assert!(
            !Backend::Opencode(id).replays_reasoning(),
            "{id} would be asking a gateway for a field its vendor never documented"
        );
        assert_eq!(Backend::Opencode(id).provider_id(), id);
    }
}

/// The catalog is the only thing that knows a dialect, so a provider that
/// cannot read one still has to take a turn rather than refuse every model.
#[test]
fn a_model_the_table_has_never_heard_of_falls_back_to_the_published_default() {
    // Both ids declare `@ai-sdk/openai-compatible` at the provider level,
    // so this is the vendor's own fallback rather than a house choice.
    assert_eq!(
        Dialect::of(None, ZEN_ID, "a-model-added-since-this-cache"),
        Ok(Dialect::Chat)
    );
    assert_eq!(
        gateway(GO_ID)
            .dialect("a-model-added-since-this-cache")
            .expect("an unknown model still takes a turn"),
        Dialect::Chat
    );
}

/// The gateway's three paths hang off one base; two of ganja's wires agree
/// with that and the third puts `/v1` in itself. Getting this wrong is a
/// `404` on a third of both rosters and nothing else — no error at
/// construction, no wrong header, just a path with the segment twice.
#[test]
fn the_messages_wire_is_handed_the_base_it_will_re_add_the_version_to() {
    assert_eq!(messages_base(ZEN_BASE_URL), "https://opencode.ai/zen");
    assert_eq!(messages_base(GO_BASE_URL), "https://opencode.ai/zen/go");
    // Which is to say: the URL that wire then builds is the gateway's own.
    for (base, want) in [
        (ZEN_BASE_URL, "https://opencode.ai/zen/v1/messages"),
        (GO_BASE_URL, "https://opencode.ai/zen/go/v1/messages"),
    ] {
        assert_eq!(format!("{}/v1/messages", messages_base(base)), want);
    }

    assert_eq!(
        messages_base("https://opencode.ai/zen/v1/"),
        "https://opencode.ai/zen",
        "a trailing slash is not a different endpoint"
    );
    assert_eq!(
        messages_base("http://127.0.0.1:8080"),
        "http://127.0.0.1:8080",
        "a base that never carried the segment is not this function's to edit"
    );
}

/// A key may not be sent anywhere the other wires' keys could not be, and
/// the refusal has to come from construction rather than from the request.
#[test]
fn every_wire_behind_one_gateway_is_held_to_the_same_endpoint_rule() {
    let refused = OpencodeProvider::at(ZEN_ID, "http://opencode.ai/zen/v1", KEY)
        .expect_err("plain http to a public host puts the key on the wire in the clear");

    assert!(
        matches!(refused, ProviderError::Transport(_)),
        "{refused:?}"
    );
}
