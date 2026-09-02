//! A stored reasoning part this build cannot read, from the record to the wire.
//!
//! Sealed reasoning is the first thing a transcript holds whose loss changes
//! **what the next request carries** rather than what a screen shows. Every
//! other part is content or bookkeeping: a record that will not decode costs a
//! line nobody could have rendered anyway, and the row stays on disk for the
//! build that can read it. This one costs the model the record of its own
//! thinking while the calls that thinking produced stay in the request — and
//! the whole point of the ruling in `storage.rs` is that this cannot happen
//! quietly.
//!
//! So the drill is one message and one turn, in the order the failure would
//! actually arrive:
//!
//! 1. A session is stored the way a turn stores one — a prompt, then a step
//!    holding sealed thinking and the call it produced.
//! 2. The reasoning row is replaced with a shape this build does not have: the
//!    reserved `reasoning` tag, and a body it cannot decode. That is a build
//!    from the future writing where this one reads.
//! 3. The transcript comes back **whole apart from the state**: the text, the
//!    call and its result are all there, and where the record stood there is a
//!    reasoning part with nothing in it. The loss is a thing the transcript
//!    says rather than an absence.
//! 4. The request built from that transcript carries no reasoning item at all —
//!    not an empty one, and above all not a guess assembled out of a record
//!    this build already failed to parse. A wrong blob is a refused request; a
//!    missing one is a model that thinks again.
//! 5. The row is still byte for byte what the future build wrote.
//!
//! Served over a real loopback socket like every other provider suite here,
//! because what has to be true is about the request that was *actually built*.
//!
//! Mutates `XDG_DATA_HOME` — one test, one binary.

use std::env;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use ganja_core::Storage;
use ganja_core::auth::{self, AuthError, OauthCredential, RefreshOauth};
use ganja_core::protocol::{
    Message, MessageId, MessageTime, Part, PartBody, PartId, REASONING_TAG, Role, ToolState, Usage,
};
use ganja_core::provider::{ChatRequest, Provider as _, ResponsesProvider};
use ganja_core::storage::{SessionId, VERSION};
use secrecy::SecretString;
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// The model the drill runs as: one the seat serves, and one that reasons.
const MODEL: &str = "gpt-5.4";

/// The state the record held before a future build rewrote it. Nothing may put
/// this on the wire — if it appears in a request, something salvaged a field
/// out of a record it could not parse.
const SEALED: &str = "sealed-state-that-must-not-be-guessed";

/// A whole reply, so the turn the drill takes ends.
fn reply() -> String {
    [
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"Rain."}"#,
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":9,"output_tokens":2}}}"#,
    ]
    .join("\n\n")
        + "\n\n"
}

/// A renewal that must never run: the credential the drill stores is live.
struct NeverRenews;

#[async_trait::async_trait]
impl RefreshOauth for NeverRenews {
    async fn refresh(
        &self,
        provider_id: &str,
        _credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        panic!("{provider_id} was renewed although its credential had hours left");
    }
}

/// The one request the endpoint served, as JSON.
async fn serve(seen: Arc<Mutex<Option<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    let address = listener.local_addr().expect("a bound socket has an address");

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };

        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match socket.read(&mut byte).await {
                Ok(0) | Err(_) => return,
                Ok(_) => head.push(byte[0]),
            }
        }
        let length: usize = String::from_utf8_lossy(&head)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())?
            })
            .unwrap_or(0);
        let mut body = vec![0_u8; length];
        if length > 0 && socket.read_exact(&mut body).await.is_err() {
            return;
        }
        *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(String::from_utf8_lossy(&body).into_owned());

        let _ = socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nconnection: close\r\n\
                     content-type: text/event-stream\r\n\r\n{}",
                    reply()
                )
                .as_bytes(),
            )
            .await;
        let _ = socket.flush().await;
    });

    format!("http://{address}")
}

/// The step a turn stored: sealed thinking, then the call it produced.
fn stored_step() -> Message {
    Message {
        id: MessageId::from("msg_1".to_owned()),
        role: Role::Assistant,
        parts: vec![
            Part { id: PartId::from("prt_1".to_owned()), body: PartBody::StepStart },
            Part {
                id: PartId::from("prt_2".to_owned()),
                body: PartBody::Reasoning {
                    provider: "openai".to_owned(),
                    item: "rs_1".to_owned(),
                    encrypted: Some(SEALED.to_owned()),
                },
            },
            Part {
                id: PartId::from("prt_3".to_owned()),
                body: PartBody::Tool {
                    call_id: "call_1".to_owned(),
                    tool: "lookup".to_owned(),
                    state: ToolState::Completed {
                        input: json!({"city": "Paris"}),
                        output: "found it".to_owned(),
                        title: "lookup ran".to_owned(),
                        metadata: json!({}),
                        started: 1,
                        completed: 2,
                    },
                },
            },
            Part {
                id: PartId::from("prt_4".to_owned()),
                body: PartBody::StepFinish { usage: Usage::default() },
            },
        ],
        time: MessageTime { created: 7, completed: Some(9) },
        model: Some(MODEL.to_owned()),
        usage: None,
    }
}

