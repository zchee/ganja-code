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

use std::{sync::Arc, time::Duration};

use ganja_core::{Engine, permission::Permissions, provider::ProviderEvent, tool::Registry};
use ganja_serve::{Handle, ServeConfig};
use ganja_testkit::ScriptedProvider;

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

    Arc::new(Engine::new(
        provider,
        "scripted-model",
        Arc::new(tools),
        permissions,
    ))
}

/// A loopback config for the working directory, on an OS-assigned port so
/// parallel suites cannot collide, heartbeating fast.
pub fn loopback_config() -> ServeConfig {
    let directory = std::env::current_dir().expect("the working directory resolves");
    let mut config = ServeConfig::in_directory(directory);
    config.port = Some(0);
    config.heartbeat = FAST_HEARTBEAT;

    config
}

/// The base URL a suite drives `handle` at.
pub fn base_url(handle: &Handle) -> String {
    format!("http://{}", handle.address())
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
