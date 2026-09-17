//! What one `evaluate` call puts in the log, and what it must never put
//! there.
//!
//! **One test, one binary**, for two reasons at once. It sets
//! `TYPESAFE_API_KEY` and `TYPESAFE_BASE_URL`, which is process-wide state
//! and is this directory's usual reason. The other is `tracing`'s
//! per-callsite interest cache: a `set_default` subscriber is thread-local
//! and does not re-register a callsite another thread already reached, so in
//! the unit-test binary a sibling reaches the client's own event first, on a
//! thread with no subscriber at all, which caches that callsite as never and
//! leaves the capture empty. The subscriber here is the **process's global
//! default**, which re-registers every callsite — and a process-global
//! subscriber is process-wide state too.
//!
//! It is filtered to this crate's own targets on purpose. What is claimed is
//! that *this tool* logs no secret; an unfiltered global subscriber at TRACE
//! would also be recording `reqwest`'s and `hyper`'s view of the wire, and a
//! green assertion there would be about somebody else's code.
//!
//! The call goes through [`EvaluateTool::configured`] and [`Tool::run`] —
//! the whole shipped road, environment included — rather than through the
//! client underneath it, because the environment is what this binary exists
//! to be allowed to touch.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ganja_tool::evaluate::EvaluateTool;
use ganja_tool::{Credentials, FileTimes, ToolCtx};
use tokio_util::sync::CancellationToken;

/// The key this binary configures, kept distinctive so the leak check can
/// look for it by name.
const KEY: &str = "sk-typesafe-log-binary-0123456789";

/// The state it sends, likewise.
const STATE: &str = "payouts have been failing for three days";

/// What the fixture answers.
const ANSWERED: &str = r#"{"model":"jev-1.13.0","answers":{"urgent":{"type":"noul","noul":0.92}},"usage":{"input_tokens":312,"output_tokens":48}}"#;

#[tokio::test]
async fn nothing_an_evaluate_call_logs_carries_the_key_or_the_state() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("ganja_tool=trace"))
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("this binary installs exactly one global subscriber");

    let endpoint = serve();

    // SAFETY: this binary holds exactly one test, so no other thread is
    // reading the environment while these are set. A second test here would
    // silently invalidate that, which is the rule this directory keeps.
    unsafe {
        std::env::set_var("TYPESAFE_API_KEY", KEY);
        std::env::set_var("TYPESAFE_BASE_URL", &endpoint);
        std::env::remove_var("TYPESAFE_DEFAULT_MODEL");
    }

    let tool = EvaluateTool::configured().expect("a configured environment offers the tool");
    let answered = tool
        .run(
            serde_json::json!({
                "state": STATE,
                "questions": {"urgent": {"type": "noul", "instructions": "Urgent?"}}
            }),
            &ctx(),
        )
        .await
        .expect("the canned answer parses");

    assert_eq!(answered.title, "1 answer(s) · jev-1.13.0 · 312 input tokens");

    let logged = capture.logged();

    // The positive control first. Without it the two assertions below hold
    // for a client that logs nothing at all, and the leak check would be
    // proving only that the subscriber was installed wrong.
    assert!(logged.contains("typesafe answered"), "the debug event was captured: {logged}");
    assert!(logged.contains("status=200"), "and it carries the real status");
    assert!(logged.contains("input_tokens=312"), "and what it claims to");
    assert!(!logged.contains(KEY), "the key reaches no event");
    assert!(!logged.contains(STATE), "the state reaches no event");
}

/// A call with every seam empty.
///
/// Spelled out rather than built by a helper: `ToolCtx::fixture` is
/// `#[cfg(test)]` inside the library and deliberately invisible from out
/// here, so that the literal stays the only shape a caller outside the crate
/// can write — which is what makes a new field a decision somebody has to
/// make rather than one a default makes for them.
fn ctx() -> ToolCtx {
    ToolCtx {
        cwd: PathBuf::from("."),
        cancel: CancellationToken::new(),
        call_id: "call".to_owned(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: None,
        postbox: None,
        tasks: None,
        ask: None,
        switch: None,
        jobs: None,
    }
}

/// A loopback endpoint answering one request with [`ANSWERED`], on a plain
/// thread.
///
/// The unit-test fixture is `#[cfg(test)]` inside the library and unreachable
/// from here, so this is a second, much smaller server — one connection is
/// all this test needs, which is exactly what the other one could not do.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is bindable");
    let base = format!("http://{}", listener.local_addr().expect("a bound socket has an address"));

    std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return;
        };
        // Enough of the request to know it arrived; the assertions here are
        // about the log, not about the wire.
        let mut chunk = [0_u8; 8192];
        let _ = socket.read(&mut chunk);
        let _ = socket.write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{ANSWERED}",
                ANSWERED.len()
            )
            .as_bytes(),
        );
        let _ = socket.flush();
    });

    base
}

/// A `tracing` writer this test reads back.
///
/// `ganja-testkit`'s `LogCapture` is the same shape, and unreachable: no
/// shipped binary links that crate and `ganja-tool` does not depend on it.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    /// What has been logged so far.
    fn logged(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("the log is never poisoned")).into_owned()
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
