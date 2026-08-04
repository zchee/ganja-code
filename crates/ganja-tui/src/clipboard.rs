//! The system clipboard, behind a seam.
//!
//! Spec: upstream `packages/tui/src/clipboard.ts` and
//! `context/clipboard.tsx`, which injects the same pair of functions so its
//! own tests can watch what was copied. Ganja needs that seam for a harder
//! reason: a headless CI has no clipboard at all, so a copy command that
//! spoke to [`arboard`] directly would be a command no test could ever run.
//!
//! Two deliberate narrowings from upstream:
//!
//! - **OSC 52 is not written** (**D109**). Upstream writes the escape *and*
//!   then a native tool; ganja writes only through [`arboard`], so copying
//!   from a tmux pane or over SSH lands on the machine the process runs on
//!   rather than on the one the terminal is attached to.
//! - **Images are not read.** The workspace pins `arboard` without its
//!   `image-data` feature, so this build has no way to ask for an image —
//!   and no way to tell an image-only clipboard from an empty one, since
//!   `arboard` answers `ContentNotAvailable` for both. Everything that is not
//!   text arrives here as [`Error::NotText`] and the app says the one thing
//!   that is true of the interesting half of that class (**D111**).
//!
//! Construction is **lazy**: the handle is built on the first copy or paste
//! and kept, so a session that never touches the clipboard never asks the
//! desktop for one — and a machine that cannot provide one costs a status
//! notice at the moment it is asked rather than a refusal at startup.

/// Why a clipboard operation could not be carried out.
///
/// Cloneable so a test double can hold one and hand it back on demand.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// There is no clipboard to talk to, or it refused.
    #[error("the clipboard is not available: {0}")]
    Unavailable(String),
    /// The clipboard holds something, but not text this build can insert —
    /// or it holds nothing at all. See the module docs: `arboard` without
    /// `image-data` cannot tell those two apart.
    #[error("the clipboard holds nothing this build can paste")]
    NotText,
}

/// Reading and writing the system clipboard.
///
/// `&mut self` on both halves because the platform handles behind them are
/// not shareable, and because it keeps the lazy construction honest: the one
/// place that may build a handle is the one place that is already exclusive.
pub trait Clipboard: Send {
    /// Puts `text` on the clipboard, replacing whatever was there.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no clipboard, or when it refused what
    /// it was handed.
    fn write(&mut self, text: &str) -> Result<(), Error>;

    /// What the clipboard holds, as text.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotText`] when it holds something else — see the
    /// module docs — and [`Error::Unavailable`] when there is nothing to ask.
    fn read(&mut self) -> Result<String, Error>;
}

/// The clipboard the desktop this process runs on provides.
#[derive(Default)]
pub struct System {
    /// Built on first use and kept: `arboard` holds a connection to the
    /// display server on Linux, and reconnecting per copy would be a
    /// round trip for every keystroke that pastes.
    handle: Option<arboard::Clipboard>,
}

impl System {
    /// The handle, building it if this is the first call.
    ///
    /// A failure is **not** remembered. Nothing here can tell a desktop that
    /// will never appear from one that is still coming up, and the cost of
    /// being wrong the optimistic way is one more failed call.
    fn handle(&mut self) -> Result<&mut arboard::Clipboard, Error> {
        if self.handle.is_none() {
            self.handle = Some(
                arboard::Clipboard::new().map_err(|error| Error::Unavailable(error.to_string()))?,
            );
        }

        self.handle
            .as_mut()
            .ok_or_else(|| Error::Unavailable("the clipboard went away".to_owned()))
    }
}

impl Clipboard for System {
    fn write(&mut self, text: &str) -> Result<(), Error> {
        self.handle()?
            .set_text(text)
            .map_err(|error| Error::Unavailable(error.to_string()))
    }

    fn read(&mut self) -> Result<String, Error> {
        self.handle()?.get_text().map_err(|error| match error {
            // The one error that is about the *contents* rather than about
            // the clipboard: `arboard` documents it as "empty, or holding a
            // different format", which is exactly the class this build
            // cannot insert.
            arboard::Error::ContentNotAvailable => Error::NotText,
            other => Error::Unavailable(other.to_string()),
        })
    }
}

