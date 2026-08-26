//! Focus-gated terminal notifications (**D468**, `tui-notifications`).
//!
//! No upstream opencode counterpart: what this ports is the Codex CLI's
//! terminal notification — an escape written to the terminal itself when a
//! turn ends or a dialog waits while nobody is looking — configured through
//! the curated `tui` table (`ganja-core` `config.rs`). Two emissions and only
//! two: OSC 9 (`ESC ] 9 ; body BEL`), which terminals that carry it surface
//! as a desktop notification, and plain BEL when the config asked for the
//! bell instead. **External-program notification is deliberately not
//! duplicated here** — a config that wants a command run at these moments
//! already has one: the `Notification`/`Stop` hooks (**D456**), which carry
//! the full JSON envelope this one-line escape never could.
//!
//! The writer is a seam for [`crate::clipboard`]'s reason: production writes
//! the same stdout the OSC 52 copy escape already rides, and a headless test
//! injects a capture buffer so the assertion is about the bytes ganja decided
//! to emit rather than about a terminal the CI does not have.

use std::io::Write;

use ganja_core::config::{NotificationEvent, NotificationMethod, TuiConfig};

/// Longest body an OSC 9 escape carries. A notification is a headline, not a
/// transcript; a terminal popup truncates anyway, and an unbounded body would
/// put a whole tool result on the wire for a popup that shows one line.
const MAX_BODY: usize = 120;

/// Writes the configured notification escapes to the terminal.
///
/// Holds the loaded `tui` table rather than being consulted about it per
/// call: whether a moment is announced at all is this seam's one decision,
/// so the config that decides it lives where the decision is made. Focus is
/// deliberately **not** held here — whether anybody is watching is the event
/// loop's knowledge, and the app suppresses before this is ever asked.
pub struct Notifier {
    config: TuiConfig,
    sink: Box<dyn Write + Send>,
}

impl Default for Notifier {
    /// A notifier over stdout under the default config — which asks for no
    /// moments at all, so the default is silent by construction rather than
    /// by a guard.
    fn default() -> Self {
        Self::to_stdout(TuiConfig::default())
    }
}

impl Notifier {
    /// The production notifier: `config`'s moments, written to stdout — the
    /// same channel the frame and the OSC 52 copy escape already ride.
    #[must_use]
    pub fn to_stdout(config: TuiConfig) -> Self {
        Self::over(config, Box::new(std::io::stdout()))
    }

    /// The same decisions over a sink of the caller's choosing, which is what
    /// lets a test capture the bytes instead of needing a terminal.
    #[must_use]
    pub fn over(config: TuiConfig, sink: Box<dyn Write + Send>) -> Self {
        Self { config, sink }
    }

    /// Announces `event` with `summary`, if the config asked for that moment.
    ///
    /// A write that fails is one lost notification, never an error: this is
    /// the posture the OSC 52 flush already takes, and a session must not
    /// fail because a popup could not be shown.
    pub fn notify(&mut self, event: NotificationEvent, summary: &str) {
        if !self.config.notifies(event) {
            return;
        }

        let escape = match self.config.notification_method() {
            NotificationMethod::Osc9 => format!("\x1b]9;{}\x07", body(summary)),
            NotificationMethod::Bel => "\x07".to_owned(),
        };
        if let Err(error) = self
            .sink
            .write_all(escape.as_bytes())
            .and_then(|()| self.sink.flush())
        {
            tracing::warn!(%error, "a terminal notification could not be written");
        }
    }
}

/// `summary`'s first line, control bytes dropped, clamped to [`MAX_BODY`].
///
/// The body rides *inside* an escape sequence, so a stray ESC or BEL in it
/// would end or corrupt the very sequence carrying it — and a summary is one
/// line by definition, so everything past the first newline is noise here.
fn body(summary: &str) -> String {
    summary
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_BODY)
        .collect()
}

/// A sink that keeps every byte it is handed, for asserting what a
/// [`Notifier`] decided to emit.
///
/// The handle survives the move into the notifier the way
/// [`crate::clipboard::Recording::log`] survives the move into the app.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct Capture {
    written: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

#[cfg(test)]
impl Capture {
    /// A handle onto whatever the notifier writes from now on.
    pub fn log(&self) -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
        std::sync::Arc::clone(&self.written)
    }
}

#[cfg(test)]
impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written
            .lock()
            .expect("the capture lock is never poisoned")
            .extend_from_slice(buf);

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "notify_tests.rs"]
mod tests;
