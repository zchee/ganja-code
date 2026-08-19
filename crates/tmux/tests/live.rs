//! Spec: pandaemonium pkg/tmux/integration_test.go.
//!
//! Upstream gates its real-tmux suite on `RUN_REAL_TMUX_TESTS=1` and skips
//! when tmux is unavailable. This port deliberately has no environment gate
//! and hard-fails with a specific requirement instead, matching the posture
//! documented by [`tmux`]. Every test owns a private server, socket, empty
//! config, and session so concurrent test processes and threads cannot share
//! state.
//!
//! The tests below the control-mode ones drive the other transport —
//! [`tmux::Server`], one plain client invocation per call — which the Go
//! package has no counterpart for and no `Spec:` line therefore names, and
//! the last one drives both at once against a single server. They share the
//! scaffolding above, because "private server, unique session, hard-fail" is
//! a property of this file rather than of either transport.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use tempfile::TempDir;
use tmux::{
    Error, PaneId, Server, SessionId, WindowId,
    commands::{
        CapturePane, HasSession, Invocation, KillPane, KillServer, KillWindow, ListPanes,
        ListSessions, ListWindows, NewSession, NewWindow, PasteBuffer, RenameSession, ResizePane,
        SendKeys, SetBuffer, SetEnvironment, SetOption, ShowBuffer, ShowEnvironment, ShowOptions,
        SplitWindow,
    },
    control_mode::{
        Arg, Client, Command, DISPLAY_MESSAGE, LIST_PANES, Notification, NotificationKind, Options,
        SubscriptionTarget,
    },
};
use tokio::time::{Instant, sleep, timeout};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

fn require_tmux() {
    let output = ProcessCommand::new("tmux")
        .arg("-V")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "the tmux crate's real integration tests require a runnable tmux binary on PATH; \
                 `tmux -V` could not start: {error}"
            )
        });
    assert!(
        output.status.success(),
        "the tmux crate's real integration tests require a runnable tmux binary on PATH; \
         `tmux -V` exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

struct KillServerGuard {
    socket: PathBuf,
}

impl Drop for KillServerGuard {
    fn drop(&mut self) {
        let _ = ProcessCommand::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .status();
    }
}

struct Scratch {
    // Rust drops fields in declaration order, so tmux dies before TempDir unlinks its socket.
    _server_guard: KillServerGuard,
    temp_dir: TempDir,
    socket: PathBuf,
    config: PathBuf,
    session: String,
}

impl Scratch {
    fn new(label: &str) -> Self {
        require_tmux();

        let temp_dir = TempDir::new().expect("create the private real-tmux scratch directory");
        let socket = temp_dir.path().join("tmux.sock");
        let config = temp_dir.path().join("tmux.conf");
        fs::write(&config, b"").expect("write the private empty tmux config");

        let test_id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let session = format!("ganja-live-{}-{test_id}-{label}", std::process::id());

        Self {
            _server_guard: KillServerGuard {
                socket: socket.clone(),
            },
            temp_dir,
            socket,
            config,
            session,
        }
    }

    fn options(&self) -> Options {
        Options::new()
            .with_socket_path(self.socket.clone())
            .with_config_file(self.config.clone())
            .with_session_name(self.session.clone())
            .with_create_session(true)
            .with_shutdown_timeout(Duration::from_secs(2))
    }

    fn session_name(&self) -> &str {
        &self.session
    }

    /// The private directory this scratch's things live in, for a test that
    /// needs a path of its own to hand tmux.
    fn dir(&self) -> &Path {
        self.temp_dir.path()
    }
}

async fn new_client(scratch: &Scratch) -> Client {
    timeout(Duration::from_secs(15), Client::new(scratch.options()))
        .await
        .expect("the private tmux handshake should finish within 15 seconds")
        .expect("the private tmux client should complete its handshake")
}

async fn close_client(client: &Client) {
    client.close().await.unwrap_or_else(|error| {
        panic!(
            "the private tmux control client should close cleanly: {error}; stderr={:?}",
            client.stderr_tail()
        )
    });
}

async fn created_pane(client: &Client) -> PaneId {
    let response = client
        .exec(LIST_PANES, [Arg::raw("-F"), Arg::string("#{pane_id}")])
        .await
        .expect("list-panes should succeed for the private session");
    assert_eq!(
        response.lines.len(),
        1,
        "the private session should contain exactly one pane"
    );
    PaneId::new(response.lines[0].clone())
        .expect("list-panes -F #{pane_id} should return a valid pane id")
}

async fn recv_until(
    client: &Client,
    wait: Duration,
    mut predicate: impl FnMut(&Notification) -> bool,
) -> Option<Notification> {
    let deadline = Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match timeout(remaining, client.recv()).await {
            Ok(Some(notification)) if predicate(&notification) => return Some(notification),
            Ok(Some(_notification)) => {}
            Ok(None) | Err(_) => return None,
        }
    }
}

