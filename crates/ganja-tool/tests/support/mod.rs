//! What the `evaluate_*` binaries that read their own log back would each
//! write otherwise: the writer they install as the process's global
//! subscriber.
//!
//! Not a test binary — `tests/support/` is a directory module, compiled only
//! into the binaries that declare `mod support;`. Not `ganja-testkit`'s
//! `LogCapture` either, though it is the same shape: that crate depends on
//! `ganja-core`, which depends on this one, and no shipped binary links it.

use std::io;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// A `tracing` writer a test reads back.
#[derive(Clone, Default)]
pub struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    /// What has been logged since the last call, leaving the capture empty.
    pub fn take(&self) -> String {
        let taken = std::mem::take(&mut *self.0.lock().expect("the log is never poisoned"));

        String::from_utf8_lossy(&taken).into_owned()
    }
}

impl io::Write for Capture {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("the log is never poisoned").extend_from_slice(buffer);

        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> MakeWriter<'writer> for Capture {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}
