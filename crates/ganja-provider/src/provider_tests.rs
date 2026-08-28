use std::convert::Infallible;

use futures::{StreamExt as _, stream};
use tokio_util::sync::CancellationToken;

use super::sse::Frame;
use super::{
    CredentialSource, Mapper, Peeked, Presented, ProviderError, ProviderEvent, check_base_url,
    configured_headers, endpoint, events, peeked, reopens, reported, responses, retry, shielded,
    shown_base_url, unusable,
};
use crate::auth;
use crate::protocol::FinishReason;

/// The three shapes an in-body error object arrives in, and the rule that
/// none of them may be answered with a sentence that says nothing.
/// D475's boundary, from the peek's side: content settles the stream,
/// an empty failure reads as a death worth judging, and a clean end
/// keeps what it buffered.
#[test]
fn peeking_settles_on_the_first_content_event_and_hands_back_the_rest() {
    use futures::StreamExt as _;

    let settled = futures::executor::block_on(peeked(
        stream::iter(vec![
            ProviderEvent::Usage(crate::protocol::Usage::default()),
            ProviderEvent::TextDelta("first".to_owned()),
            ProviderEvent::TextDelta("second".to_owned()),
        ])
        .boxed(),
    ));
    let Peeked::Content { prefix, rest } = settled else {
        panic!("text settles the stream");
    };
    assert_eq!(prefix.len(), 2, "the prefix ends with the proving event");
    assert!(matches!(prefix[1], ProviderEvent::TextDelta(ref text) if text == "first"));
    let rest: Vec<ProviderEvent> = futures::executor::block_on(rest.collect());
    assert_eq!(rest.len(), 1, "nothing past the peek is consumed");

    let died = futures::executor::block_on(peeked(
        stream::iter(vec![ProviderEvent::Failed(ProviderError::Status {
            status: 500,
            message: "overloaded".to_owned(),
        })])
        .boxed(),
    ));
    assert!(
        matches!(died, Peeked::Died { ref prefix, .. } if prefix.is_empty()),
        "a failure on an empty transcript is a death, not content"
    );

    let ended = futures::executor::block_on(peeked(
        stream::iter(vec![
            ProviderEvent::Usage(crate::protocol::Usage::default()),
            ProviderEvent::Finish(FinishReason::Completed),
        ])
        .boxed(),
    ));
    assert!(
        matches!(ended, Peeked::Ended { ref prefix } if prefix.len() == 2),
        "a clean end keeps what it buffered"
    );
}

/// D475's bound and its classification: three reopenings for a transient
/// death, none for a fourth and none for a failure a retry cannot fix.
#[test]
fn an_empty_turns_transient_death_is_reopened_at_most_three_times() {
    let overloaded = ProviderError::Status {
        status: 500,
        message: "Our servers are currently overloaded.".to_owned(),
    };
    assert!(reopens(0, &overloaded) && reopens(1, &overloaded) && reopens(2, &overloaded));
    assert!(!reopens(3, &overloaded), "the fourth death is the turn's answer");
    assert!(
        !reopens(0, &ProviderError::Auth("expired".to_owned())),
        "a failure retrying cannot fix is never reopened"
    );

    // The ported schedule, headerless: 2s, 4s, 8s, capped far later.
    assert_eq!(retry::stream_backoff(1), std::time::Duration::from_secs(2));
    assert_eq!(retry::stream_backoff(2), std::time::Duration::from_secs(4));
    assert_eq!(retry::stream_backoff(3), std::time::Duration::from_secs(8));
    assert_eq!(retry::stream_backoff(64), retry::MAX_DELAY);
}

/// The mid-stream half of the credential rule: an in-body error frame is
/// mapped by a decoder holding no [`Presented`], so [`shielded`] is where
/// the mask goes on — and a message quoting the refused credential must
/// leave the wire wearing it.
#[test]
fn a_mid_stream_failure_cannot_carry_the_credential_it_quoted() {
    use futures::StreamExt as _;

    let presented = Presented::new("sk-canary-0123456789").expect("a credential");
    let failures = stream::iter(vec![
        ProviderEvent::Failed(ProviderError::Status {
            status: 500,
            message: "the key sk-canary-0123456789 was rejected".to_owned(),
        }),
        ProviderEvent::Finish(FinishReason::Completed),
    ])
    .boxed();

    let events: Vec<ProviderEvent> = futures::executor::block_on(
        shielded(failures, presented, "https://example.test/v1".to_owned()).collect(),
    );
    let ProviderEvent::Failed(ProviderError::Status { message, .. }) = &events[0] else {
        panic!("the failure survives the shield: {events:?}");
    };
    assert!(
        !message.contains("sk-canary-0123456789") && message.contains("[redacted]"),
        "the credential must not leave the wire: {message}"
    );
    assert!(
        matches!(events[1], ProviderEvent::Finish(_)),
        "everything that is not a failure passes untouched"
    );
}

