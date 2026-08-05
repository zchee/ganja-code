//! `ganja run` end to end: the exit-code table, the nd-JSON shape, and the two
//! things a headless turn must never do — hang on a dialog, or claim somebody
//! else's session.
//!
//! Every invocation here runs the real binary against the fake provider playing
//! a written script, in its own project directory and against its own
//! `XDG_DATA_HOME`, so nothing a developer has exported or stored can decide
//! whether these pass. Stdin is closed rather than inherited: `run` reads a
//! pipe whole when standard input is not a terminal, and a test that inherited
//! the harness's would be asking a different question every time.
//!
//! Spec: upstream `packages/opencode/src/cli/cmd/run.ts`. Line numbers cited on
//! the tests that pin one of its rules.

use std::{fs, path::Path};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

/// The six `type` names an nd-JSON object may carry, spelled here rather than
/// imported: this is what a consumer of the stream has, and the point of the
/// assertion is that the binary agrees with it.
const TYPES: [&str; 6] = [
    "tool_use",
    "step_start",
    "step_finish",
    "text",
    "reasoning",
    "error",
];

/// What the last turn of every script says. One word, and one that appears
/// nowhere else, so finding it means the whole script ran.
const CLOSING: &str = "script-finished-zarquon";

/// A project directory with its own data home, and the script the fake
/// provider will play in it.
struct Run {
    project: TempDir,
    data: TempDir,
}

impl Run {
    /// Builds a run whose provider plays `script` — the JSON document
    /// `GANJA_FAKE_SCRIPT` names, whose format `ganja_core::provider::fake`
    /// documents.
    fn playing(script: &Value) -> Self {
        let run = Self {
            project: temporary(),
            data: temporary(),
        };
        fs::write(run.script(), script.to_string()).expect("the script is writable");

        run
    }

    fn script(&self) -> std::path::PathBuf {
        self.project.path().join("script.json")
    }

    fn path(&self) -> &Path {
        self.project.path()
    }

    /// An invocation of the binary in this run's project, with nothing
    /// inherited that could decide the answer.
    fn ganja(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
        command
            .current_dir(self.project.path())
            .env("XDG_DATA_HOME", self.data.path())
            .env("GANJA_FAKE_SCRIPT", self.script())
            // Unset, so the fake provider is what answers and says so.
            .env_remove("GANJA_PROVIDER")
            .env_remove("GANJA_MODEL")
            .env_remove("GANJA_CONFIG")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            // A closed pipe rather than the harness's stdin: `run` reads it
            // whole when it is not a terminal.
            .write_stdin("");

        command
    }

    /// The session ids `ganja sessions` lists for this project, newest first.
    fn sessions(&self) -> Vec<String> {
        let listed = self.ganja().arg("sessions").assert().success();
        let stdout = String::from_utf8(listed.get_output().stdout.clone()).expect("text");

        stdout
            .lines()
            .skip(1)
            .filter_map(|row| row.split_whitespace().next())
            .map(str::to_owned)
            .collect()
    }
}

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// A script whose only turn says `CLOSING` and nothing else.
fn one_word() -> Value {
    serde_json::json!({"cadence_ms": 1, "turns": [{"text": CLOSING}]})
}

/// Every object of an nd-JSON stream, parsed. A line that is not one object is
/// the failure this exists to catch, so it panics rather than being skipped.
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