fn pid_is_alive(pid: u32) -> bool {
    let output = ProcessCommand::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .expect("the process-liveness probe should run `kill -0`");
    output.status.success()
}

async fn wait_for_pid_exit(pid: u32, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_is_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "{label}: process {pid} remained alive past the teardown deadline"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

fn matching_client_pids(socket: &Path) -> Vec<u32> {
    let output = ProcessCommand::new("ps")
        .args(["-eo", "pid,ppid,command"])
        .output()
        .expect("the control-client PID probe should run ps");
    assert!(
        output.status.success(),
        "the control-client PID probe `ps -eo pid,ppid,command` failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let socket = socket.to_string_lossy();
    let parent_pid = std::process::id();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let ppid = fields.next()?.parse::<u32>().ok()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            (ppid == parent_pid && command.contains(socket.as_ref())).then_some(pid)
        })
        .collect()
}

async fn find_client_pid(socket: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let matches = matching_client_pids(socket);
        if !matches.is_empty() {
            assert_eq!(
                matches.len(),
                1,
                "the unique socket path should identify exactly one direct tmux control client"
            );
            return matches[0];
        }
        assert!(
            Instant::now() < deadline,
            "the tmux control client did not appear in ps before the deadline"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn handshake_creates_an_isolated_session() {
    let scratch = Scratch::new("handshake");
    let client = new_client(&scratch).await;

    let response = client
        .exec(
            DISPLAY_MESSAGE,
            [Arg::raw("-p"), Arg::string("#{session_name}")],
        )
        .await
        .expect("display-message should read the private session name");
    assert_eq!(response.lines.join("\n"), scratch.session_name());

    close_client(&client).await;
}

#[tokio::test]
async fn hostile_content_survives_the_quoting_ladder() {
    let scratch = Scratch::new("quoting");
    let client = new_client(&scratch).await;
    let content = "spaces 'single' \"double\" $dollar \\backslash café 東京";

    let response = client
        .exec(DISPLAY_MESSAGE, [Arg::raw("-p"), Arg::string(content)])
        .await
        .expect("display-message should accept the hostile quoted argument");
    assert_eq!(
        response.lines.join("\n"),
        content,
        "tmux should return the hostile argument byte-for-byte"
    );

    close_client(&client).await;
}

#[tokio::test]
async fn list_panes_reports_the_created_pane() {
    let scratch = Scratch::new("list-panes");
    let client = new_client(&scratch).await;

    let _pane = created_pane(&client).await;

    close_client(&client).await;
}

#[tokio::test]
async fn send_keys_output_arrives_and_decodes() {
    let scratch = Scratch::new("output");
    let client = new_client(&scratch).await;
    let pane = created_pane(&client).await;
    let marker = format!("{}-marker", scratch.session_name());

    client
        .exec(
            Command::from_static("send-keys"),
            [
                Arg::raw("-t"),
                Arg::string(pane.as_str()),
                Arg::string(format!("printf {marker}")),
                Arg::raw("Enter"),
            ],
        )
        .await
        .expect("send-keys should run the marker command in the private pane");

    let notification =
        recv_until(
            &client,
            Duration::from_secs(10),
            |notification| match notification.output() {
                Some(Ok(output)) => output.pane == pane && output.text_lossy().contains(&marker),
                Some(Err(error)) => {
                    panic!("tmux emitted a malformed %output notification: {error}")
                }
                None => false,
            },
        )
        .await
        .unwrap_or_else(|| {
            panic!(
                "no %output for pane {} contained {marker:?}; drops={} stderr={:?}",
                pane,
                client.dropped_notifications(),
                client.stderr_tail()
            )
        });
    let output = notification
        .output()
        .expect("the matched notification should be %output")
        .expect("the matched %output should decode");
    assert_eq!(output.pane, pane);
    assert!(output.text_lossy().contains(&marker));

    close_client(&client).await;
}

#[tokio::test]
async fn subscribe_format_roundtrip() {
    let scratch = Scratch::new("subscription");
    let client = new_client(&scratch).await;
    let name = "live-test";

    client
        .subscribe_format(
            name,
            &SubscriptionTarget::ATTACHED_SESSION,
            "#{session_name}",
        )
        .await
        .expect("refresh-client -B should register the format subscription");

    let notification =
        recv_until(
            &client,
            Duration::from_secs(10),
            |notification| match notification.subscription_changed() {
                Some(Ok(changed)) => changed.name == name,
                Some(Err(error)) => {
                    panic!("tmux emitted a malformed %subscription-changed notification: {error}")
                }
                None => false,
            },
        )
        .await
        .unwrap_or_else(|| {
            panic!(
                "no %subscription-changed arrived for {name:?}; drops={} stderr={:?}",
                client.dropped_notifications(),
                client.stderr_tail()
            )
        });
    let changed = notification
        .subscription_changed()
        .expect("the matched notification should be %subscription-changed")
        .expect("the matched %subscription-changed should decode");
    assert_eq!(changed.name, name);

    client
        .unsubscribe_format(name)
        .await
        .expect("refresh-client -B should remove the format subscription");

    close_client(&client).await;
}

#[tokio::test]
async fn flow_control_commands_are_accepted() {
    let scratch = Scratch::new("flow-control");
    let client = new_client(&scratch).await;
    let pane = created_pane(&client).await;

    let pause_after = client.set_pause_after(Duration::from_secs(2)).await;
    assert!(
        pause_after.is_ok(),
        "set_pause_after should be accepted: {pause_after:?}"
    );
    let pause = client.pause_pane(&pane).await;
    assert!(pause.is_ok(), "pause_pane should be accepted: {pause:?}");
    let continue_ = client.continue_pane(&pane).await;
    assert!(
        continue_.is_ok(),
        "continue_pane should be accepted: {continue_:?}"
    );
    let disable = client.disable_pane_output(&pane).await;
    assert!(
        disable.is_ok(),
        "disable_pane_output should be accepted: {disable:?}"
    );
    let enable = client.enable_pane_output(&pane).await;
    assert!(
        enable.is_ok(),
        "enable_pane_output should be accepted: {enable:?}"
    );

    close_client(&client).await;
}

#[tokio::test]
async fn close_leaves_no_tmux_process() {
    let scratch = Scratch::new("close");
    let client = new_client(&scratch).await;

    let response = client
        .exec(DISPLAY_MESSAGE, [Arg::raw("-p"), Arg::string("#{pid}")])
        .await
        .expect("display-message should report the private tmux server PID");
    assert_eq!(
        response.lines.len(),
        1,
        "display-message -p #{{pid}} should return one server PID"
    );
    let server_pid = response.lines[0]
        .parse::<u32>()
        .expect("the tmux server PID should be an unsigned integer");

    let _ = client.exec_raw("kill-server").await;
    let _ = client.close().await;
    wait_for_pid_exit(server_pid, "close_leaves_no_tmux_process").await;
}

#[tokio::test]
async fn an_unclosed_dropped_client_reaps_its_subprocess() {
    let scratch = Scratch::new("drop");
    let client = new_client(&scratch).await;

    let client_pid = find_client_pid(&scratch.socket).await;
    assert!(
        pid_is_alive(client_pid),
        "the socket-selected tmux control client should be alive before drop"
    );

    drop(client);

    wait_for_pid_exit(
        client_pid,
        "an_unclosed_dropped_client_reaps_its_subprocess",
    )
    .await;
}

/// This scratch's private server, addressed by socket alone: nothing here
/// runs inside tmux, so there is no `$TMUX` to read and no pane to inherit.
fn private_server(scratch: &Scratch) -> Server {
    Server::at(scratch.socket.clone(), None)
}

/// Starts the private server and its one session.
///
/// The only call that needs the empty config: `-f` is a *client* flag, read
/// when a server is created rather than when a running one is asked
/// something, so every later call in these tests leads with its subcommand.
async fn start_private_server(scratch: &Scratch, server: &Server) {
    let argv = vec![
        OsString::from("-f"),
        scratch.config.clone().into_os_string(),
        OsString::from("new-session"),
        OsString::from("-d"),
        OsString::from("-s"),
        OsString::from(scratch.session_name()),
    ];

    let created = server.run(argv).await.unwrap_or_else(|error| {
        panic!("new-session -d should start the private server and its session: {error}")
    });
    assert!(
        created.bytes().is_empty(),
        "a detached new-session answers with silence, not with {:?}",
        created.text_lossy()
    );
}

#[tokio::test]
async fn a_client_invocation_creates_a_session_and_reads_it_back() {
    let scratch = Scratch::new("server-roundtrip");
    let server = private_server(&scratch);
    start_private_server(&scratch, &server).await;

    let named = server
        .run([
            "display-message",
            "-p",
            "-t",
            scratch.session_name(),
            "#{session_name}",
        ])
        .await
        .expect("display-message -p should read the private session back");
    assert_eq!(
        named.text().expect("a session name is text").trim(),
        scratch.session_name(),
        "the round trip must return the session this test created and no other"
    );

    server
        .run(["kill-server"])
        .await
        .expect("kill-server should end the private server");

    let after = server
        .run(["display-message", "-p", "#{session_name}"])
        .await;
    assert!(
        matches!(after, Err(Error::ClientRefused { .. })),
        "a killed server answers nothing: {after:?}"
    );
}

/// One round trip driven entirely by the typed builders, to prove the argv
/// they assemble is argv tmux accepts — the unit tests assert the words, and
/// only a real server can say whether the words were right.
///
/// Deliberately one test rather than one per command: what varies between
/// builders is which flags they render, which is a process-free question, and
/// a live test per command would buy tmux's parser being exercised 34 times
/// over for the same answer.
#[tokio::test]
async fn the_typed_builders_split_list_and_kill_a_real_pane() {
    let scratch = Scratch::new("typed-builders");
    let server = private_server(&scratch);
    start_private_server(&scratch, &server).await;

    let created = server
        .run(
            NewWindow::new()
                .detached()
                .print()
                .format("#{window_id}")
                .window_name("typed")
                .target(scratch.session_name())
                .args(),
        )
        .await
        .expect("new-window should create a window in the private session");
    let window = WindowId::new(created.text_lossy().trim())
        .expect("new-window -P -F #{window_id} should answer with a window id");

    // The proven consumer shape: detached, printing the new pane's id, with a
    // working directory, an enumerated environment, and the program behind
    // the `--` this layer emits.
    let split = server
        .run(
            SplitWindow::new()
                .detached()
                .print()
                .format("#{pane_id}")
                .start_directory("/")
                .environment("GANJA_LIVE_TEST_PANE=1")
                .target(&window)
                .command(["sh", "-c", "sleep 30"])
                .args(),
        )
        .await
        .expect("split-window should split the window the previous call made");
    let pane = PaneId::new(split.text_lossy().trim())
        .expect("split-window -P -F #{pane_id} should answer with a pane id");

    let listed = server
        .run(ListPanes::new().format("#{pane_id}").target(&window).args())
        .await
        .expect("list-panes should list the window's panes");
    let panes: Vec<PaneId> = listed
        .text_lossy()
        .lines()
        .map(|line| PaneId::new(line).expect("list-panes -F #{pane_id} answers with pane ids"))
        .collect();
    assert_eq!(
        panes.len(),
        2,
        "the split window should hold the original pane and the new one: {panes:?}"
    );
    assert!(
        panes.contains(&pane),
        "the pane split-window named should be one of the panes list-panes reports: {panes:?}"
    );

    server
        .run(KillPane::new().target(&pane).args())
        .await
        .expect("kill-pane should destroy the pane split-window made");

    let after = server
        .run(ListPanes::new().format("#{pane_id}").target(&window).args())
        .await
        .expect("list-panes should still answer after the kill");
    assert!(
        !after.text_lossy().lines().any(|line| line == pane.as_str()),
        "the killed pane should be gone: {:?}",
        after.text_lossy()
    );

    server
        .run(["kill-server"])
        .await
        .expect("kill-server should end the private server");
}

/// Polls `capture-pane` until the pane's visible contents hold `needle`.
///
/// A pane's echo is asynchronous — the write into the pty, the program's own
/// answer and tmux's redraw all happen after the call that caused them has
/// returned — so what a capture is allowed to assert is *what arrives*,
/// never when. The builder is the caller's rather than this helper's because
/// which capture-pane flags a test needs is part of what that test is
/// asking; the polling is all that is shared.
async fn capture_until(
    server: &Server,
    capture: &CapturePane,
    needle: &str,
    label: &str,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let captured = server
            .run(capture.args())
            .await
            .expect("capture-pane -p should print the pane's visible contents");
        let visible = captured.text_lossy().into_owned();
        if visible.contains(needle) {
            return visible;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: {needle:?} never appeared; the pane held {visible:?}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

/// One round trip through the buffer and key builders, for the half of their
/// contract only a real server can answer: that a caller's own bytes come
/// back unchanged.
///
/// The marker leads with a `-` and carries a shell's metacharacters on
/// purpose. Nothing between this process and the pane is allowed a say in
/// what those bytes mean: the `--` fence keeps `set-buffer` from reading the
/// leading dash as a flag, and `send-keys -l` keeps tmux from looking any of
/// it up as a key name — which is exactly the shape a real consumer types a
/// line with.
#[tokio::test]
async fn the_typed_builders_round_trip_a_buffer_and_type_it_into_a_pane() {
    let scratch = Scratch::new("typed-buffers");
    let server = private_server(&scratch);
    start_private_server(&scratch, &server).await;

    let marker = format!("-{}-w5 'quoted' $dollar", scratch.session_name());

    server
        .run(SetBuffer::new().buffer("w5").data(&marker).args())
        .await
        .expect("set-buffer should take the marker as its data");
    let shown = server
        .run(ShowBuffer::new().buffer("w5").args())
        .await
        .expect("show-buffer should print the buffer set-buffer just filled");
    assert_eq!(
        shown.bytes(),
        marker.as_bytes(),
        "a buffer must come back byte for byte: {:?}",
        shown.text_lossy()
    );

    // A window running `cat` rather than the login shell: a pty echoes what
    // is typed into it either way, and `cat` brings no prompt of its own for
    // the capture to read past.
    let created = server
        .run(
            NewWindow::new()
                .detached()
                .print()
                .format("#{pane_id}")
                .window_name("w5")
                .target(scratch.session_name())
                .command(["/bin/cat"])
                .args(),
        )
        .await
        .expect("new-window should create the window this test types into");
    let pane = PaneId::new(created.text_lossy().trim())
        .expect("new-window -P -F #{pane_id} should answer with a pane id");

    // `Enter` is sent as a second key on purpose, and it is what makes `-l`
    // load-bearing here: with the flag it is five characters typed after the
    // marker, and without it the return key, which would end the line instead
    // of appearing in it.
    server
        .run(
            SendKeys::new()
                .target(&pane)
                .literal()
                .key(&marker)
                .key("Enter")
                .args(),
        )
        .await
        .expect("send-keys -l should type the marker and the word Enter into the pane");
    let typed = format!("{marker}Enter");

    capture_until(
        &server,
        &CapturePane::new().stdout().target(&pane),
        &typed,
        "the pane should echo what send-keys -l typed into it",
    )
    .await;

    server
        .run(["kill-server"])
        .await
        .expect("kill-server should end the private server");
}

#[tokio::test]
async fn a_refused_client_invocation_carries_tmuxs_own_stderr() {
    let scratch = Scratch::new("server-refusal");
    let server = private_server(&scratch);
    start_private_server(&scratch, &server).await;

    // tmux resolves a command name only once it has a server to resolve it
    // against — an unknown command on a dead socket fails at connect instead,
    // which is why this runs against the live private server.
    let refused = server
        .run(["bogus-subcommand"])
        .await
        .expect_err("tmux has no such command");
    match refused {
        Error::ClientRefused {
            command,
            status,
            stderr,
        } => {
            assert_eq!(
                command.as_deref(),
                Some("bogus-subcommand"),
                "the error names the word the call led with"
            );
            assert!(!status.success(), "a refusal exits non-zero: {status}");
            assert!(
                stderr.contains("unknown command") && stderr.contains("bogus-subcommand"),
                "tmux's own account of the refusal must survive verbatim: {stderr:?}"
            );
        }
        other => panic!("a running server should refuse in its own words, not: {other}"),
    }

    server
        .run(["kill-server"])
        .await
        .expect("kill-server should end the private server");
}

/// One round trip driven by the session family's builders, in the shape a
/// caller who wants a session and not a terminal writes it: create it
/// detached, ask whether it is there, rename it, read the new name back out
/// of the listing, and end the server.
///
/// Deliberately one test for the family, for the reason the pane family's own
/// live test states: which flags a builder renders is a process-free
/// question, and only whether tmux accepts the words needs a real server.
#[tokio::test]
async fn the_typed_builders_create_rename_and_list_a_real_session() {
    let scratch = Scratch::new("typed-sessions");
    let server = private_server(&scratch);

    // `-f` is a client flag, read when a server is created rather than when a
    // running one is asked something, so it leads the words the builder
    // assembles instead of living among them.
    let mut argv = vec![
        OsString::from("-f"),
        scratch.config.clone().into_os_string(),
    ];
    argv.extend(
        NewSession::new()
            .detached()
            .print()
            .format("#{session_id}")
            .session_name(scratch.session_name())
            .args(),
    );
    let created = server.run(argv).await.unwrap_or_else(|error| {
        panic!("new-session -d should start the private server and its session: {error}")
    });
    let session = SessionId::new(created.text_lossy().trim())
        .expect("new-session -P -F #{session_id} should answer with a session id");

    server
        .run(HasSession::new().target(&session).args())
        .await
        .expect("has-session should find the session new-session just made");

    let renamed = format!("{}-renamed", scratch.session_name());
    server
        .run(
            RenameSession::new()
                .target(&session)
                .new_name(renamed.as_str())
                .args(),
        )
        .await
        .expect("rename-session should rename the session new-session made");

    let listed = server
        .run(
            ListSessions::new()
                .format("#{session_id} #{session_name}")
                .args(),
        )
        .await
        .expect("list-sessions should list the private server's sessions");
    let row = format!("{session} {renamed}");
    assert!(
        listed.text_lossy().lines().any(|line| line == row),
        "the listing should carry the name rename-session gave: {:?}",
        listed.text_lossy()
    );

    server
        .run(KillServer::new().args())
        .await
        .expect("kill-server should end the private server");

    let after = server.run(HasSession::new().target(&session).args()).await;
    assert!(
        matches!(after, Err(Error::ClientRefused { .. })),
        "a killed server answers nothing: {after:?}"
    );
}

/// The read shape a real consumer already asks a live server, driven by the
/// typed builders instead: set a window option, then read it back with the
/// inherited values included, so that "unset" and "off" can be told apart.
///
/// The environment half rides along because it is the same question asked of
/// a different table — a write this crate spells, and a read that has to
/// answer with what was written rather than with something adjacent to it.
#[tokio::test]
async fn the_option_and_environment_builders_round_trip_on_a_real_server() {
    let scratch = Scratch::new("options-env");
    let server = private_server(&scratch);
    start_private_server(&scratch, &server).await;

    server
        .run(
            SetOption::new()
                .window()
                .target(scratch.session_name())
                .option("pane-border-status")
                .value("top")
                .args(),
        )
        .await
        .expect("set-option -w should set the session's current window option");

    let read_back = server
        .run(
            ShowOptions::new()
                .window()
                .quiet()
                .value_only()
                .inherited()
                .target(scratch.session_name())
                .option("pane-border-status")
                .args(),
        )
        .await
        .expect("show-options -wqvA should read the option back");
    assert_eq!(
        read_back.text().expect("an option value is text").trim(),
        "top",
        "the read must answer with what the write set, not with the default"
    );

    server
        .run(
            SetEnvironment::new()
                .target(scratch.session_name())
                .variable("GANJA_LIVE_W6")
                .value("set-by-the-typed-builder")
                .args(),
        )
        .await
        .expect("set-environment should set the session variable");

    let environment = server
        .run(
            ShowEnvironment::new()
                .target(scratch.session_name())
                .variable("GANJA_LIVE_W6")
                .args(),
        )
        .await
        .expect("show-environment should read the session variable back");
    assert_eq!(
        environment
            .text()
            .expect("an environment listing is text")
            .trim(),
        "GANJA_LIVE_W6=set-by-the-typed-builder",
        "tmux answers one NAME=value line for a named variable"
    );

    server
        .run(["kill-server"])
        .await
        .expect("kill-server should end the private server");
}

/// Adds one window running `cat` to the scratch session, and answers with the
/// window id tmux minted for it.
///
/// `cat` rather than the login shell, for the reason the buffer round trip
/// above states: a pty echoes what is typed into it either way, and `cat`
/// brings no prompt of its own for a capture to read past.
async fn cat_window(server: &Server, session: &str, name: &str) -> WindowId {
    let created = server
        .run(
            NewWindow::new()
                .detached()
                .print()
                .format("#{window_id}")
                .window_name(name)
                .target(session)
                .command(["/bin/cat"])
                .args(),
        )
        .await
        .unwrap_or_else(|error| {
            panic!("new-window should add {name} to the private session: {error}")
        });

    WindowId::new(created.text_lossy().trim()).unwrap_or_else(|error| {
        panic!("new-window -P -F #{{window_id}} should answer with a window id for {name}: {error}")
    })
}

/// Splits each line of a `#{id} #{something}` listing into its two halves.
///
/// The id is one word and everything after the first space is the value, so
/// a window name or a path holding spaces still arrives whole.
fn listed_pairs(text: &str) -> Vec<(&str, &str)> {
    text.lines()
        .map(|line| line.split_once(' ').unwrap_or((line, "")))
        .collect()
}

/// The two transports against one server: the world built entirely through
/// the typed one-shot builders is the world a control-mode client attached
/// to the same socket reads back, and a one-shot change made while that
/// client is listening reaches it as a notification.
///
/// Every other test in this file drives one transport. This is the only
/// place they meet, so it is deliberately a scenario rather than a probe: a
/// session, two windows, a split carrying a working directory and an
/// enumerated variable, a buffer pasted into that split, a resize — and only
/// then a [`Client`], which must name the same session and list the same
/// windows and panes, by the very ids the one-shot side was told. Two layers
/// that had drifted apart — a socket addressed differently, a session
/// created beside the other one instead of joined, an id spelled one way
/// here and another there — cannot pass this, and no single-transport test
/// can fail for that reason at all.
///
/// The kill at the end is the crossing in the other direction: nothing on
/// the control connection asks for it, and the client learns of it anyway.
#[tokio::test]
async fn both_transports_see_one_server() {
    let scratch = Scratch::new("dual-transport");
    let server = private_server(&scratch);
    start_private_server(&scratch, &server).await;

    // A directory of this scratch's own to hand `split-window -c`,
    // canonicalized because tmux answers `#{pane_current_path}` with the
    // path the kernel holds — on macOS the `/private` form of what TempDir
    // handed out, which a literal comparison would otherwise miss.
    let workdir = scratch.dir().join("split-cwd");
    fs::create_dir(&workdir).expect("create the directory the split is asked to start in");
    let workdir = fs::canonicalize(&workdir).expect("canonicalize the split's start directory");

    let first = cat_window(&server, scratch.session_name(), "w7-first").await;
    let second = cat_window(&server, scratch.session_name(), "w7-second").await;
    assert_ne!(first, second, "two new-windows must be two windows");

    // The proven consumer shape, with both of its caller-supplied halves
    // made observable: the pane prints the variable `-e` gave it and then
    // holds itself open with `cat`, while `-c` is read back out of tmux's
    // own view of where the process is.
    let variable = format!("{}-env", scratch.session_name());
    let split = server
        .run(
            SplitWindow::new()
                .detached()
                .print()
                .format("#{pane_id}")
                .start_directory(&workdir)
                .environment(format!("GANJA_W7_ENV={variable}"))
                .target(&second)
                .command(["sh", "-c", "printf '%s\\n' \"$GANJA_W7_ENV\"; exec cat"])
                .args(),
        )
        .await
        .expect("split-window should split the second window");
    let pane = PaneId::new(split.text_lossy().trim())
        .expect("split-window -P -F #{pane_id} should answer with a pane id");

    capture_until(
        &server,
        &CapturePane::new().stdout().join_wrapped().target(&pane),
        &variable,
        "the split's process should have been started with the variable -e named",
    )
    .await;

    let paths = server
        .run(
            ListPanes::new()
                .format("#{pane_id} #{pane_current_path}")
                .target(&second)
                .args(),
        )
        .await
        .expect("list-panes should report where each pane's process is");
    let paths = paths.text_lossy();
    assert!(
        listed_pairs(&paths)
            .iter()
            .any(|(id, path)| *id == pane.as_str() && Path::new(path) == workdir),
        "the split's process should be in the directory -c named ({}): {paths:?}",
        workdir.display()
    );

    // The buffer leg: a marker set on the server, pasted into the pane, and
    // read back off the pane's own echo — three commands from three
    // different families passing one caller's bytes between them.
    let pasted = format!("{}-pasted", scratch.session_name());
    server
        .run(SetBuffer::new().buffer("w7").data(&pasted).args())
        .await
        .expect("set-buffer should take the marker as its data");
    server
        .run(
            PasteBuffer::new()
                .buffer("w7")
                .delete()
                .target(&pane)
                .args(),
        )
        .await
        .expect("paste-buffer should paste the marker into the split pane");
    capture_until(
        &server,
        &CapturePane::new().stdout().join_wrapped().target(&pane),
        &pasted,
        "the split pane should echo the pasted buffer",
    )
    .await;

    // The resize is asserted before the control client attaches: attaching
    // gives the session a client with a size of its own, and this assertion
    // is about the height the builder asked for rather than about which
    // client the window is currently sized to.
    server
        .run(ResizePane::new().height("5").target(&pane).args())
        .await
        .expect("resize-pane -y should resize the split pane");
    let heights = server
        .run(
            ListPanes::new()
                .format("#{pane_id} #{pane_height}")
                .target(&second)
                .args(),
        )
        .await
        .expect("list-panes should report each pane's height");
    let heights = heights.text_lossy();
    assert!(
        listed_pairs(&heights)
            .iter()
            .any(|(id, height)| *id == pane.as_str() && *height == "5"),
        "the split pane should be the five rows resize-pane asked for: {heights:?}"
    );

    // Only now the other transport, against the same socket.
    let client = new_client(&scratch).await;

    // `Options` asked for `new-session -A -s <name>`, and `-A` is what makes
    // that an attach to the session the one-shot side already built rather
    // than a second session beside it. Everything below rests on this.
    let named = client
        .exec(
            DISPLAY_MESSAGE,
            [Arg::raw("-p"), Arg::string("#{session_name}")],
        )
        .await
        .expect("display-message should name the session the control client is in");
    assert_eq!(
        named.lines.join("\n"),
        scratch.session_name(),
        "the control client must have joined the one-shot side's session, not made its own"
    );

    // The command name comes from the registry's own literal, so the two
    // transports cannot end up asking for differently spelled commands: one
    // renders it into argv, the other into a control-mode line.
    let windows = client
        .exec(
            Command::from_static(<ListWindows as Invocation>::NAME),
            [
                Arg::raw("-t"),
                Arg::string(scratch.session_name()),
                Arg::raw("-F"),
                Arg::string("#{window_id} #{window_name}"),
            ],
        )
        .await
        .expect("list-windows should list the private session's windows");
    let listing = windows.lines.join("\n");
    assert_eq!(
        windows.lines.len(),
        3,
        "the session's own window plus the two the one-shot side added: {listing:?}"
    );
    for (window, name) in [(&first, "w7-first"), (&second, "w7-second")] {
        assert!(
            listed_pairs(&listing)
                .iter()
                .any(|(id, listed)| *id == window.as_str() && *listed == name),
            "the control client should see {window} named {name:?}, as the one-shot side made it: \
             {listing:?}"
        );
    }

    let panes = client
        .exec(
            LIST_PANES,
            [
                Arg::raw("-t"),
                Arg::string(second.as_str()),
                Arg::raw("-F"),
                Arg::string("#{pane_id}"),
            ],
        )
        .await
        .expect("list-panes should list the split window's panes");
    assert_eq!(
        panes.lines.len(),
        2,
        "the split window holds the window's original pane and the split one: {:?}",
        panes.lines
    );
    assert!(
        panes.lines.iter().any(|line| line == pane.as_str()),
        "the pane split-window minted should be one the control client sees: {:?}",
        panes.lines
    );

    // The other direction: a change made by the transport that is not
    // listening, observed by the one that is.
    server
        .run(KillWindow::new().target(&first).args())
        .await
        .expect("kill-window should destroy the first window the one-shot side made");

    // The predicate takes the whole window-close family, and deliberately
    // settles for it: which member carries the fact is the server's
    // version, not this crate's. tmux through 3.7 unlinks the dying window
    // from its session before composing the notification, so even a window
    // of the attached session arrives as %unlinked-window-close, while
    // next-3.8 reads linkage at close time and says %window-close. Both
    // spellings decode — the parser's own tests tell the kinds apart — so
    // what this pins is the fact both agree on: the kill crossed
    // transports, naming the window the one-shot side made.
    let closed = recv_until(&client, Duration::from_secs(10), |notification| {
        matches!(
            notification.kind,
            NotificationKind::WindowClose | NotificationKind::UnlinkedWindowClose
        ) && notification.args == [first.as_str()]
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "no window-close notification named {first}; drops={} stderr={:?}",
            client.dropped_notifications(),
            client.stderr_tail()
        )
    });
    assert_eq!(
        closed.args,
        [first.as_str()],
        "the close notification names the killed window: {:?}",
        closed.raw
    );

    // The exit path with the server still alive: `close` detaches, waits for
    // the subprocess and joins both reader tasks inside this scratch's
    // two-second shutdown budget, so its `Ok` is the reap assertion.
    close_client(&client).await;

    server
        .run(KillServer::new().args())
        .await
        .expect("kill-server should end the private server");

    let after = server
        .run(ListWindows::new().target(scratch.session_name()).args())
        .await;
    assert!(
        matches!(after, Err(Error::ClientRefused { .. })),
        "a killed server answers nothing: {after:?}"
    );
}