#[test]
fn an_error_body_is_reported_as_whatever_it_actually_carried() {
    assert_eq!(
        reported(&serde_json::json!({"message": "rate limited", "code": "429"})),
        "rate limited",
        "a message is the provider's own words and wins outright"
    );

    // The shape that produced the report this exists for: a status bar
    // reading "the provider answered 500: the provider reported an error"
    // over a body that named the failure perfectly well.
    let named = reported(&serde_json::json!({
        "type": "server_error",
        "code": "model_overloaded",
        "param": serde_json::Value::Null,
    }));
    assert!(
        named.contains("type: server_error") && named.contains("code: model_overloaded"),
        "a slug is a thing to search for where a generic sentence is not: {named}"
    );
    assert!(
        !named.contains("param"),
        "a null field is not detail, and rendering it as one is noise: {named}"
    );

    // A number where the schema says string, because `code` is a number on
    // at least one wire here and a body is not a contract.
    assert!(reported(&serde_json::json!({"code": 503})).contains("code: 503"));

    // Structure where the schema says string: skipped rather than inlined,
    // so a hostile or malformed body cannot put a blob in the message.
    let structured = reported(&serde_json::json!({
        "type": {"nested": "object"},
        "code": "model_overloaded",
    }));
    assert!(
        structured.contains("code: model_overloaded") && !structured.contains("nested"),
        "an object-valued field is not a name: {structured}"
    );

    // The wrapped shape the codex backend's mid-stream 500 wore: the
    // detail lives one level down, and reading only the wrapper renders
    // the useless `(type: error)`.
    assert_eq!(
        reported(&serde_json::json!({
            "type": "error",
            "error": {"type": "server_error", "message": "boom"},
        })),
        "boom",
        "a nested error object's message outranks the wrapper's naming"
    );
    let wrapped = reported(&serde_json::json!({
        "type": "error",
        "error": {"code": "overloaded"},
    }));
    assert!(
        wrapped.contains("code: overloaded") && !wrapped.contains("type: error"),
        "a nested error object's naming outranks the wrapper's: {wrapped}"
    );

    for empty in
        [serde_json::json!({}), serde_json::json!({"message": "   "}), serde_json::Value::Null]
    {
        assert_eq!(
            reported(&empty),
            "the provider reported an error and its body carried no detail",
            "a body with nothing in it has to say so, not sound like a message"
        );
    }
}

/// A log line is a file on disk, so the one field of a URL that is allowed
/// to carry a credential — and the one that is a documented place to put a
/// token — must never reach it.
#[test]
fn a_logged_endpoint_carries_neither_userinfo_nor_a_query_string() {
    let url = reqwest::Url::parse(
        "https://someone:sk-test-canary-XYZ@api.example.com:8443/v1/responses\
             ?auth_token=sk-test-canary-ABC",
    )
    .expect("a parseable URL");

    // A base that contributed no path of its own, so the route this build
    // appended is this build's to name.
    let rendered = endpoint(&url, "https://api.example.com:8443");

    assert_eq!(rendered, "https://api.example.com:8443/v1/responses");
    assert!(!rendered.contains("canary"), "a credential reached a log line: {rendered}");
}

/// The P22 narrowing (`od8`): a base a person configured may carry a token
/// in its *path*, and nothing here can tell a route segment from a secret
/// one — so what a base contributed is never rendered, and what the wire
/// appended still is.
#[test]
fn a_logged_endpoint_renders_no_path_segment_a_configured_base_contributed() {
    let base = "https://someone:sk-test-canary-USERINFO@compat.example.com\
                    /tenant/sk-test-canary-PATH";
    let url = reqwest::Url::parse(&format!("{base}/v1/messages")).expect("a parseable URL");

    let rendered = endpoint(&url, base);

    assert_eq!(
        rendered, "https://compat.example.com/v1/messages",
        "the route this build appended survives; the tenant path does not"
    );
    assert!(!rendered.contains("canary"), "a credential reached a log line: {rendered}");

    // Fails closed: a base this function cannot read is a base whose shape
    // it cannot vouch for, so none of the path is rendered.
    assert_eq!(
        endpoint(&url, "not a URL"),
        "https://compat.example.com",
        "an unparseable base yields no path rather than a guessed one"
    );
}

