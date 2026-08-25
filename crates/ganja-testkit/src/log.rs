//! The `tracing` capture every suite that reads its own log back shares.

use std::{
    io,
    sync::{Arc, Mutex},
};

/// A `tracing` writer a test can read back.
///
/// Handed to `tracing_subscriber::fmt().with_writer(...)`; each suite still
/// builds its own subscriber, because the level it cares about is the test's
/// business — WARN for the refusal suites, TRACE for the ones reading a
/// wire's chatter back.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    /// What has been logged so far.
    #[must_use]
    pub fn logged(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("the log is never poisoned")).into_owned()
    }
}

impl io::Write for LogCapture {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the log is never poisoned")
            .extend_from_slice(buffer);

        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogCapture {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}
