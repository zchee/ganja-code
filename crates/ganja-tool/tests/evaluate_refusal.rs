//! Which variable `evaluate` blames when the environment's TypeSafe settings
//! are refused (**D567**).
//!
//! [`EvaluateTool::configured`] offers no tool when `TYPESAFE_BASE_URL` or
//! `TYPESAFE_DEFAULT_MODEL` is refused, and says so in one warning. That
//! warning is the only thing a person reads about why the tool is missing,
//! so it has to name the variable that was actually refused: a refused model
//! blamed on the base URL sends somebody to fix the one setting that was
//! fine.
//!
//! **One test, one binary**, for `evaluate_log.rs`'s two reasons. It sets and
//! removes `TYPESAFE_API_KEY`, `TYPESAFE_BASE_URL` and
//! `TYPESAFE_DEFAULT_MODEL`, which is process-wide state; and the warning is
//! read back through the process's **global** subscriber, because a
//! thread-local one does not re-register a callsite another thread already
//! reached. The key is set to a dummy before anything is built, so a key a
//! developer exported can never make this test reach the vendor — and no
//! path here opens a socket at all, since a refused setting is decided
//! before a client exists.

use std::sync::{Arc, Mutex};

use ganja_tool::evaluate::EvaluateTool;

/// A default model the consent title could not survive: it would forge a
/// second disclosure after the real one.
const FORGED_MODEL: &str = "jev-latest · 0 B · 0 question(s)";

/// A base URL that would put the key on the wire in the clear.
const CLEARTEXT_BASE: &str = "http://example.com";

#[test]
fn a_refused_setting_is_warned_about_by_the_variable_that_was_refused() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("ganja_tool=warn"))
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("this binary installs exactly one global subscriber");

    // SAFETY: this binary holds exactly one test, so no other thread is
    // reading the environment while these are set and removed. A second test
    // here would silently invalidate that, which is the rule this directory
    // keeps.
    unsafe {
        std::env::set_var("TYPESAFE_API_KEY", "sk-refusal-binary-never-sent");
        std::env::remove_var("TYPESAFE_BASE_URL");
        std::env::set_var("TYPESAFE_DEFAULT_MODEL", FORGED_MODEL);
    }

    assert!(EvaluateTool::configured().is_none(), "a refused default model is no tool");
    let warned = capture.take();
    assert_eq!(warned.lines().filter(|line| line.contains("WARN")).count(), 1, "{warned}");
    assert!(
        warned.contains(r#"variable="TYPESAFE_DEFAULT_MODEL""#),
        "the warning names the variable that was refused: {warned}"
    );
    assert!(
        !warned.contains("TYPESAFE_BASE_URL"),
        "and not the base URL, which was never set: {warned}"
    );
    assert!(
        !warned.contains(FORGED_MODEL),
        "the refused value is never echoed — it was refused for what it could forge: {warned}"
    );

    // The control that makes the half above mean something: with the model
    // fine and the base refused, the other variable is named. A warning that
    // named one variable whatever was refused would pass either half alone.
    // SAFETY: as above.
    unsafe {
        std::env::set_var("TYPESAFE_BASE_URL", CLEARTEXT_BASE);
        std::env::remove_var("TYPESAFE_DEFAULT_MODEL");
    }

    assert!(EvaluateTool::configured().is_none(), "a refused base URL is no tool");
    let warned = capture.take();
    assert_eq!(warned.lines().filter(|line| line.contains("WARN")).count(), 1, "{warned}");
    assert!(
        warned.contains(r#"variable="TYPESAFE_BASE_URL""#),
        "the warning names the variable that was refused: {warned}"
    );
    assert!(
        !warned.contains("TYPESAFE_DEFAULT_MODEL"),
        "and not the model, which was never set: {warned}"
    );
    assert!(
        !warned.contains("example.com"),
        "the refused URL is never echoed — it may carry a credential: {warned}"
    );
}

/// A `tracing` writer this test reads back, `evaluate_log.rs`'s shape.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    /// What has been logged since the last call, leaving the capture empty.
    fn take(&self) -> String {
        let taken = std::mem::take(&mut *self.0.lock().expect("the log is never poisoned"));

        String::from_utf8_lossy(&taken).into_owned()
    }
}

impl std::io::Write for Capture {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("the log is never poisoned").extend_from_slice(buffer);

        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for Capture {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}