/// A clipboard that remembers what it was handed and answers reads from a
/// script.
///
/// The whole reason the trait above exists: every copy command asserts against
/// this, so the assertions are about what ganja decided to copy rather than
/// about what some machine's desktop happened to accept.
#[cfg(test)]
#[derive(Debug)]
pub struct Recording {
    /// Everything written, in order.
    pub written: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// What the next read answers with.
    pub holds: Result<String, Error>,
    /// What every write answers with; [`Ok`] by default.
    pub accepts: Result<(), Error>,
}

#[cfg(test)]
impl Default for Recording {
    /// A clipboard nothing has been put on, which is the answer a real one
    /// gives for an empty desktop selection.
    fn default() -> Self {
        Self {
            written: std::sync::Arc::default(),
            holds: Err(Error::NotText),
            accepts: Ok(()),
        }
    }
}

#[cfg(test)]
impl Recording {
    /// A clipboard holding `text`, accepting everything written to it.
    pub fn holding(text: &str) -> Self {
        Self {
            holds: Ok(text.to_owned()),
            ..Self::default()
        }
    }

    /// A clipboard whose reads fail with `error`.
    pub fn refusing_reads(error: Error) -> Self {
        Self {
            holds: Err(error),
            ..Self::default()
        }
    }

    /// A clipboard whose writes fail with `error`.
    pub fn refusing_writes(error: Error) -> Self {
        Self {
            accepts: Err(error),
            ..Self::default()
        }
    }

    /// A handle onto what this clipboard is handed, which survives the move
    /// into the app.
    pub fn log(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        std::sync::Arc::clone(&self.written)
    }
}

#[cfg(test)]
impl Clipboard for Recording {
    fn write(&mut self, text: &str) -> Result<(), Error> {
        self.accepts.clone()?;
        self.written
            .lock()
            .expect("the recording lock is never poisoned")
            .push(text.to_owned());

        Ok(())
    }

    fn read(&mut self) -> Result<String, Error> {
        self.holds.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{Clipboard as _, Error, Recording};

    #[test]
    fn a_recording_clipboard_keeps_every_write_in_order() {
        let mut clipboard = Recording::default();
        let log = clipboard.log();

        clipboard.write("first").expect("the write is accepted");
        clipboard.write("second").expect("the write is accepted");

        assert_eq!(
            *log.lock().expect("the lock holds"),
            vec!["first".to_owned(), "second".to_owned()]
        );
    }

    #[test]
    fn a_refused_write_is_not_recorded() {
        let mut clipboard = Recording::refusing_writes(Error::Unavailable("no display".to_owned()));
        let log = clipboard.log();

        let refusal = clipboard.write("nothing lands").expect_err("it refuses");

        assert!(format!("{refusal}").contains("no display"));
        assert!(log.lock().expect("the lock holds").is_empty());
    }

    /// The default is the interesting failure: an empty clipboard and one
    /// holding only an image are the same answer under this feature set.
    #[test]
    fn a_clipboard_nothing_was_put_on_reads_as_not_text() {
        assert_eq!(Recording::default().read(), Err(Error::NotText));
        assert_eq!(Recording::holding("typed").read(), Ok("typed".to_owned()));
    }

    /// The one thing the seam above cannot prove: that [`System`] is wired to
    /// a clipboard that really works.
    ///
    /// `#[ignore]`d because it needs a desktop CI does not have, **and because
    /// it overwrites whatever the person running it had copied** — which is
    /// why it puts something recognizable there rather than a fixture string
    /// that would look like a bug in whatever they paste it into. Run with
    /// `cargo test -p ganja-tui -- --ignored the_system_clipboard`.
    #[test]
    #[ignore = "needs a desktop clipboard, and overwrites what is on it"]
    fn the_system_clipboard_round_trips_what_it_is_handed() {
        let mut clipboard = super::System::default();
        let written = "ganja clipboard smoke test";

        clipboard.write(written).expect("the desktop accepts text");

        assert_eq!(clipboard.read().as_deref(), Ok(written));
    }
}
