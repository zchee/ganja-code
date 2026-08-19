//! Port of `pandaemonium pkg/tmux/examples/02_existing_session/main.go`.
//!
//! The Go program's `PANDAEMONIUM_TMUX_*` variables are renamed to this
//! crate's `TMUX_RS_*` prefix: `TMUX_RS_SESSION` is required;
//! `TMUX_RS_SOCKET_PATH` and `TMUX_RS_SOCKET_NAME` are optional and mutually
//! exclusive; and `TMUX_RS_CONFIG_FILE` is optional. The example only reads
//! the attached session with `display-message` and `list-panes`; cleanup merely
//! detaches this control client, so it never mutates another user's session,
//! windows, or panes.
//!
//! Run with:
//! `TMUX_RS_SESSION=my-session cargo run -p tmux --example existing_session`.

use std::{env, error::Error, io, path::PathBuf, time::Duration};

use tmux::control_mode::{Arg, Client, Options};

const SESSION_ENV: &str = "TMUX_RS_SESSION";
const SOCKET_PATH_ENV: &str = "TMUX_RS_SOCKET_PATH";
const SOCKET_NAME_ENV: &str = "TMUX_RS_SOCKET_NAME";
const CONFIG_FILE_ENV: &str = "TMUX_RS_CONFIG_FILE";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let Some(session) = non_empty_utf8_env(SESSION_ENV)? else {
        println!("set TMUX_RS_SESSION to an existing tmux session name to run this example");
        return Ok(());
    };

    let socket_path = non_empty_path_env(SOCKET_PATH_ENV);
    let socket_name = non_empty_utf8_env(SOCKET_NAME_ENV)?;
    let config_file = non_empty_path_env(CONFIG_FILE_ENV);
    if socket_name.is_some() && socket_path.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TMUX_RS_SOCKET_NAME and TMUX_RS_SOCKET_PATH are mutually exclusive",
        )
        .into());
    }

    run_existing_session(ExistingSessionConfig {
        session,
        socket_path,
        socket_name,
        config_file,
    })
    .await
}

struct ExistingSessionConfig {
    session: String,
    socket_path: Option<PathBuf>,
    socket_name: Option<String>,
    config_file: Option<PathBuf>,
}

async fn run_existing_session(config: ExistingSessionConfig) -> Result<(), Box<dyn Error>> {
    let mut options = Options::new()
        .with_session_name(&config.session)
        .with_create_session(false)
        .with_shutdown_timeout(Duration::from_secs(2));
    if let Some(socket_path) = config.socket_path.as_deref() {
        options = options.with_socket_path(socket_path);
    }
    if let Some(socket_name) = config.socket_name.as_deref() {
        options = options.with_socket_name(socket_name);
    }
    if let Some(config_file) = config.config_file.as_deref() {
        options = options.with_config_file(config_file);
    }

    let client = Client::new(options).await.map_err(|err| {
        io::Error::other(format!(
            "attach to existing tmux session {:?}: {err}",
            config.session
        ))
    })?;

    let result = inspect_session(&client, &config.session).await;
    let _ = client.close().await;
    result
}

async fn inspect_session(client: &Client, session: &str) -> Result<(), Box<dyn Error>> {
    let session_response = client
        .exec(
            tmux::control_mode::DISPLAY_MESSAGE,
            [Arg::raw("-p"), Arg::string("#{session_name}")],
        )
        .await
        .map_err(|err| io::Error::other(format!("read attached session name: {err}")))?;
    let attached_session = session_response.lines.join("\n");
    if attached_session != session {
        return Err(io::Error::other(format!(
            "attached to session {attached_session:?}, want {session:?}"
        ))
        .into());
    }

    let pane_response = client
        .exec(
            tmux::control_mode::DISPLAY_MESSAGE,
            [Arg::raw("-p"), Arg::string("#{pane_id}")],
        )
        .await
        .map_err(|err| io::Error::other(format!("read active pane id: {err}")))?;
    let pane_id = pane_response.lines.join("\n");
    if pane_id.is_empty() {
        return Err(io::Error::other("active pane id is empty").into());
    }

    let panes_response = client
        .exec(
            tmux::control_mode::LIST_PANES,
            [
                Arg::raw("-t"),
                Arg::string(session),
                Arg::raw("-F"),
                Arg::string("#{pane_id}:#{window_index}.#{pane_index}:#{pane_current_command}"),
            ],
        )
        .await
        .map_err(|err| io::Error::other(format!("list panes in {session:?}: {err}")))?;
    let first_pane = panes_response
        .lines
        .first()
        .ok_or_else(|| io::Error::other(format!("session {session:?} has no panes")))?;

    println!("attached session: {attached_session}");
    println!("active pane: {pane_id}");
    println!("panes: {}", panes_response.lines.len());
    println!("first pane: {first_pane}");
    println!("dropped notifications: {}", client.dropped_notifications());
    Ok(())
}

fn non_empty_utf8_env(name: &str) -> Result<Option<String>, io::Error> {
    match env::var(name) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} must be valid UTF-8"),
        )),
    }
}

fn non_empty_path_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
