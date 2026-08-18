//! The cross-session socket, end to end (**D505**, AC-9): a message
//! addressed `uds:<path>` crosses from one process into another session's
//! next turn; nothing structured crosses; a socket directory that is not a
//! private one of ours is refused; and `ganja sessions --live` lists the
//! sockets that answer, unlinks the ones nobody serves, and leaves a held
//! name alone however silent it is.
//!
//! This is the one binary that holds **both processes together**: the far end
//! of every socket here is a real `ganja_serve::serve` over a real engine that
//! leads a real team, and the sender in the first test is a second process —
//! this very binary, re-executed the way `ganja-team/tests/contention.rs`
//! re-executes itself, driving the landed `uds:` arm of `ganja-core`'s
//! postbox from a registry of its own. That arm is one layer under the
//! `send_message` tool and is exactly what the tool calls, and it is driven
//! directly rather than through a scripted turn because the shipped headless
//! binary (`ganja run`) installs no team — only the terminal UI does — so no
//! child `ganja` could reach the tool, and a scripted in-process turn would
//! only wrap the same call in a provider this crate does not build.
//!
//! **What this binary cannot hold**, in the standard `ganja-serve/tests/uds.rs`
//! set for itself: the other-uid legs. Everything here runs as one uid and
//! nothing fakes another, so a directory owned by somebody else and a peer
//! whose uid is not ours cannot be made without privilege. Those verdicts are
//! pinned where they are decided — `ganja-tool/src/socket.rs`'s pure `vet`,
//! each arm a unit test, and `ganja-serve/src/socket.rs`'s `PeerChecked` on a
//! real accept against a test-only uid — and what is held here is the
//! *mode* arm on a real directory, all the way through the client and through
//! the shipped `sessions --live`.
//!
//! Every socket directory is a fresh `tempfile` one at `0700`, never the real
//! `/tmp/ganja-<uid>/`: one test here loosens a directory and another unlinks
//! files inside one, and neither is a thing to do to the developer's own —
//! which is what the hidden `--socket-dir` door on `sessions --live` exists
//! for. The child processes get their environment on the `Command`, never
//! through `set_var`, so this binary may hold its six tests.

#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use futures::StreamExt as _;
use ganja_client::Client;
use ganja_core::{
    Engine, Postbox,
    provider::fake::FakeProvider,
    teammate::{TeammateRegistry, lead_inbox::LeadInbox},
    tool::{
        Registry,
        team::{Address, Body, Postbox as _, Undelivered},
    },
};
use ganja_permission::Permissions;
use ganja_protocol::{Command, Event, PartBody, Role, SessionId, team::PeerPayload};
use ganja_serve::{
    Handle, Listen, ServeConfig,
    socket::{self, EXTENSION, LOCK_EXTENSION},
};
use tempfile::TempDir;

/// How long any single wait here — a child process, an event, a health
/// answer — may take before the fixture is declared broken.
const DEADLINE: Duration = Duration::from_secs(60);

/// The environment the re-executed sender reads its role from: the socket to
/// deliver to, the config home its own registry lives under, and the file it
/// writes what the far side answered into. Set on the child's `Command`
/// alone; a run without them is the parent.
const SEND_TO: &str = "GANJA_UDS_TEST_SEND_TO";
const SENDER_HOME: &str = "GANJA_UDS_TEST_SENDER_HOME";
const SENDER_REPORT: &str = "GANJA_UDS_TEST_SENDER_REPORT";

/// The session the sending process leads. A fixed id, so the team its
/// registry derives — and the identity it stamps on what it sends — is a
/// thing the parent can spell without asking the child.
const SENDER_SESSION: &str = "0198c1a2-9999-7d5e-8f60-718293a4b5c6";

/// What the sender says.
const TEXT: &str = "the release is out; pick up W8 when you are idle";
const SUMMARY: &str = "release";

