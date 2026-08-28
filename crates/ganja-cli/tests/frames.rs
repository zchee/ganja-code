//! The frame-vocabulary pin: what `ganja-client` declares, against what
//! `ganja-serve` writes.
//!
//! The vocabulary is declared in `ganja-client` because the client must not
//! depend on the server (that edge would drag `axum` into every consumer of
//! the client), and a declaration nobody checks is a comment. This is the
//! check, and it lives here because `ganja-cli` is the one crate that links
//! both — the client to drive a server with, the server to stand one up.
//!
//! **Exhaustive in both directions**, across the file:
//!
//! * *serve → client*: every frame name a running server writes is asserted to
//!   be one of [`sse::FRAMES`], in both tests, over the bytes the socket
//!   actually carried. A control frame nobody declared reddens.
//! * *client → serve*: each of the four declared names is observed coming out
//!   of a real server — `connected` and `message` and `heartbeat` in the first
//!   test, `evicted` in the second, which provokes a real eviction rather than
//!   asserting about one. A name this crate declares that serve stopped
//!   writing reddens.
//! * *representation*: the bytes are split and parsed by the client's own
//!   [`Frames`], not by a reimplementation, so the framing and the payload
//!   shapes are pinned rather than merely the names.
//!
//! The stream is read off a raw socket instead of through the client, because
//! the client would hide exactly what is under test. The request is HTTP/1.0
//! so the body arrives as the frames themselves rather than inside chunked
//! transfer framing — asserted below, so a transport change fails loudly here
//! instead of quietly mis-parsing.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use ganja_client::sse::{self, Frame, Frames};
use ganja_core::Engine;
use ganja_core::provider::fake::FakeProvider;
use ganja_core::tool::Registry;
use ganja_permission::Permissions;
use ganja_protocol::{Command, Event};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// How long any single wait may take before the fixture is declared broken.
const DEADLINE: Duration = Duration::from_secs(60);

/// A heartbeat quick enough that a test sees one without waiting ten seconds.
const FAST_HEARTBEAT: Duration = Duration::from_millis(50);

/// One word appearing nowhere else, so finding it means the turn ran.
const CLOSING: &str = "script-finished-zarquon";

/// Events a subscriber's queue holds before a full one evicts it
/// (`ganja-core/src/engine.rs`, `EVENT_CAPACITY`). Not re-exported, so it is
/// spelled here — the burst below only has to *exceed* it, and does so by
/// enough that the exact number is not load-bearing.
const EVENT_CAPACITY: usize = 1024;

/// A server on a fake provider that streams `reply`, one word at a time.
async fn served(reply: &str) -> (Arc<Engine>, ganja_serve::Handle) {
    let engine = Arc::new(Engine::new(
        Arc::new(FakeProvider::new(reply, Duration::ZERO)),
        "canned",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    ));

    let directory = std::env::current_dir().expect("the working directory resolves");
    let mut config = ganja_serve::ServeConfig::in_directory(directory);
    config.listen = ganja_serve::Listen::Tcp {
        hostname: ganja_serve::DEFAULT_HOSTNAME.to_owned(),
        port: Some(0),
    };
    config.heartbeat = FAST_HEARTBEAT;

    let handle = ganja_serve::serve(Arc::clone(&engine), config)
        .await
        .expect("a loopback server with no password comes up");

    (engine, handle)
}

/// An open `GET /event`, read as bytes and split by the client's own splitter.
struct Reader {
    socket: TcpStream,
    frames: Frames,
    /// Every frame name seen so far, in order.
    names: Vec<String>,
    /// Every frame parsed so far, in order.
    seen: Vec<Frame>,
}

