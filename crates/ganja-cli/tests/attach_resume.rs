//! Resumption through dropped connections and real server process restarts.

#![cfg(unix)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ganja_client::sse::{EventId, Frame, Frames};
use ganja_client::{
    AttemptOutcome, Client, ConnectPolicy, ConnectionPermit, Event, Prompt, Resumable, Resumed,
};
use ganja_protocol::{Message, Role, SessionId};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{RwLock, watch};

mod served_child;

use served_child::{DEADLINE, Reaped};

struct Server {
    project: TempDir,
    data: TempDir,
    config: TempDir,
    child: Option<Reaped>,
    address: SocketAddr,
}

impl Server {
    fn new(script: serde_json::Value) -> Self {
        let project = ganja_testkit::temp_dir();
        let data = ganja_testkit::temp_dir();
        let config = ganja_testkit::temp_dir();
        std::fs::write(project.path().join("script.json"), script.to_string())
            .expect("script writes");
        let mut server = Self {
            project,
            data,
            config,
            child: None,
            address: "127.0.0.1:1".parse().expect("address"),
        };
        server.start();
        server
    }

    fn start(&mut self) {
        let mut child = served_child::spawn_with(
            self.project.path(),
            self.data.path(),
            self.config.path(),
            self.data.path(),
            &self.project.path().join("script.json"),
            &[("GANJA_DISABLE_MODELS_FETCH", "1"), ("GANJA_PROVIDER", "fake")],
        );
        let line = child.announcement("the server address");
        let port: u16 =
            line.trim().rsplit(':').next().expect("port").parse().expect("numeric port");
        self.address = SocketAddr::from(([127, 0, 0, 1], port));
        self.child = Some(child);
    }

    fn restart(&mut self) {
        drop(self.child.take());
        self.start();
    }
}

#[derive(Clone, Default)]
struct Forwarded {
    deltas: usize,
    permission: bool,
}

#[derive(Debug, Default)]
struct Connections(AtomicUsize);

impl ConnectPolicy for Connections {
    fn admit(&self) -> futures::future::BoxFuture<'_, Option<ConnectionPermit>> {
        Box::pin(async { Some(Box::new(()) as ConnectionPermit) })
    }

    fn observe(&self, _outcome: AttemptOutcome) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A stable address whose active connections can be cut independently of the child.
struct Relay {
    address: SocketAddr,
    target: Arc<RwLock<SocketAddr>>,
    cut: watch::Sender<u64>,
    forwarded: watch::Receiver<Forwarded>,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    async fn new(target: SocketAddr) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("relay binds");
        let address = listener.local_addr().expect("relay address");
        let target = Arc::new(RwLock::new(target));
        let (cut, _) = watch::channel(0);
        let (forwarded, received) = watch::channel(Forwarded::default());
        let task = tokio::spawn({
            let target = target.clone();
            let cut = cut.clone();
            async move {
                while let Ok((mut client, _)) = listener.accept().await {
                    let target = *target.read().await;
                    let mut cut = cut.subscribe();
                    let forwarded = forwarded.clone();
                    tokio::spawn(async move {
                        let Ok(mut server) = TcpStream::connect(target).await else { return };
                        let (mut input, mut output) = client.split();
                        let (mut source, mut sink) = server.split();
                        let downstream = async {
                            let mut frames = Frames::new();
                            let mut buffer = [0; 8192];
                            loop {
                                let read = source.read(&mut buffer).await?;
                                if read == 0 {
                                    return std::io::Result::Ok(());
                                }
                                output.write_all(&buffer[..read]).await?;
                                // Serve emits whole SSE frames as HTTP chunks. The existing
                                // splitter ignores the HTTP headers and chunk-size lines.
                                frames.push(&buffer[..read]);
                                while let Some(frame) = frames.pop() {
                                    if let Frame::Message(_, event) = frame.expect("relay frame") {
                                        forwarded.send_modify(|state| match *event {
                                            Event::PartDelta { .. } => state.deltas += 1,
                                            Event::PermissionRequested { .. } => {
                                                state.permission = true
                                            }
                                            _ => {}
                                        });
                                    }
                                }
                            }
                        };
                        tokio::select! {
                            _ = cut.changed() => {}
                            _ = async { tokio::try_join!(tokio::io::copy(&mut input, &mut sink), downstream) } => {}
                        }
                    });
                }
            }
        });
        Self { address, target, cut, forwarded: received, task }
    }

    fn client(&self) -> Client {
        Client::new(&format!("http://{}", self.address), None).expect("relay client")
    }

    fn sever(&self) {
        self.cut.send_modify(|generation| *generation += 1);
    }

    async fn retarget(&self, target: SocketAddr) {
        *self.target.write().await = target;
        self.sever();
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        self.sever();
    }
}

async fn next(resumable: &Resumable) -> Resumed {
    tokio::time::timeout(DEADLINE, resumable.next())
        .await
        .expect("stream progresses")
        .expect("stream remains active")
        .expect("known wire")
}

async fn finished(resumable: &Resumable) {
    loop {
        match next(resumable).await {
            Resumed::Event(event) if matches!(*event, Event::MessageFinished { .. }) => return,
            Resumed::Event(_) => {}
            Resumed::Gap { .. } => panic!("unexpected history gap"),
        }
    }
}