/// A fresh directory at `0700` — what a socket directory has to be, where
/// `tempdir()`'s default is whatever the umask leaves.
fn private_dir() -> TempDir {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .expect("a private directory")
}

/// An engine that leads a team under `home` — the shape the terminal UI
/// installs for the session it opens — on a fake provider that answers one
/// word, with the registry beside it for the assertions that read the team.
fn led_engine(home: &Path) -> (Arc<Engine>, Arc<TeammateRegistry>) {
    let engine = Engine::new(
        Arc::new(FakeProvider::new("thanks", Duration::ZERO)),
        "canned",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    let cwd = env::current_dir().expect("the working directory resolves");
    let registry = Arc::new(TeammateRegistry::for_session(
        home,
        engine.session_id().as_str(),
        cwd,
    ));

    (
        Arc::new(engine.with_teammates(Arc::clone(&registry))),
        registry,
    )
}

/// `engine`'s own session socket, bound under `directory` — `Listen::Session`,
/// the per-session door, so the name is the one the scheme gives it.
async fn serve_session(engine: &Arc<Engine>, directory: &Path) -> Handle {
    let mut config =
        ServeConfig::in_directory(env::current_dir().expect("the working directory resolves"));
    config.listen = Listen::Session {
        id: engine.session_id(),
        directory: directory.to_path_buf(),
    };

    ganja_serve::serve(Arc::clone(engine), config)
        .await
        .expect("a session socket comes up in a private directory")
}

/// The socket path a handle is bound on — every server here binds one.
fn bound(handle: &Handle) -> PathBuf {
    match handle.address() {
        ganja_serve::Address::Unix(path) => path.clone(),
        ganja_serve::Address::Tcp(address) => {
            panic!("a socket was asked for, and tcp {address} came up")
        }
    }
}

/// The lead's inbox under `registry`, read as the JSON array §2.3 stores:
/// an inbox nothing has written is an absent file, and empty.
fn lead_inbox(registry: &TeammateRegistry) -> Vec<serde_json::Value> {
    match fs::read(registry.lead_inbox()) {
        Ok(bytes) => serde_json::from_slice(&bytes).expect("an inbox is a JSON array"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("the inbox does not read: {error}"),
    }
}

/// A socket file nobody serves: bound, then dropped. The kernel keeps the
/// file; nothing accepts behind it.
fn dead_socket(path: &Path) {
    let listener = std::os::unix::net::UnixListener::bind(path).expect("the dead socket binds");
    drop(listener);
    assert!(path.exists(), "dropping a listener leaves its file behind");
}

/// The shipped `ganja sessions --live` over `directory`, in its own homes
/// so its log lands nowhere near the developer's — a `tokio` child, because
/// the servers it probes are tasks on this test's own runtime and a blocking
/// wait would starve them.
fn sessions_live(directory: &Path, homes: &TempDir) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        .args(["sessions", "--live", "--socket-dir"])
        .arg(directory)
        .env("XDG_DATA_HOME", homes.path())
        .env("XDG_CONFIG_HOME", homes.path())
        .env("HOME", homes.path())
        .env_remove("GANJA_CONFIG_HOME")
        .env_remove("GANJA_CONFIG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    command
}

/// The sender's half of the first test, in the child process: a registry of
/// its own under its own home, and the landed `uds:` arm driven through the
/// lead's postbox — `GET /team` to learn who leads the far session, then
/// `POST /team/{lead}/message` stamped `<name>@<team>`. What the far side
/// answered is written to the report file for the parent to read.
async fn send_as_child(socket: PathBuf, home: PathBuf, report: PathBuf) {
    let cwd = env::current_dir().expect("the working directory resolves");
    let registry = Arc::new(TeammateRegistry::for_session(&home, SENDER_SESSION, cwd));
    let postbox = Postbox::lead(&registry);

    let sent = tokio::time::timeout(
        DEADLINE,
        postbox.deliver(
            Address::Uds { path: socket },
            Body::Text {
                text: TEXT.to_owned(),
                summary: Some(SUMMARY.to_owned()),
            },
        ),
    )
    .await
    .expect("the socket answers within the deadline")
    .expect("a listening session takes the message");

    fs::write(
        &report,
        serde_json::json!({ "to": sent.to, "note": sent.note }).to_string(),
    )
    .expect("the report writes");
}

// ---------------------------------------------------------------------------
// AC-9: the four
// ---------------------------------------------------------------------------

/// Two processes, one socket: the receiver — this test — serves its
/// session's socket and leads a team; the sender — this binary re-executed —
/// delivers through the landed `uds:` arm from a team of its own. The
/// message lands in the receiver's lead inbox stamped with the sender's
/// derived identity, the lead's own §6.2 pass hands it back as a delivery,
/// and that delivery becomes a peer part on the receiver's next turn — the
/// same three steps the terminal UI's tick takes, so what is pinned is the
/// whole road and not one stone of it.
#[tokio::test]
async fn a_message_addressed_uds_reaches_the_peers_next_turn() {
    // The child's role, when this binary was re-executed as the sender.
    if let (Some(socket), Some(home), Some(report)) = (
        env::var_os(SEND_TO),
        env::var_os(SENDER_HOME),
        env::var_os(SENDER_REPORT),
    ) {
        send_as_child(socket.into(), home.into(), report.into()).await;

        return;
    }

    let directory = private_dir();
    let receiver_home = TempDir::new().expect("a home for the receiving team");
    let sender_home = TempDir::new().expect("a home for the sending team");
    let (engine, registry) = led_engine(receiver_home.path());
    let handle = serve_session(&engine, directory.path()).await;
    let socket = bound(&handle);
    let mut events = engine.subscribe().await.expect("a subscriber joins");

    let report = sender_home.path().join("sent.json");
    let status = tokio::time::timeout(
        DEADLINE,
        tokio::process::Command::new(env::current_exe().expect("a test binary knows its own path"))
            .args([
                "a_message_addressed_uds_reaches_the_peers_next_turn",
                "--exact",
                "--test-threads=1",
            ])
            .env(SEND_TO, &socket)
            .env(SENDER_HOME, sender_home.path())
            .env(SENDER_REPORT, &report)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .expect("the sender finishes within the deadline")
    .expect("the sender process is waitable");
    assert!(status.success(), "the sender failed: {status}");

    // What the sender was told: it reached the far session's lead, named in
    // that session's terms.
    let sent: serde_json::Value =
        serde_json::from_slice(&fs::read(&report).expect("the sender wrote its report"))
            .expect("the report is JSON");
    assert_eq!(
        sent["to"],
        format!("team-lead@{}", registry.team()),
        "the sender is told which session's lead it reached: {sent}"
    );
    assert!(
        sent["note"].as_str().is_some_and(|note| !note.is_empty()),
        "and what became of the message: {sent}"
    );

    // What landed: the message, in the receiver's lead inbox, stamped with the
    // sender's derived identity — never a bare name this team could mistake
    // for one of its own.
    let cwd = env::current_dir().expect("the working directory resolves");
    let sender_identity = format!(
        "team-lead@{}",
        TeammateRegistry::for_session(sender_home.path(), SENDER_SESSION, cwd).team()
    );
    let inbox = lead_inbox(&registry);
    assert_eq!(inbox.len(), 1, "one message landed: {inbox:?}");
    assert_eq!(inbox[0]["from"], sender_identity);
    assert_eq!(inbox[0]["text"], TEXT);
    assert_eq!(inbox[0]["summary"], SUMMARY);

    // The lead's own pass over its inbox hands the message back as a
    // delivery, which is what a frontend puts on the next turn.
    let pass = LeadInbox::reading(Arc::clone(&registry), None).poll().await;
    assert_eq!(
        pass.messages.len(),
        1,
        "the pass hands back the one plain message: {pass:?}"
    );
    let delivered = &pass.messages[0];
    assert_eq!(delivered.from, sender_identity);
    assert_eq!(delivered.body, TEXT);
    assert_eq!(delivered.summary.as_deref(), Some(SUMMARY));

    // The next turn: the delivery, as the terminal UI would send it, becomes
    // a peer part on the user's own message — the receiver's model reads it.
    engine
        .send(Command::SendPrompt {
            text: String::new(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: vec![PeerPayload::new(
                &delivered.from,
                delivered.summary.clone(),
                delivered.color.clone(),
                &delivered.body,
            )],
        })
        .await
        .expect("an idle engine takes the next turn");
    let user = tokio::time::timeout(DEADLINE, async {
        loop {
            match events.next().await {
                Some(Event::MessageStarted { message, .. }) if message.role == Role::User => {
                    break message;
                }
                Some(_) => {}
                None => panic!("the event stream ended before the turn opened"),
            }
        }
    })
    .await
    .expect("the next turn opens within the deadline");
    assert!(
        user.parts.iter().any(|part| matches!(
            &part.body,
            PartBody::Peer { from, body, summary, .. }
                if *from == sender_identity && body == TEXT && summary.as_deref() == Some(SUMMARY)
        )),
        "the sender's words are a peer part of the receiver's next turn: {:?}",
        user.parts
    );

    assert!(
        engine.settle(DEADLINE).await,
        "the fake provider's turn settles"
    );
    handle.shutdown().await.expect("a clean stop");
}

/// Nothing structured crosses the socket (§5.2-6), and the sender is told so
/// in typed form: the landed core arm refuses a frame *body* before any
/// connection is tried, and reports the server's `400` for a frame smuggled
/// as text. Neither writes a byte into the receiver's inbox.
#[tokio::test]
async fn a_structured_message_does_not_cross_a_socket() {
    let directory = private_dir();
    let receiver_home = TempDir::new().expect("a home for the receiving team");
    let sender_home = TempDir::new().expect("a home for the sending team");
    let (engine, registry) = led_engine(receiver_home.path());
    let handle = serve_session(&engine, directory.path()).await;
    let socket = bound(&handle);

    let frame = serde_json::json!({
        "type": "shutdown_request",
        "requestId": "r1",
        "from": "team-lead",
        "reason": "done",
    });

    // The core arm, a frame as the body: refused as a rule, before a client
    // is even built.
    let cwd = env::current_dir().expect("the working directory resolves");
    let sender = Arc::new(TeammateRegistry::for_session(
        sender_home.path(),
        SENDER_SESSION,
        cwd,
    ));
    let postbox = Postbox::lead(&sender);
    let refused = postbox
        .deliver(
            Address::Uds {
                path: socket.clone(),
            },
            Body::Frame(frame.clone()),
        )
        .await;
    let Err(Undelivered::Failed { reason }) = refused else {
        panic!("a frame body is refused, not {refused:?}");
    };
    assert!(
        reason.contains("does not cross a socket"),
        "the refusal names the rule: {reason}"
    );

    // The core arm, a frame as the text: the server classifies it and answers
    // 400, and that answer reaches the sender with the server's own sentence.
    let refused = tokio::time::timeout(
        DEADLINE,
        postbox.deliver(
            Address::Uds {
                path: socket.clone(),
            },
            Body::Text {
                text: frame.to_string(),
                summary: None,
            },
        ),
    )
    .await
    .expect("the socket answers within the deadline");
    let Err(Undelivered::Failed { reason }) = refused else {
        panic!("a frame in the text is refused by the far side, not {refused:?}");
    };
    assert!(
        reason.contains("(400)")
            && reason.contains("does not cross a socket")
            && reason.contains("shutdown_request"),
        "the far side's refusal, status and sentence and frame: {reason}"
    );

    assert!(
        lead_inbox(&registry).is_empty(),
        "and nothing was written: {:?}",
        lead_inbox(&registry)
    );
    handle.shutdown().await.expect("a clean stop");
}

/// A socket directory that is not a private one of ours is refused by the
/// shipped listing — by name, mode and requirement — and nothing inside it
/// is unlinked. Held on the **mode** arm and the **link** arm, which one uid
/// can make. The binder's own refusal of the same directory is
/// `ganja-serve/tests/uds.rs`'s
/// (`a_socket_directory_that_is_not_private_is_refused_naming_its_mode`),
/// and a client at a name nothing serves is
/// `ganja-client/tests/socket.rs`'s. AC-9's other-uid leg is *not* what this
/// test holds, and its name says so: the *owner* arm — the `/tmp` squat —
/// cannot be raised without a second uid and is pinned as the pure `vet`'s
/// unit test in `ganja-tool/src/socket.rs`; the peer whose uid is not ours
/// is pinned on a real accept, against a test-only uid, in
/// `ganja-serve/src/socket.rs` (`a_connection_from_another_uid_is_closed_unread`).
#[tokio::test]
async fn a_socket_directory_that_is_not_private_is_refused_end_to_end() {
    let homes = TempDir::new().expect("homes for the listing");

    // A directory anybody may read: what a socket directory must never be.
    let loose = tempfile::tempdir().expect("a directory");
    fs::set_permissions(loose.path(), fs::Permissions::from_mode(0o755))
        .expect("the test may loosen its own directory");
    // Something inside it that a listing must not touch when it refuses.
    let planted = loose.path().join(format!("0198c1a2.{EXTENSION}"));
    dead_socket(&planted);

    // The shipped listing refuses the directory by name, mode and
    // requirement — and unlinks nothing inside it.
    let output = tokio::time::timeout(DEADLINE, sessions_live(loose.path(), &homes).output())
        .await
        .expect("the listing finishes within the deadline")
        .expect("the listing runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a loose directory is refused, not listed: {stderr}"
    );
    assert!(
        stderr.contains(&loose.path().display().to_string())
            && stderr.contains("0755")
            && stderr.contains("0700"),
        "the sentence says which directory, what it is, and what it must be: {stderr}"
    );
    assert!(
        planted.exists(),
        "nothing inside a refused directory is unlinked"
    );

    // A link to a perfectly good private directory is refused too — a link
    // is what somebody plants in a world-writable /tmp — and nothing behind
    // it is touched.
    let real = private_dir();
    let behind = real.path().join(format!("0198c1a3.{EXTENSION}"));
    dead_socket(&behind);
    let parent = tempfile::tempdir().expect("a directory");
    let link = parent.path().join("ganja-link");
    std::os::unix::fs::symlink(real.path(), &link).expect("the link is made");
    let output = tokio::time::timeout(DEADLINE, sessions_live(&link, &homes).output())
        .await
        .expect("the listing finishes within the deadline")
        .expect("the listing runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a link is refused, however good its target: {stderr}"
    );
    assert!(
        stderr.contains(&link.display().to_string()) && stderr.contains("not a directory"),
        "the sentence names the link: {stderr}"
    );
    assert!(behind.exists(), "and nothing behind the link was unlinked");
}

/// `ganja sessions --live` over a directory holding one socket a session
/// serves and one nobody does: the living one is listed under the session
/// its server reports — the id, not the eight-digit stem — and the dead
/// file is gone afterwards, while every `.lock` file stays exactly where the
/// socket module's design leaves it.
#[tokio::test]
async fn sessions_live_lists_the_living_and_unlinks_the_dead() {
    let directory = private_dir();
    let homes = TempDir::new().expect("homes for the listing");
    let (engine, _registry) = led_engine(homes.path());
    let handle = serve_session(&engine, directory.path()).await;
    let living = bound(&handle);
    let living_lock = socket::lock_path(&living);
    assert!(
        living_lock.exists(),
        "a served socket has its lock beside it"
    );

    // The dead one: bound and dropped, with the lock file a binder would
    // have left — unheld, since its holder is gone.
    let dead = directory.path().join(format!("deadbeef.{EXTENSION}"));
    dead_socket(&dead);
    let dead_lock = dead.with_extension(LOCK_EXTENSION);
    fs::write(&dead_lock, b"").expect("the stale lock file writes");

    // Which session the living socket is: asked of the server, exactly as
    // the listing asks it.
    let health = Client::on_socket(&living)
        .expect("a socket client builds")
        .health()
        .await
        .expect("the living socket answers");
    assert_eq!(health.session_id, engine.session_id());

    let output = tokio::time::timeout(DEADLINE, sessions_live(directory.path(), &homes).output())
        .await
        .expect("the listing finishes within the deadline")
        .expect("the listing runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the listing succeeds:\n{stdout}\n{stderr}"
    );

    // The living, listed under its session and its socket.
    let session: &SessionId = &health.session_id;
    let row = stdout
        .lines()
        .find(|line| line.contains(session.as_str()))
        .unwrap_or_else(|| panic!("the living session is listed by id:\n{stdout}"));
    assert!(
        row.contains(&living.display().to_string()),
        "on the same row as its socket: {row}"
    );
    assert!(
        stdout.contains("SESSION") && stdout.contains("SOCKET"),
        "under the listing's own header:\n{stdout}"
    );
    assert!(
        !stdout.contains(&dead.display().to_string()),
        "the dead socket is not listed:\n{stdout}"
    );

    // The dead, gone — and named on the way out — while its lock stays.
    assert!(!dead.exists(), "the dead socket file was unlinked");
    assert!(
        stderr.contains("removed the dead socket") && stderr.contains(&dead.display().to_string()),
        "the removal is said, by path: {stderr}"
    );
    assert!(
        dead_lock.exists(),
        "the dead socket's lock file is left: lock files are never removed"
    );
    assert!(living.exists(), "the living socket is untouched");
    assert!(living_lock.exists(), "and so is its lock");

    handle.shutdown().await.expect("a clean stop");
}

/// The branch the design exists for: a socket that does not answer while a
/// live binder holds its name's lock is **not** dead — a live server whose
/// accept backlog is full refuses a connection exactly as an empty file does
/// — so the listing leaves it where it is, lists it as live under a `(held)`
/// mark of its own, and explains itself on stderr. The holder is a
/// process other than the listing's — this test's own, the `flock` taken and
/// kept across the whole run — which is what an advisory lock is judged
/// against; a socket nobody accepts behind stands in for the full backlog,
/// since it reaches the same silent-socket branch and needs no platform to
/// block a connect (`ganja-serve/tests/uds.rs` makes the same substitution
/// for the same reason).
#[tokio::test]
async fn a_held_name_that_does_not_answer_is_listed_and_left_in_place() {
    let directory = private_dir();
    let homes = TempDir::new().expect("homes for the listing");
    let held = directory.path().join(format!("0198c1a2.{EXTENSION}"));
    dead_socket(&held);
    let lock_file = socket::lock_path(&held);
    // The binder's own token, held for as long as the flock must stand.
    let lock = socket::NameLock::claim(&held)
        .expect("the lock file opens")
        .expect("the test takes the name's lock first");

    let output = tokio::time::timeout(DEADLINE, sessions_live(directory.path(), &homes).output())
        .await
        .expect("the listing finishes within the deadline")
        .expect("the listing runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the listing succeeds:\n{stdout}\n{stderr}"
    );

    assert!(
        held.exists(),
        "a held name is never unlinked, whatever the silence: {stderr}"
    );
    assert!(lock_file.exists(), "nor is its lock");
    assert!(
        stderr.contains("held by a live server that did not answer")
            && stderr.contains(&held.display().to_string()),
        "the silence is explained, by path: {stderr}"
    );
    let row = stdout
        .lines()
        .find(|line| line.contains(&held.display().to_string()))
        .unwrap_or_else(|| panic!("a held socket is listed as live:\n{stdout}"));
    assert!(
        row.trim_start().starts_with("(held)"),
        "under the held mark — nothing answered, which is not an unreadable answer: {row}"
    );
    assert!(
        !stdout.contains("no live sessions"),
        "stdout and stderr tell one story:\n{stdout}"
    );

    drop(lock);
}

/// The walk is seconds long — one silent socket burns the whole health
/// deadline — and a peer that stops inside that window unlinks its own
/// socket file exactly as `Handle::shutdown` does. A listing that read the
/// directory before and inspects the entry after must walk past the gap:
/// exit 0, the vanished entry skipped without a word, and the table about
/// the sessions that *are* running still printed. Reproduced here as it was
/// found: a stalled socket first in sort order (accepted and never answered,
/// its lock held, so the health check waits its deadline out and the lock
/// says live), a real session after it, and a dead file last that vanishes
/// while the stall is being waited on — unlinked on the walk's own connect
/// rather than a clock, so the gap cannot close before the walk is in it.
#[tokio::test]
async fn a_socket_that_vanishes_mid_walk_does_not_end_the_listing() {
    let directory = private_dir();
    let homes = TempDir::new().expect("homes for the listing");
    let (engine, _registry) = led_engine(homes.path());
    let handle = serve_session(&engine, directory.path()).await;
    let living = bound(&handle);

    // Sorts before every UUIDv7-named socket: the walk meets it first.
    let stalled = directory.path().join(format!("00000000.{EXTENSION}"));
    let silent = std::os::unix::net::UnixListener::bind(&stalled).expect("the silent socket binds");
    silent
        .set_nonblocking(true)
        .expect("the listener goes nonblocking");
    let silent =
        tokio::net::UnixListener::from_std(silent).expect("the listener joins the runtime");
    // The binder's own token, held for as long as the flock must stand.
    let lock = socket::NameLock::claim(&stalled)
        .expect("the lock file opens")
        .expect("the test takes the stalled name's lock first");

    // Sorts after: still unvisited while the stall is waited on.
    let dead = directory.path().join(format!("ffffffff.{EXTENSION}"));
    dead_socket(&dead);
    let vanishing = dead.clone();
    let unlinker = tokio::spawn(async move {
        // The health connect landing here *is* the walk past the directory
        // read and inside the stall, so the entry vanishes exactly then. The
        // accepted stream is answered nothing and handed back alive, so the
        // client waits its deadline out instead of reading a close.
        let (stream, _) = silent
            .accept()
            .await
            .expect("the listing connects to the stalled socket");
        fs::remove_file(&vanishing).expect("the peer unlinks its own socket");

        stream
    });

    let output = tokio::time::timeout(DEADLINE, sessions_live(directory.path(), &homes).output())
        .await
        .expect("the listing finishes within the deadline")
        .expect("the listing runs");
    let _stalling = unlinker.await.expect("the unlinker ran");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "a vanished entry does not end the listing:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains(&engine.session_id().as_str().to_owned())
            && stdout.contains(&living.display().to_string()),
        "the session that is running is still listed:\n{stdout}"
    );
    assert!(
        stdout.contains("(held)") && stdout.contains(&stalled.display().to_string()),
        "and so is the stalled one, under its mark:\n{stdout}"
    );
    assert!(
        !stdout.contains(&dead.display().to_string())
            && !stderr.contains(&dead.display().to_string()),
        "the vanished entry is skipped without a word:\n{stdout}\n{stderr}"
    );
    assert!(stalled.exists(), "the stalled socket was not unlinked");

    drop(lock);
    handle.shutdown().await.expect("a clean stop");
}
