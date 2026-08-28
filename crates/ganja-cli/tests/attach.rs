//! `ganja run --attach` against a real `ganja serve`: the same turn, read the
//! same way, whether the engine is in this process or on the other end of a
//! socket.
//!
//! This is the acceptance for the attached path, and it is deliberately an
//! *identity* test rather than a set of assertions about the attached output:
//! the point of `--attach` is that a script cannot tell which engine answered
//! it, and the only way to assert that is to run the same turn both ways and
//! hold the transcripts against each other. The default format is compared
//! byte for byte — it carries nothing minted — and the nd-JSON stream is
//! compared object for object with the minted identifiers normalized, because
//! two runs are two sessions and part ids are born per turn.
//!
//! Unix-only for the reason `serve.rs` is: the server is a real child process
//! this suite has to end, and ending it is a signal.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::Path;
use std::process::{Child, Command as Spawn, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use ganja_testkit::temp_dir as temporary;
use serde_json::Value;
use tempfile::TempDir;

/// How long any single wait may take before the fixture is declared broken.
const DEADLINE: Duration = Duration::from_secs(60);

/// What the script's one turn says. One word, appearing nowhere else, so
/// finding it means the whole turn ran.
const CLOSING: &str = "script-finished-zarquon";

/// Every key whose value is minted per run — a session, a part, a call, or a
/// clock reading. Nothing else may differ between the two transcripts, which
/// is what the comparison below is for.
const MINTED: [&str; 6] = ["sessionID", "timestamp", "id", "call_id", "started", "completed"];

/// A script whose only turn says `CLOSING`.
fn one_word() -> Value {
    serde_json::json!({"cadence_ms": 1, "turns": [{"text": CLOSING}]})
}

/// A script whose turn asks to run a shell command, which is a call the
/// permission engine asks about — and nobody is there to answer.
fn asks_to_run_something() -> Value {
    serde_json::json!({
        "cadence_ms": 1,
        "turns": [
            {"text": "Let me look.", "tool_calls": [{"name": "bash", "args": {"command": SHELL_COMMAND}}]},
            {"text": CLOSING},
        ],
    })
}

/// What [`asks_to_run_something`] and [`asks_then_questions`] run, and what a
/// completed call is reported under: the shell tool titles a call with its own
/// command.
const SHELL_COMMAND: &str = "echo attached";

/// A script that first asks to run something and then asks the *person*
/// something.
///
/// Two dialogs of different kinds in one turn is the whole point: `--auto`
/// exists to answer the first, and must never answer the second, so a run that
/// treats them alike is visible here and nowhere else.
fn asks_then_questions() -> Value {
    serde_json::json!({
        "cadence_ms": 1,
        "turns": [
            {"text": "Let me look.", "tool_calls": [{"name": "bash", "args": {"command": SHELL_COMMAND}}]},
            {"text": "Now to ask.", "tool_calls": [{"name": "question", "args": {"questions": [{
                "question": "Which database?",
                "header": "Database",
                "options": [
                    {"label": "Postgres", "description": "Relational"},
                    {"label": "SQLite", "description": "A file"},
                ],
            }]}}]},
            {"text": CLOSING},
        ],
    })
}

/// The server-side rule that puts a `question` call in front of the dialog
/// loop at all.
///
/// Nothing asks about `question` by default — it is not in the permission
/// engine's ask-by-default set — and a `serve` engine installs none of `run`'s
/// standing refusals, so without this the call would simply be allowed and
/// there would be no dialog to observe. A deployment that wants to see its
/// model's questions writes exactly this.
fn asks_before_questioning() -> &'static str {
    "[permission]\nquestion = \"ask\"\n"
}

/// Everything a `ganja` invocation must not inherit from the machine running
/// the suite.
fn sealed(command: &mut Command, project: &Path, data: &Path, config: &Path) {
    command
        .current_dir(project)
        .env("XDG_DATA_HOME", data)
        .env("XDG_CONFIG_HOME", config)
        // The other two doors to a global home, closed with it: an exported
        // `GANJA_CONFIG_HOME` outranks the pinned XDG dir, and an empty
        // pinned XDG dir falls through to `~/.ganja` via `HOME`.
        .env("HOME", data)
        .env_remove("GANJA_CONFIG_HOME")
        .env_remove("GANJA_PROVIDER")
        .env_remove("GANJA_MODEL")
        .env_remove("GANJA_CONFIG")
        .env_remove("GANJA_SERVER_PASSWORD")
        .env_remove("GANJA_SERVER_USERNAME")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        // A closed pipe rather than the harness's stdin: `run` reads it whole
        // when it is not a terminal.
        .write_stdin("");
}

