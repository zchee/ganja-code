//! `ganja run` on the `claude-code` wire, end to end through the **real**
//! binary (**D556**, AC-4.7).
//!
//! Three processes, and that is the point: this test binary spawns the shipped
//! `ganja`, which selects the wire, which spawns *this binary again* as the
//! `claude` CLI. Every seam a person's own invocation crosses is crossed here
//! — the environment, `provider::select`'s floor check, the argv builder, the
//! child's stdio — and none of it reaches a live `claude`, because the binary
//! at the end of it is the fake.
//!
//! `harness = false` for the reason `ganja-provider`'s spawn suite has it: the
//! `main` below checks `GANJA_FAKE_CLAUDE_SCRIPT` first and re-execs itself as
//! the CLI when that names a script, and the flags on that command line are
//! the wire's own, which libtest would refuse on sight. The suite's sibling —
//! `ganja-core/tests/claude_code_bridge.rs` — is a plain harness instead, for
//! the opposite reason: nothing there spawns a process at all.

use std::path::PathBuf;
use std::process::Command;

use ganja_testkit::fake_claude::{self, Call, Script, Turn, Usage};
use serde_json::Value;

/// The tool the scripted turn calls.
///
/// The recording's own probe was `ganja_ping`, a tool the live spike
/// registered for the occasion; a real `ganja run` offers the shipped registry
/// and nothing else, so the call has to name something in it. `glob` is the
/// honest stand-in: read-only, allowed without a dialog, and answered from the
/// project directory the run is started in — so what this asserts is a
/// registry tool really running, which is what the AC is about.
const TOOL: &str = "glob";

/// What the scripted turn says before it calls anything.
const SAID: &str = "looking";

/// What its `result` reports, so the closing row is identifiable.
const ANSWERED: &str = "found them";

/// The four cases this binary holds.
const CASES: [&str; 4] = [
    "a_headless_run_on_the_claude_code_wire_streams_its_turn_as_json",
    "a_cli_that_refuses_its_own_argv_fails_the_run_with_the_clis_own_sentence",
    "ganja_models_claude_code_lists_the_seats_own_roster_under_its_notice",
    "a_selection_hands_the_configured_idle_bound_to_the_wire",
];

fn main() {
    // The re-exec arm, checked before anything else: under a script this
    // process **is** the CLI.
    if std::env::var_os(fake_claude::SCRIPT_ENV).is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(fake_claude::main());
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--list") {
        if !args.iter().any(|arg| arg == "--ignored") {
            for case in CASES {
                println!("{case}: test");
            }
        }

        return;
    }
    let filter = args.iter().find(|arg| !arg.starts_with('-'));
    let selected: Vec<&str> =
        CASES.into_iter().filter(|case| filter.is_none_or(|name| name == case)).collect();
    if selected.is_empty() {
        println!("running 0 tests");

        return;
    }

    println!("running {} tests", selected.len());
    for case in &selected {
        match *case {
            "a_headless_run_on_the_claude_code_wire_streams_its_turn_as_json" => {
                a_headless_run_on_the_claude_code_wire_streams_its_turn_as_json();
            }
            "a_cli_that_refuses_its_own_argv_fails_the_run_with_the_clis_own_sentence" => {
                a_cli_that_refuses_its_own_argv_fails_the_run_with_the_clis_own_sentence();
            }
            "ganja_models_claude_code_lists_the_seats_own_roster_under_its_notice" => {
                ganja_models_claude_code_lists_the_seats_own_roster_under_its_notice();
            }
            _ => a_selection_hands_the_configured_idle_bound_to_the_wire(),
        }
        println!("test {case} ... ok");
    }
    println!("test result: ok. {} passed; 0 failed; 0 ignored", selected.len());
}

