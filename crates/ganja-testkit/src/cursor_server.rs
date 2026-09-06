//! A loopback cursor agent backend that hosts the Run RPC as a **real
//! duplex**, so a whole bridged turn runs end to end against a real
//! [`Engine`](ganja_core::Engine) (**D552**, W4).
//!
//! # What this is for, and why it is here rather than in `ganja-provider`
//!
//! `crates/ganja-provider/tests/cursor_wire.rs` already serves real bytes over
//! loopback, and its own header says what it deliberately does not: it
//! de-chunks *exactly one* Connect envelope — the run request — and never
//! hosts the duplex, because the wire crate's suites drive the ask-answer
//! paths through in-memory channels instead. That is the right shape for
//! testing a wire in isolation, and the wrong one for the bridge: a
//! `mcp_exec` is answered by the **engine** — permission dialogs, the tool
//! registry, the transcript — so the only party that can prove the round trip
//! is the one that can drive both ends at once. `ganja-core` is that party,
//! `ganja-testkit` is where its fixtures live, and this is the fixture.
//!
//! # What it hosts
//!
//! - `GetUsableModels` — a fixed two-entry listing, so a wire that lists
//!   before it runs is answered rather than hung.
//! - `Run` — headers and the first frame go out immediately; from then on the
//!   socket is split and driven by a [`Script`] the test hands in. Each
//!   Connect frame is one HTTP chunk, flushed, and the request body is
//!   de-chunked incrementally *while the response is open*. That the
//!   transport permits this is not assumed: it is measured by
//!   `crates/ganja-core/tests/cursor_bridge.rs`'s first test, over the same
//!   reqwest/hyper stack the wire uses.
//!
//! A [`Step`] that asks — a context exec, an `mcp_args` exec, any other exec —
//! **waits** for the client's answer on the still-open request body before the
//! script moves on, and records it decoded. Answers are matched by `id` rather
//! than by arrival order, because the live server issues concurrent execs (two
//! `grep_args` within 5 ms in the recording at
//! `crates/ganja-provider/tests/fixtures/cursor-mcp-tools-probe.txt`) and a
//! bridge that assumed one outstanding exec at a time would be wrong about the
//! thing this fixture exists to prove. [`Step::Batch`] is what actually puts
//! two in flight at once; a lone [`Step::Exec`] settles before the next frame
//! is written, so the id-matching has nothing to disambiguate.
//!
//! `client_heartbeat = 7` frames are counted rather than recorded, so a test
//! can assert the run-level cadence without every other assertion having to
//! step over them.
//!
//! # What it is not
//!
//! Not a cursor emulator. It replays scripts a test wrote; it has no model, no
//! agent loop, and no opinion about what a good answer looks like. Where its
//! behaviour and the recorded fixture disagree, the fixture is right and this
//! file is a bug.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buffa::{Message as _, MessageField};
use ganja_core::provider::cursor::{proto, value};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::Notify;

/// The Connect EndStream flag, as the live probe recorded it.
const END_STREAM: u8 = 0b0000_0010;

/// How long one step may wait for the answer it asked for.
///
/// Generous because CI machines stall, and reached only when the client never
/// answers at all — in which case the test's own failure message is what
/// matters, not the wait.
const PATIENCE: Duration = Duration::from_secs(20);

