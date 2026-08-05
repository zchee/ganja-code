//! `ganja serve` end to end: the real binary comes up, says where it
//! listens on stdout and that it is unsecured on stderr, answers a real
//! socket, and a SIGTERM ends it cleanly.
//!
//! Unix-only because the clean-shutdown half *is* the SIGTERM half; the
//! signal a supervisor sends is the thing under test.

#![cfg(unix)]

use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::TcpStream,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use tempfile::TempDir;

/// How long any single wait may take before the fixture is declared broken.
const DEADLINE: Duration = Duration::from_secs(60);

#[test]
fn serve_comes_up_answers_health_and_dies_cleanly_on_sigterm() {
    let project = TempDir::new().expect("a project directory is creatable");
    let data = TempDir::new().expect("a data home is creatable");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ganja"))
        .args(["serve", "--port", "0"])
        .current_dir(project.path())
        .env("XDG_DATA_HOME", data.path())
        // Unset, so the fake provider answers and no password is configured —
        // the unsecured warning below is the point.
        .env_remove("GANJA_PROVIDER")
        .env_remove("GANJA_MODEL")
        .env_remove("GANJA_CONFIG")
        .env_remove("GANJA_SERVER_PASSWORD")
        .env_remove("GANJA_SERVER_USERNAME")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts");

    // The address line, read on a thread so a server that never speaks fails
    // the deadline instead of hanging the harness.
    let stdout = child.stdout.take().expect("stdout is piped");
    let (line_tx, line_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = line_tx.send(line);
    });
    let line = line_rx
        .recv_timeout(DEADLINE)
        .expect("the server announces itself within the deadline");
    let line = line.trim();
    assert!(
        line.starts_with("ganja server listening on http://127.0.0.1:"),
        "upstream's address line under this build's name: {line:?}"
    );
    let port: u16 = line
        .rsplit(':')
        .next()
        .expect("the line ends with the port")
        .parse()
        .expect("the port is a number");

    // The reported address answers: one raw HTTP request, closed after.
    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("the reported address accepts");
    socket
        .set_read_timeout(Some(DEADLINE))
        .expect("a read timeout installs");
    socket
        .write_all(b"GET /global/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("the request writes");
    let mut answer = String::new();
    socket
        .read_to_string(&mut answer)
        .expect("the response reads");
    assert!(
        answer.starts_with("HTTP/1.1 200"),
        "health answers 200: {answer:?}"
    );
    assert!(
        answer.contains("\"healthy\":true"),
        "health says so: {answer:?}"
    );

    // A supervisor's SIGTERM ends it cleanly — exit 0, not a kill.
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success(), "the signal was delivered");

    let deadline = Instant::now() + DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child is waitable") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the server should exit on SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "a clean shutdown exits 0: {status:?}");

    // Diagnostics went to stderr: the unsecured warning names the variable
    // that would have secured it.
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr is piped")
        .read_to_string(&mut stderr)
        .expect("stderr reads");
    assert!(
        stderr.contains("GANJA_SERVER_PASSWORD is not set; server is unsecured."),
        "the unsecured warning is on stderr: {stderr:?}"
    );
}
