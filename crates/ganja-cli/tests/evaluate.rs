//! `ganja evaluate` (**D564**), driven through the shipped binary against a
//! loopback endpoint, plus the recipe script that ships beside it.
//!
//! The binary is really run, which is the point of every case here: what a
//! hook gets back is an **exit code**, and a code is produced by a process.
//! Nothing in this file calls a library function to decide one.
//!
//! No environment variable is ever set on *this* process — every one of them
//! is set on the child, so the cases cannot race each other and a developer
//! whose shell exports a real `TYPESAFE_API_KEY` (as `.envrc` does in this
//! repository) runs the same suite CI does. The two cases that need the
//! variable *absent* remove it from the child explicitly for that reason.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use assert_cmd::Command as Asserted;
use ganja_testkit::{Homes, responses_server};
use serde_json::{Value, json};

/// The key every case sends. It never leaves loopback.
const KEY: &str = "sk-typesafe-suite-0123456789";

/// A whole answer carrying one of each primitive, so the two formats have
/// something to disagree about if they ever drift.
const ANSWERED: &str = r#"{"model":"jev-1.13.0","answers":{"dept":{"type":"choice","choice":"technical","probabilities":{"billing":0.08,"technical":0.92},"confidence":0.82},"urgent":{"type":"noul","noul":0.92}},"usage":{"input_tokens":312,"output_tokens":48}}"#;

/// Two questions, one of each of the two types [`ANSWERED`] answers.
fn questions() -> String {
    json!({
        "urgent": {"type": "noul", "instructions": "Is the message urgent?"},
        "dept": {
            "type": "choice",
            "instructions": "Which desk owns it?",
            "criteria": {"billing": "money", "technical": "the product"},
        },
    })
    .to_string()
}

// ---------------------------------------------------------------- endpoint

/// A loopback endpoint answering every connection the same way and keeping
/// what it was sent.
///
/// `std`, not `tokio`: this suite drives a child process rather than a
/// client, so there is no runtime here to borrow and a thread is the whole
/// machinery needed. It is stopped and joined on drop rather than detached —
/// every test in a binary shares one process, and a detached accept loop per
/// fixture would outlive its case and run to the end of the suite.
struct Endpoint {
    /// What to hand the child as `TYPESAFE_BASE_URL`.
    base: String,
    /// Each request as it arrived, headers and body.
    seen: Arc<Mutex<Vec<String>>>,
    /// Set on drop; the accept loop reads it between polls.
    stop: Arc<AtomicBool>,
    /// Taken in `drop` so the thread can be joined.
    server: Option<JoinHandle<()>>,
}

impl Endpoint {
    /// The base URL the child is pointed at.
    fn base(&self) -> &str {
        &self.base
    }

    /// How many requests arrived. One, for a client with no retry.
    fn count(&self) -> usize {
        self.seen.lock().expect("the request log is never poisoned").len()
    }

    /// The body of the first request, as JSON.
    fn body(&self) -> Value {
        let seen = self.seen.lock().expect("the request log is never poisoned");
        let first = seen.first().expect("a request arrived").clone();
        let (_, body) = first.split_once("\r\n\r\n").expect("a request has a body");

        serde_json::from_str(body).unwrap_or_else(|error| panic!("the body is JSON: {error}"))
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// An endpoint answering `status` with `body` to every connection.
fn serve(status: u16, body: &str) -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is bindable");
    let base = format!("http://{}", listener.local_addr().expect("a bound socket has an address"));
    // So the accept loop can notice `stop` instead of blocking on a
    // connection that is never going to come.
    listener.set_nonblocking(true).expect("a listener takes a mode");

    let response = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let log = Arc::clone(&seen);
    let halt = Arc::clone(&stop);

    let server = std::thread::spawn(move || {
        while !halt.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => answer(stream, &response, &log),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        }
    });

    Endpoint { base, seen, stop, server: Some(server) }
}

/// Reads one whole request, records it, and writes `response` back.
fn answer(mut stream: TcpStream, response: &str, log: &Arc<Mutex<Vec<String>>>) {
    // An accepted socket inherits the listener's mode on some platforms and
    // not others, so it is said here rather than assumed.
    stream.set_nonblocking(false).expect("a served socket takes a mode");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("a served socket takes a timeout");

    let mut request = Vec::new();
    let mut chunk = vec![0_u8; 8192];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        if complete(&request) {
            break;
        }
    }
    log.lock()
        .expect("the request log is never poisoned")
        .push(String::from_utf8_lossy(&request).into_owned());

    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Whether `request` holds the headers and as many body bytes as its
