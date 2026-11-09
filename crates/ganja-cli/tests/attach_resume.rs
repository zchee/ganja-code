//! Resumption through dropped connections and real server process restarts.

#![cfg(unix)]

use std::net::SocketAddr;
use std::sync::Arc;

use ganja_client::{Client, Event, Prompt, Resumable, Resumed};
use ganja_protocol::{Message, Role, SessionId};
use tempfile::TempDir;
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

/// A stable address whose active connections can be cut independently of the child.
struct Relay {
    address: SocketAddr,
    target: Arc<RwLock<SocketAddr>>,
    cut: watch::Sender<u64>,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    async fn new(target: SocketAddr) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("relay binds");
        let address = listener.local_addr().expect("relay address");
        let target = Arc::new(RwLock::new(target));
        let (cut, _) = watch::channel(0);
        let task = tokio::spawn({
            let target = target.clone();
            let cut = cut.clone();
            async move {
                while let Ok((mut client, _)) = listener.accept().await {
                    let target = *target.read().await;
                    let mut cut = cut.subscribe();
                    tokio::spawn(async move {
                        let Ok(mut server) = TcpStream::connect(target).await else { return };
                        tokio::select! {
                            _ = cut.changed() => {}
                            _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        }
                    });
                }
            }
        });
        Self { address, target, cut, task }
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
    let resumable = Resumable::new(client.clone());
    resumable.connect().await.expect("registered");
    client.prompt(&session, &Prompt::new("first prompt")).await.expect("accepted");
    finished(&resumable).await;
    let before = stored(&client, &session).await;
    let old = resumable.cursor().await.expect("cursor");
    server.restart();
    relay.retarget(server.address).await;
    let Resumed::Gap { head } = next(&resumable).await else {
        panic!("cold restart must declare a gap")
    };
    assert_ne!(head.epoch(), old.epoch());
    let connected = resumable.connect().await.expect("fresh registration").expect("not held");
    assert_eq!(connected.replay, ganja_client::sse::Replay::Fresh);
    assert_eq!(stored(&client, &session).await, before);
    client.prompt(&session, &Prompt::new("second prompt")).await.expect("accepted after restart");
    finished(&resumable).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_child_restart_reports_a_gap_without_fabricating_a_finish() {
    let mut server = Server::new(
        serde_json::json!({"cadence_ms":100,"turns":[{"text":"one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen"}]}),
    );
    let relay = Relay::new(server.address).await;
    let client = relay.client();
    let session = client.health().await.expect("health").session_id;
    let resumable = Resumable::new(client.clone());
    resumable.connect().await.expect("registered");
    client.prompt(&session, &Prompt::new("interrupted prompt")).await.expect("accepted");
    loop {
        if matches!(next(&resumable).await, Resumed::Event(event) if matches!(*event, Event::PartDelta { .. }))
        {
            break;
        }
    }
    let old = resumable.cursor().await.expect("cursor");
    server.restart();
    relay.retarget(server.address).await;
    let Resumed::Gap { head } = next(&resumable).await else {
        panic!("cold restart declares interruption")
    };
    assert_ne!(head.epoch(), old.epoch());
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
    resumable.hold().await;
}
