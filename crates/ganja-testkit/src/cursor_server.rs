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
//! `Run`, and nothing else: headers and the first frame go out immediately;
//! from then on the socket is split and driven by the [`Step`]s the test
//! hands in. Each Connect frame is one HTTP chunk, flushed, and the request
//! body is de-chunked incrementally *while the response is open*. That the
//! transport permits this is not assumed: it is measured by
//! `crates/ganja-core/tests/cursor_bridge.rs`'s first test, over the same
//! reqwest/hyper stack the wire uses.
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
//! Three steps exist for the turn a server *drops* rather than finishes
//! (**D553**'s recovery, W3): [`Step::ExecNoWait`] writes an exec and moves
//! on without its answer, [`Step::Hangup`] ends the connection under it once
//! the test says the Run is held, and [`Step::KvGet`]/[`Step::KvGetComposed`]
//! ask the kv channel for what the next Run's state names — the gets a fresh
//! Run over a composed history has to answer. `PATIENCE` is a bound on a
//! fixture bug, never a mechanism: no script here waits for it to expire, and
//! the drop is a step, not a timeout. Expiring at a [`Step::Hangup`] nobody
//! released, it writes an EndStream **error** frame naming the unreleased
//! hangup before the connection ends, so a script that resumed the Run
//! without releasing it fails the turn by that name rather than passing
//! twenty seconds late. What the frame cannot reach is a Run still *held*
//! through the expiry: its fold sits in the held-run table unread, the
//! keeper's next beat drops it as `Closed`, and the resume recovers on a
//! fresh Run — the frame lands on a body nobody is reading, and that shape
//! is bounded by the expiry alone.
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
pub const END_STREAM: u8 = 0b0000_0010;

