//! What every socket-driving suite in this directory rebuilds otherwise: an
//! engine on a scripted provider, a server bound to an OS-assigned loopback
//! port, and an SSE frame parser.
//!
//! Not a test binary — `tests/support/` is a directory module, compiled only
//! into the binaries that declare `mod support;`.

// Compiled once per binary, and each binary uses its own subset; what one
// leaves unused another is built on, so the per-binary dead-code lint has
// nothing useful to say here.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ganja_core::Engine;
use ganja_core::permission::Permissions;
use ganja_core::provider::ProviderEvent;
use ganja_core::tool::Registry;
use ganja_serve::{Credentials, DEFAULT_HOSTNAME, Handle, Listen, ServeConfig};
use ganja_testkit::{ScriptedProvider, says};
use secrecy::SecretString;

/// A heartbeat quick enough that a suite sees one without waiting ten
/// seconds.
pub const FAST_HEARTBEAT: Duration = Duration::from_millis(50);

/// How long any single wait in these suites may take before the fixture is
/// declared broken.
pub const DEADLINE: Duration = Duration::from_secs(30);

/// An ephemeral engine playing `scripts`, with `tools` registered and every
/// call allowed or asked exactly as `permissions` says.
pub fn scripted_engine(
    scripts: Vec<Vec<ProviderEvent>>,
    tools: Registry,
    permissions: Permissions,
) -> Arc<Engine> {
    let (provider, _requests) = ScriptedProvider::new(scripts);

    Arc::new(Engine::new(provider, "scripted-model", Arc::new(tools), permissions))
}

/// The one-turn engine most suites open with: a scripted "hi", no tools, and
/// default permissions.
pub fn engine() -> Arc<Engine> {
    scripted_engine(vec![says("hi")], Registry::new(Vec::new()), Permissions::default())
}

/// A loopback config for the working directory, on an OS-assigned port so
/// parallel suites cannot collide, heartbeating fast.
pub fn loopback_config() -> ServeConfig {
    let directory = std::env::current_dir().expect("the working directory resolves");
    let mut config = ServeConfig::in_directory(directory);
    config.listen = tcp(Some(0));
    config.heartbeat = FAST_HEARTBEAT;

    config
}

/// A loopback TCP ask on `port` — the shape every suite here but the socket
/// one binds.
pub fn tcp(port: Option<u16>) -> Listen {
    Listen::Tcp { hostname: DEFAULT_HOSTNAME.to_owned(), port }
}

/// The loopback fixture with its listen swapped for `listen` — the same
/// directory, the same fast heartbeat, bound wherever the suite says.
pub fn with_listen(listen: Listen) -> ServeConfig {
    let mut config = loopback_config();
    config.listen = listen;

    config
}

/// The URL a socket-bound client is given: the host is a label, unread by
/// the router, because `reqwest` resolves nothing once a socket is set.
pub const SOCKET_URL: &str = "http://ganja";

/// A client bound to `path` and nothing else — one per socket, the rule
/// every caller of `unix_socket` in this workspace keeps.
pub fn socket_client(path: &Path) -> reqwest::Client {
    reqwest::Client::builder().unix_socket(path).build().expect("a socket-bound client builds")
}

/// The credential a `GANJA_SERVER_PASSWORD` export resolves to
/// (`Credentials::from_env`), built directly so no suite here mutates the
/// process environment.
pub fn credentials() -> Credentials {
    Credentials { username: "ganja".to_owned(), password: SecretString::from("hunter2".to_owned()) }
}

/// The `Authorization` header for [`credentials`].
pub fn basic() -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("ganja:hunter2"))
}

/// The base URL a suite drives `handle` at — a TCP one; the socket suite
/// speaks through a client bound to the path instead.
pub fn base_url(handle: &Handle) -> String {
    format!("http://{}", handle.address().tcp().expect("these suites bind tcp"))
}

/// One SSE frame: the event name and the data line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub event: String,
    pub data: String,
}

/// Every complete frame at the front of `buffer`, drained out of it; a
/// partial frame stays for the next read to finish.
pub fn drain_frames(buffer: &mut Vec<u8>) -> Vec<Frame> {
    let mut frames = Vec::new();

    while let Some(end) = find_blank_line(buffer) {
        let raw: Vec<u8> = buffer.drain(..end + 2).collect();
        let text = String::from_utf8_lossy(&raw);
        let mut event = String::new();
        let mut data = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                event = rest.to_owned();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data = rest.to_owned();
            }
        }
        frames.push(Frame { event, data });
    }

    frames
}

fn find_blank_line(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|pair| pair == b"\n\n")
}