/// **AC-4.7.** A headless turn on this wire exits 0 and writes the four rows
/// `--format json` promises: the step's start, the reply's text, the tool call
/// with `status: completed`, and the step's finish carrying the usage the CLI
/// reported.
fn a_headless_run_on_the_claude_code_wire_streams_its_turn_as_json() {
    let run = Run::playing(&answering());
    let output = run.ganja().args(["run", "--format", "json", "find the rust files"]).output();
    let output = output.expect("the binary runs");
    let stdout = String::from_utf8(output.stdout).expect("nd-JSON is text");
    let rows: Vec<Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("a JSON line: {line}")))
        .collect();
    let kinds: Vec<&str> = rows.iter().filter_map(|row| row["type"].as_str()).collect::<Vec<_>>();

    assert!(
        output.status.success(),
        "the run exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    // The stream's own spellings: a row is `{type, part, sessionID, timestamp}`
    // and a tool call's row is `tool_use`, whatever the part inside it is
    // called. Asserted against what the format really writes rather than
    // against the AC's prose, which names the parts.
    for wanted in ["step_start", "text", "tool_use", "step_finish"] {
        assert!(kinds.contains(&wanted), "no {wanted} row among {kinds:?}\n{stdout}");
    }

    let tool = rows
        .iter()
        .find(|row| row["type"] == "tool_use")
        .unwrap_or_else(|| panic!("a tool row: {stdout}"));
    assert_eq!(tool["part"]["tool"], TOOL, "the registry tool the script called: {tool}");
    assert_eq!(
        tool["part"]["state"]["status"], "completed",
        "which really ran rather than being refused or left pending: {tool}"
    );
    assert!(
        tool["part"]["state"]["output"].as_str().is_some_and(|found| found.contains("main.rs")),
        "and answered out of the project it was started in: {tool}"
    );

    // The **last** `step_finish`, which is the one the CLI's own `result`
    // reported: a turn that called a tool closes a step before it runs one.
    let finish = rows
        .iter()
        .rev()
        .find(|row| row["type"] == "step_finish")
        .unwrap_or_else(|| panic!("a step_finish row: {stdout}"));
    assert_eq!(
        finish["part"]["usage"]["input_tokens"], 1_234,
        "the counters the CLI's own `result` reported: {finish}"
    );
    assert_eq!(finish["part"]["usage"]["output_tokens"], 56);
    assert_eq!(finish["part"]["usage"]["cache_read_tokens"], 7);
    assert_eq!(finish["part"]["usage"]["cache_write_tokens"], 8);

    let text: String = rows
        .iter()
        .filter(|row| row["type"] == "text")
        .filter_map(|row| row["part"]["text"].as_str())
        .collect();
    assert!(text.contains(SAID), "the reply's own words reached the stream: {stdout}");
}

/// The other half of AC-4.7: a CLI that refuses the argv it was handed fails
/// the run with **its own sentence**, not with one ganja invented.
///
/// The refusal is real rather than manufactured — it is what the binary says
/// when `--print` and `--output-format=stream-json` arrive without
/// `--verbose` — and what this pins is that the sentence survives the whole
/// way out: through the child's stderr, the wire's failure mapping, and
/// `ganja run`'s exit.
fn a_cli_that_refuses_its_own_argv_fails_the_run_with_the_clis_own_sentence() {
    // Run 7's own shape, which is the recording's name for exactly this: an
    // argv the CLI accepts at parse and refuses once it has started. The
    // sentence is the CLI's own — what it says when `--print` and
    // `--output-format=stream-json` arrive without `--verbose`.
    let refusing =
        Script { exit_before_init: Some(fake_claude::NEEDS_VERBOSE.to_owned()), ..answering() };
    let run = Run::playing(&refusing);
    let output = run
        .ganja()
        .args(["run", "--format", "json", "find the rust files"])
        .output()
        .expect("the binary runs");
    let stderr = String::from_utf8(output.stderr).expect("text");

    assert!(!output.status.success(), "a refused turn is a failed run");
    assert!(
        stderr.contains(fake_claude::NEEDS_VERBOSE),
        "the CLI's own sentence reaches the shell: {stderr}"
    );
}