/// One thing the server does on an open Run stream.
///
/// The asking variants block the script until the client answers on the
/// request body; the rest are written and moved past.
pub enum Step {
    /// `request_context_args = 10` — the context ask, whose `ExecRequest.id`
    /// is absent on the wire (measured; the recording's incidental findings).
    /// Waits for the `request_context_result`, records its `RequestContext` —
    /// the declared tool roster included — and expects no `stream_close`.
    Context,
    /// One exec, handed whole so a test can ask for any kind without this
    /// module growing an arm per kind. Waits for `responses` matching
    /// `ExecResponse` frames and then the `stream_close` that ends every exec,
    /// refused or served.
    Exec {
        /// What to write. `id` is the correlation key; give each exec its own.
        ///
        /// Boxed: an `ExecRequest` carries every kind's args inline, so the
        /// bare variant is 856 bytes and every other `Step` — a `String`, a
        /// unit — pays for it (`clippy::large_enum_variant`).
        request: Box<proto::ExecRequest>,
        /// How many `ExecResponse` frames this kind answers with. One for
        /// every kind but `shell_stream`, which is several.
        responses: usize,
    },
    /// Several execs written **back-to-back**, then answered in whatever order
    /// the client answers them.
    ///
    /// The live server issues concurrent execs — two `grep_args` within 5 ms in
    /// the recording — so a bridge that assumed one outstanding exec at a time
    /// would be wrong about the thing this fixture exists to prove. A
    /// sequential [`Step::Exec`] cannot replay that: it blocks on its own
    /// answer before the next frame is written, so nothing is ever in flight
    /// twice and `Inbox`'s id-matching never has two candidates to choose
    /// between. This is the step that does.
    ///
    /// Built by [`Step::batch`], which is what fixes each member's response
    /// count.
    Batch(Vec<(Box<proto::ExecRequest>, usize)>),
    /// A `text_delta`.
    Text(String),
    /// `turn_ended = 14`.
    TurnEnded,
    /// The Connect EndStream frame that closes the response, cleanly.
    ///
    /// No failure payload: an in-body Connect verdict is a *wire* fact rather
    /// than a bridge one, and it is measured where it can be measured against
    /// the redaction that has to survive it — `crates/ganja-core/tests/secrets_env.rs`'s
    /// cursor arm, over a real socket, on both of that wire's failure paths.
    EndStream,
}

impl Step {
    /// An `mcp_args` exec for `tool_name`, with `args` as a real argument
    /// object.
    ///
    /// `provider_identifier` is `"ganja"`, which is what the live server sends
    /// back for a tool this client declared (the recording's (a)). A test
    /// proving the foreign-server refusal passes its own identifier through
    /// [`Step::mcp_from`].
    #[must_use]
    pub fn mcp(id: u32, tool_name: &str, args: &serde_json::Value) -> Self {
        Self::mcp_from(id, tool_name, args, "ganja")
    }

