//! The cross-session admission gate across a **true process boundary**
//! (**D523**–**D525**, **D532**, **D534**): what a person's review does to a
//! message one running `ganja` sent another, and what the sender is told
//! about it — synchronously on the POST, and again when the hold settles.
//!
//! `crates/ganja-cli/tests/uds.rs` is this file's pattern and its neighbour:
//! that binary pins the socket crossing itself (a `uds:` address, a bare
//! name, the identity a solo sender is stamped with), and this one picks up
//! where its accept path stops — at the gate that may hold the message
//! instead, and at the receipt that follows the hold. The two share the same
//! shape deliberately: a receiving session assembled here and served on a
//! real socket, a sending session that is **this binary re-executed**, and a
//! socket directory that is a fresh `0700` `tempfile` one rather than the
//! developer's own `/tmp/ganja-<uid>/`.
//!
//! # What each side of a drill is, said plainly
//!
//! The receiver (**A**) is a **testkit-assembled engine bound to a real
//! socket**, not a launched `ganja`. That is not a shortcut around a process:
//! releasing or denying a held message is a keystroke in the `/held` dialog,
//! and a process pair cannot press it headlessly. So the parent assembles A,
//! serves its session socket, and answers the hold through
//! [`Command::SettleHeld`] — the one seam that dialog itself sends. The
//! sender (**B**) is a real second process throughout, so every byte a drill
//! asserts on crossed a socket between two operating-system processes.
//!
//! B leads a team of its own and binds its own socket, which is what a
//! *reply-capable* sender is in a shipped build: the solo postbox `uds.rs`
//! sends its bare-name drill from binds nothing at all and says so in every
//! success note, so driving a settlement back to one would mean assembling a
//! session this build cannot produce and then quoting its answer as evidence.
//!
//! # What is pinned here, and what is pinned elsewhere
//!
//! What only this binary can pin is a **receipt that was applied**. A
//! settlement is admitted against the sender's own outstanding-id registry,
//! and an id enters that registry only by this session having made a send the
//! far end held — so a suite that owns the route but not the send has nothing
//! outstanding to settle, and can assert the route's answer without ever
//! reaching what the answer was for. `ganja-serve`'s own suite is in exactly
//! that position: it can drive `POST /peer/receipt` and pin that the four id
//! cases answer identically, and it cannot make an id outstanding from that
//! crate. Here the same id makes the whole round trip — minted by a real
//! sending process, carried out on its `SocketMessage`, paired at the
//! receiver with the hold it caused, posted back when a person settles it,
//! and read by the sending session's own model — which is why drills 2 and 3
//! below assert on the settlement's `status` from B's side rather than on the
//! route's status code.
//!
//! The **cardinality** bound on a settlement's reflection — at most one
//! connect attempt, never a retry, and the outcomes that reach a third
//! session not at all — is `ganja-core`'s
//! `tests/peer_receipts.rs::a_reply_to_naming_a_third_session_is_a_bounded_reflection`,
//! which already drives it over a real `UnixListener` with a counting
//! instrument on the far end. A process pair adds nothing to that bound: a
//! connect is a socket connect whichever process owns the listener, and this
//! binary has no counter to put behind a shipped route. What the boundary
//! *does* add is pinned below — that a sender-asserted `reply_to` really
//! steers the settlement away from the sender, into a third live session that
//! is unaffected by it.
//!
//! Every drill prints what crossed, from both sides — the sender's own lines
//! marked `[sender]` and the receiver's `[drill …]` — because these four runs
//! are the E2E evidence the landing's own bundle quotes, and a bundle can
//! only quote what a test said out loud.
//!
//! The child processes get their environment on the `Command`, never through
//! `set_var`, so this binary may hold all four drills.

#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::StreamExt as _;
use ganja_core::{
    Engine,
    config::{DialogExpiry, InboundPolicy},
    protocol::Part,
    provider::fake::FakeProvider,
    teammate::{TeammateRegistry, lead_inbox::LeadInbox},
    tool::{Registry, registry},
};
use ganja_permission::Permissions;
use ganja_protocol::{
    Command, Event, HeldDecision, PartBody, PolicySource, Role, TeamView, team::PeerPayload,
};
use ganja_serve::{Handle, Listen, ServeConfig};
use tempfile::TempDir;