/// The builtin bases that carry a path of their own — which is why this
/// strips a prefix rather than classifying a base as shipped or declared.
/// Each keeps exactly the route its wire appended, so two backends sharing
/// a host stay tellable apart in a log.
#[test]
fn a_builtin_base_that_carries_its_own_path_still_names_the_route() {
    for (base, route) in [
        // `openai::DEFAULT_BASE_URL`, the platform Responses backend.
        ("https://api.openai.com/v1", "/responses"),
        // `responses::DEFAULT_BASE_URL`, the ChatGPT codex backend.
        ("https://chatgpt.com/backend-api/codex", "/responses"),
        // A base written with a trailing slash is the same base.
        ("https://api.openai.com/v1/", "/chat/completions"),
        // And one with no path of its own renders the whole path.
        ("https://api.anthropic.com", "/v1/messages"),
    ] {
        let url = reqwest::Url::parse(&format!("{}{route}", base.trim_end_matches('/')))
            .expect("a parseable URL");
        let rendered = endpoint(&url, base);

        assert!(rendered.ends_with(route), "{base} should still name {route}; got {rendered}");
    }
}

/// `headers` is where a configured endpoint's token goes, so a refusal
/// about one may name the header and never its value.
#[test]
fn a_header_a_request_cannot_carry_is_refused_by_name_and_not_by_value() {
    let mut declared = super::BTreeMap::new();
    declared.insert("x-route".to_owned(), "gpu-0".to_owned());
    let carried = configured_headers("local-llama", &declared).expect("an ordinary header");
    assert_eq!(carried["x-route"], "gpu-0");

    let mut refused = super::BTreeMap::new();
    refused.insert("x authorization".to_owned(), "sk-test-canary-XYZ".to_owned());
    let error = configured_headers("local-llama", &refused)
        .expect_err("a space is not legal in a header name");
    let rendered = format!("{error} / {error:?}");
    assert!(rendered.contains("x authorization"), "{rendered}");
    assert!(!rendered.contains("sk-test-canary-XYZ"), "the value reached the refusal: {rendered}");

    let mut unencodable = super::BTreeMap::new();
    unencodable.insert("x-route".to_owned(), "sk-test-canary-XYZ\n".to_owned());
    let error = configured_headers("local-llama", &unencodable)
        .expect_err("a newline cannot travel in a header value");
    let rendered = format!("{error} / {error:?}");
    assert!(rendered.contains("x-route"), "{rendered}");
    assert!(!rendered.contains("sk-test-canary-XYZ"), "the value reached the refusal: {rendered}");
}

/// The value the subscription backend actually hands over, held to the one
/// property that makes it correct. `openai_provider` is what pairs the two,
/// and `responses_wire.rs` is where that pairing is observed with a store
/// and an environment behind it.
#[test]
fn the_subscription_backends_default_is_one_that_backend_serves() {
    assert!(
        responses::serves(responses::SUBSCRIPTION_DEFAULT),
        "a seat that cannot run its own default cannot take a turn at all"
    );
}

/// Emits whatever a frame's data spells, so that the plumbing can be
/// tested without a provider's JSON in the way.
struct Echo;

impl Mapper for Echo {
    fn frame(&mut self, frame: &Frame, events: &mut Vec<ProviderEvent>) {
        match frame.data.as_str() {
            "done" => {
                events.push(ProviderEvent::Usage(crate::protocol::Usage::default()));
                events.push(ProviderEvent::Finish(FinishReason::Completed));
                events.push(ProviderEvent::TextDelta("after the end".to_owned()));
            }
            data => events.push(ProviderEvent::TextDelta(data.to_owned())),
        }
    }
}

/// Feeds `chunks` through the real pipeline.
fn pipeline(
    chunks: Vec<&'static str>,
    cancel: CancellationToken,
) -> impl futures::Stream<Item = ProviderEvent> {
    events(
        stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok::<&[u8], Infallible>(chunk.as_bytes()))
                .collect::<Vec<_>>(),
        ),
        cancel,
        Echo,
    )
}

#[test]
fn a_credential_never_renders_itself() {
    let key = Presented::new("sk-test-canary-XYZ").expect("a non-blank key");

    assert_eq!(format!("{key:?}"), "Presented([redacted])");
    assert_eq!(key.redact("rejected sk-test-canary-XYZ, sorry"), "rejected [redacted], sorry");
    assert_eq!(key.expose(), "sk-test-canary-XYZ");
    assert!(Presented::new("   ").is_none(), "a blank key is not a key");
    assert!(Presented::new("").is_none());

    // The same has to hold of the source a request resolves one from, or a
    // provider's own `Debug` — which is what every `tracing` field holding
    // one becomes — would print what the type it wraps refuses to.
    let held = CredentialSource::Key(key);
    assert_eq!(format!("{held:?}"), "Key(Presented([redacted]))");
    assert!(!format!("{held:?}").contains("sk-test-canary-XYZ"));
}