/// How long one step may wait for the answer it asked for.
///
/// Generous because CI machines stall, and reached only when the client never
/// answers at all — or, for a [`Step::Hangup`], when the test never releases
/// it, which is answered with an EndStream error frame naming the unreleased
/// hangup so the turn reading that body fails by name — in which case the
/// test's own failure message is what matters, not the wait. **Not a
/// mechanism**: a script that leaned on this expiring would stall a
/// workspace run by twenty seconds per case, so every drop a test wants is a
/// step it writes.
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
    /// module growing an arm per kind. Waits for its answers.
    ///
    /// `id` is the correlation key; give each exec its own. Boxed: an
    /// `ExecRequest` carries every kind's args inline, large enough that
    /// clippy's `large_enum_variant` fires, and every other `Step` — a
    /// `String`, a unit — would pay for it.
    Exec(Box<proto::ExecRequest>),
    /// The same exec, written and **not** waited on: the script advances
    /// without its answer or its `stream_close`. For a Run the script is about
    /// to drop — a bridged exec is answered on the request that *resumes* the
    /// Run, so a script that hangs up under one while also waiting for its
    /// answer would be waiting for what it made impossible. Built by
    /// [`Step::no_wait`].
    ExecNoWait(Box<proto::ExecRequest>),
    /// Several execs written back-to-back, answered in any order — the
    /// concurrency the header cites; [`Step::Exec`] settles each before the
    /// next is written. Built by [`Step::batch`].
    Batch(Vec<Box<proto::ExecRequest>>),
    /// `kv_request = 4` asking for one blob by id, waited on. The
    /// `kv_response` is recorded as a [`KvAnswer`] **found or not**: a
    /// not-found answer is the client's honest reply about an id nobody
    /// composed and nobody set, and what a test asserts about is the
    /// answer, so a miss never fails the script.
    KvGet(Vec<u8>),
    /// A `kv_request` for every blob the opening frame's state named — its
    /// `root_prompt_messages_json` ids, then its `turns`, then the user
    /// message and steps each turn decodes to — the walk the live server
    /// makes over a composed history (**D553**). Exists because a composed id
    /// is the sha256 of bytes the engine assembles, which a script written
    /// before the Run opens cannot know. Each get is recorded the way
    /// [`Step::KvGet`] records one; a turn that is not found, or does not
    /// decode, ends its own branch of the walk and nothing else.
    KvGetComposed,
    /// A `text_delta`.
    Text(String),
    /// `turn_ended = 14`.
    TurnEnded,
    /// The Connect EndStream frame that closes the response, cleanly. A script
    /// that never reaches one leaves the response open, which is a real state
    /// a turn can be in and one a cancellation test wants; every other script
    /// should end with one.
    ///
    /// No failure payload: an in-body Connect verdict is a *wire* fact rather
    /// than a bridge one, and it is measured where it can be measured against
    /// the redaction that has to survive it — `crates/ganja-core/tests/secrets_env.rs`'s
    /// cursor arm, over a real socket, on both of that wire's failure paths.
    EndStream,
    /// The drop. The terminal chunk goes out with no EndStream frame ahead of
    /// it, the reader is ended and both socket halves are dropped, so the
    /// client's connection takes a FIN with the exchange unfinished and its
    /// request-body sender fails on its next write. Nothing after this step
    /// runs.
    ///
    /// **Released by the test**, through [`CursorServer::hang_up`], rather
    /// than written the moment the script reaches it. The FIN has to land
    /// *after* the Run is held, and the only party that can see a hold is the
    /// engine — the dialog a bridged call raises is its proof: the wire
    /// gathers a bridged exec for a window before it pauses (`cursor.rs`'s
    /// `GATHER_WINDOW`), and a FIN written straight after the exec frame
    /// reaches the still-streaming fold inside that window as a body that
    /// ended before the exchange did, which the wire reports and then clears
    /// the gathering execs for — a failed turn with nothing held and nothing
    /// to drop. Waiting for the client's heartbeat instead would cost one
    /// interval per case for the same ordering, on a signal the streaming
    /// fold also sends.
    Hangup,
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
        Self::Exec(Box::new(proto::ExecRequest {
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
        }))
    }

    /// A native `read_args = 7` exec: the path that never touches the mcp
    /// channel.
    #[must_use]
    pub fn read(id: u32, path: &str) -> Self {
        Self::Exec(Box::new(proto::ExecRequest {
            id: Some(id),
            read_args: MessageField::some(proto::ReadArgs {
                path: Some(path.to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    /// Sets the one flag **Dv-3** is about: `smart_mode_approval_only = 7`,
    /// which the client answers `approved` to without executing anything.
    ///
    /// # Panics
    ///
    /// On a step that is not an `mcp_args` exec, which is a fixture bug rather
    /// than a server behaviour: the flag has no meaning on any other kind, and
    /// a script that set it there would be driving the ordinary path while
    /// claiming a preflight.
    #[must_use]
    pub fn approval_only(mut self) -> Self {
        let args = match &mut self {
            Self::Exec(request) => request.mcp_args.as_option_mut(),
            _ => None,
        }
        .expect("`approval_only` is an `mcp_args` flag; no other step has a preflight");
        args.smart_mode_approval_only = Some(true);

        self
    }

    /// The same exec as a [`Step::ExecNoWait`], so a script about to drop a
    /// Run is spelled with the builders every other script uses —
    /// `Step::mcp(1, ..).no_wait()`.
    ///
    /// # Panics
    ///
    /// On a step that is not a lone exec, which is a fixture bug rather than a
    /// server behaviour: nothing else waits for an answer, so nothing else has
    /// a wait to skip.
    #[must_use]
    pub fn no_wait(self) -> Self {
        match self {
            Self::Exec(request) => Self::ExecNoWait(request),
            _ => panic!("`no_wait` skips a lone exec's wait; no other step has one"),
        }
    }

    /// Several execs as one [`Step::Batch`], written together.
    ///
    /// Takes whole [`Step::Exec`]s rather than bare requests so a batch is
    /// spelled with the same builders a lone exec is — `Step::batch(vec![
    /// Step::mcp(1, ..), Step::read(2, ..)])`.
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
                    Self::Exec(request) => request,
                    _ => panic!("a batch holds execs; every other step is written on its own"),
                })
                .collect(),
        )
    }
}

/// The shortest complete turn: nothing said, cleanly ended — what a Run past
/// the end of the scripts a test handed in is served.
#[must_use]
pub fn finished() -> Vec<Step> {
    vec![Step::TurnEnded, Step::EndStream]
}

/// One blob's bytes as the wire's own message type, or [`None`] when they
/// are not one.
///
/// Here because `buffa`'s trait is what decodes, and `ganja-core` — whose
/// suites read the blobs a Run served — does not name that crate; a caller
/// names only the `proto` type it expects, which it already can.
#[must_use]
pub fn decoded<M: buffa::Message>(bytes: &[u8]) -> Option<M> {
    M::decode_from_slice(bytes).ok()
}

/// One thing the client sent, decoded.
///
/// Only what a test *asserts about*: a variant here is a variant some accessor
/// reads. Everything else a client writes — kv answers, the `stream_close`
/// that ends each exec — reaches `Inbox`, where the steps waiting on it claim
/// it.
#[derive(Clone)]
enum Recorded {
    /// The opening frame of a Run, with the declared tool roster on it.
    RunRequest(Box<proto::RunRequest>),
    /// An exec answer of any kind.
    ExecResponse(Box<proto::ExecResponse>),
    /// What one kv get came back with. Filed by the step that asked rather
    /// than by the reader, because the answer echoes the request's id and not
    /// the blob's, and the blob's is what a test matches on.
    KvAnswer(KvAnswer),
}

/// What the client answered one kv get with.
#[derive(Clone, Debug)]
pub struct KvAnswer {
    /// The id the server asked for.
    pub blob_id: Vec<u8>,
    /// The bytes the client holds under it, or [`None`] for the not-found
    /// shape — a present result with no data in it.
    pub data: Option<Vec<u8>>,
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
    /// The test's release of a [`Step::Hangup`]. One permit, kept until a
    /// script reaches the step, so a test may release before or after.
    hangup: Notify,
}

impl CursorServer {
    /// Starts a server whose one Run stream runs `script`.
    ///
    /// Later Runs are served [`finished`].
    pub async fn start(script: Vec<Step>) -> Self {
        Self::with_scripts(vec![script]).await
    }

    /// The same, with one script per Run in order.
    pub async fn with_scripts(scripts: Vec<Vec<Step>>) -> Self {
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

    /// Everything the client sent that `pick` accepts, in order.
    fn picked<T>(&self, pick: impl Fn(Recorded) -> Option<T>) -> Vec<T> {
        let recorded = self.state.recorded.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        recorded.iter().cloned().filter_map(pick).collect()
    }

    /// Every `mcp_result` the client answered with, in order.
    #[must_use]
    pub fn mcp_results(&self) -> Vec<proto::McpResult> {
        self.picked(|entry| match entry {
            Recorded::ExecResponse(response) => response.mcp_result.into_option(),
            Recorded::RunRequest(_) | Recorded::KvAnswer(_) => None,
        })
    }

    /// Every `read_result` the client answered with, in order.
    #[must_use]
    pub fn read_results(&self) -> Vec<proto::ReadResult> {
        self.picked(|entry| match entry {
            Recorded::ExecResponse(response) => response.read_result.into_option(),
            Recorded::RunRequest(_) | Recorded::KvAnswer(_) => None,
        })
    }

    /// Every `RequestContext` the client answered a context ask with — where
    /// the declared tool roster arrives.
    #[must_use]
    pub fn context_answers(&self) -> Vec<proto::RequestContext> {
        self.picked(|entry| match entry {
            Recorded::ExecResponse(response) => response
                .request_context_result
                .into_option()
                .and_then(|result| result.success.into_option())
                .and_then(|success| success.request_context.into_option()),
            Recorded::RunRequest(_) | Recorded::KvAnswer(_) => None,
        })
    }

    /// The tool roster declared on the opening frame of every Run, in order —
    /// one entry per Run, empty for a Run that declared none.
    #[must_use]
    pub fn declared_rosters(&self) -> Vec<Vec<proto::McpToolDefinition>> {
        self.picked(|entry| match entry {
            Recorded::RunRequest(run) => {
                Some(run.mcp_tools.into_option().map(|tools| tools.mcp_tools).unwrap_or_default())
            }
            Recorded::ExecResponse(_) | Recorded::KvAnswer(_) => None,
        })
    }

    /// The opening frame of every Run, in order — where the composed state,
    /// the action and the `conversation_id` ride (**D553**), so a test can
    /// say what a *second* Run of one turn carried.
    #[must_use]
    pub fn run_requests(&self) -> Vec<proto::RunRequest> {
        self.picked(|entry| match entry {
            Recorded::RunRequest(run) => Some(*run),
            Recorded::ExecResponse(_) | Recorded::KvAnswer(_) => None,
        })
    }

    /// Every kv get a script asked and what the client answered, in order —
    /// found or not.
    #[must_use]
    pub fn kv_answers(&self) -> Vec<KvAnswer> {
        self.picked(|entry| match entry {
            Recorded::KvAnswer(answer) => Some(answer),
            Recorded::RunRequest(_) | Recorded::ExecResponse(_) => None,
        })
    }

    /// Releases the [`Step::Hangup`] a script is waiting at, or will reach.
    ///
    /// One release, one hangup: the permit is kept until a script consumes
    /// it, so a test that releases before the Run has reached the step is not
    /// lost, and a test that serves two dropping Runs releases twice, each
    /// after the dialog that proves *that* Run is held.
    pub fn hang_up(&self) {
        self.state.hangup.notify_one();
    }

    /// How many `client_heartbeat` frames arrived.
    #[must_use]
    pub fn heartbeats(&self) -> usize {
        *self.state.heartbeats.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One connection: read the head, and serve it if it is a Run.
async fn serve_one(
    mut socket: tokio::net::TcpStream,
    state: &Arc<State>,
    served: &Mutex<std::collections::VecDeque<Vec<Step>>>,
) {
    let Some(head) = read_head(&mut socket).await else {
        return;
    };

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
            queue.pop_front().unwrap_or_else(finished)
        };
        serve_run(socket, script, state).await;
    }
}

/// The Run stream: answer the head at once, then split and drive the script
/// against a reader that never stops draining the request body.
async fn serve_run(socket: tokio::net::TcpStream, script: Vec<Step>, state: &Arc<State>) {
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
    // `finished` — would otherwise write its whole response and close the
    // reader before that frame had been decoded, and the roster would go
    // missing on a race rather than on a behaviour. Kept, too: the state it
    // names is what `KvGetComposed` asks for.
    let Ok(opening) = inbox.claim("run request", |message| message.run_request.is_set()).await
    else {
        return;
    };
    let mut run = Run {
        writer,
        inbox: &inbox,
        recording: state,
        opening: opening.run_request.into_option().unwrap_or_default(),
        next_kv: 1,
    };

    for step in script {
        match run_step(step, &mut run).await {
            Ok(Flow::Continue) => {}
            Ok(Flow::Hangup) | Err(_) => break,
        }
    }
    let Run { mut writer, .. } = run;

    // The terminal chunk, written whatever happened — after the last step,
    // after a step that gave up waiting, and after a hangup. A script that
    // ended without ending its response body leaves the client waiting on one
    // more chunk forever, which turns every fixture bug into a hung test
    // instead of a failed one: the per-step timeout above is only readable if
    // the turn it belongs to can actually finish.
    let _ = writer.write_all(b"0\r\n\r\n").await;
    let _ = writer.flush().await;

    // And now end the reader, so both halves drop and the client's connection
    // gets a FIN rather than being held half-open until the process exits.
    // Safe here and only here: every asking step claimed its answers before the
    // script advanced, so nothing a test asserts about arrives after this
    // point — and the one step that did not wait, `ExecNoWait`, is the one
    // whose answer a hangup makes unreachable on purpose. Awaited after the
    // abort, because an abort is honoured at the task's next poll rather than
    // at once, and the FIN a `Hangup` is for only goes out once both halves
    // are really gone.
    reading.abort();
    drop(writer);
    let _ = reading.await;
}

/// One Run's serving state, as its steps see it.
struct Run<'a> {
    writer: tokio::io::WriteHalf<tokio::net::TcpStream>,
    inbox: &'a Inbox,
    recording: &'a State,
    /// The opening frame, whose state names what [`Step::KvGetComposed`]
    /// asks for.
    opening: proto::RunRequest,
    /// The id the next kv request carries; the live server's ascend within a
    /// Run, and each answer echoes the one it was asked under.
    next_kv: u32,
}

/// Whether the script goes on after a step.
enum Flow {
    Continue,
    /// The step was [`Step::Hangup`]: end the connection now, and run nothing
    /// after it.
    Hangup,
}

/// One step, written and — where it asks — waited on.
async fn run_step(step: Step, run: &mut Run<'_>) -> std::io::Result<Flow> {
    match step {
        Step::Context => {
            write_server_message(
                &mut run.writer,
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
            run.inbox
                .claim("context answer", |message| {
                    message
                        .exec_response
                        .as_option()
                        .is_some_and(|answer| answer.request_context_result.is_set())
                })
                .await?;
        }
        Step::Exec(request) => {
            let id = write_exec(&mut run.writer, *request).await?;
            settle_exec(run.inbox, id).await?;
        }
        Step::ExecNoWait(request) => {
            write_exec(&mut run.writer, *request).await?;
        }
        Step::Batch(execs) => {
            // Every request first, so they really are in flight together...
            let mut outstanding = Vec::with_capacity(execs.len());
            for request in execs {
                outstanding.push(write_exec(&mut run.writer, *request).await?);
            }
            // ...and only then the answers, which `Inbox` matches by id, so the
            // client may answer them in either order.
            for id in outstanding {
                settle_exec(run.inbox, id).await?;
            }
        }
        Step::KvGet(blob_id) => {
            kv_get(run, blob_id).await?;
        }
        Step::KvGetComposed => {
            let state = run.opening.conversation_state.as_option().cloned().unwrap_or_default();
            for id in state.root_prompt_messages_json {
                kv_get(run, id).await?;
            }
            for id in state.turns {
                // A turn is a blob naming blobs: the user message and each
                // step. Only a turn id is decoded — a root entry is JSON, and
                // protobuf's leniency could read a `{` as a field it is not.
                let Some(bytes) = kv_get(run, id).await? else { continue };
                let Ok(turn) = proto::ConversationTurn::decode_from_slice(&bytes) else {
                    continue;
                };
                let inner = turn.agent_conversation_turn.into_option().unwrap_or_default();
                for id in inner.user_message.into_iter().chain(inner.steps) {
                    kv_get(run, id).await?;
                }
            }
        }
        Step::Text(text) => {
            write_update(
                &mut run.writer,
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
                &mut run.writer,
                proto::Update {
                    turn_ended: MessageField::some(proto::TurnEnded::default()),
                    ..Default::default()
                },
            )
            .await?;
        }
        Step::EndStream => {
            write_chunk(&mut run.writer, &envelope(END_STREAM, b"{}")).await?;
        }
        Step::Hangup => {
            if tokio::time::timeout(PATIENCE, run.recording.hangup.notified()).await.is_err() {
                // Named on the wire and not only in this error: the error
                // below reaches nothing a test reads, where an EndStream
                // verdict reaches the fold — so a script that resumed the Run
                // without releasing the hangup fails its turn by this
                // sentence, rather than twenty seconds late by a truncation
                // it cannot tell from any other.
                let complaint = "the test never released the hangup this script reached";
                let verdict = serde_json::json!({ "error": { "code": "deadline_exceeded", "message": complaint } });
                write_chunk(&mut run.writer, &envelope(END_STREAM, verdict.to_string().as_bytes()))
                    .await?;

                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, complaint));
            }

            return Ok(Flow::Hangup);
        }
    }

    Ok(Flow::Continue)
}

/// Asks the kv channel for one blob, waits for the answer, and records it —
/// found or not — handing back what was found.
async fn kv_get(run: &mut Run<'_>, blob_id: Vec<u8>) -> std::io::Result<Option<Vec<u8>>> {
    let id = run.next_kv;
    run.next_kv += 1;
    write_server_message(
        &mut run.writer,
        proto::ServerMessage {
            kv_request: MessageField::some(proto::KvRequest {
                id: Some(id),
                get_blob_args: MessageField::some(proto::GetBlobArgs {
                    blob_id: Some(blob_id.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await?;

    let answer = run
        .inbox
        .claim("kv answer", |message| {
            message.kv_response.as_option().is_some_and(|response| response.id == Some(id))
        })
        .await?;
    let data = answer
        .kv_response
        .into_option()
        .and_then(|response| response.get_blob_result.into_option())
        .and_then(|result| result.blob_data);

    run.recording
        .recorded
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(Recorded::KvAnswer(KvAnswer { blob_id, data: data.clone() }));

    Ok(data)
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

/// Waits for one exec's answer and the `stream_close` that ends it, refused
/// or served.
async fn settle_exec(inbox: &Inbox, id: Option<u32>) -> std::io::Result<()> {
    inbox
        .claim("exec answer", |message| {
            message.exec_response.as_option().is_some_and(|answer| answer.id == id)
        })
        .await?;
    inbox
        .claim("exec stream_close", |message| {
            message.exec_control.as_option().is_some_and(|control| {
                control.stream_close.as_option().is_some_and(|close| close.id == id)
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
#[must_use]
pub fn envelope(flags: u8, payload: &[u8]) -> Vec<u8> {
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

    // Only what an accessor reads; see `Recorded`.
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
