//! The `tracing` capture every suite that reads its own log back shares.

use std::io;
use std::sync::{Arc, Mutex};

/// A `tracing` writer a test can read back.
///
/// Handed to `tracing_subscriber::fmt().with_writer(...)`, or installed whole
/// by [`LogCapture::install`]. The level is the caller's either way, because
/// the level it cares about is the test's business — WARN for the refusal
/// suites, TRACE for the ones reading a wire's chatter back.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    /// A capture installed at `level` as the calling thread's default
    /// subscriber, beside the guard that keeps it installed — without ANSI,
    /// so what a test reads back is the text a line says.
    ///
    /// Thread-local rather than global, because a test binary holds many
    /// tests and a plain `cargo test` runs them on parallel threads; under
    /// the current-thread runtime a `#[tokio::test]` runs on, every task the
    /// test spawns logs from this thread too. The guard must live as long as
    /// the test does.
    #[must_use]
    pub fn install(level: tracing::Level) -> (Self, tracing::subscriber::DefaultGuard) {
        let capture = Self::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .with_max_level(level)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);

        (capture, guard)
    }

    /// What has been logged so far.
    #[must_use]
    pub fn logged(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("the log is never poisoned")).into_owned()
    }
}

impl io::Write for LogCapture {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("the log is never poisoned").extend_from_slice(buffer);

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