async fn gap(
    resumable: &Resumable,
    mut previous: EventId,
    connections: &Connections,
    busy: bool,
) -> usize {
    tokio::time::timeout(DEADLINE, async {
        let mut queued = 0;
        loop {
            let item = resumable.next().await.expect("stream remains active").expect("known wire");
            match item {
                Resumed::Event(event) => {
                    let cursor = resumable.cursor().await.expect("queued event has a cursor");
                    assert_eq!(cursor.epoch(), previous.epoch(), "event before gap: {event:?}");
                    assert!(
                        cursor.seq() > previous.seq(),
                        "queued cursor advances: {previous:?} -> {cursor:?}, event={event:?}"
                    );
                    assert_eq!(
                        connections.0.load(Ordering::SeqCst),
                        1,
                        "queued events precede reconnect"
                    );
                    assert!(
                        !busy || !matches!(*event, Event::MessageFinished { .. }),
                        "a permission-blocked turn cannot finish: {event:?}"
                    );
                    previous = cursor;
                    queued += 1;
                }
                Resumed::Gap { head } => {
                    assert_ne!(head.epoch(), previous.epoch());
                    assert!(resumable.cursor().await.is_none(), "a gap clears the old cursor");
                    assert!(
                        connections.0.load(Ordering::SeqCst) >= 2,
                        "connection loss automatically reconnects"
                    );
                    return queued;
                }
            }
        }
    })
    .await
    .expect("cold restart declares a gap")
}

async fn stored(client: &Client, session: &SessionId) -> Vec<Message> {
    client.messages(session).await.expect("stored transcript")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_severed_child_connection_replays_without_duplicate_events() {
    let server = Server::new(
        serde_json::json!({"cadence_ms":20,"turns":[{"text":"one two three four five six seven eight nine ten"}]}),
    );
    let relay = Relay::new(server.address).await;
    let client = relay.client();
    let session = client.health().await.expect("health").session_id;
    let resumable = Resumable::new(client.clone());
    resumable.connect().await.expect("registered").expect("not held");
    client.prompt(&session, &Prompt::new("say the words")).await.expect("accepted");
    next(&resumable).await;
    let before = resumable.cursor().await.expect("committed cursor");
    relay.sever();
    let mut previous = before;
    loop {
        let event = next(&resumable).await;
        let cursor = resumable.cursor().await.expect("cursor");
        assert_eq!(cursor.epoch(), previous.epoch());
        assert!(cursor.seq() > previous.seq(), "replay never yields the same event twice");
        previous = cursor;
        if matches!(&event, Resumed::Event(event) if matches!(**event, Event::MessageFinished { .. }))
        {
            break;
        }
        assert!(!matches!(event, Resumed::Gap { .. }));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_child_restart_reports_one_gap_and_preserves_the_transcript() {
    let mut server =
        Server::new(serde_json::json!({"cadence_ms":1,"turns":[{"text":"complete reply"}]}));
    let relay = Relay::new(server.address).await;
    let client = relay.client();
    let session = client.health().await.expect("health").session_id;
    let connections = Arc::new(Connections::default());
    let resumable = Resumable::new(client.clone()).with_policy(connections.clone());
    resumable.connect().await.expect("registered");
    client.prompt(&session, &Prompt::new("first prompt")).await.expect("accepted");
    finished(&resumable).await;
    let before = stored(&client, &session).await;
    let old = resumable.cursor().await.expect("cursor");
    server.restart();
    relay.retarget(server.address).await;
    gap(&resumable, old, &connections, false).await;
    let connected = resumable.connect().await.expect("fresh registration").expect("not held");
    assert_eq!(connected.replay, ganja_client::sse::Replay::Fresh);
    assert_eq!(stored(&client, &session).await, before);
    client.prompt(&session, &Prompt::new("second prompt")).await.expect("accepted after restart");
    finished(&resumable).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_child_restart_reports_a_gap_without_fabricating_a_finish() {
    let mut server = Server::new(
        serde_json::json!({"cadence_ms":100,"turns":[{"text":"one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen", "tool_calls":[{"name":"bash", "args":{"command":"echo attached"}}]}]}),
    );
    let relay = Relay::new(server.address).await;
    let client = relay.client();
    let session = client.health().await.expect("health").session_id;
    let connections = Arc::new(Connections::default());
    let resumable = Resumable::new(client.clone()).with_policy(connections.clone());
    resumable.connect().await.expect("registered");
    client.prompt(&session, &Prompt::new("interrupted prompt")).await.expect("accepted");
    loop {
        if matches!(next(&resumable).await, Resumed::Event(event) if matches!(*event, Event::PartDelta { .. }))
        {
            break;
        }
    }
    let old = resumable.cursor().await.expect("cursor");
    let mut forwarded = relay.forwarded.clone();
    tokio::time::timeout(
        DEADLINE,
        forwarded.wait_for(|state| state.deltas >= 2 && state.permission),
    )
    .await
    .expect("queued delta and unanswered permission reach the client")
    .expect("relay remains active");
    assert_eq!(client.permissions().await.expect("pending permissions").len(), 1);
    server.restart();
    relay.retarget(server.address).await;
    let queued = gap(&resumable, old, &connections, true).await;
    assert!(queued > 0, "the acknowledged old response tail is delivered before the gap");
    resumable.connect().await.expect("fresh registration");
    let messages = stored(&client, &session).await;
    assert!(messages.iter().any(|message| message.role == Role::User));
    assert!(
        messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .all(|message| message.time.completed.is_none()),
        "a killed turn is not presented as complete"
    );
}