fn types(objects: &[Value]) -> Vec<String> {
    objects
        .iter()
        .map(|object| {
            object["type"]
                .as_str()
                .unwrap_or_else(|| panic!("an object carries no type: {object}"))
                .to_owned()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The exit-code table (`run.ts:420-428`, `:465-467`, `:836-838`, `:866-870`).
// ---------------------------------------------------------------------------

#[test]
fn a_run_with_neither_a_message_nor_a_command_is_refused() {
    Run::playing(&one_word())
        .ganja()
        .arg("run")
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "You must provide a message or a command",
        ));
}

/// Whitespace is not a message. Upstream trims before it decides
/// (`run.ts:420`), and a run that sent a blank prompt would spend a request to
/// learn nothing.
#[test]
fn a_message_of_nothing_but_whitespace_is_no_message() {
    Run::playing(&one_word())
        .ganja()
        .args(["run", "   ", "\t"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "You must provide a message or a command",
        ));
}

#[test]
fn forking_with_no_session_to_fork_names_what_is_missing() {
    Run::playing(&one_word())
        .ganja()
        .args(["run", "--fork", "hello"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "--fork requires --continue or --session",
        ));
}

/// The validation above is upstream's and is ported whole; the fork itself is
/// not portable, and the refusal says so rather than continuing into a run that
/// wrote to the session it was told to leave alone.
#[test]
fn forking_a_session_that_exists_says_this_build_cannot_do_it() {
    let run = Run::playing(&one_word());
    run.ganja().args(["run", "hello"]).assert().success();

    run.ganja()
        .args(["run", "--continue", "--fork", "again"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--fork is not available"));
}

/// Upstream's wording, because a script that greps for it is greping for
/// upstream's (`run.ts:465`).
#[test]
fn a_session_the_store_does_not_hold_is_refused() {
    Run::playing(&one_word())
        .ganja()
        .args(["run", "--session", "ses_nothing_here", "hello"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Session not found"));
}

#[test]
fn a_turn_that_completed_exits_zero_and_writes_what_the_model_said() {
    Run::playing(&one_word())
        .ganja()
        .args(["run", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains(CLOSING));
}

/// A provider that could not answer fails the turn, and a failed turn is a
/// failed run — upstream sets `process.exitCode = 1` from the accumulated
/// stream error (`run.ts:836-838`).
#[test]
fn a_turn_the_provider_could_not_answer_exits_one() {
    let run = Run::playing(&one_word());
    fs::remove_file(run.script()).expect("the script is removable");

    run.ganja()
        .args(["run", "hello"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("cannot be read"));
}

/// `--command` is the other way a turn starts, and its whole point is that the
/// message is the command's *arguments* rather than the prompt — so a run that
/// names one needs no message at all (`run.ts:420`, `:840-848`).
#[test]
fn a_configured_command_runs_as_a_turn_with_no_message_of_its_own() {
    let run = Run::playing(&one_word());
    fs::write(
        run.path().join("ganja.json"),
        serde_json::json!({
            "command": {"greet": {"template": "say hello to $ARGUMENTS"}},
        })
        .to_string(),
    )
    .expect("the config is writable");

    run.ganja()
        .args(["run", "--command", "greet"])
        .assert()
        .success()
        .stdout(predicate::str::contains(CLOSING));
    assert_eq!(run.sessions().len(), 1, "the command started a turn");
}

/// The other side of the same branch: a command the engine will not accept is
/// reported and the run stops, without waiting on a turn that never began
/// (`run.ts:849-853`). This is the only test of that path, and the useful half
/// of "no such command" is which ones there are.
#[test]
fn a_command_nothing_answers_to_exits_one_and_names_the_roster() {
    Run::playing(&one_word())
        .ganja()
        .args(["run", "--command", "nothing-answers-to-this"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("nothing-answers-to-this"))
        // `init` is the one command every build ships, so the roster is never
        // empty and an error that named none would be the bug.
        .stderr(predicate::str::contains("init"));
}

/// The same failure has to reach a script parsing the stream, not only a
/// person reading the terminal.
#[test]
fn a_failed_turn_emits_an_error_object_before_it_exits_one() {
    let run = Run::playing(&one_word());
    fs::remove_file(run.script()).expect("the script is removable");

    let failed = run
        .ganja()
        .args(["run", "--format", "json", "hello"])
        .assert()
        .code(1);
    let stdout = String::from_utf8(failed.get_output().stdout.clone()).expect("text");

    assert!(
        types(&objects(&stdout)).contains(&"error".to_owned()),
        "no error object in the stream: {stdout:?}"
    );
}

/// The two name different sessions, so picking a winner would be inventing an
/// answer. Upstream lets `--session` quietly win; clap refuses the pair.
#[test]
fn continuing_and_naming_a_session_at_once_fails_to_parse() {
    Run::playing(&one_word())
        .ganja()
        .args(["run", "--continue", "--session", "ses_1", "hello"])
        .assert()
        .failure();
}

// ---------------------------------------------------------------------------
// The nd-JSON shape (`run.ts:678-691`, `:717-798`).
// ---------------------------------------------------------------------------

/// A turn that says something, calls a tool, and says something else, so the
/// stream carries every type this build has a source for.
fn a_turn_with_a_call() -> Value {
    serde_json::json!({
        "cadence_ms": 1,
        "turns": [
            {
                "text": "reading it",
                "tool_calls": [{"name": "read", "args": {"filePath": "target.txt"}}],
            },
            {"text": CLOSING},
        ],
    })
}

#[test]
fn every_object_of_the_stream_carries_one_of_the_six_type_names() {
    let run = Run::playing(&a_turn_with_a_call());
    fs::write(run.path().join("target.txt"), "seeded\n").expect("the target is writable");

    let ran = run
        .ganja()
        .args(["run", "--format", "json", "what is in the file"])
        .assert()
        .success();
    let stdout = String::from_utf8(ran.get_output().stdout.clone()).expect("text");
    let emitted = types(&objects(&stdout));

    assert!(!emitted.is_empty(), "a turn has to emit something");
    for kind in &emitted {
        assert!(
            TYPES.contains(&kind.as_str()),
            "an object carried a type outside the set: {kind} in {stdout:?}"
        );
    }
    // Not merely a subset by accident of emitting nothing interesting: the
    // turn ran a tool and said two things, so four of the six have a source.
    for expected in ["step_start", "step_finish", "text", "tool_use"] {
        assert!(
            emitted.iter().any(|kind| kind == expected),
            "no {expected} object in {stdout:?}"
        );
    }
}

/// The rule the whole format hangs on. The id is captured once, before a single
/// event is read (`run.ts:676`), and it is the session this run created —
/// which `ganja sessions` is the independent witness for.
#[test]
fn every_object_carries_the_session_this_run_created() {
    let run = Run::playing(&one_word());

    let ran = run
        .ganja()
        .args(["run", "--format", "json", "hello"])
        .assert()
        .success();
    let stdout = String::from_utf8(ran.get_output().stdout.clone()).expect("text");

    let sessions = run.sessions();
    assert_eq!(sessions.len(), 1, "the run created exactly one session");
    let stamped: Vec<String> = objects(&stdout)
        .iter()
        .map(|object| {
            object["sessionID"]
                .as_str()
                .unwrap_or_else(|| panic!("an object carries no sessionID: {object}"))
                .to_owned()
        })
        .collect();

    assert!(!stamped.is_empty(), "a turn has to emit something");
    for id in stamped {
        assert_eq!(
            id, sessions[0],
            "an object named a session that is not this run's: {stdout:?}"
        );
    }
}

/// The corollary of the engine property `ganja-core/tests/task.rs` pins: a
/// subagent's events never reach the subscribed stream, so a turn that
/// delegates emits the parent's `task` call and nothing belonging to the
/// session that call spawned. This is what lets `run` skip upstream's
/// per-event session filter (`run.ts:717`, `:790`, `:798`) rather than merely
/// forget it.
#[test]
fn a_delegating_turn_emits_nothing_attributable_to_a_child_session() {
    let run = Run::playing(&serde_json::json!({
        "cadence_ms": 1,
        "turns": [
            {
                "text": "delegating",
                "tool_calls": [{
                    "name": "task",
                    "args": {
                        "description": "find the thing",
                        "prompt": "go and find the thing",
                        "subagent_type": "general",
                    },
                }],
            },
            // The subagent's own turn: requests are counted across the whole
            // script, so this is what the child plays.
            {"text": "the child answered"},
            {"text": CLOSING},
        ],
    }));

    let ran = run
        .ganja()
        // `task` asks by default, and a delegation nobody allowed would be
        // refused before it ever spawned a child.
        .args(["run", "--auto", "--format", "json", "delegate this"])
        .assert()
        .success();
    let stdout = String::from_utf8(ran.get_output().stdout.clone()).expect("text");
    let emitted = objects(&stdout);

    assert!(
        stdout.contains("\"task\""),
        "the turn has to have delegated: {stdout:?}"
    );
    let named: std::collections::BTreeSet<&str> = emitted
        .iter()
        .filter_map(|object| object["sessionID"].as_str())
        .collect();
    assert_eq!(
        named.len(),
        1,
        "more than one session reached the stream: {named:?}"
    );
    // And it is the parent — the one a listing of this project's roots shows.
    let sessions = run.sessions();
    assert_eq!(
        named.into_iter().next(),
        Some(sessions[0].as_str()),
        "the stream named something other than the run's own session"
    );
}

/// Subscribing after prompting would put the head of the turn behind the
/// subscription. In this build the queue is created with the engine and is
/// lossless, so a late subscriber does not *lose* the head — it wedges once the
/// turn fills a queue nobody drains — which makes this an assertion that the
/// account starts where the turn starts rather than part-way through it.
#[test]
fn the_stream_opens_on_the_turns_first_step() {
    let run = Run::playing(&a_turn_with_a_call());
    fs::write(run.path().join("target.txt"), "seeded\n").expect("the target is writable");

    let ran = run
        .ganja()
        .args(["run", "--format", "json", "what is in the file"])
        .assert()
        .success();
    let stdout = String::from_utf8(ran.get_output().stdout.clone()).expect("text");

    assert_eq!(
        types(&objects(&stdout)).first().map(String::as_str),
        Some("step_start"),
        "the first object is not the turn's first step: {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// Permissions: two mechanisms, and neither of them waits (`run.ts:430-448`,
// `:796-816`).
// ---------------------------------------------------------------------------

/// A script that tries to write a file through the shell, which asks by
/// default.
fn a_turn_that_asks() -> Value {
    serde_json::json!({
        "cadence_ms": 1,
        "turns": [
            {
                "text": "running it",
                "tool_calls": [{"name": "bash", "args": {"command": "touch marker.txt"}}],
            },
            {"text": CLOSING},
        ],
    })
}

/// The whole point of the headless path: a dialog nobody can answer is
/// answered for them, loudly, and the run finishes.
#[test]
fn a_permission_request_is_rejected_with_a_warning_rather_than_waited_on() {
    let run = Run::playing(&a_turn_that_asks());

    run.ganja()
        .args(["run", "write a marker"])
        .assert()
        .success()
        .stderr(predicate::str::contains("auto-rejecting"))
        .stderr(predicate::str::contains("bash"))
        // The turn carries on after a refusal — a denial is information the
        // model reads, never a turn abort — so the script reaches its end.
        .stdout(predicate::str::contains(CLOSING));

    assert!(
        !run.path().join("marker.txt").exists(),
        "a rejected call must not have run"
    );
}

/// The warning is a diagnostic, so it must not land in the middle of a stream
/// a script is parsing.
#[test]
fn the_rejection_warning_never_reaches_the_nd_json_stream() {
    let run = Run::playing(&a_turn_that_asks());

    let ran = run
        .ganja()
        .args(["run", "--format", "json", "write a marker"])
        .assert()
        .success();
    let stdout = String::from_utf8(ran.get_output().stdout.clone()).expect("text");
    let stderr = String::from_utf8(ran.get_output().stderr.clone()).expect("text");

    assert!(stderr.contains("auto-rejecting"), "no warning: {stderr:?}");
    assert!(
        !stdout.contains("auto-rejecting"),
        "the warning corrupted the stream: {stdout:?}"
    );
    // Every line still parses, which is the assertion the one above only
    // approximates.
    let _ = objects(&stdout);
}

#[test]
fn auto_allows_the_call_a_default_run_refuses() {
    let run = Run::playing(&a_turn_that_asks());

    run.ganja()
        .args(["run", "--auto", "write a marker"])
        .assert()
        .success()
        .stdout(predicate::str::contains(CLOSING));

    assert!(
        run.path().join("marker.txt").exists(),
        "an allowed call has to have run"
    );
}

/// Upstream's two hidden spellings of the same switch (`run.ts:247-256`), kept
/// because scripts written against it pass them.
#[test]
fn the_hidden_spellings_of_auto_mean_what_auto_means() {
    for flag in ["--yolo", "--dangerously-skip-permissions"] {
        let run = Run::playing(&a_turn_that_asks());

        run.ganja()
            .args(["run", flag, "write a marker"])
            .assert()
            .success();
        assert!(
            run.path().join("marker.txt").exists(),
            "{flag} did not allow the call"
        );
    }
}

/// An "always" answer is what a dialog stores; a headless run has no dialog, so
/// it must leave nothing behind that would quietly allow the next one.
#[test]
fn a_rejected_run_stores_no_answer_for_the_next_one() {
    let run = Run::playing(&a_turn_that_asks());
    run.ganja()
        .args(["run", "write a marker"])
        .assert()
        .success();

    // Wherever under the data home the store lands — the layout is
    // `ganja_permission::project`'s business, not this test's.
    let stored: Vec<String> = walk(run.data.path())
        .into_iter()
        .filter(|path| path.ends_with("permissions.json"))
        .map(|path| fs::read_to_string(path).unwrap_or_default())
        .collect();

    for answers in stored {
        assert!(
            !answers.contains("allow"),
            "a headless run left an allow behind: {answers}"
        );
    }
}

/// Every file under `root`, or nothing when there is no `root`.
fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }

    found
}

// ---------------------------------------------------------------------------
// What the model is actually asked (`run.ts:40-50`, `:288-290`, `:416-417`).
// ---------------------------------------------------------------------------

/// The message a session was started with, as the fallback title records it:
/// the first fifty characters of the first prompt, which is the one surface a
/// headless run leaves behind that names what was asked.
fn asked(run: &Run) -> String {
    let listed = run.ganja().arg("sessions").assert().success();
    let stdout = String::from_utf8(listed.get_output().stdout.clone()).expect("text");
    let row = stdout.lines().nth(1).expect("one session was created");

    // The listing is `SESSION UPDATED TOKENS TITLE`, and the title is the rest
    // of the row after the three fixed columns.
    row.split_whitespace()
        .skip(4)
        .collect::<Vec<&str>>()
        .join(" ")
}

#[test]
fn the_message_arguments_are_joined_with_spaces() {
    let run = Run::playing(&one_word());
    run.ganja()
        .args(["run", "alpha", "bravo", "charlie"])
        .assert()
        .success();

    assert_eq!(asked(&run), "alpha bravo charlie");
}

/// Everything after `--` is part of the message too (`run.ts:272`), which is
/// how a message that starts with a dash is sent at all.
#[test]
fn everything_after_the_separator_is_part_of_the_message() {
    let run = Run::playing(&one_word());
    run.ganja()
        .args(["run", "--", "alpha", "--format"])
        .assert()
        .success();

    assert_eq!(asked(&run), "alpha --format");
}

/// Piped text joins what was typed, and goes last (`run.ts:40-50`). The
/// listing renders the newline between them as a space, so this asserts the
/// order rather than the separator — which the unit tests in `run.rs` pin
/// exactly.
#[test]
fn piped_text_joins_the_typed_message_and_comes_last() {
    let run = Run::playing(&one_word());
    run.ganja()
        .args(["run", "alpha-typed"])
        .write_stdin("bravo-piped")
        .assert()
        .success();

    assert_eq!(asked(&run), "alpha-typed bravo-piped");
}

/// A pipe on its own is a whole message: `git diff | ganja run` has to work.
#[test]
fn a_run_with_only_piped_text_sends_it_as_the_message() {
    let run = Run::playing(&one_word());
    run.ganja()
        .arg("run")
        .write_stdin("only-piped")
        .assert()
        .success()
        .stdout(predicate::str::contains(CLOSING));

    assert_eq!(asked(&run), "only-piped");
}

// ---------------------------------------------------------------------------
// Session selection (`run.ts:456-533`).
// ---------------------------------------------------------------------------

/// Continuing is the difference between two conversations and one, so the
/// second run has to land in the session the first created.
#[test]
fn continuing_lands_in_the_session_the_last_run_created() {
    let run = Run::playing(&serde_json::json!({
        "cadence_ms": 1,
        "turns": [{"text": "first"}, {"text": CLOSING}],
    }));

    run.ganja()
        .args(["run", "the first thing"])
        .assert()
        .success();
    let first = run.sessions();
    assert_eq!(first.len(), 1);

    run.ganja()
        .args(["run", "--continue", "the second thing"])
        .assert()
        .success();
    assert_eq!(
        run.sessions(),
        first,
        "continuing started a second session instead of resuming the first"
    );
}

/// Naming a session is the caller saying it wants *that* conversation, and
/// getting it is the whole point.
#[test]
fn naming_a_session_lands_in_that_one() {
    let run = Run::playing(&serde_json::json!({
        "cadence_ms": 1,
        "turns": [{"text": "first"}, {"text": CLOSING}],
    }));

    run.ganja()
        .args(["run", "the first thing"])
        .assert()
        .success();
    let first = run.sessions();

    run.ganja()
        .args(["run", "--session", &first[0], "the second thing"])
        .assert()
        .success();
    assert_eq!(run.sessions(), first);
}

/// Nothing to continue is not an error: upstream falls through to a fresh
/// session when its listing holds no parentless entry (`run.ts:492`, `:510`).
#[test]
fn continuing_with_nothing_stored_starts_a_fresh_session() {
    let run = Run::playing(&one_word());

    run.ganja()
        .args(["run", "--continue", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains(CLOSING));
    assert_eq!(run.sessions().len(), 1);
}