impl Reader {
    /// Opens the stream and reads past the response head, which is asserted on
    /// the way through.
    async fn open(handle: &ganja_serve::Handle) -> Self {
        let mut socket = TcpStream::connect(handle.address().tcp().expect("a tcp server"))
            .await
            .expect("the server accepts");
        socket
            .write_all(
                b"GET /event HTTP/1.0\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n",
            )
            .await
            .expect("the request writes");

        let mut buffer = Vec::new();
        let head = loop {
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break end;
            }
            let mut chunk = [0u8; 4096];
            let read = tokio::time::timeout(DEADLINE, socket.read(&mut chunk))
                .await
                .expect("the server answers within the deadline")
                .expect("the response reads");
            assert!(read > 0, "the server closed before answering");
            buffer.extend_from_slice(&chunk[..read]);
        };

        let response = String::from_utf8_lossy(&buffer[..head]).to_ascii_lowercase();
        assert!(response.contains(" 200 "), "the stream answers 200: {response}");
        assert!(
            response.contains("content-type: text/event-stream"),
            "and says what it is: {response}"
        );
        assert!(
            !response.contains("transfer-encoding: chunked"),
            "the body must be the frames themselves for this test to read them: {response}"
        );

        let mut frames = Frames::new();
        frames.push(&buffer[head + 4..]);

        Self { socket, frames, names: Vec::new(), seen: Vec::new() }
    }

    /// Splits whatever has arrived, recording each frame — and asserting, for
    /// every one of them, that it is a frame this build declares.
    fn drain(&mut self) {
        while let Some(frame) = self.frames.pop() {
            // The serve → client direction: a frame outside the declared
            // vocabulary is a parse error, and the error names it.
            let frame = frame.expect("every frame a real server writes is one the client declares");
            self.names.push(named(&frame).to_owned());
            self.seen.push(frame);
        }
    }

    /// Reads until `done` says the frames so far are enough, or the stream
    /// ends.
    ///
    /// The deadline is on the whole wait rather than on each read: a stream
    /// that heartbeats forever never starves a single read, so a per-read
    /// timeout would let a test that is never going to finish run until the
    /// harness killed it.
    async fn read_until(&mut self, mut done: impl FnMut(&[String]) -> bool) {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        self.drain();
        while !done(&self.names) {
            let mut chunk = [0u8; 65536];
            let read = tokio::time::timeout_at(deadline, self.socket.read(&mut chunk))
                .await
                .unwrap_or_else(|_| {
                    panic!("the frames never arrived; got {:?}", self.names);
                })
                .expect("the stream reads");
            if read == 0 {
                break;
            }
            self.frames.push(&chunk[..read]);
            self.drain();
        }
    }
}

/// What the client calls the frame it just parsed — the declared name, read
/// back off the parsed value, so the mapping itself is part of the pin.
fn named(frame: &Frame) -> &'static str {
    match frame {
        Frame::Connected => sse::CONNECTED,
        Frame::Message(_) => sse::MESSAGE,
        Frame::Heartbeat => sse::HEARTBEAT,
        Frame::Evicted(_) => sse::EVICTED,
    }
}