/// A dead refresh token and a token endpoint that could not be reached are
/// two different situations, and the difference is not a wording choice:
/// [`ProviderError::is_retryable`] is what the retry driver reads, and a
/// refresh sits exactly where that driver applies. Classifying a refusal as
/// transport turns one expired grant into a retry storm against an identity
/// provider; classifying an unreachable endpoint as auth sends someone
/// whose network dropped through a browser login they did not need.
#[test]
fn only_a_refusal_is_worth_a_new_login_and_only_a_reachable_failure_is_worth_retrying() {
    let refused = unusable(
        &auth::AuthError::ReauthRequired {
            provider_id: "grok".to_owned(),
            reason: "HTTP 401, invalid_grant".to_owned(),
        }
        .into(),
    );
    let unreachable = unusable(
        &auth::AuthError::RefreshUnavailable {
            provider_id: "grok".to_owned(),
            reason: "connection refused".to_owned(),
        }
        .into(),
    );

    assert!(
        matches!(refused, ProviderError::Auth(_)),
        "a dead refresh token is not a transport failure: {refused:?}"
    );
    assert!(
        !refused.is_retryable(),
        "retrying a refused grant is a storm against an identity provider"
    );
    assert!(
        format!("{refused}").contains("ganja auth login grok"),
        "the message is what a status bar shows, and only a login fixes this: {refused}"
    );

    assert!(
        matches!(unreachable, ProviderError::Transport(_)),
        "an endpoint that never answered has not refused anything: {unreachable:?}"
    );
    assert!(
        unreachable.is_retryable(),
        "trying again is exactly what fixes a refresh that could not be reached"
    );

    // The rest of the taxonomy is a credential that has to be replaced or a
    // file that has to be repaired, and repeating the request fixes
    // neither. One case since P22 (`flp`), where there were two: the
    // `Expired` variant retired with the kind it mapped to, having had no
    // production constructor once `usable_access` went.
    let absent = unusable(
        &auth::AuthError::NotOauth { provider_id: "grok".to_owned(), found: "an API key" }.into(),
    );

    assert!(
        matches!(absent, ProviderError::Auth(_)) && !absent.is_retryable(),
        "{absent:?} should be a non-retryable auth failure"
    );
}

/// The key travels in a header on every request, so the transport is what
/// decides who else gets to read it. Loopback is exempt because the bytes
/// never reach a network — which is what the test suite and a local
/// inference server both depend on.
#[test]
fn only_https_or_loopback_may_carry_a_key() {
    let allowed = [
        "https://api.anthropic.com",
        "https://gateway.example/v1",
        "http://127.0.0.1:8080",
        // The whole 127/8 block is loopback, not just the one address.
        "http://127.10.20.30:1234",
        "http://[::1]:8080/v1",
        "http://localhost:11434/v1",
        // Userinfo is legal configuration, and does not change the hop.
        "http://ganja:secret@127.0.0.1:8080",
    ];
    // Every one of these is an ordinary host belonging to whoever
    // registered it, and every one of them defeats some cheaper spelling of
    // this check: a prefix match, a substring match, a suffix match, or a
    // look at the URL rather than at its host.
    let refused = [
        "http://api.anthropic.com",
        "http://192.168.1.10:8080",
        "http://127.0.0.1.evil.com",
        "http://127.0.0.1@evil.com",
        "http://localhost@evil.com",
        "http://localhost.evil.com",
        "http://evil.com/127.0.0.1",
        "http://evil.com/?host=localhost",
        "http://evil.com#127.0.0.1",
        "http://notlocalhost",
        // An IPv4-mapped IPv6 address does reach loopback, but `is_loopback`
        // is only true of `::1`; refusing it fails in the safe direction.
        "http://[::ffff:127.0.0.1]",
        "ftp://127.0.0.1",
        "file:///etc/passwd",
        "not a url at all",
        "",
    ];

    for base_url in allowed {
        assert!(check_base_url(base_url).is_ok(), "{base_url} should be usable");
    }
    for base_url in refused {
        let error =
            check_base_url(base_url).expect_err(&format!("{base_url} should not be handed a key"));

        assert!(matches!(error, ProviderError::Transport(_)), "{base_url}: got {error:?}");
        // A base URL is allowed to carry credentials in its userinfo, so
        // the refusal must describe the rule rather than quote the URL.
        assert!(
            !format!("{error} / {error:?}").contains(base_url) || base_url.is_empty(),
            "{base_url} was echoed back by its own refusal"
        );
    }
}