/// `content-length` declared.
fn complete(request: &[u8]) -> bool {
    let text = String::from_utf8_lossy(request);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let declared = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or_default();

    body.len() >= declared
}

// ------------------------------------------------------------------ driver

/// A run of the shipped binary with its homes pinned and the vendor variables
/// set **on the child**.
fn ganja(homes: &Homes, base: Option<&str>) -> Asserted {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ganja"));
    homes.pin(&mut command, Path::new("unused.json"));
    command.env("TYPESAFE_API_KEY", KEY).env_remove("TYPESAFE_DEFAULT_MODEL");
    match base {
        Some(base) => command.env("TYPESAFE_BASE_URL", base),
        // Not merely unset here: the developer running this has one exported.
        None => command.env_remove("TYPESAFE_BASE_URL"),
    };

    Asserted::from_std(command)
}

/// The code a finished run answered with, and what it printed.
struct Ran {
    /// The exit code. Every case here has one; nothing is signalled.
    code: i32,
    /// Standard output, which is what a hook reads.
    stdout: String,
    /// Standard error, which is where every refusal explains itself.
    stderr: String,
}

/// Runs `command` and reads what it did.
///
/// `&mut` because every builder method of `assert_cmd::Command` answers one,
/// so a call site reads `ran(ganja(..).args(..))` with no binding in between.
fn ran(command: &mut Asserted) -> Ran {
    let output = command.output().expect("the binary runs");

    Ran {
        code: output.status.code().expect("the run exited rather than being signalled"),
        stdout: String::from_utf8(output.stdout).expect("stdout is text"),
        stderr: String::from_utf8(output.stderr).expect("stderr is text"),
    }
}

// ------------------------------------------------------------- the codes

/// **Criterion 7.** An answered call exits 0 and prints the answers, in both
/// formats — `json` being the whole response and `text` the tool's own line
/// rendering, which is what `docs/recipes/`'s awk parses.
#[test]
fn an_answered_call_exits_zero_and_prints_the_answers() {
    let endpoint = serve(200, ANSWERED);
    let homes = Homes::new();

    let json = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("payouts have failed for three days"));
    assert_eq!(json.code, 0, "stderr:\n{}", json.stderr);
    let printed: Value = serde_json::from_str(&json.stdout).expect("--format json prints JSON");
    assert_eq!(printed["model"], json!("jev-1.13.0"), "the served model, not the alias asked for");
    assert_eq!(printed["answers"]["urgent"]["noul"], json!(0.92));
    assert_eq!(printed["usage"]["input_tokens"], json!(312));

    let text = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--format", "text", "--questions", &questions()])
        .write_stdin("payouts have failed for three days"));
    assert_eq!(text.code, 0, "stderr:\n{}", text.stderr);
    assert_eq!(
        text.stdout.trim(),
        "dept: choice=technical p=0.92 confidence=0.82 (billing 0.08)\nurgent: noul=0.92",
        "one line per question, in id order",
    );
}

/// **Criterion 7.** Text that is not JSON is a text state, which is the case
/// that lets a log or a diff be piped in as it stands.
#[test]
fn a_state_that_is_not_json_is_sent_as_text() {
    let endpoint = serve(200, ANSWERED);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("not json, just a sentence"));

    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
    assert_eq!(endpoint.body()["state"], json!("not json, just a sentence"));
}

/// **Criterion 7, addendum A1.** `--model` reaches the request body, and the
/// other alias is a value this build passes through rather than one it knows.
#[test]
fn the_requested_model_reaches_the_body() {
    let endpoint = serve(200, ANSWERED);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--model", "jev-preview", "--questions", &questions()])
        .write_stdin(r#"{"ticket": "the card was declined"}"#));

    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
    assert_eq!(endpoint.body()["model"], json!("jev-preview"));
    assert_eq!(endpoint.body()["state"], json!({"ticket": "the card was declined"}));
    assert_eq!(endpoint.count(), 1, "one attempt");
}

/// **Criterion 7, addendum A1.** With no `--model` the configured default is
/// what travels, and `TYPESAFE_DEFAULT_MODEL` is what configures it.
#[test]
fn the_configured_default_model_travels_when_none_is_asked_for() {
    let endpoint = serve(200, ANSWERED);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .env("TYPESAFE_DEFAULT_MODEL", "jev-1.13.0")
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
    assert_eq!(endpoint.body()["model"], json!("jev-1.13.0"));
}

/// **Criterion 7.** No key is exit 3, and nothing is asked of anybody.
#[test]
fn an_unset_key_is_not_configured() {
    let endpoint = serve(200, ANSWERED);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .env_remove("TYPESAFE_API_KEY")
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 3, "stderr:\n{}", run.stderr);
    assert!(run.stdout.is_empty(), "a hook reads stdout; got {:?}", run.stdout);
    assert!(run.stderr.contains("TYPESAFE_API_KEY"), "the variable is named: {}", run.stderr);
    assert_eq!(endpoint.count(), 0, "nothing was sent");
}