#[tokio::test]
async fn a_stored_reasoning_part_this_build_cannot_read_costs_only_its_continuity() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        env::remove_var("OPENAI_API_KEY");
    }
    let mut credential = OauthCredential::new(
        SecretString::from("rt-downgrade".to_owned()),
        SecretString::from("at-downgrade".to_owned()),
        auth::now_ms() + 86_400_000,
    );
    credential.account_id = Some("acct_downgrade".to_owned());
    auth::set_oauth(auth::openai::PROVIDER_ID, &credential).expect("the credential stores");

    // ---- 1. A session, stored the way a turn stores one. -------------------
    let directory = tempfile::tempdir().expect("a temp directory");
    let storage = Storage::open(directory.path().join("storage"));
    let session = SessionId::from("ses_1".to_owned());
    // Ids are pinned rather than minted, because a transcript comes back in id
    // order and a minted one would decide which message that is by the clock.
    let asked = Message {
        id: MessageId::from("msg_0".to_owned()),
        role: Role::User,
        parts: vec![Part {
            id: PartId::from("prt_0".to_owned()),
            body: PartBody::Text { text: "what is the weather".to_owned() },
        }],
        time: MessageTime { created: 1, completed: Some(1) },
        model: None,
        usage: None,
    };
    for message in [&asked, &stored_step()] {
        storage.save_message(&session, message).expect("the envelope stores");
        for part in &message.parts {
            storage.save_part(&session, &message.id, part).expect("the part stores");
        }
    }

    // Continuity across a process at all is the premise of the rest: a part
    // that did not survive the disk would make every assertion below true for
    // the wrong reason.
    let intact = storage.load_transcript(&session).expect("the transcript loads");
    assert_eq!(
        intact[1].parts[1].body,
        PartBody::Reasoning {
            provider: "openai".to_owned(),
            item: "rs_1".to_owned(),
            encrypted: Some(SEALED.to_owned()),
        },
        "a stored reasoning part comes back whole, or a resumed session could \
         never replay anything"
    );

    // ---- 2. A build from the future rewrites the reasoning row. ------------
    let ahead = json!({
        "version": VERSION,
        "payload": {
            "id": "prt_2",
            // The reserved prefix, which is the whole of what a reader that
            // cannot decode the rest is promised.
            "type": format!("{REASONING_TAG}_v2"),
            "provider": "openai",
            "item": "rs_1",
            "segments": [{"sealed": SEALED, "scheme": "something-later"}],
        },
    })
    .to_string();
    let connection = rusqlite::Connection::open(storage.database()).expect("the database opens");
    connection
        .execute("UPDATE part SET data = ?1 WHERE id = 'prt_2'", rusqlite::params![ahead])
        .expect("the row is replaced");

    // ---- 3. The message survives, minus its continuity. --------------------
    let transcript = storage.load_transcript(&session).expect("the transcript loads");
    let [prompt, step] = transcript.as_slice() else {
        panic!("two messages were stored, got {}", transcript.len());
    };
    assert_eq!(prompt.parts.len(), 1, "the prompt is untouched");
    assert_eq!(
        step.parts.iter().map(|part| part.id.as_str()).collect::<Vec<_>>(),
        vec!["prt_1", "prt_2", "prt_3", "prt_4"],
        "every other part of the step is still there, in place"
    );
    assert_eq!(
        step.parts[1].body,
        PartBody::Reasoning {
            provider: "openai".to_owned(),
            item: "rs_1".to_owned(),
            encrypted: None,
        },
        "the loss belongs in the transcript, where whoever builds the next \
         request can see it — not only in a log line nobody reads"
    );

    // ---- 4. The request built from it invents nothing. ---------------------
    let seen: Arc<Mutex<Option<String>>> = Arc::default();
    let base_url = serve(Arc::clone(&seen)).await;
    let provider = ResponsesProvider::at(&base_url, Arc::new(NeverRenews))
        .expect("loopback may carry a token");
    let streamed: Vec<_> = provider
        .stream(
            ChatRequest {
                turn_start: 0,
                effort_options: Default::default(),
                model: MODEL.to_owned(),
                system: None,
                messages: transcript,
                tools: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("the endpoint answered")
        .collect()
        .await;
    assert!(!streamed.is_empty(), "an answered turn streams something");

    let body = seen
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .expect("the endpoint saw a request");
    assert!(
        !body.contains(SEALED),
        "state was salvaged out of a record this build could not parse; a \
         blob read under an assumption the failure already disproved is a \
         refused request: {body}"
    );

    let sent: serde_json::Value = serde_json::from_str(&body).expect("the body is JSON");
    assert_eq!(
        sent["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "what is the weather"}]},
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": r#"{"city":"Paris"}"#,
            },
            {"type": "function_call_output", "call_id": "call_1", "output": "found it"},
        ]),
        "the conversation is whole and the reasoning item is simply absent — \
         an empty one is what the backend refuses: {sent}"
    );
    assert_eq!(
        sent["include"],
        json!(["reasoning.encrypted_content"]),
        "and the turn still asks for state it *can* keep: {sent}"
    );

    // ---- 5. The record is still the future build's. ------------------------
    let stored: String = connection
        .query_row("SELECT data FROM part WHERE id = 'prt_2'", [], |row| row.get(0))
        .expect("the row reads");
    assert_eq!(
        stored, ahead,
        "reading a record this build cannot decode must never rewrite it: the \
         build that can read it is the only one that still could"
    );
}
