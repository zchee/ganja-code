//! The OpenCode gateways' one load-bearing behaviour, end to end on a socket:
//! **the catalog decides which wire a turn takes, and the wire decides which
//! header the key travels under.**
//!
//! Nothing smaller proves this. The dialect comes from a catalog row, the
//! header comes from whichever of three wires that row selected, and the
//! failure mode the probe found — `/messages` answering a bearer with
//! `401 AuthError "Missing API key."` — is invisible to any test that does not
//! look at the bytes actually sent. So this drives real turns against a
//! loopback listener and asserts the request line and the header names that
//! arrived.
//!
//! Four facts, one test each, plus the refusal:
//!
//! 1. a row with no transport of its own → `POST /chat/completions`, bearer;
//! 2. `@ai-sdk/openai` → `POST /responses`, bearer;
//! 3. `@ai-sdk/anthropic` → `POST /messages`, **`x-api-key` and no bearer**;
//! 4. the same model id, `minimax-m3`, takes **different wires on the two ids**
//!    — chat on Zen, Messages on Go — which is the case that makes "per
//!    (provider, model)" load-bearing rather than a wording preference;
//! 5. `@ai-sdk/google` is refused by name **without opening a connection**.
//!
//! One test, one binary, on purpose: it points the catalog at a file and turns
//! fetching off, which is process-wide, and the table it installs is one every
//! other test in the binary would read.

use std::{env, fs};

use futures::StreamExt as _;
use ganja_core::catalog;
use ganja_core::protocol::Message;
use ganja_core::provider::{ChatRequest, OpencodeProvider, Provider, ProviderError, opencode};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// The key every turn here presents. Nothing may render it; what the
/// assertions read is only *which header name* carried it.
const KEY: &str = "sk-zen-wire-canary-6270";

/// A catalog in the shape the endpoint publishes, trimmed to the rows that
/// exercise every branch — including the one model that differs between the
/// two ids, which is the whole reason the dialect cannot be a per-provider
/// setting.
const PAYLOAD: &str = r#"{
  "opencode": {
    "id": "opencode",
    "name": "OpenCode Zen",
    "env": ["OPENCODE_API_KEY"],
    "api": "https://opencode.ai/zen/v1",
    "npm": "@ai-sdk/openai-compatible",
    "models": {
      "glm-5": {
        "id": "glm-5",
        "provider": null,
        "cost": { "input": 1.0, "output": 2.0 },
        "limit": { "context": 200000, "output": 8000 }
      },
      "minimax-m3": {
        "id": "minimax-m3",
        "provider": null,
        "cost": { "input": 1.0, "output": 2.0 },
        "limit": { "context": 200000, "output": 8000 }
      },
      "gpt-5.6-luna": {
        "id": "gpt-5.6-luna",
        "provider": { "npm": "@ai-sdk/openai" },
        "cost": { "input": 1.0, "output": 2.0 },
        "limit": { "context": 200000, "output": 8000 }
      },
      "qwen3.6-plus": {
        "id": "qwen3.6-plus",
        "provider": { "npm": "@ai-sdk/anthropic" },
        "cost": { "input": 1.0, "output": 2.0 },
        "limit": { "context": 200000, "output": 8000 }
      },
      "gemini-3-pro": {
        "id": "gemini-3-pro",
        "provider": { "npm": "@ai-sdk/google" },
        "cost": { "input": 1.0, "output": 2.0 },
        "limit": { "context": 200000, "output": 8000 }
      }
    }
  },
  "opencode-go": {
    "id": "opencode-go",
    "name": "OpenCode Go",
    "env": ["OPENCODE_API_KEY"],
    "api": "https://opencode.ai/zen/go/v1",
    "npm": "@ai-sdk/openai-compatible",
    "models": {
      "minimax-m3": {
        "id": "minimax-m3",
        "provider": { "npm": "@ai-sdk/anthropic" },
        "cost": { "input": 1.0, "output": 2.0 },
        "limit": { "context": 200000, "output": 8000 }
      }
    }
  }
}"#;

/// A chat-completions stream that ends properly, so the turn finishes rather
/// than tripping the retry driver and opening a second connection.
const CHAT_BODY: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\
      \"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\n",
    "data: [DONE]\n\n",
);

/// The same for the Responses API, whose terminator is a named frame.
const RESPONSES_BODY: &str = concat!(
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":\
      {\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
);

/// Accepts one turn, hands back the request head it saw, and answers `body` so
/// the stream completes.
///
/// Reads until the blank line that ends the head **and** the model id in the
/// body have both arrived, which is the same "read past the head" the sibling
/// wire tests do — a request whose head is in hand but whose body is still in
/// flight would let the reply race the send.
async fn one_turn(listener: &TcpListener, model: &str, body: &'static str) -> String {
    let (mut socket, _) = listener.accept().await.expect("the turn connects");

    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = socket.read(&mut buffer).await.expect("the request arrives");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&request);
        if text.contains("\r\n\r\n") && text.contains(model) {
            break;
        }
    }

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.expect("the reply is writable");
    socket.flush().await.expect("the reply flushes");
    drop(socket);

    String::from_utf8_lossy(&request).into_owned()
}