/// What a base URL may carry into a rendering, and what it may not. The
/// stripped parts are the ones a credential fits in; the kept parts are the
/// ones that say which endpoint this is.
#[test]
fn a_shown_base_url_keeps_the_endpoint_and_drops_the_secrets() {
    let cases = [
        ("https://api.anthropic.com", "https://api.anthropic.com/"),
        ("https://ganja:secret@gateway.invalid:8443/v1", "https://gateway.invalid:8443/v1"),
        // A token in a query string is a real shape for a gateway URL.
        ("https://gateway.invalid/v1?token=secret", "https://gateway.invalid/v1"),
        ("https://gateway.invalid/v1#secret", "https://gateway.invalid/v1"),
        // Userinfo with no password at all still names an account.
        ("https://secret@gateway.invalid", "https://gateway.invalid/"),
        ("http://127.0.0.1:8080/v1", "http://127.0.0.1:8080/v1"),
        // Nothing parsed, so nothing can be said to be safe.
        ("not a url at all", "[unparseable]"),
        ("", "[unparseable]"),
    ];

    for (base_url, expected) in cases {
        assert_eq!(shown_base_url(base_url), expected, "showing {base_url}");
    }
}

#[tokio::test]
async fn nothing_survives_a_terminal_event() {
    let seen: Vec<ProviderEvent> =
        pipeline(vec!["data: hi\n\ndata: done\n\ndata: more\n\n"], CancellationToken::new())
            .collect()
            .await;

    assert_eq!(
        seen,
        vec![
            ProviderEvent::TextDelta("hi".to_owned()),
            ProviderEvent::Usage(crate::protocol::Usage::default()),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "a finish ends the stream, including events from the same frame"
    );
}

#[tokio::test]
async fn a_body_that_just_stops_fails_rather_than_completing() {
    let seen: Vec<ProviderEvent> =
        pipeline(vec!["data: hi\n\n"], CancellationToken::new()).collect().await;

    assert_eq!(
        seen,
        vec![
            ProviderEvent::TextDelta("hi".to_owned()),
            ProviderEvent::Failed(ProviderError::Transport(
                "the response body ended before the model finished".to_owned()
            )),
        ]
    );
}

#[tokio::test]
async fn a_transport_error_mid_body_becomes_a_failure() {
    let chunks = stream::iter(vec![
        Ok::<&[u8], &str>(b"data: hi\n\n".as_slice()),
        Err("connection reset by peer"),
    ]);
    let seen: Vec<ProviderEvent> = events(chunks, CancellationToken::new(), Echo).collect().await;

    assert_eq!(
        seen,
        vec![
            ProviderEvent::TextDelta("hi".to_owned()),
            ProviderEvent::Failed(ProviderError::Transport("connection reset by peer".to_owned())),
        ]
    );
}

#[tokio::test]
async fn a_cancelled_stream_stops_without_reporting_a_failure() {
    let cancel = CancellationToken::new();
    let mut stream =
        Box::pin(pipeline(vec!["data: one\n\ndata: two\n\ndata: three\n\n"], cancel.clone()));

    assert_eq!(stream.next().await, Some(ProviderEvent::TextDelta("one".to_owned())));
    cancel.cancel();

    assert_eq!(
        stream.next().await,
        None,
        "a cancelled stream ends, and never with a failure the engine would report"
    );
}

/// The merge rule every wire's send site leans on, stated once at the
/// helper: the effort map goes in first, so on a shared key the wire's
/// own field is what survives.
#[test]
fn a_spliced_body_keeps_the_wires_fields_over_the_efforts() {
    let options = serde_json::json!({"model": "theirs", "extra": 1})
        .as_object()
        .cloned()
        .expect("the fixture options are an object");
    let body = serde_json::json!({"model": "ours", "stream": true});

    let merged = serde_json::to_value(super::splice_effort(&options, &body))
        .expect("a spliced body serializes");

    assert_eq!(merged, serde_json::json!({"model": "ours", "stream": true, "extra": 1}));

    let untouched = serde_json::to_value(super::splice_effort(&serde_json::Map::new(), &body))
        .expect("a spliced body serializes");
    assert_eq!(untouched, body, "no effort means the wire's body exactly");
}