/// One `ganja run` driving an engine in its own process, playing `script`.
struct Local {
    project: TempDir,
    data: TempDir,
    config: TempDir,
}

impl Local {
    fn playing(script: &Value) -> Self {
        let local = Self { project: temporary(), data: temporary(), config: temporary() };
        fs::write(local.project.path().join("script.json"), script.to_string())
            .expect("the script is writable");

        local
    }

    /// Runs the turn and answers stdout and stderr.
    fn run(&self, arguments: &[&str]) -> (String, String) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
        sealed(&mut command, self.project.path(), self.data.path(), self.config.path());
        let assert = command
            .env("GANJA_FAKE_SCRIPT", self.project.path().join("script.json"))
            .args(arguments)
            .assert()
            .success();
        let output = assert.get_output();

        (
            String::from_utf8(output.stdout.clone()).expect("the output is text"),
            String::from_utf8(output.stderr.clone()).expect("the diagnostics are text"),
        )
    }
}

/// A real `ganja serve`, in its own project, playing `script`.
struct Server {
    child: Child,
    port: u16,
    _project: TempDir,
    _data: TempDir,
    _config: TempDir,
}

impl Server {
    fn playing(script: &Value) -> Self {
        Self::playing_under(script, None)
    }

    /// The same, with `configured` written to the project's `ganja.toml`.
    ///
    /// The server's config is the only tier an attached run can move: this
    /// process assembles no engine, and the one it drives belongs to whoever
    /// started it. A rule a test wants the *dialog loop* to meet therefore has
    /// to be written here rather than passed on the client's command line.
    fn playing_under(script: &Value, configured: Option<&str>) -> Self {
        let project = temporary();
        let data = temporary();
        let config = temporary();
        fs::write(project.path().join("script.json"), script.to_string())
            .expect("the script is writable");
        if let Some(configured) = configured {
            fs::write(project.path().join("ganja.toml"), configured)
                .expect("the config is writable");
        }

        let mut child = Spawn::new(env!("CARGO_BIN_EXE_ganja"))
            .args(["serve", "--port", "0"])
            .current_dir(project.path())
            .env("XDG_DATA_HOME", data.path())
            .env("XDG_CONFIG_HOME", config.path())
            // See the client builder above: all three doors move together.
            .env("HOME", data.path())
            .env_remove("GANJA_CONFIG_HOME")
            .env("GANJA_FAKE_SCRIPT", project.path().join("script.json"))
            .env_remove("GANJA_PROVIDER")
            .env_remove("GANJA_MODEL")
            .env_remove("GANJA_CONFIG")
            .env_remove("GANJA_SERVER_PASSWORD")
            .env_remove("GANJA_SERVER_USERNAME")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
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
        let port = line
            .trim()
            .rsplit(':')
            .next()
            .expect("the line ends with the port")
            .parse()
            .expect("the port is a number");

        Self { child, port, _project: project, _data: data, _config: config }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Runs one `ganja run --attach` against this server, from a directory
    /// that holds nothing: an attached run assembles no engine, so its own
    /// working directory decides nothing about the turn.
    fn attached_run(&self, arguments: &[&str]) -> (String, String) {
        let elsewhere = temporary();
        let data = temporary();
        let config = temporary();

        let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
        sealed(&mut command, elsewhere.path(), data.path(), config.path());
        let assert = command
            // A run is a wait like every other in this file, and this is the
            // one that can be made to never end: a dialog the client opened on
            // the server and then declined to answer holds the turn open, and
            // there is nothing left to close it. Bounded here so that becomes
            // a named failure rather than a suite that stops progressing.
            .timeout(DEADLINE)
            .args(["run", "--attach", &self.url()])
            .args(arguments)
            .assert()
            .success();
        let output = assert.get_output();

        (
            String::from_utf8(output.stdout.clone()).expect("the output is text"),
            String::from_utf8(output.stderr.clone()).expect("the diagnostics are text"),
        )
    }

    /// Ends the server the way a supervisor would, and answers its stderr.
    fn stop(mut self) -> String {
        let killed = Spawn::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill runs");
        assert!(killed.success(), "the signal was delivered");

        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.child.try_wait().expect("the child is waitable").is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "the server should exit on SIGTERM");
            std::thread::sleep(Duration::from_millis(50));
        }

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

/// Every object of an nd-JSON stream, parsed.
fn objects(stream: &str) -> Vec<Value> {
    stream
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("a line is not one JSON object: {line:?} ({error})"))
        })
        .collect()
}