/// How long any single wait here — a child process, a hold, a receipt — may
/// take before the fixture is declared broken.
const DEADLINE: Duration = Duration::from_secs(60);

/// How long a receipt that must **not** arrive is given to prove it did not.
/// The receipt client's own attempt is bounded at two seconds, so a silence
/// asserted after this is a silence rather than a race won —
/// `ganja-core/tests/peer_receipts.rs`'s own reasoning, at the process level.
const GRACE: Duration = Duration::from_secs(3);

/// How often a poll for a fact another process is producing re-reads it.
const POLL: Duration = Duration::from_millis(50);

/// The environment a re-executed sender reads its whole role from. Set on the
/// child's `Command` alone; a run without them is the parent.
const DRILL_DIR: &str = "GANJA_DRILL_DIR";
const DRILL_NAME: &str = "GANJA_DRILL_NAME";
const DRILL_REPORT: &str = "GANJA_DRILL_REPORT";
/// `own` — bind a socket and name it as the reply address; `third:<path>` —
/// bind a socket but name **somebody else's** as the reply address.
const DRILL_REPLY: &str = "GANJA_DRILL_REPLY";
/// `receipt` — wait for a settlement to arrive; `silence` — wait [`GRACE`]
/// and report that none did.
const DRILL_AWAIT: &str = "GANJA_DRILL_AWAIT";

/// The name the receiving session registers under — what the sender resolves.
const RECEIVER_NAME: &str = "backend";

/// The name every team's lead is known by, so what a peer is stamped with is
/// `team-lead@<that session's team>` — the child reports the whole of it
/// rather than the parent guessing at the half it cannot derive.
const SENDER_LEAD: &str = "team-lead";

/// The session the sending process's own team is derived from. A **fixed**
/// id, and not the engine's own: two v7 ids minted seconds apart share their
/// leading bytes, so two teams named from live ids would render the same
/// `session-<stem>` and an identity assertion across them could pass without
/// meaning anything. `uds.rs` fixes its sender's id for the same reason.
const SENDER_SESSION: &str = "0198c1a2-8888-7d5e-8f60-718293a4b5c6";

/// What the sender says. Appears nowhere else in this binary.
const TEXT: &str = "the parity lane is green; the gate is yours to answer";

/// What a held answer's prose says, whatever the cause
/// (`ganja-core`'s `subagent::held_note`).
const HELD_PROSE: &str = "held for a person's review";

/// The cause an explicitly configured hold names, in that same sentence.
const HELD_CAUSE: &str = "an explicit hold policy from its global config";

// ---------------------------------------------------------------------------
// Fixtures — uds.rs's, re-declared because a test binary shares nothing
// ---------------------------------------------------------------------------

/// A fresh directory at `0700` — what a socket directory has to be, where
/// `tempdir()`'s default is whatever the umask leaves.
fn private_dir() -> TempDir {
    tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .expect("a private directory")
}

