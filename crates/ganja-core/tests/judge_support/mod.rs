//! What the judge's suites (**D567**) share: a TypeSafe double on loopback, a
//! tool double whose output and metadata a test decides, an MCP server
//! double over streamable HTTP, and the two engine helpers every binary
//! driving a turn needs (`turn`, `mcp_engine`).
//!
//! The vendor double answers each request by a policy the test gives it, over
//! what the request carried — the segment's `content`, its `tool`, the
//! `model` it named — and it records every request, counts every connection,
//! and keeps a gauge of requests it holds unanswered, which is how a test
//! sees the judge's concurrency caps from the far side of a real socket.
//! Every answer it gives names `jev-1.13.0` unless the test asks otherwise,
//! because that is the only model the judge fires on.
//!
//! Nothing here reaches the vendor: every judge a suite builds is built by
//! [`Judge::from_settings`] over [`Vendor::settings`], a loopback base.

#![allow(dead_code, reason = "each suite uses a different part of the support")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use ganja_core::judge::{Judge, Screen, Tuning};
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Event, PartBody, ToolState};
use ganja_core::provider::Provider;
use ganja_core::tool::typesafe::Settings;
use ganja_core::tool::{Registry, Tool, ToolCtx, ToolError, ToolOutput};
use ganja_core::{Config, Engine, McpServers};
use ganja_testkit::{drain_allowing, prompt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// The key every suite's judge carries, distinctive so a leak check can look
/// for it by name.
pub const KEY: &str = "sk-judge-suite-key-0123456789";

/// How long any wait here may take before the test calls it a hang.
pub const PATIENCE: Duration = Duration::from_secs(30);

/// Sends `text` and drains the turn, letting every dialog through.
pub async fn turn(
    engine: &Engine,
    events: &mut BoxStream<'static, Event>,
    text: &str,
) -> Vec<Event> {
    engine.send(prompt(text)).await.expect("an idle engine accepts a prompt");

    tokio::time::timeout(PATIENCE, drain_allowing(engine, events)).await.expect("the turn finishes")
}

/// An engine over `provider` with `config`'s MCP servers connected.
pub async fn mcp_engine(provider: Arc<dyn Provider>, config: &Config, judge: Arc<Judge>) -> Engine {
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_mcp(McpServers::new(config.mcp.clone(), std::path::Path::new(".")))
    .with_judge(judge);
    engine.connect_mcp();
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while tokio::time::Instant::now() < deadline && engine.mcp_status().is_empty() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    engine
}

/// What the double answers one request with.
pub enum Reply {
    /// A usable answer from `jev-1.13.0`, fired or not.
    Answer {
        /// Whether all three answers cross the thresholds.
        fire: bool,
    },
    /// A usable answer naming another model.
    Served {
        /// The model the answer says served it.
        model: String,
        /// Whether its answers cross the thresholds.
        fire: bool,
    },
    /// An error status with a small JSON body.
    Status(u16),
    /// A status and a body, exactly.
    Raw {
        /// The status line's code.
        status: u16,
        /// The body.
        body: String,
    },
    /// A 200 declaring a body larger than the client will hold.
    TooLarge,
    /// The inner reply, after a delay.
    After(Duration, Box<Reply>),
    /// The inner reply, once [`Vendor::release`] is called.
    Held(Box<Reply>),
    /// Nothing, ever.
    Never,
}

/// One request the double received.
#[derive(Clone, Debug)]
pub struct Seen {
    /// The body, parsed.
    pub body: Value,
    /// The body's length in bytes.
    pub bytes: usize,
    /// The body as it arrived, before parsing.
    pub raw: String,
    /// `state.content`.
    pub content: String,
    /// `state.tool`.
    pub tool: String,
    /// `model`.
    pub model: String,
    /// The request's head, header lines and all.
    pub head: String,
}

type Policy = Box<dyn Fn(&Seen) -> Reply + Send + Sync>;

struct State {
    policy: Mutex<Policy>,
    seen: Mutex<Vec<Seen>>,
    connections: AtomicUsize,
    open: AtomicUsize,
    peak: AtomicUsize,
    released: tokio::sync::watch::Sender<bool>,
}

/// A TypeSafe double on a loopback port.
#[derive(Clone)]
pub struct Vendor {
    /// The base URL a judge is pointed at.
    pub base: String,
    state: Arc<State>,
}

impl Vendor {
    /// A double answering every request by `policy`.
    pub async fn start(policy: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
        let address = listener.local_addr().expect("a bound socket has an address");
        let (released, _) = tokio::sync::watch::channel(false);
        let state = Arc::new(State {
            policy: Mutex::new(Box::new(policy)),
            seen: Mutex::default(),
            connections: AtomicUsize::new(0),
            open: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            released,
        });

        let accepting = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                accepting.connections.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve(stream, Arc::clone(&accepting)));
            }
        });

        Self { base: format!("http://{address}"), state }
    }

    /// Settings pointing at this double, naming `model` as their default.
    pub fn settings_naming(&self, model: &str) -> Settings {
        Settings::from_parts(KEY.to_owned(), &self.base, model.to_owned())
            .expect("a loopback base and a usable model are accepted")
    }

    /// Settings pointing at this double.
    pub fn settings(&self) -> Settings {
        self.settings_naming("jev-1.13.0")
    }

    /// A judge over this double.
    pub fn judge(&self, screen: Screen, tuning: Tuning) -> Arc<Judge> {
        Judge::from_settings(Some(self.settings()), screen, tuning)
            .expect("settings and a named source build a judge")
    }

    /// Answers every later request by `policy` instead.
    pub fn answer_with(&self, policy: impl Fn(&Seen) -> Reply + Send + Sync + 'static) {
        *self.state.policy.lock().expect("the policy is never poisoned") = Box::new(policy);
    }

    /// Every request received so far, in arrival order.
    pub fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().expect("the log is never poisoned").clone()
    }

    /// How many requests arrived.
    pub fn requests(&self) -> usize {
        self.state.seen.lock().expect("the log is never poisoned").len()
    }

    /// How many connections were opened.
    pub fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    /// Requests received and not yet answered, right now.
    pub fn open(&self) -> usize {
        self.state.open.load(Ordering::SeqCst)
    }

    /// The most requests ever held unanswered at once.
    pub fn peak(&self) -> usize {
        self.state.peak.load(Ordering::SeqCst)
    }

    /// Lets every [`Reply::Held`] request through, now and later.
    pub fn release(&self) {
        self.state.released.send_replace(true);
    }

    /// Waits until at least `count` requests have arrived, or panics after
    /// `patience`.
    pub async fn wait_for_requests(&self, count: usize, patience: Duration) {
        let deadline = tokio::time::Instant::now() + patience;
        while self.requests() < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "waited {patience:?} for {count} requests and saw {}",
                self.requests()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// A usable answer body from `model`.
pub fn answer(model: &str, fire: bool) -> String {
    let (addresses, requests, instructs, choice) =
        if fire { (0.91, 0.83, 0.87, "instructs_reader") } else { (0.07, 0.21, 0.04, "informs") };

    json!({
        "model": model,
        "answers": {
            "addresses_agent": { "type": "noul", "noul": addresses },
            "requests_action": { "type": "noul", "noul": requests },
            "stance": {
                "type": "choice",
                "choice": choice,
                "probabilities": {
                    "informs": 1.0 - instructs - 0.05,
                    "discusses_instructions": 0.05,
                    "instructs_reader": instructs,
                },
                "confidence": 0.8,
            },
        },
        "usage": { "input_tokens": 120, "output_tokens": 3 },
    })
    .to_string()
}

/// One connection, kept alive across as many requests as the client sends.
async fn serve(mut stream: TcpStream, state: Arc<State>) {
    let mut buffer = Vec::new();
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        let (head, body) = loop {
            if let Some(request) = whole(&buffer) {
                break request;
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        buffer.drain(..head.len() + 4 + body.len());

        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let text = |pointer: &str| {
            parsed.pointer(pointer).and_then(Value::as_str).unwrap_or_default().to_owned()
        };
        let seen = Seen {
            content: text("/state/content"),
            tool: text("/state/tool"),
            model: text("/model"),
            bytes: body.len(),
            raw: String::from_utf8_lossy(&body).into_owned(),
            body: parsed.clone(),
            head,
        };
        let reply = (state.policy.lock().expect("the policy is never poisoned"))(&seen);
        state.seen.lock().expect("the log is never poisoned").push(seen);

        let now = state.open.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak.fetch_max(now, Ordering::SeqCst);
        let response = respond(reply, &state).await;
        state.open.fetch_sub(1, Ordering::SeqCst);

        if stream.write_all(&response).await.is_err() {
            return;
        }
        let _ = stream.flush().await;
    }
}

/// The bytes one reply puts on the wire, after whatever wait it asks for.
async fn respond(reply: Reply, state: &State) -> Vec<u8> {
    let mut reply = reply;
    loop {
        reply = match reply {
            Reply::After(delay, inner) => {
                tokio::time::sleep(delay).await;
                *inner
            }
            Reply::Held(inner) => {
                let mut released = state.released.subscribe();
                let _ = released.wait_for(|released| *released).await;
                *inner
            }
            Reply::Never => std::future::pending().await,
            Reply::Answer { fire } => return http(200, &answer("jev-1.13.0", fire)),
            Reply::Served { model, fire } => return http(200, &answer(&model, fire)),
            Reply::Status(status) => return http(status, r#"{"detail":"refused by the double"}"#),
            Reply::Raw { status, body } => return http(status, &body),
            Reply::TooLarge => {
                return b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2000000\r\n\r\n"
                    .to_vec();
            }
        };
    }
}

fn http(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// One whole request out of `buffer` — its head and its body — or [`None`]
/// while the bytes are still arriving.
fn whole(buffer: &[u8]) -> Option<(String, Vec<u8>)> {
    let end = buffer.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buffer[..end]).ok()?.to_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let body = buffer.get(end + 4..end + 4 + length)?.to_vec();

    Some((head, body))
}

/// A screen naming exactly `webfetch`.
pub fn webfetch_only() -> Screen {
    Screen { webfetch: true, ..Screen::default() }
}

/// A screen naming exactly `websearch`.
pub fn websearch_only() -> Screen {
    Screen { websearch: true, ..Screen::default() }
}

/// A screen naming exactly these MCP servers.
pub fn mcp_only(servers: &[&str]) -> Screen {
    Screen { mcp: servers.iter().map(|server| (*server).to_owned()).collect(), ..Screen::default() }
}

/// Tuning a suite can wait out.
pub fn tuning(deadline_ms: u64, failures: u32, cooldown_ms: u64) -> Tuning {
    Tuning {
        deadline: Duration::from_millis(deadline_ms),
        failures,
        cooldown: Duration::from_millis(cooldown_ms),
    }
}

/// A `webfetch` result as the shipped tool stamps a public page.
pub fn fetched(text: &str) -> ToolOutput {
    ToolOutput {
        title: "https://example.test/page (text/html)".to_owned(),
        output: text.to_owned(),
        metadata: json!({ "private_allowed": false, "truncated": false }),
    }
}

/// A result of `tool` with this `metadata`.
pub fn result(title: &str, text: &str, metadata: Value) -> ToolOutput {
    ToolOutput { title: title.to_owned(), output: text.to_owned(), metadata }
}

/// A page of `blocks` blocks separated by blank lines, each carrying `marker`
/// and its index, and each large enough to be a segment of its own.
pub fn page(marker: &str, blocks: usize) -> String {
    (0..blocks).map(|index| format!("{marker} block {index:03} {}\n\n", "p".repeat(2100))).collect()
}

/// A tool double that answers under whatever id it is given, building its
/// output from the call's arguments.
pub struct Stub {
    id: String,
    make: Box<dyn Fn(&Value) -> ToolOutput + Send + Sync>,
}

impl Stub {
    /// A tool named `id` answering every call with `make(args)`.
    pub fn new(id: &str, make: impl Fn(&Value) -> ToolOutput + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { id: id.to_owned(), make: Box::new(make) })
    }

    /// A tool named `id` answering every call with `output`.
    pub fn fixed(id: &str, output: ToolOutput) -> Arc<Self> {
        Self::new(id, move |_| output.clone())
    }
}

#[async_trait]
impl Tool for Stub {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        "answers with whatever the test decided"
    }

    fn schema(&self) -> schemars::Schema {
        ganja_testkit::placeholder_schema()
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        Ok((self.make)(&args))
    }
}

/// The final state of every tool part `seen` carries for `tool`: its title,
/// output and metadata, in the order they completed.
pub fn completed(seen: &[Event], tool: &str) -> Vec<(String, String, Value)> {
    let mut last: Vec<(String, (String, String, Value))> = Vec::new();
    for event in seen {
        if let Event::PartUpdated { part, .. } = event
            && let PartBody::Tool {
                tool: name,
                state: ToolState::Completed { title, output, metadata, .. },
                ..
            } = &part.body
            && name == tool
        {
            let entry = (title.clone(), output.clone(), metadata.clone());
            match last.iter_mut().find(|(id, _)| id.as_str() == part.id.as_str()) {
                Some(slot) => slot.1 = entry,
                None => last.push((part.id.as_str().to_owned(), entry)),
            }
        }
    }

    last.into_iter().map(|(_, entry)| entry).collect()
}

/// An MCP server double over streamable HTTP, lending one tool, `fetch`,
/// whose text `answer` builds from the call's arguments.
pub async fn mcp_server(answer: impl Fn(&Value) -> String + Send + Sync + 'static) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port is available");
    let address = listener.local_addr().expect("the socket has an address");
    let answer: Arc<dyn Fn(&Value) -> String + Send + Sync> = Arc::new(answer);

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let answer = Arc::clone(&answer);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = vec![0_u8; 16 * 1024];
                loop {
                    let (head, body) = loop {
                        if let Some(request) = whole(&buffer) {
                            break request;
                        }
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                    };
                    buffer.drain(..head.len() + 4 + body.len());

                    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let response = match rpc(&request, answer.as_ref()) {
                        Some(result) => {
                            let body = result.to_string();
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                 content-length: {}\r\n\r\n{body}",
                                body.len()
                            )
                        }
                        None => "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_owned(),
                    };
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = stream.flush().await;
                }
            });
        }
    });

    address
}

/// What the MCP double answers one JSON-RPC request with, or [`None`] for a
/// notification.
fn rpc(request: &Value, answer: &(dyn Fn(&Value) -> String + Send + Sync)) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request.get("method")?.as_str()?;
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "hub", "version": "0.0.0" },
        }),
        "tools/list" => json!({
            "tools": [{
                "name": "fetch",
                "description": "Answers whatever the test decided.",
                "inputSchema": { "type": "object", "properties": { "kind": { "type": "string" } } },
            }],
        }),
        "tools/call" => {
            let arguments = request.pointer("/params/arguments").cloned().unwrap_or(Value::Null);
            json!({ "content": [{ "type": "text", "text": answer(&arguments) }] })
        }
        _ => json!({}),
    };

    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}