/// Replaces every minted identifier with a marker, recursively, leaving
/// everything a turn actually said untouched.
fn normalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if MINTED.contains(&key.as_str()) {
                    *child = Value::from("<minted>");
                } else {
                    normalize(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize),
        _ => {}
    }
}

/// The lines of `stream` that are permission warnings, which is the one part
/// of a headless run's diagnostics both paths must agree on word for word.
fn warnings(stream: &str) -> Vec<&str> {
    stream.lines().filter(|line| line.starts_with("! permission requested")).collect()
}

/// The headline acceptance: a person reading the two runs cannot tell them
/// apart, because the bytes are the same bytes.
///
/// The agent is named on both sides so the header can be compared at all: an
/// attached run knows the agent it asked for and has no route that would tell
/// it the server's default, which is the one thing the two paths render
/// differently when nothing is named (deviation:
/// an-attached-run-names-the-agent-it-asked-for).
#[test]
fn a_turn_over_a_socket_reads_exactly_the_way_the_same_turn_reads_in_process() {
    let script = one_word();
    let server = Server::playing(&script);
    let (attached, attached_err) = server.attached_run(&["--agent", "build", "hello"]);
    let served = server.stop();

    let local = Local::playing(&script);
    let (in_process, in_process_err) = local.run(&["run", "--agent", "build", "hello"]);

    assert!(in_process.contains(CLOSING), "the local turn ran at all: {in_process:?}");
    assert_eq!(attached, in_process, "the same turn reads differently over a socket");

    // The one diagnostic that legitimately differs: a local run announces the
    // provider it selected, and an attached run selects none.
    assert!(
        in_process_err.contains("note:"),
        "the local run says which provider answered: {in_process_err:?}"
    );
    assert!(
        attached_err.is_empty(),
        "an attached run has no provider of its own to announce: {attached_err:?}"
    );
    assert!(
        served.contains("GANJA_SERVER_PASSWORD is not set"),
        "the server was the unsecured loopback one: {served:?}"
    );
}

/// The same identity in the format a script parses, where the minted halves
/// are what has to be normalized away — and the normalization is named rather
/// than a blanket one, so a field that started differing would still show.
#[test]
fn the_nd_json_of_an_attached_turn_matches_the_local_one_object_for_object() {
    let script = one_word();
    let server = Server::playing(&script);
    let (attached, _) = server.attached_run(&["--format", "json", "--agent", "build", "hello"]);
    server.stop();

    let local = Local::playing(&script);
    let (in_process, _) = local.run(&["run", "--format", "json", "--agent", "build", "hello"]);

    let attached_objects = objects(&attached);
    let local_objects = objects(&in_process);
    assert!(!attached_objects.is_empty(), "a turn emits something");

    // Two runs are two sessions: the transcripts are identical *because* the
    // identifiers were normalized, not because they happened to match.
    assert_ne!(
        attached_objects[0]["sessionID"], local_objects[0]["sessionID"],
        "these have to be two different sessions to be worth comparing"
    );

    let normalized = |mut objects: Vec<Value>| {
        objects.iter_mut().for_each(normalize);
        objects
    };
    let attached_normalized = normalized(attached_objects);
    assert_eq!(attached_normalized, normalized(local_objects));

    // And what survived normalization is the turn itself.
    let text = attached_normalized
        .iter()
        .find(|object| object["type"] == "text")
        .expect("the turn said something");
    assert_eq!(text["part"]["text"].as_str(), Some(CLOSING));
}

/// A dialog reaches an attached run over the event stream and is answered over
/// the reply route — the same refusal, with the same warning, that a local run
/// answers in-process. A headless run that opened a dialog and waited would
/// hang until it was killed, wherever the engine is.
#[test]
fn an_attached_run_refuses_a_dialog_nobody_is_there_to_answer() {
    let script = asks_to_run_something();
    let server = Server::playing(&script);
    let (attached, attached_err) = server.attached_run(&["--agent", "build", "run something"]);
    server.stop();

    let local = Local::playing(&script);
    let (in_process, in_process_err) = local.run(&["run", "--agent", "build", "run something"]);

    assert!(!warnings(&attached_err).is_empty(), "the refusal has to be said: {attached_err:?}");
    assert_eq!(
        warnings(&attached_err),
        warnings(&in_process_err),
        "the same dialog is refused with the same words"
    );
    assert!(attached_err.contains("bash"), "the tool is named: {attached_err:?}");
    // The turn survives the refusal — a rejected call is information the model
    // reads, not the end of the turn — and both paths report the same account
    // of what ran.
    assert_eq!(attached, in_process);
    assert!(attached.contains(CLOSING), "{attached:?}");
}