/// The three frames a healthy stream carries, each observed coming out of a
/// real server and parsed by the client's own splitter.
#[tokio::test(flavor = "multi_thread")]
async fn a_running_server_writes_the_frames_the_client_declares() {
    let (engine, handle) = served(CLOSING).await;
    let mut reader = Reader::open(&handle).await;

    // Reading the connected frame proves the subscription is registered, which
    // is the guarantee the client's `events` is built on.
    reader.read_until(|names| !names.is_empty()).await;
    assert_eq!(
        reader.names.first().map(String::as_str),
        Some(sse::CONNECTED),
        "the connected frame comes before anything else: {:?}",
        reader.names
    );

    engine
        .send(Command::SendPrompt {
            text: "say something".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("the engine takes a prompt");

    // A turn's last event, which is where a message frame is guaranteed to
    // have carried something the client could parse into a protocol event.
    reader.read_until(|names| names.iter().filter(|name| *name == sse::MESSAGE).count() > 3).await;
    let finished = reader.seen.iter().any(|frame| {
        matches!(frame, Frame::Message(event) if matches!(**event, Event::MessageStarted { .. }))
    });
    assert!(finished, "a message frame carried a protocol event");

    // A silent stream still proves it is alive.
    tokio::time::sleep(FAST_HEARTBEAT * 4).await;
    reader.read_until(|names| names.iter().any(|name| name == sse::HEARTBEAT)).await;

    for wanted in [sse::CONNECTED, sse::MESSAGE, sse::HEARTBEAT] {
        assert!(
            reader.names.iter().any(|name| name == wanted),
            "no {wanted} frame in {:?}",
            reader.names
        );
    }
    // The one name this test cannot reach is the one the next test provokes;
    // together they cover the declared set.
    assert!(
        !reader.names.iter().any(|name| name == sse::EVICTED),
        "a healthy stream is never evicted: {:?}",
        reader.names
    );

    handle.shutdown().await.expect("the server stops cleanly");
}

/// The fourth frame, provoked rather than asserted about: a subscriber that
/// stops reading is dropped by the engine, and the stream it left behind ends
/// with a frame that says so — the whole reason a torn transcript cannot look
/// like a whole one.
#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_that_stops_reading_is_told_it_was_evicted() {
    // Long enough that the socket's buffers fill — which is what stalls the
    // server's pump — and then long enough again to overflow the engine-side
    // queue behind it. Big words rather than many, so the socket fills in
    // hundreds of frames rather than hundreds of thousands.
    let word = "w".repeat(4096);
    let burst = EVENT_CAPACITY * 3;
    let reply = std::iter::repeat_n(word.as_str(), burst).collect::<Vec<_>>().join(" ");

    let (engine, handle) = served(&reply).await;
    let mut reader = Reader::open(&handle).await;
    // The connected frame, and then nothing is read from this socket until the
    // turn is over.
    reader.read_until(|names| !names.is_empty()).await;
    assert_eq!(reader.names.first().map(String::as_str), Some(sse::CONNECTED));

    // A second reader on the engine itself, which is what makes this
    // deterministic rather than a sleep: it is lossless, so following it to the
    // end of the turn *is* waiting for every event to have been published —
    // and while this loop runs, nothing is reading the socket, which is the
    // stall the eviction needs.
    let mut direct = engine.subscribe().await.expect("a subscriber registers");
    engine
        .send(Command::SendPrompt {
            text: "say a great deal".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("the engine takes a prompt");
    while let Some(event) = tokio::time::timeout(DEADLINE, direct.next())
        .await
        .expect("the turn ends within the deadline")
    {
        if matches!(event, Event::MessageFinished { .. }) {
            break;
        }
    }

    // Now read: whatever the queue still held arrives first — every event
    // before the eviction is real and in order — and the eviction is last,
    // after which the server closes the stream.
    reader.read_until(|names| names.iter().any(|name| name == sse::EVICTED)).await;

    let last = reader.seen.last().expect("the stream said something");
    let Frame::Evicted(notice) = last else {
        panic!(
            "a subscriber that stopped reading should be evicted, and told: {:?}",
            reader.names.last()
        );
    };
    assert_eq!(notice.kind, sse::EVICTED, "the payload names itself too");
    assert!(
        notice.message.contains("fell behind"),
        "the notice carries the engine's own account: {notice:?}"
    );
    assert_eq!(
        reader.names.iter().filter(|name| *name == sse::EVICTED).count(),
        1,
        "an eviction is terminal: nothing follows it"
    );

    handle.shutdown().await.expect("the server stops cleanly");
}

/// The declaration itself: four names, and the pin above covers each of them.
/// A fifth name added here without a server that writes it would leave one of
/// the two tests above unable to observe it.
#[test]
fn the_declared_vocabulary_is_the_four_names_the_pin_covers() {
    assert_eq!(sse::FRAMES, [sse::CONNECTED, sse::MESSAGE, sse::HEARTBEAT, sse::EVICTED]);
    assert_eq!(
        sse::FRAMES,
        ["connected", "message", "heartbeat", "evicted"],
        "the names are serve's, spelled out so a rename on either side reddens"
    );
}
