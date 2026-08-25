//! `ganja serve` end to end: the real binary comes up, says where it
//! listens on stdout and that it is unsecured on stderr, answers a real
//! socket, and a SIGTERM ends it cleanly — and the engine behind that socket
//! is assembled the way the other two frontends assemble theirs.
//!
//! Unix-only because the clean-shutdown half *is* the SIGTERM half; the
//! signal a supervisor sends is the thing under test.

#![cfg(unix)]

use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use ganja_testkit::temp_dir as temporary;
use tempfile::TempDir;

/// How long any single wait may take before the fixture is declared broken.
const DEADLINE: Duration = Duration::from_secs(60);

/// A real `ganja serve` child, listening on a port the kernel picked.
struct Served {
    child: Child,
    port: u16,
    _data: TempDir,
    _config: TempDir,
}

impl Served {
    /// Spawns the server in `project` and waits for it to announce itself.
    ///
    /// All three homes are pinned, not merely `XDG_DATA_HOME`: ganja's global
    /// config home is resolved against `GANJA_CONFIG_HOME`, `XDG_CONFIG_HOME`
    /// and `HOME` in that order, and a developer holding any of them would
    /// otherwise hand this server a config — and a skills tier — that no
    /// assertion here accounts for.
    fn in_project(project: &Path) -> Self {
        let data = temporary();
        let config = temporary();

        let mut child = Command::new(env!("CARGO_BIN_EXE_ganja"))
            .args(["serve", "--port", "0"])
            .current_dir(project)
            .env("XDG_DATA_HOME", data.path())
            .env("XDG_CONFIG_HOME", config.path())
            .env("HOME", config.path())
            .env_remove("GANJA_CONFIG_HOME")
            // Unset, so the fake provider answers and no password is
            // configured — the unsecured warning below is the point.
            .env_remove("GANJA_PROVIDER")
            .env_remove("GANJA_MODEL")
            .env_remove("GANJA_CONFIG")
            .env_remove("GANJA_SERVER_PASSWORD")
            .env_remove("GANJA_SERVER_USERNAME")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .env("GANJA_FAKE_SCRIPT", project.join("script.json"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary starts");

        // The address line, read on a thread so a server that never speaks
        // fails the deadline instead of hanging the harness.
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
        let port = line
            .rsplit(':')
            .next()
            .expect("the line ends with the port")
            .parse()
            .expect("the port is a number");

        Self {
            child,
            port,
            _data: data,
            _config: config,
        }
    }

    /// One `ganja run --attach` against this server, answering its stdout.
    ///
    /// Run from a directory that holds nothing, and with every home of its
    /// own: an attached run assembles no engine, no config and no tool
    /// registry, so anything the turn reflects came off the server's side of
    /// the socket rather than this process's.
    fn attached_run(&self, arguments: &[&str]) -> String {
        let elsewhere = temporary();
        let data = temporary();
        let config = temporary();

        let output = Command::new(env!("CARGO_BIN_EXE_ganja"))
            .args([
                "run",
                "--attach",
                &format!("http://127.0.0.1:{}", self.port),
            ])
            .args(arguments)
            .current_dir(elsewhere.path())
            .env("XDG_DATA_HOME", data.path())
            .env("XDG_CONFIG_HOME", config.path())
            .env("HOME", config.path())
            .env_remove("GANJA_CONFIG_HOME")
            .env_remove("GANJA_PROVIDER")
            .env_remove("GANJA_MODEL")
            .env_remove("GANJA_CONFIG")
            .env_remove("GANJA_SERVER_PASSWORD")
            .env_remove("GANJA_SERVER_USERNAME")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .stdin(Stdio::null())
            .output()
            .expect("the attached run finishes");
        assert!(
            output.status.success(),
            "the attached run exits zero: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8(output.stdout).expect("the output is text")
    }

    /// Ends the server the way a supervisor would, and answers its stderr.
    fn stop(mut self) -> String {
        let killed = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill runs");
        assert!(killed.success(), "the signal was delivered");

        let deadline = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("the child is waitable") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "the server should exit on SIGTERM"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(status.success(), "a clean shutdown exits 0: {status:?}");

        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .expect("stderr is piped")
            .read_to_string(&mut stderr)
            .expect("stderr reads");

        stderr
    }
}

/// A script whose only turn says something and nothing else.
fn one_word() -> serde_json::Value {
    serde_json::json!({"cadence_ms": 1, "turns": [{"text": "script-finished-zarquon"}]})
}

#[test]
fn serve_comes_up_answers_health_and_dies_cleanly_on_sigterm() {
    let project = temporary();
    std::fs::write(project.path().join("script.json"), one_word().to_string())
        .expect("the script is writable");
    let served = Served::in_project(project.path());

    // The reported address answers: one raw HTTP request, closed after.
    let mut socket =
        TcpStream::connect(("127.0.0.1", served.port)).expect("the reported address accepts");
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

    // A supervisor's SIGTERM ends it cleanly — exit 0, not a kill — and the
    // diagnostics went to stderr: the unsecured warning names the variable
    // that would have secured it.
    let stderr = served.stop();
    assert!(
        stderr.contains("GANJA_SERVER_PASSWORD is not set; server is unsecured."),
        "the unsecured warning is on stderr: {stderr:?}"
    );
}

/// What the prompt offers, a call over the socket can load.
///
/// `serve` assembles its engine the way `run` and the UI assemble theirs, so
/// the skill tool it installs has to hold the roots its own prompt was
/// composed from. A server that advertised a skill and then refused to load it
/// would be lying to the model about what it can do, and this is the one
/// frontend no in-process test reaches — the client here is the binary itself,
/// which renders whatever the server's event stream carried and nothing of its
/// own.
#[test]
fn a_skill_beside_the_served_project_loads_over_the_socket() {
    let project = temporary();
    std::fs::write(
        project.path().join("script.json"),
        serde_json::json!({
            "cadence_ms": 1,
            "turns": [
                {
                    "text": "loading it",
                    "tool_calls": [{"name": "skill", "args": {"name": "porting"}}],
                },
                {"text": "script-finished-zarquon"},
            ],
        })
        .to_string(),
    )
    .expect("the script is writable");
    let skill = project.path().join(".ganja").join("skills").join("porting");
    std::fs::create_dir_all(&skill).expect("the skill's directory is creatable");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: porting\ndescription: How to port a module.\n---\nRead the upstream file first.\n",
    )
    .expect("the skill is writable");

    let served = Served::in_project(project.path());
    let stdout = served.attached_run(&["--format", "json", "load the porting skill"]);

    assert!(
        stdout.contains("script-finished-zarquon"),
        "the whole script has to have run: {stdout:?}"
    );
    assert!(
        stdout.contains("<skill_content name=\\\"porting\\\">")
            && stdout.contains("Read the upstream file first."),
        "the call is handed the skill's own instructions: {stdout:?}"
    );
    // The failure this exists to catch spells itself out, so name it: a tool
    // holding no roots answers every call with this sentence, whatever the
    // prompt beside it offered.
    assert!(
        !stdout.contains("Available skills: none"),
        "and the tool was handed the roots the prompt was composed from: {stdout:?}"
    );

    served.stop();
}