/// `--auto` answers the dialogs a person would have answered, and refuses the
/// one whose whole purpose *is* a person.
///
/// The distinction only exists on the attached path. A local run installs
/// standing rules that refuse `question` inside the engine before a dialog can
/// open; a `serve` engine deliberately keeps its dialogs interactive and is not
/// this run's to reconfigure, so the same rule has to be applied here, on the
/// dialogs the event stream delivers. One script exercises both halves at once
/// — a shell call `--auto` answers and a question it must not — because a run
/// that treated them alike would still pass every assertion either half could
/// make on its own.
#[test]
fn an_attached_auto_run_answers_a_shell_dialog_and_still_refuses_a_question() {
    let server = Server::playing_under(&asks_then_questions(), Some(asks_before_questioning()));
    let (attached, attached_err) =
        server.attached_run(&["--auto", "--agent", "build", "have a look"]);
    server.stop();

    // `--auto` is really in force: the shell dialog was answered rather than
    // refused, so the command ran and is reported as a completed call. Without
    // this the assertions below would also hold for a run that refused
    // everything, which is a different build.
    assert!(
        attached.contains(SHELL_COMMAND),
        "the answered dialog's call has to have run: {attached:?}"
    );
    // One dialog was refused, and it is the question's. A second line here
    // would mean `--auto` answered nothing; none would mean it answered this.
    let refusals = warnings(&attached_err);
    assert_eq!(refusals.len(), 1, "exactly one dialog is refused: {attached_err:?}");
    assert!(
        refusals[0].contains("permission requested: question ("),
        "and it is the question's: {refusals:?}"
    );
    // A refusal is information the model reads, not the end of the turn: the
    // call is reported failed and the script runs on to its last word.
    assert!(
        attached.contains("question failed"),
        "the refused call is reported as a failed one: {attached:?}"
    );
    assert!(attached.contains(CLOSING), "the turn survived the refusal: {attached:?}");
}

/// The attached path's own refusals: a session the server does not hold is
/// refused in upstream's words, and before anything is printed.
#[test]
fn a_session_the_server_does_not_hold_is_refused_the_way_a_local_one_is() {
    let server = Server::playing(&one_word());
    let elsewhere = temporary();
    let data = temporary();
    let config = temporary();

    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    sealed(&mut command, elsewhere.path(), data.path(), config.path());
    let assert = command
        .args(["run", "--attach", &server.url(), "--session", "ses_nothing_here", "hello"])
        .assert()
        .code(1);
    let output = assert.get_output();
    assert!(output.stdout.is_empty(), "nothing was printed first");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Session not found"),
        "upstream's wording: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    server.stop();
}

/// Nothing answers, and the run says so — naming the address it tried rather
/// than a transport error nobody can act on.
#[test]
fn attaching_to_nothing_names_the_address_that_was_tried() {
    let elsewhere = temporary();
    let data = temporary();
    let config = temporary();

    // A port that was bound and released: a real address with nothing behind
    // it, rather than one guessed at.
    let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port binds");
    let address = format!("http://{}", taken.local_addr().expect("the address reads"));
    drop(taken);

    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    sealed(&mut command, elsewhere.path(), data.path(), config.path());
    command
        .args(["run", "--attach", &address, "hello"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains(address));
}

/// The flag table's rule, enforced by clap: a flag that would parse and then
/// decide nothing is refused rather than accepted and ignored.
#[test]
fn attaching_refuses_the_flags_that_only_mean_something_to_a_local_engine() {
    for conflicting in [vec!["--config", "ganja.toml"], vec!["--command", "review"]] {
        let elsewhere = temporary();
        let data = temporary();
        let config = temporary();

        let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
        sealed(&mut command, elsewhere.path(), data.path(), config.path());
        command
            .args(["run", "--attach", "http://127.0.0.1:4096"])
            .args(&conflicting)
            .arg("hello")
            .assert()
            .failure()
            .stderr(predicates::str::contains("cannot be used with"));
    }
}