/// A script whose one turn says something, calls [`TOOL`], and reports usage.
fn answering() -> Script {
    Script {
        turns: vec![Turn {
            text: vec![SAID.to_owned()],
            tool_calls: vec![Call {
                id: "toolu_1".to_owned(),
                name: TOOL.to_owned(),
                input: serde_json::json!({"pattern": "**/*.rs"}),
                call_first: false,
            }],
            result: ANSWERED.to_owned(),
            usage: Usage {
                input_tokens: 1_234,
                output_tokens: 56,
                cache_read_input_tokens: 7,
                cache_creation_input_tokens: 8,
            },
            ..Turn::default()
        }],
        ..Script::default()
    }
}

/// A project directory with its own homes, and the script the fake CLI plays.
struct Run {
    project: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Run {
    fn playing(script: &Script) -> Self {
        let run = Self {
            project: tempfile::TempDir::new().expect("a temporary directory"),
            data: tempfile::TempDir::new().expect("a temporary directory"),
        };
        std::fs::write(run.script(), serde_json::to_string(script).expect("a script serializes"))
            .expect("the script is writable");
        // Something for the scripted `glob` to find, so the tool answers with
        // a result rather than with an empty listing.
        std::fs::write(run.project.path().join("main.rs"), "fn main() {}\n")
            .expect("the fixture file is writable");

        run
    }

    fn script(&self) -> PathBuf {
        self.project.path().join("script.json")
    }

    /// An invocation of the shipped binary, with every home pinned to this
    /// run's own directories — `run.rs`'s rule, and for its reason: a
    /// `default_provider` in a developer's real config would otherwise decide
    /// provider selection itself.
    fn ganja(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
        command
            .current_dir(self.project.path())
            .env("XDG_DATA_HOME", self.data.path())
            .env("HOME", self.data.path())
            .env("XDG_CONFIG_HOME", self.data.path().join("config"))
            .env_remove("GANJA_CONFIG_HOME")
            .env_remove("GANJA_CONFIG")
            .env_remove("GANJA_MODEL")
            .env("GANJA_PROVIDER", ganja_core::provider::claude_code::ID)
            // **This binary is the CLI.** Absolute, which the wire requires of
            // an override, and a path that re-execs as the fake the moment it
            // sees the script variable below.
            .env(
                ganja_core::provider::claude_code::BIN_ENV,
                std::env::current_exe().expect("this test binary has a path"),
            )
            .env(fake_claude::SCRIPT_ENV, self.script())
            .stdin(std::process::Stdio::null());

        command
    }
}

/// **AC-4.8**'s listing half. `ganja models claude-code` spawns a listing,
/// prints the notice, and lists the roster the seat named — with **nothing**
/// marked current.
///
/// The listing takes no turn: it sends one `initialize` and reads the reply,
/// so it never reaches `system/init` and has no `current_model` key to read
/// even if it had. What a person is shown is what this seat *may* name; what
/// it is *running* is `/usage`'s `Served model:` row.
fn ganja_models_claude_code_lists_the_seats_own_roster_under_its_notice() {
    let listing = Script {
        models: vec![
            ("default".to_owned(), "Default (recommended)".to_owned()),
            ("opus".to_owned(), "Opus 5".to_owned()),
        ],
        ..answering()
    };
    let run = Run::playing(&listing);
    let output = run
        .ganja()
        .args(["models", ganja_core::provider::claude_code::ID])
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8(output.stdout).expect("text");

    assert!(
        output.status.success(),
        "the listing exited {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("live from the CLI's own seat"),
        "the notice says where the roster came from: {stdout}"
    );
    assert!(
        stdout.contains("uncataloged, so sizing and cost display are off"),
        "and what does not apply to it: {stdout}"
    );
    for (id, name) in [("default", "Default (recommended)"), ("opus", "Opus 5")] {
        let row = stdout
            .lines()
            .find(|line| line.starts_with(id))
            .unwrap_or_else(|| panic!("a row for {id}: {stdout}"));
        assert!(row.contains(name), "carrying the label the seat gave it: {row}");
    }
    assert!(
        !stdout.to_lowercase().contains("current"),
        "and nothing is marked current, because a listing never learns which is: {stdout}"
    );
}

/// **AC-4.15**'s `Selection` half. The curated `claude_code.idle_bound`
/// reaches the wire, and the entry it builds really is closed at the second
/// the config named.
///
/// Driven through `provider::select` rather than through
/// `with_idle_bound` directly, because what this pins is the *wiring*: the key
/// is read once, at selection, and handed to the one door that rebuilds the
/// held-process table. A test that called the door itself would prove the door
/// works and say nothing about whether anything calls it.
fn a_selection_hands_the_configured_idle_bound_to_the_wire() {
    // A turn that calls **nothing**: this case drives the selected provider
    // directly, so there is no engine here to answer a `can_use_tool`, and a
    // scripted tool call would simply wait for one.
    let talking = Script {
        turns: vec![Turn {
            text: vec![SAID.to_owned()],
            result: ANSWERED.to_owned(),
            usage: Usage { input_tokens: 1, output_tokens: 1, ..Usage::default() },
            ..Turn::default()
        }],
        ..Script::default()
    };
    let run = Run::playing(&talking);
    // SAFETY: `select` resolves the binary from this variable, which is
    // process-wide. This binary runs its cases one at a time on one thread and
    // nothing else in it reads the environment, so there is no concurrent
    // reader.
    unsafe {
        std::env::set_var(
            ganja_core::provider::claude_code::BIN_ENV,
            std::env::current_exe().expect("this test binary has a path"),
        );
        std::env::set_var(fake_claude::SCRIPT_ENV, run.script());
    }

    // **Not** `start_paused`: `from_env` runs `<bin> --version` under a real
    // 10 s bound, and a clock paused from the start would burn it before the
    // child had drawn breath. The clock is paused below, once the wire is
    // built and the only thing left to prove is a bound measured on it.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(async {
            let config = ganja_core::Config {
                default_provider: Some(ganja_core::provider::claude_code::ID.to_owned()),
                claude_code: ganja_core::config::ClaudeCodeConfig { idle_bound: Some(30) },
                ..ganja_core::Config::default()
            };
            let selection = ganja_core::provider::select(&config)
                .await
                .expect("the fake CLI is above the floor");

            assert_eq!(selection.provider.id(), ganja_core::provider::claude_code::ID);
            assert_eq!(
                selection.model,
                ganja_core::provider::claude_code::DEFAULT_MODEL,
                "and the wire's own default, which argv turns back into no --model at all"
            );

            // One turn, then the bound the config named — not the wire's own
            // 600 s, which would leave the entry standing here.
            let request = ganja_core::provider::ChatRequest {
                model: ganja_core::provider::claude_code::DEFAULT_MODEL.to_owned(),
                system: None,
                messages: vec![ganja_protocol::Message::user("hello")],
                turn_start: 0,
                tools: vec![ganja_core::tool::ToolDefinition {
                    name: "noop".to_owned(),
                    description: "a roster of one, so this is no one-shot".to_owned(),
                    schema: serde_json::json!({"type": "object", "properties": {}}),
                }],
                effort_options: serde_json::Map::new(),
            };
            let mut stream = selection
                .provider
                .stream(request, tokio_util::sync::CancellationToken::new())
                .await
                .expect("the turn opens");
            while futures::StreamExt::next(&mut stream).await.is_some() {}

            // Observed through the trait rather than through the wire's own
            // `held_entries`: a `Selection` hands back an `Arc<dyn Provider>`,
            // which is exactly the point — what this proves is that the value
            // a *caller* is given was built with the bound the config named.
            // `last_eviction` fills at the eviction, so it is the one door the
            // trait offers onto that fact.
            assert!(selection.provider.last_eviction().is_none(), "nothing has been evicted yet");

            // From here on nothing touches a real process, so the bound can be
            // reached by arithmetic rather than by waiting half a minute.
            tokio::time::pause();
            tokio::time::advance(std::time::Duration::from_secs(31)).await;
            for _ in 0..5_000 {
                if selection.provider.last_eviction().is_some() {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("the held process outlived the 30 s the config named");
        });
}