/// **Criterion 7.** A base URL that would put the key on the wire in the
/// clear is exit 3 — the same row as no key, because both mean there is no
/// configured endpoint this build will use.
#[test]
fn a_base_url_in_the_clear_is_not_configured() {
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some("http://example.com"))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 3, "stderr:\n{}", run.stderr);
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("TYPESAFE_BASE_URL"), "the variable is named: {}", run.stderr);
    // The URL itself is configuration and may carry a credential in its
    // userinfo, so it is named as a variable and never echoed.
    assert!(!run.stderr.contains("example.com"), "the value is not echoed: {}", run.stderr);
}

/// **Criterion 7.** A 422 is the vendor refusing the questions: exit 4, and
/// what it said about which field survives to stderr.
#[test]
fn a_rejected_body_is_a_refusal() {
    let endpoint =
        serve(422, r#"{"detail":[{"loc":["body","questions"],"msg":"field required"}]}"#);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 4, "stderr:\n{}", run.stderr);
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("field required"), "the vendor's reason survives: {}", run.stderr);
}

/// **Criterion 7.** A 401 is the credential being refused, which is the other
/// half of exit 4.
#[test]
fn a_refused_credential_is_a_refusal() {
    let endpoint = serve(401, r#"{"detail":{"error_type":"authentication_error"}}"#);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 4, "stderr:\n{}", run.stderr);
    assert_eq!(endpoint.count(), 1, "one attempt, even for a refusal");
}

/// **Criterion 7.** A 529 is exit 5 — and exactly one attempt, because the
/// row that most invites a retry is the one this client does not give.
#[test]
fn an_overloaded_vendor_is_unavailable_and_is_asked_once() {
    let endpoint = serve(529, r#"{"detail":"overloaded"}"#);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 5, "stderr:\n{}", run.stderr);
    assert!(run.stdout.is_empty());
    assert_eq!(endpoint.count(), 1, "no retry");
}

/// **Criterion 7.** Questions that are not a JSON object are this command's
/// own argument error, `EX_USAGE`, and nothing is sent.
#[test]
fn malformed_questions_are_a_usage_error() {
    let endpoint = serve(200, ANSWERED);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", "{not json"])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 64, "stderr:\n{}", run.stderr);
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("--questions"), "the flag is named: {}", run.stderr);
    assert_eq!(endpoint.count(), 0, "nothing was sent");
}

/// **Criterion 7.** A questions file that is not there says so about the
/// file, rather than about the JSON it does not contain.
#[test]
fn a_questions_file_that_is_missing_says_so() {
    let homes = Homes::new();
    let missing = homes.project().join("nope.json");

    let run = ran(ganja(&homes, None)
        .args(["evaluate", "--questions", &format!("@{}", missing.display())])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 64, "stderr:\n{}", run.stderr);
    assert!(run.stderr.contains("nope.json"), "the path is named: {}", run.stderr);
}

/// **Criterion 7.** A state that is valid JSON but a bare number is refused
/// by name. Wrapping it in quotes would send something nobody wrote.
#[test]
fn a_numeric_state_is_a_usage_error() {
    let endpoint = serve(200, ANSWERED);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("42"));

    assert_eq!(run.code, 64, "stderr:\n{}", run.stderr);
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("--state"), "the flag is named: {}", run.stderr);
    assert_eq!(endpoint.count(), 0, "nothing was sent");
}