/// Runs one turn through `provider` and returns what the socket received.
async fn sent(
    listener: &TcpListener,
    provider: &OpencodeProvider,
    model: &str,
    body: &'static str,
) -> String {
    let request = ChatRequest {
        turn_start: 0,
        effort_options: Default::default(),
        model: model.to_owned(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
    };

    let served = one_turn(listener, model, body);
    let driven = async {
        let stream = provider
            .stream(request, CancellationToken::new())
            .await
            .expect("the wire accepted the request");
        // Drained rather than dropped: a stream dropped mid-body can close the
        // socket before the server has finished writing, which would race the
        // assertion rather than test anything.
        stream.collect::<Vec<_>>().await;
    };
    let (head, ()) = tokio::join!(served, driven);

    head
}

/// The lowercased header names, and nothing else.
///
/// Names, never values: the request head is the one place the credential is
/// *supposed* to appear — that is what an auth header is — so what these
/// assertions read is which header carried it, never what it said. Nothing
/// here prints a head that has not first been reduced to this.
fn header_names(head: &str) -> Vec<String> {
    head.lines()
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.trim().to_ascii_lowercase())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_catalogs_transport_hint_picks_the_wire_and_the_wire_picks_the_header() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let payload = home.path().join("api.json");
    fs::write(&payload, PAYLOAD).expect("the fixture is writable");

    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_CACHE_HOME", home.path());
        env::set_var(catalog::MODELS_PATH_ENV, &payload);
        env::set_var(catalog::DISABLE_FETCH_ENV, "1");
    }
    assert!(catalog::load_cached(), "the fixture catalog is adopted");
    assert!(
        catalog::carries(opencode::ZEN_ID) && catalog::carries(opencode::GO_ID),
        "both gateways' rows landed under their own ids"
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    // Shaped like the real thing, `/v1` and all: the three wires disagree about
    // where that segment lives, and a bare host would hide the disagreement
    // rather than test it.
    let base =
        format!("http://{}/zen/v1", listener.local_addr().expect("a bound socket has an address"));
    let zen = OpencodeProvider::at(opencode::ZEN_ID, &base, KEY).expect("loopback may carry a key");
    let go = OpencodeProvider::at(opencode::GO_ID, &base, KEY).expect("loopback may carry a key");

    // 1. No transport of its own: the provider-level default, which is chat.
    let head = sent(&listener, &zen, "glm-5", CHAT_BODY).await;
    assert!(
        head.starts_with("POST /zen/v1/chat/completions "),
        "a row that overrides nothing takes the dialect its provider declared: {head}"
    );
    let names = header_names(&head);
    assert!(names.contains(&"authorization".to_owned()), "{names:?}");
    assert!(
        !names.contains(&"x-api-key".to_owned()),
        "the Messages header has no business on this path: {names:?}"
    );

    // 2. `@ai-sdk/openai` is the Responses API, still on a bearer.
    let head = sent(&listener, &zen, "gpt-5.6-luna", RESPONSES_BODY).await;
    assert!(head.starts_with("POST /zen/v1/responses "), "{head}");
    let names = header_names(&head);
    assert!(names.contains(&"authorization".to_owned()), "{names:?}");
    assert!(!names.contains(&"x-api-key".to_owned()), "{names:?}");

    // 3. `@ai-sdk/anthropic` is Messages — and this is the one the probe
    //    proved cannot be a bearer.
    let messages_body =
        include_str!("../../ganja-provider/tests/fixtures/anthropic_happy_path.sse");
    let head = sent(&listener, &zen, "qwen3.6-plus", messages_body).await;
    assert!(head.starts_with("POST /zen/v1/messages "), "{head}");
    let names = header_names(&head);
    assert!(
        names.contains(&"x-api-key".to_owned()),
        "the gateway answers a bearer here with 401 `Missing API key.`: {names:?}"
    );
    assert!(
        !names.contains(&"authorization".to_owned()),
        "and a bearer beside it is the credential travelling twice: {names:?}"
    );
    assert!(
        names.contains(&"anthropic-version".to_owned()),
        "optional at this gateway, and still what the wire has always sent: {names:?}"
    );

    // 4. The same model id, two ids, two wires. This is the fact that makes the
    //    hint per (provider, model) rather than per model name.
    let head = sent(&listener, &zen, "minimax-m3", CHAT_BODY).await;
    assert!(
        head.starts_with("POST /zen/v1/chat/completions "),
        "minimax-m3 is chat on Zen: {head}"
    );
    let head = sent(&listener, &go, "minimax-m3", messages_body).await;
    assert!(
        head.starts_with("POST /zen/v1/messages "),
        "and Messages on Go — one id apart, same model name: {head}"
    );
    assert!(
        header_names(&head).contains(&"x-api-key".to_owned()),
        "so the header follows the row and not the provider"
    );

    // 5. A transport with no wire is refused by name, and refused *before* a
    //    socket is opened — asserted by there being no connection left to
    //    accept, since every turn above consumed exactly one.
    let refused = zen
        .stream(
            ChatRequest {
                turn_start: 0,
                effort_options: Default::default(),
                model: "gemini-3-pro".to_owned(),
                system: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("this build has no Google wire and will not invent one");
    let ProviderError::Transport(message) = refused else {
        panic!("a request declined before it is made is a transport refusal");
    };
    assert!(message.contains("gemini-3-pro"), "{message}");
    assert!(message.contains("@ai-sdk/google"), "{message}");

    // Nothing connected for the refusal: a further accept would have to wait
    // for a client that never came.
    let idle = tokio::time::timeout(std::time::Duration::from_millis(150), listener.accept()).await;
    assert!(idle.is_err(), "a refused dialect must not reach the network at all");
}