/// A receiving session: an engine that leads a team under `home` — the shape
/// the terminal UI installs — on a fake provider, under `policy` as the
/// admission gate's own configured value, with the registry beside it for the
/// assertions that read the team.
fn receiving_engine(
    home: &Path,
    policy: Option<(InboundPolicy, PolicySource)>,
) -> (Arc<Engine>, Arc<TeammateRegistry>) {
    let engine = Engine::new(
        Arc::new(FakeProvider::new("thanks", Duration::ZERO)),
        "canned",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    // The seam a loaded `cross_session_inbound` key lands on. Named here
    // rather than written into a `ganja.toml` because this engine is
    // assembled rather than launched: the config loader's own refusals are
    // `ganja-core`'s to pin, and what this drill is about is what the gate
    // does once it holds the value.
    .with_inbound_policy(policy, DialogExpiry::default());
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

/// The stem a bound socket's registration record is filed under.
fn stem_of(socket: &Path) -> String {
    socket
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("the bound socket has a stem")
        .to_owned()
}

/// The lead's inbox under `registry`, read as the JSON array §2.3 stores: an
/// inbox nothing has written is an absent file, and empty.
fn lead_inbox(registry: &TeammateRegistry) -> Vec<serde_json::Value> {
    match fs::read(registry.lead_inbox()) {
        Ok(bytes) => serde_json::from_slice(&bytes).expect("an inbox is a JSON array"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("the inbox does not read: {error}"),
    }
}

/// A D527 registration record at `stem`, named `name`, written exactly as a
/// lead's TUI writes one on `Synced::Bound` — the fixture the sender resolves
/// a bare name against, standing in for `ganja-tui`'s own writer the way
/// [`receiving_engine`] stands in for a launched session's engine.
fn write_registered(directory: &Path, stem: &str, name: &str) {
    registry::write(
        directory,
        stem,
        &registry::Record {
            format: registry::FORMAT,
            session_id: format!("{stem}-0000-7000-8000-000000000001"),
            name: name.to_owned(),
            name_source: registry::NameSource::User,
            cwd: "/work".into(),
            root: "/work".into(),
            pid: 4242,
            started_at: 1_756_150_000_000,
        },
    )
    .expect("a registry record writes");
}

/// The team a receiving session leads, for the roster assertions.
fn team_of(engine: &Engine) -> TeamView {
    engine
        .team_view()
        .expect("a receiving session in these drills always leads a team")
}

// ---------------------------------------------------------------------------
// The sender, in the child process
// ---------------------------------------------------------------------------

/// A `send_message` tool call, one turn's worth — `uds.rs`'s own idiom, since
/// `SoloPostbox` itself is crate-private and unreachable from here.
fn send_call(to: &str, message: &str) -> Vec<ganja_core::provider::ProviderEvent> {
    ganja_testkit::tool_call(
        ganja_core::tool::send_message::ID,
        serde_json::json!({ "to": to, "message": message }),
    )
}

/// Where a sending session names its reply address.
enum ReplyTo {
    /// Its own bound socket — the ordinary case, and the only one a shipped
    /// `ganja` produces.
    Own,
    /// Somebody else's — the reflection primitive, driven through
    /// `Engine::set_peer_address` because that cell is exactly what a
    /// sender-asserted `reply_to` is read from, and asserting a third
    /// session's address there is what a hostile sender would do.
    Third(PathBuf),
}

/// What a sending session does after its send has been answered.
enum Await {
    /// Wait for a settlement to arrive, then take one more turn so the
    /// model-facing rendering can be read off the request it composes.
    Receipt,
    /// Wait [`GRACE`] and report that nothing arrived.
    Silence,
}

/// The whole sending half, in the child process: a session over the shared
/// `--socket-dir` with its own socket bound and served (so a settlement has
/// somewhere to land), resolving the receiver by **bare name** — nothing here
/// types a `uds:` address — and reporting four things to the parent: the
/// identity it stamps, what its send was answered with synchronously, what
/// settlement arrived afterwards if any, and what its own model read on the
/// next turn.
async fn send_as_child(
    directory: PathBuf,
    name: String,
    report: PathBuf,
    reply: ReplyTo,
    wait: Await,
) {
    let (provider, requests) = ganja_testkit::ScriptedProvider::new(vec![
        send_call(&name, TEXT),
        ganja_testkit::says("sent"),
    ]);
    let home = TempDir::new().expect("a home for the sending team");
    let engine = Engine::new(
        Arc::clone(&provider) as Arc<dyn ganja_core::provider::Provider>,
        "canned",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_socket_directory(directory.clone());
    let sender_registry = Arc::new(TeammateRegistry::for_session(
        home.path(),
        SENDER_SESSION,
        env::current_dir().expect("the working directory resolves"),
    ));
    // A session that **leads a team**, deliberately, rather than the solo
    // postbox `uds.rs`'s own bare-name drill sends from: what these drills
    // need is a sender that is *reply-capable*, and in a shipped build a bound
    // socket and a team are the same condition — the solo arm binds nothing at
    // all, and its every send says so in as many words. Driving the receipt
    // half from a solo postbox would mean assembling a session this build
    // cannot produce and then quoting its answer as evidence.
    let engine = Arc::new(engine.with_teammates(Arc::clone(&sender_registry)));

    // A sender binds and serves its own socket for one reason: a receipt is a
    // `POST` back, and a session with nowhere to be posted to emits no
    // `reply_to` and registers no outstanding id at all (**AC-32**).
    let handle = serve_session(&engine, &directory).await;
    let own = bound(&handle);
    match &reply {
        ReplyTo::Own => engine.set_peer_address(Some(&own)),
        ReplyTo::Third(path) => engine.set_peer_address(Some(path)),
    }

    let seen: Arc<Mutex<Vec<Event>>> = Arc::default();
    let mut stream = engine.subscribe().await.expect("a subscriber joins");
    let sink = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            sink.lock()
                .expect("the child's event log is never poisoned")
                .push(event);
        }
    });

    engine
        .send(Command::SendPrompt {
            text: "go".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts the prompt");
    assert!(engine.settle(DEADLINE).await, "the send's turn settles");

    let answered = tool_output(&seen).expect("the send_message call completed");
    println!("[sender] the send was answered synchronously with: {answered}");

    let settlement = match wait {
        Await::Receipt => {
            let receipt = receipt_within(&seen, DEADLINE)
                .await
                .expect("a settlement reaches a reply-capable sender within the deadline");
            println!("[sender] and a settlement followed: {receipt}");
            Some(receipt)
        }
        Await::Silence => {
            tokio::time::sleep(GRACE).await;
            let quiet = receipt_now(&seen);
            println!("[sender] after the grace, settlements seen: {quiet:?}");
            quiet
        }
    };

    // One more turn, so what the model actually read is a thing the parent can
    // assert on rather than infer.
    provider.push(ganja_testkit::says("noted"));
    engine
        .send(Command::SendPrompt {
            text: "and now".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts the second prompt");
    assert!(engine.settle(DEADLINE).await, "the reading turn settles");

    fs::write(
        &report,
        serde_json::json!({
            "answered": answered,
            "settlement": settlement,
            "intake": newest_user_text(&requests),
            "socket": own.display().to_string(),
            "identity": format!("{SENDER_LEAD}@{}", sender_registry.team()),
        })
        .to_string(),
    )
    .expect("the report writes");

    handle.shutdown().await.expect("a clean stop");
}

/// What the one `send_message` call in the child's transcript was answered
/// with — the tool's own output line, which is where a held answer's note
/// arrives.
fn tool_output(seen: &Arc<Mutex<Vec<Event>>>) -> Option<String> {
    seen.lock()
        .expect("the child's event log is never poisoned")
        .iter()
        .find_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    tool,
                    state: ganja_protocol::ToolState::Completed { output, .. },
                    ..
                } if tool == ganja_core::tool::send_message::ID => Some(output.clone()),
                _ => None,
            },
            _ => None,
        })
}

/// The settlement this session has been told about, as the parent reads it:
/// `{status, to, id}`, or [`None`] while nothing has arrived.
fn receipt_now(seen: &Arc<Mutex<Vec<Event>>>) -> Option<serde_json::Value> {
    seen.lock()
        .expect("the child's event log is never poisoned")
        .iter()
        .find_map(|event| match event {
            Event::PeerReceipt { id, status, to, .. } => Some(serde_json::json!({
                "id": id.as_str(),
                "status": status,
                "to": to,
            })),
            _ => None,
        })
}

/// The same, waited for.
async fn receipt_within(
    seen: &Arc<Mutex<Vec<Event>>>,
    within: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Some(receipt) = receipt_now(seen) {
            return Some(receipt);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The text of the **newest user message** in the last request — the bytes
/// that turn's own intake added, rather than the whole transcript.
fn newest_user_text(requests: &Arc<Mutex<Vec<ganja_core::provider::ChatRequest>>>) -> String {
    requests
        .lock()
        .expect("the request log is never poisoned")
        .last()
        .expect("at least one request")
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .expect("every request carries a user message")
        .parts
        .iter()
        .filter_map(Part::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// The parent's side of a re-execution
// ---------------------------------------------------------------------------

/// This binary, re-executed as the sender of `test`, left running so the
/// parent can answer the hold it is waiting on.
fn spawn_sender(
    test: &str,
    directory: &Path,
    report: &Path,
    reply: &str,
    wait: &str,
) -> tokio::process::Child {
    tokio::process::Command::new(env::current_exe().expect("a test binary knows its own path"))
        .args([test, "--exact", "--test-threads=1", "--nocapture"])
        .env(DRILL_DIR, directory)
        .env(DRILL_NAME, RECEIVER_NAME)
        .env(DRILL_REPORT, report)
        .env(DRILL_REPLY, reply)
        .env(DRILL_AWAIT, wait)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("the sender process starts")
}

/// The child's whole role, when this binary was re-executed as one — [`None`]
/// in the parent, which is what the tests below branch on.
fn child_role() -> Option<(PathBuf, String, PathBuf, ReplyTo, Await)> {
    let (directory, name, report) = (
        env::var_os(DRILL_DIR)?,
        env::var_os(DRILL_NAME)?,
        env::var_os(DRILL_REPORT)?,
    );
    let reply = env::var(DRILL_REPLY).expect("a re-executed sender is told where to be answered");
    let wait = env::var(DRILL_AWAIT).expect("a re-executed sender is told what to wait for");

    Some((
        directory.into(),
        name.into_string().expect("the target name is utf-8"),
        report.into(),
        match reply.strip_prefix("third:") {
            Some(path) => ReplyTo::Third(PathBuf::from(path)),
            None => ReplyTo::Own,
        },
        match wait.as_str() {
            "receipt" => Await::Receipt,
            "silence" => Await::Silence,
            other => panic!("no such wait: {other}"),
        },
    ))
}

/// Waits until `engine` is holding a message for review — the fact the
/// `/held` dialog is drawn from, polled here because the person that dialog
/// waits for is this test.
async fn eventually_held(engine: &Engine) -> ganja_core::teammate::inbound::HeldEntry {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if let Some(entry) = engine.held_messages().into_iter().next() {
            return entry;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sender's message never reached the gate's hold arm"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Waits for a re-executed sender to finish, and reads what it reported.
async fn reported(mut child: tokio::process::Child, report: &Path) -> serde_json::Value {
    let status = tokio::time::timeout(DEADLINE, child.wait())
        .await
        .expect("the sender finishes within the deadline")
        .expect("the sender process is waitable");
    assert!(status.success(), "the sender failed: {status}");

    serde_json::from_slice(&fs::read(report).expect("the sender wrote its report"))
        .expect("the report is JSON")
}

/// The one message the lead's own §6.2 pass hands back, put on the receiving
/// session's next turn exactly as the terminal UI's tick does, and asserted to
/// have reached its model as a peer part.
async fn reaches_the_model(engine: &Arc<Engine>, registry: &Arc<TeammateRegistry>) -> String {
    let pass = LeadInbox::reading(Arc::clone(registry), None).poll().await;
    assert_eq!(
        pass.messages.len(),
        1,
        "the lead's pass hands back the one released message: {pass:?}"
    );
    let delivered = &pass.messages[0];

    let mut events = engine.subscribe().await.expect("a subscriber joins");
    engine
        .send(Command::SendPrompt {
            text: String::new(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
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

    let peer = user
        .parts
        .iter()
        .find_map(|part| match &part.body {
            PartBody::Peer { from, body, .. } if body == TEXT => Some(from.clone()),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "the sender's words are a peer part of the next turn: {:?}",
                user.parts
            )
        });
    assert!(
        engine.settle(DEADLINE).await,
        "the fake provider's turn settles"
    );

    peer
}

// ---------------------------------------------------------------------------
// Drill A′ — the ordinary arm, unchanged
// ---------------------------------------------------------------------------

/// **Drill A′, the regression pin.** A session with a config home and **zero
/// teammates** is addressed by bare name from a second real process, and the
/// message reaches its model through the team path exactly as it did before
/// this landing.
///
/// The two landed `uds.rs` drills each hold one half of this: the bare-name
/// one stops at the receiver's inbox on disk, and the full-road one addresses
/// a `uds:` path from a sender that leads a team of its own. What is new is
/// the union of the two, plus the three facts this landing could have broken.
///
/// The roster on the receiving side really is empty, so nothing about the
/// team path is carrying the message on a teammate's behalf.
///
/// The envelope rides along and is admitted, which a drill at this level
/// reads off two absences rather than off the body: the sender's own send
/// stamps all four new fields unconditionally, so a receiver that could not
/// parse them would have answered the peer's own `400` and failed the send
/// outright, and a composition that mishandled the mode this sender asserts
/// would have answered a **hold**. Neither happened, and the message arrived.
///
/// And an **accept** leaves the sender with nothing to wait for (**AC-46** at
/// the boundary): this sender binds a socket and names it as its reply
/// address, so it is exactly the session a settlement *could* have reached —
/// and it is told nothing further, because no outcome but a held one ever
/// produces one.
#[tokio::test]
async fn a_bare_name_reaches_a_zero_teammate_session_exactly_as_it_did_before() {
    if let Some((directory, name, report, reply, wait)) = child_role() {
        send_as_child(directory, name, report, reply, wait).await;

        return;
    }

    let directory = private_dir();
    let home = TempDir::new().expect("a home for the receiving team");
    let reports = TempDir::new().expect("a place for the sender's report");
    let (engine, registry) = receiving_engine(home.path(), None);
    let handle = serve_session(&engine, directory.path()).await;
    write_registered(directory.path(), &stem_of(&bound(&handle)), RECEIVER_NAME);

    let team = team_of(&engine);
    let teammates: Vec<_> = team
        .members
        .iter()
        .filter(|member| !member.is_lead)
        .collect();
    assert!(
        teammates.is_empty(),
        "the receiving session leads a team of nobody but itself: {teammates:?}"
    );

    let report = reports.path().join("sender.json");
    let sent = reported(
        spawn_sender(
            "a_bare_name_reaches_a_zero_teammate_session_exactly_as_it_did_before",
            directory.path(),
            &report,
            "own",
            "silence",
        ),
        &report,
    )
    .await;

    let answered = sent["answered"].as_str().expect("the send was answered");
    assert!(
        !answered.contains(HELD_PROSE),
        "an ordinary receiver admits it rather than holding it: {answered}"
    );
    assert!(
        sent["settlement"].is_null(),
        "an accept leaves the sender nothing to wait for: {sent}"
    );
    assert!(
        !sent["intake"]
            .as_str()
            .expect("the sender took a second turn")
            .contains("<peer_receipt>"),
        "and its model is told of no settlement either: {sent}"
    );

    let inbox = lead_inbox(&registry);
    assert_eq!(inbox.len(), 1, "one message landed: {inbox:?}");
    // Told apart first, so the assertion under it means something: two v7 ids
    // minted moments apart share their leading bytes, and two teams named from
    // live ids would render alike.
    let receiver_identity = format!("{SENDER_LEAD}@{}", registry.team());
    assert_ne!(
        sent["identity"].as_str(),
        Some(receiver_identity.as_str()),
        "the two sessions are distinguishable at all: {sent:?} against {receiver_identity}"
    );
    assert_eq!(
        inbox[0]["from"], sent["identity"],
        "stamped with the sending session's own derived identity, never a bare name this team \
         could mistake for one of its own: {inbox:?}"
    );

    let from = reaches_the_model(&engine, &registry).await;
    println!(
        "[drill A'] zero-teammate receiver: {from} said {TEXT:?}; the sender was told {answered:?} \
         and heard nothing further"
    );

    handle.shutdown().await.expect("a clean stop");
}

// ---------------------------------------------------------------------------
// Drill C — held, then released by a person
// ---------------------------------------------------------------------------

/// **Drill C.** The receiving session is configured `cross_session_inbound:
/// "hold"`; the sending process learns that **synchronously**, in the very
/// answer to its POST, with the cause named in prose; nothing is written into
/// the receiver while it is held; a person releases it; the message then
/// reaches the receiver's model, and a `delivered` settlement reaches the
/// sender's — one `<peer_receipt>` part on its next turn.
///
/// Both halves of the channel in one drill and across one real socket, which
/// is the point: the sender is told twice, and the two tellings ride different
/// transports.
#[tokio::test]
async fn a_person_releasing_a_held_message_delivers_it_and_receipts_the_sender() {
    if let Some((directory, name, report, reply, wait)) = child_role() {
        send_as_child(directory, name, report, reply, wait).await;

        return;
    }

    let directory = private_dir();
    let home = TempDir::new().expect("a home for the receiving team");
    let reports = TempDir::new().expect("a place for the sender's report");
    let (engine, registry) = receiving_engine(
        home.path(),
        Some((InboundPolicy::Hold, PolicySource::Global)),
    );
    let handle = serve_session(&engine, directory.path()).await;
    write_registered(directory.path(), &stem_of(&bound(&handle)), RECEIVER_NAME);

    let report = reports.path().join("sender.json");
    let child = spawn_sender(
        "a_person_releasing_a_held_message_delivers_it_and_receipts_the_sender",
        directory.path(),
        &report,
        "own",
        "receipt",
    );

    let held = eventually_held(&engine).await;
    assert!(
        lead_inbox(&registry).is_empty(),
        "a held message writes nothing into the receiver: {:?}",
        lead_inbox(&registry)
    );
    println!(
        "[drill C] the gate holds one message from {} ({:?}); the inbox is still empty",
        held.from, held.cause
    );

    engine
        .send(Command::SettleHeld {
            id: held.id.clone(),
            decision: HeldDecision::Release,
        })
        .await
        .expect("a settle is never refused");

    let sent = reported(child, &report).await;
    let answered = sent["answered"].as_str().expect("the send was answered");
    assert!(
        answered.contains(HELD_PROSE) && answered.contains(HELD_CAUSE),
        "the sender learned the hold and its cause on the POST itself: {answered}"
    );
    assert_eq!(
        sent["settlement"]["status"], "delivered",
        "and the release settled it delivered: {sent}"
    );
    assert!(
        sent["intake"]
            .as_str()
            .expect("the sender took a reading turn")
            .contains("<peer_receipt>"),
        "which its own model read, once, on its next turn: {sent}"
    );

    let from = reaches_the_model(&engine, &registry).await;
    println!(
        "[drill C] released: {from} said {TEXT:?} to the receiver's model; the sender was told \
         {answered:?} synchronously and {:?} afterwards",
        sent["settlement"]
    );

    handle.shutdown().await.expect("a clean stop");
}

// ---------------------------------------------------------------------------
// Drill D — held, then denied by a person
// ---------------------------------------------------------------------------

/// **Drill D.** The same, denied. The message reaches the receiver's model at
/// no point — the lead's own pass hands back nothing, before and after the
/// decision — and the sender is told `denied`, having already been told
/// `held` on the POST.
///
/// The negative is the load-bearing half: a denial is the one settlement whose
/// whole meaning is that nothing was delivered, and a receiver that wrote the
/// message anyway would still have reported it honestly.
#[tokio::test]
async fn a_person_denying_a_held_message_delivers_nothing_and_receipts_the_sender() {
    if let Some((directory, name, report, reply, wait)) = child_role() {
        send_as_child(directory, name, report, reply, wait).await;

        return;
    }

    let directory = private_dir();
    let home = TempDir::new().expect("a home for the receiving team");
    let reports = TempDir::new().expect("a place for the sender's report");
    let (engine, registry) = receiving_engine(
        home.path(),
        Some((InboundPolicy::Hold, PolicySource::Global)),
    );
    let handle = serve_session(&engine, directory.path()).await;
    write_registered(directory.path(), &stem_of(&bound(&handle)), RECEIVER_NAME);

    let report = reports.path().join("sender.json");
    let child = spawn_sender(
        "a_person_denying_a_held_message_delivers_nothing_and_receipts_the_sender",
        directory.path(),
        &report,
        "own",
        "receipt",
    );

    let held = eventually_held(&engine).await;
    assert!(
        lead_inbox(&registry).is_empty(),
        "nothing is written while it is held"
    );

    engine
        .send(Command::SettleHeld {
            id: held.id.clone(),
            decision: HeldDecision::Deny,
        })
        .await
        .expect("a settle is never refused");

    let sent = reported(child, &report).await;
    let answered = sent["answered"].as_str().expect("the send was answered");
    assert!(
        answered.contains(HELD_PROSE) && answered.contains(HELD_CAUSE),
        "the sender learned the hold and its cause on the POST itself: {answered}"
    );
    assert_eq!(
        sent["settlement"]["status"], "denied",
        "and the denial settled it denied: {sent}"
    );

    assert!(
        lead_inbox(&registry).is_empty(),
        "a denied message is written nowhere: {:?}",
        lead_inbox(&registry)
    );
    let pass = LeadInbox::reading(Arc::clone(&registry), None).poll().await;
    assert!(
        pass.messages.is_empty(),
        "and the lead's own pass hands its model nothing: {pass:?}"
    );
    assert!(
        engine.held_messages().is_empty(),
        "the hold itself is settled and gone"
    );

    println!(
        "[drill D] denied: the receiver's model was handed nothing at all, and the sender was told \
         {answered:?} synchronously and {:?} afterwards",
        sent["settlement"]
    );

    handle.shutdown().await.expect("a clean stop");
}

// ---------------------------------------------------------------------------
// Drill D, the reflection's boundary half
// ---------------------------------------------------------------------------

/// **Drill D's third session.** `reply_to` is asserted by the **sender** and
/// vetted only for shape, so a sender may name a socket that is not its own —
/// and a settlement then goes there rather than back to it. Across real
/// processes: the sender names a third live session's socket, the receiver's
/// person releases the message, and the sender hears **nothing at all** while
/// the third session takes the settlement and is unchanged by it.
///
/// What this drill deliberately does **not** claim is the cardinality.
/// "At most one connect attempt, never a retry", and the outcomes that reach
/// a third session not at all, are pinned in `ganja-core`'s
/// `tests/peer_receipts.rs::a_reply_to_naming_a_third_session_is_a_bounded_reflection`,
/// which drives them over a real socket with a counting instrument behind it.
/// A process pair adds nothing to a bound about how many times something
/// connects — a connect is a socket connect whichever process owns the
/// listener, and a shipped route is not a thing this binary can count
/// through — and the round trip from one running session's route to
/// another's is already pinned two drills above, on a settlement the sender
/// actually reads. So what is left for this one is what neither of those
/// covers: the steering itself, end to end, and the third session's
/// indifference to what it was handed.
#[tokio::test]
async fn a_reply_to_naming_a_third_session_steers_the_settlement_away_from_the_sender() {
    if let Some((directory, name, report, reply, wait)) = child_role() {
        send_as_child(directory, name, report, reply, wait).await;

        return;
    }

    let directory = private_dir();
    let home = TempDir::new().expect("a home for the receiving team");
    let third_home = TempDir::new().expect("a home for the third session");
    let reports = TempDir::new().expect("a place for the sender's report");

    let (engine, registry) = receiving_engine(
        home.path(),
        Some((InboundPolicy::Hold, PolicySource::Global)),
    );
    let handle = serve_session(&engine, directory.path()).await;
    write_registered(directory.path(), &stem_of(&bound(&handle)), RECEIVER_NAME);

    // The third session: an ordinary live one, serving the shipped socket
    // routes, which has sent nobody anything and is holding no id.
    let (third, third_registry) = receiving_engine(third_home.path(), None);
    let third_handle = serve_session(&third, directory.path()).await;
    let third_socket = bound(&third_handle);

    let report = reports.path().join("sender.json");
    let child = spawn_sender(
        "a_reply_to_naming_a_third_session_steers_the_settlement_away_from_the_sender",
        directory.path(),
        &report,
        &format!("third:{}", third_socket.display()),
        "silence",
    );

    let held = eventually_held(&engine).await;
    engine
        .send(Command::SettleHeld {
            id: held.id.clone(),
            decision: HeldDecision::Release,
        })
        .await
        .expect("a settle is never refused");

    let sent = reported(child, &report).await;
    assert!(
        sent["settlement"].is_null(),
        "the sender named somewhere else, and hears nothing: {sent}"
    );
    assert!(
        !sent["intake"]
            .as_str()
            .expect("the sender took a second turn")
            .contains("<peer_receipt>"),
        "so its model is told of no settlement either: {sent}"
    );

    // The third session took a settlement for an id it never sent, and it
    // changed nothing there: no delivery, no hold, nothing for its model.
    assert!(
        lead_inbox(&third_registry).is_empty(),
        "a receipt writes nothing into the session that receives it: {:?}",
        lead_inbox(&third_registry)
    );
    assert!(
        third.held_messages().is_empty(),
        "and raises nothing for review"
    );
    let pass = LeadInbox::reading(Arc::clone(&third_registry), None)
        .poll()
        .await;
    assert!(
        pass.messages.is_empty(),
        "and hands the third session's model nothing: {pass:?}"
    );

    // The release itself is unaffected by where it could not be reported.
    let from = reaches_the_model(&engine, &registry).await;
    println!(
        "[drill D'] the settlement went to {} rather than to the sender, which heard nothing; \
         the released message still reached the receiver's model from {from}",
        third_socket.display()
    );

    third_handle.shutdown().await.expect("a clean stop");
    handle.shutdown().await.expect("a clean stop");
}