/// **Criterion 7.** A flag this command does not have is clap's refusal, and
/// clap's code — **2**, the one code ganja never chooses, because a
/// `PreToolUse` hook reads it as a block.
#[test]
fn an_unknown_flag_is_claps_own_two() {
    let homes = Homes::new();

    let run =
        ran(ganja(&homes, None).args(["evaluate", "--questions", &questions(), "--nonesuch"]));

    assert_eq!(run.code, 2, "stderr:\n{}", run.stderr);
}

/// **Criterion 7.** A project whose `ganja.toml` does not load still answers:
/// this arm is dispatched before any config or project discovery, which is
/// what makes a hook survive a config somebody is halfway through editing.
#[test]
fn a_broken_config_in_the_project_does_not_stop_it() {
    let endpoint = serve(200, ANSWERED);
    let homes = Homes::new();
    std::fs::write(homes.project().join("ganja.toml"), "this is not = = toml\n")
        .expect("the fixture is writable");

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(
        run.code, 0,
        "a broken config is not this command's business\nstderr:\n{}",
        run.stderr
    );
}

// ------------------------------------------------------- the overlay (11)

/// A `ganja run` against the Responses recorder, in `json_schema_run.rs`'s
/// shape: the one wire whose endpoint an environment variable can move.
fn running(homes: &Homes, base: &str) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ganja"));
    homes.pin(&mut command, Path::new("unused.json"));
    command
        .env("GANJA_PROVIDER", "openai")
        .env("GANJA_MODEL", "gpt-5.5")
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", base)
        .env_remove("GANJA_FAKE_SCRIPT")
        .env_remove("TYPESAFE_BASE_URL")
        .stdin(std::process::Stdio::null());

    command
}

/// One whole Responses turn that says nothing and stops.
fn quiet_turn() -> String {
    [
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}"#,
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":4,"output_tokens":0}}}"#,
    ]
    .join("\n\n")
        + "\n\n"
}

