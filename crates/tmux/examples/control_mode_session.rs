//! Port of `pandaemonium pkg/tmux/examples/01_control_mode_session/main.go`.
//!
//! This example deliberately retains the Go program's `RUN_REAL_TMUX_TESTS=1`
//! opt-in gate because a person runs an example binary specifically to touch a
//! real tmux server. That differs from this workspace's integration-test policy,
//! which hard-fails when a required real dependency is missing instead of
//! reporting a skipped success.
//!
//! Run with:
//! `RUN_REAL_TMUX_TESTS=1 cargo run -p tmux --example control_mode_session`.

use std::error::Error;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, io};

use tmux::control_mode::{Arg, Client, Options};

const RUN_REAL_TMUX_TESTS_ENV: &str = "RUN_REAL_TMUX_TESTS";
const TMUX_EXECUTABLE: &str = "tmux";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if env::var_os(RUN_REAL_TMUX_TESTS_ENV).as_deref() != Some(OsStr::new("1")) {
        println!(
            "set RUN_REAL_TMUX_TESTS=1 to run this example against an isolated real tmux server"
        );
        return Ok(());
    }

    run_real_tmux().await
}

async fn run_real_tmux() -> Result<(), Box<dyn Error>> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| io::Error::other(format!("read system time for scratch directory: {err}")))?
        .as_nanos();
    let temp_root = fs::canonicalize(env::temp_dir())
        .map_err(|err| io::Error::other(format!("resolve temporary directory: {err}")))?;
    // Keep the component short because macOS caps Unix-domain socket paths.
    let scratch_path = temp_root.join(format!("tr-{}-{nonce:x}", std::process::id()));
    let scratch = ScratchServer::create(scratch_path)
        .map_err(|err| io::Error::other(format!("create temporary directory: {err}")))?;

    let config = scratch.root().join("tmux.conf");
    write_empty_config(&config)
        .map_err(|err| io::Error::other(format!("write empty tmux config: {err}")))?;

    let session = format!("ganja-tmux-example-{}", std::process::id());
    let options = Options::new()
        .with_socket_path(scratch.socket())
        .with_config_file(config)
        .with_session_name(&session)
        .with_create_session(true)
        .with_shutdown_timeout(Duration::from_secs(2));

    // The guard is dropped only after `run_client` has awaited its close, so
    // teardown stays ordered the way Go's LIFO defers order it.
    let result = run_client(options, &session).await;
    drop(scratch);
    result
}

async fn run_client(options: Options, session: &str) -> Result<(), Box<dyn Error>> {
    let client = Client::new(options)
        .await
        .map_err(|err| io::Error::other(format!("start tmux control client: {err}")))?;

    let result = inspect_session(&client, session).await;
    let _ = client.close().await;
    result
}

async fn inspect_session(client: &Client, session: &str) -> Result<(), Box<dyn Error>> {
    let message_response = client
        .exec(
            tmux::control_mode::DISPLAY_MESSAGE,
            [Arg::raw("-p"), Arg::string("hello from pkg/tmux")],
        )
        .await
        .map_err(|err| io::Error::other(format!("display message: {err}")))?;
    let message = message_response.lines.join("\n");

    let panes_response = client
        .exec(
            tmux::control_mode::LIST_PANES,
            [
                Arg::raw("-a"),
                Arg::raw("-F"),
                Arg::string("#{session_name}:#{window_index}.#{pane_index}"),
            ],
        )
        .await
        .map_err(|err| io::Error::other(format!("list panes: {err}")))?;
    let first_pane = panes_response
        .lines
        .first()
        .ok_or_else(|| io::Error::other("list panes returned no panes"))?;

    println!("session: {session}");
    println!("message: {message}");
    println!("panes: {}", panes_response.lines.len());
    println!("first pane: {first_pane}");
    Ok(())
}

fn write_empty_config(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let _file = options.open(path)?;
    Ok(())
}

struct ScratchServer {
    root: PathBuf,
    socket: PathBuf,
}

impl ScratchServer {
    fn create(root: PathBuf) -> io::Result<Self> {
        fs::create_dir(&root)?;
        let socket = root.join("tmux.sock");
        Ok(Self { root, socket })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for ScratchServer {
    fn drop(&mut self) {
        // Cleanup is deliberately best-effort: it must not replace the error
        // from setup or a control command with a secondary teardown failure.
        let _ = std::process::Command::new(TMUX_EXECUTABLE)
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = fs::remove_dir_all(&self.root);
    }
}