    /// The same, under an arbitrary `provider_identifier`.
    #[must_use]
    pub fn mcp_from(
        id: u32,
        tool_name: &str,
        args: &serde_json::Value,
        provider_identifier: &str,
    ) -> Self {
        Self::Exec {
            request: Box::new(proto::ExecRequest {
                id: Some(id),
                mcp_args: MessageField::some(proto::McpArgs {
                    // Every field the recording saw present on a real
                    // `mcp_args`: a `server_identifier` is *normal*, not a
                    // sign of a foreign server.
                    name: Some(tool_name.to_owned()),
                    args: json_entries(args),
                    tool_call_id: Some(format!("toolu_{id}")),
                    provider_identifier: Some(provider_identifier.to_owned()),
                    tool_name: Some(tool_name.to_owned()),
                    server_identifier: Some("ganja".to_owned()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            responses: 1,
        }
    }

    /// A native `read_args = 7` exec, the kind AC-17 proves the execution site
    /// of on the path that never touches the mcp channel.
    #[must_use]
    pub fn read(id: u32, path: &str) -> Self {
        Self::Exec {
            request: Box::new(proto::ExecRequest {
                id: Some(id),
                read_args: MessageField::some(proto::ReadArgs {
                    path: Some(path.to_owned()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            responses: 1,
        }
    }

    /// Sets the one flag **Dv-3** is about: `smart_mode_approval_only = 7`,
    /// which the client answers `approved` to without executing anything.
    ///
    /// A no-op on a step that is not an `mcp_args` exec — the flag has no
    /// meaning on any other kind, and a builder that panicked about it would
    /// be a fixture with an opinion.
    #[must_use]
    pub fn approval_only(mut self) -> Self {
        if let Self::Exec { request, .. } = &mut self
            && let Some(args) = request.mcp_args.as_option_mut()
        {
            args.smart_mode_approval_only = Some(true);
        }

        self
    }

    /// Several execs as one [`Step::Batch`], written together.
    ///
    /// Takes whole [`Step::Exec`]s rather than bare requests so a batch is
    /// spelled with the same builders a lone exec is — `Step::batch(vec![
    /// Step::mcp(1, ..), Step::read(2, ..)])` — and so each member keeps its
    /// own response count.
    ///
    /// # Panics
    ///
    /// A member that is not an exec, which is a fixture bug rather than a
    /// server behaviour: nothing else has an `id` to match an answer by, and
    /// writing a `text_delta` inside a batch is just writing it before one.
    #[must_use]
    pub fn batch(steps: Vec<Self>) -> Self {
        Self::Batch(
            steps
                .into_iter()
                .map(|step| match step {
                    Self::Exec { request, responses } => (request, responses),
                    _ => panic!("a batch holds execs; every other step is written on its own"),
                })
                .collect(),
        )
    }
}

/// What one Run stream does, in order.
///
/// A script that never reaches its [`Step::EndStream`] leaves the response
/// open, which is a real state a turn can be in and one a cancellation test
/// wants; every other script should end with one.
#[derive(Default)]
pub struct Script(Vec<Step>);

impl Script {
    /// An empty script: headers, then nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `step`.
    #[must_use]
    pub fn then(mut self, step: Step) -> Self {
        self.0.push(step);

        self
    }

    /// The shortest complete turn: nothing said, cleanly ended. What a Run
    /// past the end of a test's own scripts is served — the title one-shot a
    /// finished turn earns, most often.
    #[must_use]
    pub fn finished() -> Self {
        Self::new().then(Step::TurnEnded).then(Step::EndStream)
    }
}

/// One thing the client sent, decoded.
///
/// `client_heartbeat` frames are counted in [`CursorServer::heartbeats`]
/// rather than landing here, and everything else a client writes — kv answers,
/// the `stream_close` that ends each exec — reaches `Inbox`, where the steps
/// waiting on it can claim it. Only what a test *asserts about* is recorded,
/// so a variant here is a variant some accessor reads.
#[derive(Clone, Debug)]
pub enum Recorded {
    /// The opening frame of a Run, with the declared tool roster on it.
    RunRequest(Box<proto::RunRequest>),
    /// An exec answer of any kind.
    ExecResponse(Box<proto::ExecResponse>),
}

/// The endpoint, and everything it saw.
pub struct CursorServer {
    base_url: String,
    state: Arc<State>,
    /// The accept loop, ended when this server is dropped. Held rather than
    /// detached because a test binary that runs several of these — a plain
    /// `cargo test`, where nextest's process-per-test does not apply — would
    /// otherwise leave every one of them bound and accepting for the life of
    /// the process.
    accepting: tokio::task::JoinHandle<()>,
}

impl Drop for CursorServer {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

/// Shared between the accept loop and every reader task.
#[derive(Default)]
struct State {
    /// Everything decoded, in arrival order.
    recorded: Mutex<Vec<Recorded>>,
    /// `client_heartbeat = 7` frames, counted rather than recorded.
    heartbeats: Mutex<usize>,
    /// How many Run streams have been opened, which is the fact the no-drop
    /// test reads: a title one-shot is a second Run, not a second turn.
    runs: Mutex<usize>,
}

impl CursorServer {
    /// Starts a server whose one Run stream runs `script`.
    ///
    /// Later Runs — the title one-shot a finished turn earns — are served
    /// [`Script::finished`].
    pub async fn start(script: Script) -> Self {
        Self::with_scripts(vec![script]).await
    }

    /// The same, with one script per Run in order.
    pub async fn with_scripts(scripts: Vec<Script>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback binds");
        let address = listener.local_addr().expect("the bound port is readable");
        let state = Arc::new(State::default());
        let served = Arc::new(Mutex::new(std::collections::VecDeque::from(scripts)));

        let recording = Arc::clone(&state);
        let accepting = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&recording);
                let served = Arc::clone(&served);
                tokio::spawn(async move { serve_one(socket, &state, &served).await });
            }
        });

        Self { base_url: format!("http://{address}"), state, accepting }
    }

    /// Where a wire is pointed at this server.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Everything the client sent, in order.
    #[must_use]
    pub fn recorded(&self) -> Vec<Recorded> {
        self.state.recorded.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Every `mcp_result` the client answered with, in order — the shape
    /// AC-12, AC-13, AC-14 and AC-22 each read one field of.
    #[must_use]
    pub fn mcp_results(&self) -> Vec<proto::McpResult> {
        self.recorded()
            .into_iter()
            .filter_map(|entry| match entry {
                Recorded::ExecResponse(response) => response.mcp_result.into_option(),
                _ => None,
            })
            .collect()
    }

    /// Every `read_result` the client answered with, for the native half of
    /// AC-17.
    #[must_use]
    pub fn read_results(&self) -> Vec<proto::ReadResult> {
        self.recorded()
            .into_iter()
            .filter_map(|entry| match entry {
                Recorded::ExecResponse(response) => response.read_result.into_option(),
                _ => None,
            })
            .collect()
    }

    /// Every `RequestContext` the client answered a context ask with — where
    /// the declared tool roster arrives.
    #[must_use]
    pub fn context_answers(&self) -> Vec<proto::RequestContext> {
        self.recorded()
            .into_iter()
            .filter_map(|entry| match entry {
                Recorded::ExecResponse(response) => response
                    .request_context_result
                    .into_option()
                    .and_then(|result| result.success.into_option())
                    .and_then(|success| success.request_context.into_option()),
                _ => None,
            })
            .collect()
    }

    /// The tool roster declared on the opening frame of every Run, in order —
    /// one entry per Run, empty for a Run that declared none.
    #[must_use]
    pub fn declared_rosters(&self) -> Vec<Vec<proto::McpToolDefinition>> {
        self.recorded()
            .into_iter()
            .filter_map(|entry| match entry {
                Recorded::RunRequest(run) => Some(
                    run.mcp_tools.into_option().map(|tools| tools.mcp_tools).unwrap_or_default(),
                ),
                _ => None,
            })
            .collect()
    }

    /// How many `client_heartbeat` frames arrived, which is how a test asserts
    /// the run-level cadence without stepping over them everywhere else.
    #[must_use]
    pub fn heartbeats(&self) -> usize {
        *self.state.heartbeats.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many Run streams were opened. Two, on a turn that earned a title.
    #[must_use]
    pub fn runs(&self) -> usize {
        *self.state.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One connection: read the head, route by path.
async fn serve_one(
    mut socket: tokio::net::TcpStream,
    state: &Arc<State>,
    served: &Mutex<std::collections::VecDeque<Script>>,
) {
    let Some(head) = read_head(&mut socket).await else {
        return;
    };

    if head.contains("/GetUsableModels") {
        serve_models(&mut socket, &head).await;

        return;
    }
    if head.contains("/Run") {
        // The whole fixture reads the request body as HTTP chunked transfer
        // encoding, because that is what a body of unknown length is framed as
        // and a held-open Run body has no length. Nothing checks that
        // downstream: `next_chunk` would read a protobuf byte as the front of a
        // hex size line, fail to parse it, and return `None` for the rest of
        // the connection — so a client that switched framing would present as
        // every asking step timing out at once, twenty seconds apart, with
        // nothing in any message naming the cause. This is one line, and it
        // turns that into a sentence.
        if !chunked(&head) {
            let complaint = "this fixture de-chunks the request body, and this Run's head \
                             declared no `transfer-encoding: chunked`";
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\
                         content-type: text/plain\r\ncontent-length: {}\r\n\r\n{complaint}",
                        complaint.len(),
                    )
                    .as_bytes(),
                )
                .await;
            let _ = socket.flush().await;

            return;
        }

        let script = {
            let mut queue = served.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            queue.pop_front().unwrap_or_else(Script::finished)
        };
        *state.runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;
        serve_run(socket, script, state).await;
    }
}

/// The unary listing: two entries, one of them the `default` id cursor's own
/// wire publishes.
async fn serve_models(socket: &mut tokio::net::TcpStream, head: &str) {
    // Read and discard the request body so the client's write completes.
    if let Some(length) = content_length(head)
        && length > 0
    {
        let mut body = vec![0_u8; length];
        let _ = socket.read_exact(&mut body).await;
    }

    let listing = proto::GetUsableModelsResponse {
        models: vec![
            proto::ModelEntry {
                model_id: Some("default".to_owned()),
                display_model_id: Some("auto".to_owned()),
                display_name: Some("Auto".to_owned()),
                ..Default::default()
            },
            proto::ModelEntry {
                model_id: Some("gpt-5.3-codex".to_owned()),
                display_model_id: Some("gpt-5.3-codex".to_owned()),
                display_name: Some("Codex 5.3".to_owned()),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
    .encode_to_vec();

    let _ = socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: application/proto\r\n\
                 content-length: {}\r\n\r\n",
                listing.len(),
            )
            .as_bytes(),
        )
        .await;
    let _ = socket.write_all(&listing).await;
    let _ = socket.flush().await;
}

/// The Run stream: answer the head at once, then split and drive the script
/// against a reader that never stops draining the request body.
async fn serve_run(socket: tokio::net::TcpStream, script: Script, state: &Arc<State>) {
    let (reader, mut writer) = tokio::io::split(socket);
    let inbox = Arc::new(Inbox::default());

    let draining = Arc::clone(&inbox);
    let recording = Arc::clone(state);
    let reading = tokio::spawn(async move { drain_body(reader, &draining, &recording).await });

    if writer
        .write_all(
            b"HTTP/1.1 200 OK\r\n\
              content-type: application/connect+proto\r\n\
              transfer-encoding: chunked\r\n\r\n",
        )
        .await
        .is_err()
    {
        return;
    }
    let _ = writer.flush().await;

    // A Run begins with the client's opening frame, and the tool roster a test
    // reads through `declared_rosters` rides on it. Waiting for it here is what
    // makes ending the reader below safe: a script with no asking step at all —
    // `Script::finished`, which is what the title one-shot gets — would
    // otherwise write its whole response and close the reader before that frame
    // had been decoded, and the roster would go missing on a race rather than
    // on a behaviour.
    if inbox.claim("run request", |message| message.run_request.is_set()).await.is_err() {
        return;
    }

    for step in script.0 {
        if run_step(step, &mut writer, &inbox).await.is_err() {
            break;
        }
    }

    // The terminal chunk, written whatever happened — after the last step, and
    // after a step that gave up waiting. A script that ended without ending its
    // response body leaves the client waiting on one more chunk forever, which
    // turns every fixture bug into a hung test instead of a failed one: the
    // 20-second per-step timeout above is only readable if the turn it belongs
    // to can actually finish.
    let _ = writer.write_all(b"0\r\n\r\n").await;
    let _ = writer.flush().await;

    // And now end the reader, so both halves drop and the client's connection
    // gets a FIN rather than being held half-open until the process exits.
    // Safe here and only here: every asking step claimed its answers before the
    // script advanced, so nothing a test asserts about arrives after this
    // point.
    reading.abort();
}

/// One step, written and — where it asks — waited on.
async fn run_step(
    step: Step,
    writer: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
    inbox: &Inbox,
) -> std::io::Result<()> {
    match step {
        Step::Context => {
            write_server_message(
                writer,
                proto::ServerMessage {
                    exec_request: MessageField::some(proto::ExecRequest {
                        // No `id`: the live context ask carries none.
                        request_context_args: MessageField::some(proto::ContextArgs::default()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await?;
            inbox
                .claim("context answer", |message| {
                    message
                        .exec_response
                        .as_option()
                        .is_some_and(|answer| answer.request_context_result.is_set())
                })
                .await?;
        }
        Step::Exec { request, responses } => {
            let id = write_exec(writer, *request).await?;
            settle_exec(inbox, id, responses).await?;
        }
        Step::Batch(execs) => {
            // Every request first, so they really are in flight together...
            let mut outstanding = Vec::with_capacity(execs.len());
            for (request, responses) in execs {
                outstanding.push((write_exec(writer, *request).await?, responses));
            }
            // ...and only then the answers, which `Inbox` matches by id, so the
            // client may answer them in either order.
            for (id, responses) in outstanding {
                settle_exec(inbox, id, responses).await?;
            }
        }
        Step::Text(text) => {
            write_update(
                writer,
                proto::Update {
                    text_delta: MessageField::some(proto::TextDelta {
                        text: Some(text),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await?;
        }
        Step::TurnEnded => {
            write_update(
                writer,
                proto::Update {
                    turn_ended: MessageField::some(proto::TurnEnded::default()),
                    ..Default::default()
                },
            )
            .await?;
        }
        Step::EndStream => {
            let body =
                serde_json::to_vec(&serde_json::json!({})).expect("a JSON object serializes");
            write_chunk(writer, &envelope(END_STREAM, &body)).await?;
        }
    }

    Ok(())
}

/// Writes one exec and hands back the `id` its answers will carry.
async fn write_exec(
    writer: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
    request: proto::ExecRequest,
) -> std::io::Result<Option<u32>> {
    let id = request.id;
    write_server_message(
        writer,
        proto::ServerMessage { exec_request: MessageField::some(request), ..Default::default() },
    )
    .await?;

    Ok(id)
}

/// Waits for one exec's answers and the `stream_close` that ends it, refused
/// or served.
async fn settle_exec(inbox: &Inbox, id: Option<u32>, responses: usize) -> std::io::Result<()> {
    for _ in 0..responses {
        inbox
            .claim("exec answer", |message| {
                message.exec_response.as_option().is_some_and(|answer| answer.id == id)
            })
            .await?;
    }
    inbox
        .claim("exec stream_close", |message| {
            message.exec_control.as_option().is_some_and(|control| {
                control.stream_close.as_option().is_some_and(|close| close.id == id)
                    || control.throw.as_option().is_some_and(|thrown| thrown.id == id)
            })
        })
        .await?;

    Ok(())
}

/// Writes one `ServerMessage` as one Connect frame in one HTTP chunk.
async fn write_server_message(
    writer: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
    message: proto::ServerMessage,
) -> std::io::Result<()> {
    write_chunk(writer, &envelope(0, &message.encode_to_vec())).await
}

/// The same for an `interaction_update`, which is most of what a turn is.
async fn write_update(
    writer: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
    update: proto::Update,
) -> std::io::Result<()> {
    write_server_message(
        writer,
        proto::ServerMessage {
            interaction_update: MessageField::some(update),
            ..Default::default()
        },
    )
    .await
}

/// One HTTP chunk, flushed: an unflushed frame is a frame the client cannot
/// answer, which would deadlock every asking step.
async fn write_chunk(
    writer: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
    bytes: &[u8],
) -> std::io::Result<()> {
    writer.write_all(format!("{:x}\r\n", bytes.len()).as_bytes()).await?;
    writer.write_all(bytes).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await
}

/// The Connect envelope: one flag byte, a big-endian length, the payload.
fn envelope(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(5 + payload.len());
    framed.push(flags);
    framed.extend_from_slice(
        &u32::try_from(payload.len()).expect("a fixture frame is far under 4 GiB").to_be_bytes(),
    );
    framed.extend_from_slice(payload);

    framed
}

/// Where a reader task puts what it decoded, and how a step reaches it.
#[derive(Default)]
struct Inbox {
    /// Every `ClientMessage` seen, with the ones a step has already consumed
    /// marked — so two concurrent execs answered out of order each find their
    /// own.
    seen: Mutex<Vec<proto::ClientMessage>>,
    claimed: Mutex<HashSet<usize>>,
    arrived: Notify,
}

impl Inbox {
    /// Waits for the first unclaimed message `matches` accepts, and claims it.
    ///
    /// Registering interest *before* re-checking is what makes this safe
    /// against an answer that lands between the check and the wait.
    ///
    /// A client that never answers is an [`std::io::ErrorKind::TimedOut`]
    /// rather than a discarded [`Option`]: the script stops, the response body
    /// ends unfinished, and the test fails at its own assertion about a turn
    /// that did not complete — which says more than this fixture could.
    async fn claim<F>(&self, what: &str, matches: F) -> std::io::Result<proto::ClientMessage>
    where
        F: Fn(&proto::ClientMessage) -> bool,
    {
        tokio::time::timeout(PATIENCE, async {
            loop {
                let pending = self.arrived.notified();
                if let Some(found) = self.take(&matches) {
                    return found;
                }
                pending.await;
            }
        })
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("the client never answered the {what} this script asked for"),
            )
        })
    }

    /// One pass over what has arrived.
    fn take<F>(&self, matches: &F) -> Option<proto::ClientMessage>
    where
        F: Fn(&proto::ClientMessage) -> bool,
    {
        let seen = self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut claimed = self.claimed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        for (index, message) in seen.iter().enumerate() {
            if !claimed.contains(&index) && matches(message) {
                claimed.insert(index);

                return Some(message.clone());
            }
        }

        None
    }

    /// Files a decoded message and wakes every waiting step.
    fn push(&self, message: proto::ClientMessage) {
        self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(message);
        self.arrived.notify_waiters();
    }
}

/// De-chunks the request body forever, decoding one Connect envelope at a
/// time and filing it.
///
/// Never waits for a body EOF: a live Run's request body is deliberately held
/// open for the whole turn, which is the property the whole fixture exists to
/// honour.
async fn drain_body(
    mut reader: tokio::io::ReadHalf<tokio::net::TcpStream>,
    inbox: &Inbox,
    recording: &State,
) {
    // Two buffers, two framings. `raw` is what the socket delivers — HTTP
    // chunked transfer-encoding, because the client's body length is unknown
    // to it — and `body` is what falls out of that, which is where the Connect
    // envelopes live. Conflating them reads a chunk's hex size line as the
    // front of an envelope and decodes nothing, forever.
    let mut raw = Vec::new();
    let mut body = Vec::new();
    let mut buffer = [0_u8; 4096];

    loop {
        while let Some(chunk) = next_chunk(&mut raw) {
            body.extend_from_slice(&chunk);
        }
        while let Some(payload) = next_envelope(&mut body) {
            let Ok(message) = proto::ClientMessage::decode_from_slice(&payload) else {
                continue;
            };
            file(&message, recording);
            inbox.push(message);
        }

        let Ok(read) = reader.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        raw.extend_from_slice(&buffer[..read]);
    }
}

/// Pops one HTTP chunk's data off the front of `raw`, if a whole chunk is
/// buffered.
///
/// The terminal `0\r\n\r\n` yields an empty vector, which appends nothing —
/// a live Run's request body never sends one anyway, because it is held open
/// for the whole turn.
fn next_chunk(raw: &mut Vec<u8>) -> Option<Vec<u8>> {
    let line = raw.windows(2).position(|pair| pair == b"\r\n")?;
    // A chunk size may carry `;ext=value` extensions; nothing here sends one,
    // but reading past a semicolon costs one `split` and cannot be wrong.
    let size = usize::from_str_radix(
        std::str::from_utf8(&raw[..line]).ok()?.split(';').next()?.trim(),
        16,
    )
    .ok()?;

    // The size line, its CRLF, the data, and the CRLF after it.
    let whole = line + 2 + size + 2;
    if raw.len() < whole {
        return None;
    }
    let data = raw[line + 2..line + 2 + size].to_vec();
    raw.drain(..whole);

    Some(data)
}

/// Records what `message` is, or counts it if it is a heartbeat.
fn file(message: &proto::ClientMessage, recording: &State) {
    if message.client_heartbeat.is_set() {
        *recording.heartbeats.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;

        return;
    }

    // Only what an accessor reads. A kv answer and the `stream_close` that
    // ends an exec both still reach [`Inbox`] — that is what the steps waiting
    // on them claim — but nothing asserts *about* them, and a recorded variant
    // no test can name is a fixture path that has never run.
    let entry = if let Some(run) = message.run_request.as_option() {
        Recorded::RunRequest(Box::new(run.clone()))
    } else if let Some(answer) = message.exec_response.as_option() {
        Recorded::ExecResponse(Box::new(answer.clone()))
    } else {
        return;
    };

    recording.recorded.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(entry);
}

/// Pops one whole Connect envelope's payload off the front of `body`, if one
/// is buffered.
///
/// `body` is post-de-chunking — [`next_chunk`] has already stripped HTTP's own
/// framing — so what is at the front here is a flag byte and a big-endian
/// length.
fn next_envelope(body: &mut Vec<u8>) -> Option<Vec<u8>> {
    if body.len() < 5 {
        return None;
    }
    let declared = u32::from_be_bytes(body[1..5].try_into().ok()?) as usize;
    if body.len() < 5 + declared {
        return None;
    }
    let payload = body[5..5 + declared].to_vec();
    body.drain(..5 + declared);

    Some(payload)
}

/// Reads an HTTP head, returning it as text.
async fn read_head(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];

    while !head.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => head.push(byte[0]),
        }
    }

    Some(String::from_utf8_lossy(&head).into_owned())
}

/// Whether the head declares the chunked transfer encoding this fixture's
/// body reader assumes.
fn chunked(head: &str) -> bool {
    head.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        })
    })
}

/// `content-length`, when the head declares one.
fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
    })
}

/// A JSON object as the `McpArgs.args` entry list the server really sends.
///
/// The values go through [`value::encode`] rather than a second copy of it
/// here — but that buys **less** than symmetry usually does, and the difference
/// is worth stating. `value::encode` has no shipped caller: the roster declares
/// on `input_schema_json = 6` alone, so nothing outside tests encodes a
/// `JsonValue` at all, and these bytes are therefore the exact mirror image of
/// the decoder they are fed to. Transposing `JsonStruct`'s and `JsonList`'s
/// field numbers would leave this fixture, its round-trip test and every
/// bridged-call assertion green, and break only on the first live call carrying
/// a nested argument object.
///
/// Nor is there a recording to diff against: the live probe declared an
/// argument-less tool, so `args = 2` was **absent** on all five runs
/// (`crates/ganja-provider/tests/fixtures/cursor-mcp-tools-probe.txt`). The
/// argument encoding this fixture exercises is unmeasured against cursor's
/// server. What pins it to the wire instead of to itself is
/// `cursor/value_tests.rs`'s `a_nested_value_encodes_the_bytes_the_descriptor_numbers_spell`,
/// which asserts one nested shape's bytes against the descriptor's own field
/// numbers.
///
/// Anything but an object becomes one entry keyed `value`, which is what the
/// server would have to do with a bare scalar too.
fn json_entries(args: &serde_json::Value) -> Vec<proto::McpArgEntry> {
    let entry = |key: &str, value: &serde_json::Value| proto::McpArgEntry {
        key: Some(key.to_owned()),
        value: MessageField::some(value::encode(value)),
        ..Default::default()
    };

    match args {
        serde_json::Value::Object(fields) => {
            fields.iter().map(|(key, field)| entry(key, field)).collect()
        }
        other => vec![entry("value", other)],
    }
}