/// Every tool name the first request advertised.
fn advertised(endpoint: &responses_server::Endpoint) -> Vec<String> {
    let seen = endpoint.seen();
    assert!(!seen.is_empty(), "the run reached the recorder at all");

    seen[0].json()["tools"]
        .as_array()
        .expect("a request carries a tool roster")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

/// **Criterion 11.** The overlay is *wired*, not merely written: a run whose
/// environment configures TypeSafe advertises `evaluate` to the model.
///
/// This covers `assemble.rs`, which serves both `run` and `serve`. The two
/// sites in `ganja-tui` have no headless door and are W5's own steps — a
/// frontend that forgets the line fails silently, which is why they are
/// mandatory there rather than optional.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_run_offers_the_tool() {
    let endpoint = responses_server::serve().await;
    endpoint.answers_turns_with(quiet_turn());
    let homes = Homes::new();

    let output = running(&homes, &endpoint.base_url)
        .env("TYPESAFE_API_KEY", KEY)
        .args(["run", "say nothing"])
        .output()
        .expect("the binary runs");
    assert!(
        output.status.success(),
        "the run exited {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let names = advertised(&endpoint);
    assert!(names.contains(&"evaluate".to_owned()), "the roster was {names:?}");
}

/// **Criterion 11.** And absent is literal: with no key there is no
/// `evaluate` in the roster at all, rather than one that would refuse at call
/// time. That is what buys the prompt bytes back for everybody who never
/// asked for this.
#[tokio::test(flavor = "multi_thread")]
async fn an_unconfigured_run_does_not_offer_the_tool() {
    let endpoint = responses_server::serve().await;
    endpoint.answers_turns_with(quiet_turn());
    let homes = Homes::new();

    let output = running(&homes, &endpoint.base_url)
        .env_remove("TYPESAFE_API_KEY")
        .args(["run", "say nothing"])
        .output()
        .expect("the binary runs");
    assert!(
        output.status.success(),
        "the run exited {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let names = advertised(&endpoint);
    assert!(!names.contains(&"evaluate".to_owned()), "the roster was {names:?}");
    // A control, so that a roster which came back empty for some unrelated
    // reason cannot pass this as an absence.
    assert!(names.contains(&"read".to_owned()), "the roster was {names:?}");
}

// --------------------------------------------------- the shipped recipe

/// The repository root, from this crate's manifest directory.
#[cfg(unix)]
fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels under the repository root")
        .to_owned()
}

/// A run of the **shipped** recipe script under `sh`, with the built binary
/// first on its PATH so that the `ganja` it calls is this one.
///
/// The script itself is never copied or rewritten here: a recipe tested as a
/// paraphrase of itself is a recipe nobody tested.
#[cfg(unix)]
fn recipe(homes: &Homes, base: &str, variant: &str) -> Asserted {
    let script = repository().join("docs/recipes/typesafe-pretooluse-hook.sh");
    assert!(script.exists(), "the recipe ships at {}", script.display());

    let binary = Path::new(env!("CARGO_BIN_EXE_ganja"))
        .parent()
        .expect("the built binary sits in a directory")
        .to_owned();
    let path = std::env::var("PATH").unwrap_or_default();

    let mut command = std::process::Command::new("sh");
    command
        .arg(&script)
        .arg(variant)
        .current_dir(homes.project())
        .env("PATH", format!("{}:{path}", binary.display()))
        .env("TYPESAFE_API_KEY", KEY)
        .env("TYPESAFE_BASE_URL", base)
        .env("XDG_DATA_HOME", homes.data())
        .env("HOME", homes.data())
        .env_remove("TYPESAFE_DEFAULT_MODEL");

    Asserted::from_std(command)
}

/// The envelope a `PreToolUse` hook is handed for a `bash` call.
#[cfg(unix)]
fn envelope() -> String {
    json!({
        "session_id": "ses_1",
        "cwd": "/tmp/project",
        "hook_event_name": "PreToolUse",
        "tool_name": "bash",
        "tool_input": {"command": "rm -rf \"$HOME\"/work"},
    })
    .to_string()
}

/// An answer putting `destructive` at `noul`.
///
/// The id is the shipped `questions.json`'s and the thresholds in the shipped
/// script are keyed by it, so a fixture that invented its own id would make
/// every assertion below vacuous.
#[cfg(unix)]
fn judged(noul: f64) -> String {
    json!({
        "model": "jev-1.13.0",
        "answers": {"destructive": {"type": "noul", "noul": noul}},
        "usage": {"input_tokens": 120, "output_tokens": 8},
    })
    .to_string()
}

/// **Criterion 7.** Over the shipped threshold, `annotate` prints an
/// `additionalContext` envelope — and exits 0, as every path through that
/// script does.
#[test]
#[cfg(unix)]
fn the_recipe_annotates_above_the_threshold() {
    let endpoint = serve(200, &judged(0.92));
    let homes = Homes::new();

    let run = ran(recipe(&homes, endpoint.base(), "annotate").write_stdin(envelope()));

    assert_eq!(run.code, 0, "the script always exits 0\nstderr:\n{}", run.stderr);
    let printed: Value = serde_json::from_str(run.stdout.trim())
        .unwrap_or_else(|error| panic!("the hook prints JSON ({error}): {:?}", run.stdout));
    let context = printed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("an annotate variant carries additionalContext");
    assert!(!context.is_empty(), "the context says something");
    assert!(context.contains("destructive"), "it names the question: {context}");
    assert!(
        printed["hookSpecificOutput"]["permissionDecision"].is_null(),
        "annotate decides nothing: {printed}"
    );
}

/// **Criterion 7.** Over the same threshold, `deny` refuses the call before
/// it runs and says why. This is the only variant that guards anything.
#[test]
#[cfg(unix)]
fn the_recipe_denies_above_the_threshold() {
    let endpoint = serve(200, &judged(0.92));
    let homes = Homes::new();

    let run = ran(recipe(&homes, endpoint.base(), "deny").write_stdin(envelope()));

    assert_eq!(run.code, 0, "the script always exits 0\nstderr:\n{}", run.stderr);
    let printed: Value = serde_json::from_str(run.stdout.trim())
        .unwrap_or_else(|error| panic!("the hook prints JSON ({error}): {:?}", run.stdout));
    assert_eq!(printed["hookSpecificOutput"]["permissionDecision"], json!("deny"));
    let reason = printed["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .expect("a deny carries its reason");
    assert!(!reason.is_empty(), "the model is told why");
    assert!(reason.contains("destructive"), "it names the question: {reason}");
}

/// **Criterion 7.** Under the threshold the script says nothing, so the call
/// is decided by the permission rules exactly as it would have been.
///
/// The control for the two cases above: without it, a script that printed
/// nothing for an unrelated reason would still pass them by never being
/// reached, and both would be asserting about a judgement that never happened.
#[test]
#[cfg(unix)]
fn the_recipe_says_nothing_below_the_threshold() {
    let endpoint = serve(200, &judged(0.10));
    let homes = Homes::new();

    let run = ran(recipe(&homes, endpoint.base(), "deny").write_stdin(envelope()));

    assert_eq!(run.code, 0);
    assert!(run.stdout.trim().is_empty(), "nothing is printed: {:?}", run.stdout);
    assert_eq!(endpoint.count(), 1, "it did ask, and got an answer under the threshold");
}

/// **Criterion 7.** A broken questions file exits 0 with empty stdout: the
/// judgement is lost and the session is left exactly where it would have been
/// with no hook at all.
#[test]
#[cfg(unix)]
fn a_broken_questions_file_degrades_to_the_normal_ask() {
    let endpoint = serve(200, &judged(0.99));
    let homes = Homes::new();
    let broken = homes.project().join("broken.json");
    std::fs::write(&broken, "{not json").expect("the fixture is writable");

    let run = ran(recipe(&homes, endpoint.base(), "deny")
        .env("GANJA_TYPESAFE_HOOK_QUESTIONS", &broken)
        .write_stdin(envelope()));

    assert_eq!(run.code, 0, "a broken recipe never blocks a call\nstderr:\n{}", run.stderr);
    assert!(run.stdout.trim().is_empty(), "nothing is printed: {:?}", run.stdout);
    assert_eq!(endpoint.count(), 0, "it never got as far as asking");
}

/// **Criterion 7.** The same when there is no key: exit 3 inside the script
/// is swallowed, and the hook is silent rather than blocking.
#[test]
#[cfg(unix)]
fn an_unconfigured_recipe_is_silent() {
    let endpoint = serve(200, &judged(0.99));
    let homes = Homes::new();

    let run = ran(recipe(&homes, endpoint.base(), "deny")
        .env_remove("TYPESAFE_API_KEY")
        .write_stdin(envelope()));

    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
    assert!(run.stdout.trim().is_empty(), "nothing is printed: {:?}", run.stdout);
    assert_eq!(endpoint.count(), 0);
}

/// **Criterion 7.** The shipped `questions.json` is what the shipped script
/// asks, and the two agree about the ids: every question the file carries is
/// one the script has a threshold for.
///
/// A drift test, because the two files are edited by hand and a question
/// renamed in one of them would make the hook inert without failing anything.
#[test]
#[cfg(unix)]
fn the_shipped_questions_are_the_ones_the_script_thresholds() {
    let recipes = repository().join("docs/recipes");
    let asked: BTreeMap<String, Value> = serde_json::from_str(
        &std::fs::read_to_string(recipes.join("questions.json")).expect("the questions ship"),
    )
    .expect("the questions are a JSON object");
    let script = std::fs::read_to_string(recipes.join("typesafe-pretooluse-hook.sh"))
        .expect("the script ships");
    let thresholds = script
        .lines()
        .find_map(|line| line.strip_prefix("THRESHOLDS=\"")?.strip_suffix('"'))
        .expect("the script carries one THRESHOLDS line");

    let listed: Vec<&str> = thresholds
        .split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .map(|(id, _)| id)
        .collect();
    for id in asked.keys() {
        assert!(listed.contains(&id.as_str()), "`{id}` has no threshold; THRESHOLDS is {listed:?}");
    }
    for id in &listed {
        assert!(asked.contains_key(*id), "`{id}` is thresholded but never asked");
    }
}

// ------------------------------------------------- vendor-controlled text

/// A `choice` whose value and option names carry terminal control sequences.
///
/// The escapes are **JSON's**, not Rust's: a raw control byte inside a JSON
/// string is not JSON at all and the client rejects the whole response as
/// malformed long before any of this. What a hostile response really looks
/// like is well-formed JSON whose *decoded* string holds the escape — which
/// is what reaches a terminal, and so what these cases are about.
///
/// `destructive` rides along so the recipe case below has a `noul` line to
/// act on: the point there is that filtering the neighbouring line does not
/// disturb the one `awk` reads.
const HOSTILE: &str = r#"{"model":"jev-1.13.0","answers":{"destructive":{"type":"noul","noul":0.92},"desk":{"type":"choice","choice":"\u001b[31mbilling\u0007","probabilities":{"\u001b[31mbilling\u0007":0.9,"tech\u001b[0m":0.1},"confidence":0.8}},"usage":{"input_tokens":10,"output_tokens":2}}"#;

/// **W2 review.** `--format text` is a third party's text on a hook author's
/// terminal, so it goes through the reporter's own `printable` filter: no
/// escape reaches the screen, and the words around it survive.
#[test]
fn control_characters_never_reach_the_terminal_in_text() {
    let endpoint = serve(200, HOSTILE);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--format", "text", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
    assert!(!run.stdout.contains('\u{1b}'), "an escape survived: {:?}", run.stdout);
    assert!(!run.stdout.contains('\u{7}'), "a BEL survived: {:?}", run.stdout);
    // Not merely stripped to nothing: the line is still the line, with the
    // control characters standing in as replacement characters.
    assert!(run.stdout.contains("billing"), "the text around them is intact: {:?}", run.stdout);
    assert!(run.stdout.contains("desk: choice="), "the shape is unchanged: {:?}", run.stdout);
    // The join is what makes this a per-line filter rather than a whole-string
    // one; a whole-string filter would have eaten it.
    assert_eq!(run.stdout.lines().count(), 2, "both answers, on their own lines");
}

/// **W2 review.** `--format json` needs no filter and gets none: `serde_json`
/// escapes a control character rather than emitting it, so a caller piping
/// this into a parser gets the vendor's bytes back exactly.
#[test]
fn control_characters_survive_json_because_json_escapes_them() {
    let endpoint = serve(200, HOSTILE);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
    assert!(!run.stdout.contains('\u{1b}'), "the raw byte is escaped, not emitted");
    let printed: Value = serde_json::from_str(&run.stdout).expect("--format json prints JSON");
    assert_eq!(
        printed["answers"]["desk"]["choice"],
        json!("\u{1b}[31mbilling\u{7}"),
        "the parser gets the vendor's own string back"
    );
}

/// **W2 review.** And the recipe still works over a filtered rendering: the
/// `noul` line `awk` reads is untouched by what happened to its neighbour.
#[test]
#[cfg(unix)]
fn the_recipe_still_parses_a_filtered_rendering() {
    let endpoint = serve(200, HOSTILE);
    let homes = Homes::new();

    let run = ran(recipe(&homes, endpoint.base(), "deny").write_stdin(envelope()));

    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
    let printed: Value = serde_json::from_str(run.stdout.trim())
        .unwrap_or_else(|error| panic!("the hook prints JSON ({error}): {:?}", run.stdout));
    assert_eq!(printed["hookSpecificOutput"]["permissionDecision"], json!("deny"));
}

/// **`lines` contract.** A response carrying no answers prints **nothing** in
/// `text` — not a blank line, which the recipe's `awk` would read as one more
/// record that happens not to match.
#[test]
fn an_empty_answer_set_prints_nothing_at_all() {
    let endpoint = serve(200, r#"{"model":"jev-1.13.0","answers":{},"usage":{"input_tokens":3}}"#);
    let homes = Homes::new();

    let text = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--format", "text", "--questions", &questions()])
        .write_stdin("a sentence"));
    assert_eq!(text.code, 0, "stderr:\n{}", text.stderr);
    assert!(text.stdout.is_empty(), "not even a newline: {:?}", text.stdout);

    // `json` still prints its document: an empty `answers` map is an answer
    // about the request, and a script reading JSON is not reading lines.
    let json = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));
    assert_eq!(json.code, 0);
    let printed: Value = serde_json::from_str(&json.stdout).expect("a document is printed");
    assert_eq!(printed["answers"], json!({}));
}

