//! A loopback endpoint that answers Responses requests and records them.
//!
//! Hoisted out of `ganja-core/tests/responses_wire.rs` for **D563**, whose
//! headless suites need the same recorder: what they assert on is the request
//! body a real provider built and a real socket carried, not a request value
//! read off a fake. Nothing here parses what it serves — a phase sets the
//! event-stream body, and every request is answered with it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// One request the endpoint was asked to serve.
#[derive(Clone, Debug)]
pub struct Recorded {
    /// Request line and headers, verbatim.
    pub head: String,
    /// The body, for a request that had one.
    pub body: String,
}

impl Recorded {
    /// The path asked for, which is what tells the two wires apart.
    pub fn path(&self) -> &str {
        self.head
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .split('?')
            .next()
            .unwrap_or_default()
    }

    /// The value of `name`, compared case-insensitively the way a header name
    /// is. [`None`] where the request did not carry it at all, which is a
    /// different answer from carrying it empty.
    pub fn header(&self, name: &str) -> Option<String> {
        let prefix = format!("{name}:");

        self.head.lines().find_map(|line| {
            let (found, value) = line.split_once(':')?;
            found
                .trim()
                .eq_ignore_ascii_case(prefix.trim_end_matches(':'))
                .then(|| value.trim().to_owned())
        })
    }

    /// The body as JSON, for the phases that assert on the whole request.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("the body should be JSON ({error}): {}", self.body))
    }
}

/// Everything the server task and the test both hold.
struct State {
    seen: Mutex<Vec<Recorded>>,
    reply: Mutex<String>,
    /// Bodies for the next requests, oldest first, ahead of [`State::reply`].
    ///
    /// A turn that calls a tool is two requests, and the second one cannot be
    /// answered with the first one's body — a reply that calls the tool again
    /// is a loop with no end.
    scripted: Mutex<VecDeque<String>>,
}

/// A loopback endpoint serving whatever the current phase set.
pub struct Endpoint {
    /// What a provider is pointed at.
    pub base_url: String,
    state: Arc<State>,
    /// Kept so the server outlives the test talking to it.
    _server: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    /// Every request served so far, oldest first.
    pub fn seen(&self) -> Vec<Recorded> {
        self.state.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// The one request this phase produced.
    pub fn only(&self) -> Recorded {
        let seen = self.seen();
        let [request] = seen.as_slice() else {
            panic!("one turn is one request, got {}", seen.len());
        };

        request.clone()
    }

    /// Forgets what has been served, so a phase counts only its own traffic.
    pub fn forget(&self) {
        self.state.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }

    /// Sets the event-stream body every turn is answered with from now on.
    pub fn answers_turns_with(&self, body: impl Into<String>) {
        *self.state.reply.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = body.into();
    }

    /// Queues one body per request, consumed in order before the standing one.
    pub fn answers_the_next_requests_with(&self, bodies: impl IntoIterator<Item = String>) {
        *self.state.scripted.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            bodies.into_iter().collect();
    }
}

/// Starts an endpoint that answers every connection for as long as the test
/// holds it.
pub async fn serve() -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    let address = listener.local_addr().expect("a bound socket has an address");
    let state = Arc::new(State {
        seen: Mutex::new(Vec::new()),
        reply: Mutex::new(responses_transcript()),
        scripted: Mutex::new(VecDeque::new()),
    });

    let served = Arc::clone(&state);
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let state = Arc::clone(&served);

            tokio::spawn(async move {
                let Some(request) = read_request(&mut socket).await else {
                    return;
                };
                let body = state
                    .scripted
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pop_front()
                    .unwrap_or_else(|| {
                        state.reply.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
                    });
                state.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(request);

                let _ = socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nconnection: close\r\n\
                             content-type: text/event-stream\r\n\r\n{body}"
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = socket.flush().await;
                // Dropping the socket ends a close-delimited body.
            });
        }
    });

    Endpoint { base_url: format!("http://{address}/backend-api/codex"), state, _server: server }
}

/// Reads one whole request: head to the blank line, then whatever
/// `content-length` promised.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Recorded> {
    let mut buffer = Vec::new();
    let mut byte = [0_u8; 1];

    while !buffer.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => buffer.push(byte[0]),
        }
    }
    let head = String::from_utf8_lossy(&buffer).into_owned();

    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim().eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    if length > 0 && socket.read_exact(&mut body).await.is_err() {
        return None;
    }

    Some(Recorded { head, body: String::from_utf8_lossy(&body).into_owned() })
}

/// A whole Responses turn: a thought, two fragments of reply, and the bill.
pub fn responses_transcript() -> String {
    [
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6"}}"#,
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"Short is right."}"#,
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}"#,
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"Hello, "}"#,
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"world!"}"#,
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":42,"input_tokens_details":{"cached_tokens":16},"output_tokens":9,"output_tokens_details":{"reasoning_tokens":4}}}}"#,
    ]
    .join("\n\n")
        + "\n\n"
}
