//! Spec: pandaemonium pkg/tmux/integration_test.go.
//!
//! Upstream gates its real-tmux suite on `RUN_REAL_TMUX_TESTS=1` and skips
//! when tmux is unavailable. This port deliberately has no environment gate
//! and hard-fails with a specific requirement instead, matching the posture
//! documented by [`tmux`]. Every test owns a private server, socket, empty
//! config, and session so concurrent test processes and threads cannot share
//! state.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use tempfile::TempDir;
use tmux::{
    Arg, Client, Command, DISPLAY_MESSAGE, LIST_PANES, Notification, Options, PaneId,
    SubscriptionTarget,
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

impl KillServerGuard {
    fn new(socket: PathBuf) -> Self {
        Self { socket }
    }
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
    _temp_dir: TempDir,
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
        let server_guard = KillServerGuard::new(socket.clone());

        Self {
            _server_guard: server_guard,
            _temp_dir: temp_dir,
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