/// **W2 review.** The vendor's own 422 `detail` reaches this command's stderr,
/// so it is filtered on the way out too.
#[test]
fn control_characters_never_reach_the_terminal_on_stderr() {
    let endpoint = serve(422, r#"{"detail":"bad \u001b[31mfield\u0007"}"#);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 4, "stderr:\n{}", run.stderr);
    assert!(!run.stderr.contains('\u{1b}'), "an escape survived: {:?}", run.stderr);
    assert!(!run.stderr.contains('\u{7}'), "a BEL survived: {:?}", run.stderr);
    assert!(run.stderr.contains("field"), "the reason still reads: {}", run.stderr);
}

// ------------------------------------------------------------ model ids

/// **W2 review, F1.** A `--model` outside the id rule is *this invocation's*
/// mistake, so it is 64 — and it is refused here, with the endpoint never
/// hearing the request.
#[test]
fn a_model_id_that_is_not_one_is_a_usage_error() {
    // A marker no refusal sentence could contain on its own, so "the value is
    // not echoed" is a claim about the value rather than about a word that
    // happens to appear in the explanation (`jev-latest` and `jev-1.13.0` are
    // both *in* that explanation, so a naive substring check would pass for
    // the wrong reason).
    const FORGED: &str = "Zzq7Forged";

    for bad in [
        &format!("jev-latest \u{b7} 999 B \u{b7} 0 question(s) {FORGED}"),
        &"j".repeat(65),
        &format!("jev {FORGED}"),
        &format!("jev\n{FORGED}"),
    ] {
        let bad: &str = bad;
        let endpoint = serve(200, ANSWERED);
        let homes = Homes::new();

        let run = ran(ganja(&homes, Some(endpoint.base()))
            .args(["evaluate", "--model", bad, "--questions", &questions()])
            .write_stdin("a sentence"));

        assert_eq!(run.code, 64, "`{bad}` should be a usage error\nstderr:\n{}", run.stderr);
        assert!(run.stdout.is_empty());
        assert_eq!(endpoint.count(), 0, "`{bad}` never reached the vendor");
        // The id rule exists because this value reaches the consent dialog's
        // `·`-separated title, where it could forge a second disclosure.
        // A refusal that quoted it would print the forgery instead.
        assert!(
            !run.stderr.contains(FORGED),
            "the refused value must not be echoed: {}",
            run.stderr
        );
    }
}

/// **W2 review, F1.** A `TYPESAFE_DEFAULT_MODEL` outside the same rule is
/// *configuration* that was already wrong before this ran, so it is 3 — the
/// row no caller should confuse with the one above, because the two send
/// somebody to fix different things.
#[test]
fn a_configured_model_id_that_is_not_one_is_not_configured() {
    let endpoint = serve(200, ANSWERED);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .env("TYPESAFE_DEFAULT_MODEL", "bad Zzq7Forged model")
        .args(["evaluate", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 3, "stderr:\n{}", run.stderr);
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("TYPESAFE_DEFAULT_MODEL"), "the variable is named: {}", run.stderr);
    assert!(
        !run.stderr.contains("Zzq7Forged"),
        "and the value it held is not echoed: {}",
        run.stderr
    );
    assert_eq!(endpoint.count(), 0);
}

/// **W2 review.** An explicit `--model` still wins over a configured default,
/// and a good one is not refused by the rule that refuses a bad one.
#[test]
fn a_good_model_id_passes_the_rule() {
    let endpoint = serve(200, ANSWERED);
    let homes = Homes::new();

    let run = ran(ganja(&homes, Some(endpoint.base()))
        .args(["evaluate", "--model", "jev-1.13.0", "--questions", &questions()])
        .write_stdin("a sentence"));

    assert_eq!(run.code, 0, "stderr:\n{}", run.stderr);
    assert_eq!(endpoint.body()["model"], json!("jev-1.13.0"));
}

// ------------------------------------------------ a terminal on stdin

/// **W2 review.** `--state` defaults to `-`, and `-` with a **terminal** on
/// standard input is a usage error rather than a hang.
///
/// Run under a real pty, because that is the only way the child's
/// `IsTerminal` answers true: `assert_cmd` hands a child a pipe or a null
/// device, and against either of those this path is the ordinary read. The
/// deadline is the assertion that matters — a build that hung here would
/// hang a person's terminal with no prompt and no hint it was waiting.
#[test]
#[cfg(unix)]
fn a_terminal_on_standard_input_is_a_usage_error_not_a_hang() {
    use std::time::Instant;

    use expectrl::Session;
    use expectrl::process::unix::WaitStatus;

    let homes = Homes::new();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ganja"));
    homes.pin(&mut command, Path::new("unused.json"));
    command
        .env("TYPESAFE_API_KEY", KEY)
        .env_remove("TYPESAFE_BASE_URL")
        .env_remove("TYPESAFE_DEFAULT_MODEL")
        .args(["evaluate", "--questions", &questions()]);

    let started = Instant::now();
    let session = Session::spawn(command).expect("`ganja` spawns in a pty");
    let status = session.get_process().wait().expect("the child is reaped");

    assert!(
        started.elapsed() < Duration::from_secs(30),
        "it answered rather than waiting for a keystroke nobody was going to type"
    );
    assert!(
        matches!(status, WaitStatus::Exited(_, 64)),
        "a terminal on standard input is a usage error; got {status:?}"
    );
}
