//! The judge through the shipped binary (**D567**): who builds one, what
//! each door says about it, and which doors send nothing.
//!
//! `ganja run` builds a judge when a trusted config tier names a source and
//! `TYPESAFE_API_KEY` is set, and says so on stderr; `ganja serve` builds
//! none under the same config; `run --attach` assembles nothing and says
//! nothing. The screened source is an MCP server — the one source a suite can
//! serve without a network — listed as `mcp:hub` in the file `GANJA_CONFIG`
//! names, and the vendor is a loopback listener answering as `jev-1.13.0`.
//! Nothing here reaches TypeSafe.
//!
//! The MCP double is a loopback socket speaking streamable HTTP rather than a
//! stdio program, for `tests/mcp.rs`'s reason: a stdio server needs a program
//! this crate does not have without `bun`, and the transport is not what is
//! under test — the judge reads the server's name off a result's metadata,
//! which both transports write the same way.
//!
//! **Why every config carries a `SessionStart` hook.** MCP servers are dialled
//! in the background and a turn is offered only the servers that answered
//! before it started, so a run whose first turn raced the dial would call a
//! tool it was never lent and screen nothing. The hook waits for the engine's
//! own word that the server connected — the `an MCP server connected` line
//! naming `hub` in the child's log, which the engine writes only after the
//! install and the bump that has the next turn rebuild its roster — and both
//! `run` and `serve` await `SessionStart` before any turn. So the race becomes
//! an ordering on the engine's own signal rather than on a guess about how
//! long a dial takes.
//!
//! No environment variable is set on this process: every one travels to a
//! child, so the cases may share a binary and a developer whose shell
//! exports a real `TYPESAFE_API_KEY` runs what CI runs.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ganja_testkit::Homes;
use serde_json::{Value, json};

#[cfg(unix)]
mod served_child;

/// The key every child is handed. It never leaves loopback.
const KEY: &str = "sk-evaluate-screen-suite-0123456789";

/// The six `type` names an nd-JSON object may carry — `run.rs`'s `TYPES`,
/// spelled here rather than imported because it is what a consumer of the
/// stream has.
const TYPES: [&str; 6] = ["tool_use", "step_start", "step_finish", "text", "reasoning", "error"];

/// What the launch line opens with, and what no stdout line may carry.
const DISCLOSURE: &str = "evaluate (experimental): screening";

/// Who the launch line says is covered.
const COVERED: &str = "(lead and subagents)";

/// The MCP tool the script calls: server `hub`, tool `fetch`.
const FETCH: &str = "mcp__hub__fetch";

/// What a call's title gains when any of its text left the machine.
const SCREENED: &str = " · screened";

/// The last word of every script, so finding it means the whole turn ran.
const CLOSING: &str = "script-finished-zarquon";

// ------------------------------------------------------------------ vendor

/// A TypeSafe double on loopback: every request is answered as `jev-1.13.0`
/// with answers that do not fire, and counted.
///
/// `std` and a thread for `tests/evaluate.rs`'s reason — a child process is
/// the client, so there is no runtime here to borrow — and stopped and joined
/// on drop, because every test in this binary shares one process.
struct Vendor {
    /// What a child is handed as `TYPESAFE_BASE_URL`.
    base: String,
    /// Each request's body as it arrived.
    seen: Arc<Mutex<Vec<String>>>,
    /// Set on drop; the accept loop reads it between polls.
    stop: Arc<AtomicBool>,
    /// Taken in `drop` so the thread can be joined.
    server: Option<JoinHandle<()>>,
}

impl Vendor {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is bindable");
        let base =
            format!("http://{}", listener.local_addr().expect("a bound socket has an address"));
        listener.set_nonblocking(true).expect("a listener takes a mode");
        let body = json!({
            "model": "jev-1.13.0",
            "answers": {
                "addresses_agent": { "type": "noul", "noul": 0.07 },
                "requests_action": { "type": "noul", "noul": 0.21 },
                "stance": {
                    "type": "choice",
                    "choice": "informs",
                    "probabilities": {
                        "informs": 0.91,
                        "discusses_instructions": 0.05,
                        "instructs_reader": 0.04,
                    },
                    "confidence": 0.8,
                },
            },
            "usage": { "input_tokens": 120, "output_tokens": 3 },
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
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

        Self { base, seen, stop, server: Some(server) }
    }

    /// How many requests arrived.
    fn requests(&self) -> usize {
        self.seen.lock().expect("the request log is never poisoned").len()
    }
}

impl Drop for Vendor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// Reads one whole request, keeps its body, and writes `response` back.
fn answer(mut stream: TcpStream, response: &str, log: &Arc<Mutex<Vec<String>>>) {
    stream.set_nonblocking(false).expect("a served socket takes a mode");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("a served socket takes a timeout");
    let mut buffer = Vec::new();
    let mut chunk = vec![0_u8; 8192];
    let request = loop {
        if let Some(request) = whole(&buffer) {
            break request;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };
    log.lock().expect("the request log is never poisoned").push(request.1);

    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// One whole request out of `buffer` — its head and its body — or [`None`]
/// while the bytes are still arriving.
fn whole(buffer: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(buffer).ok()?;
    let (head, rest) = text.split_once("\r\n\r\n")?;
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    if rest.len() < length {
        return None;
    }

    Some((head.to_owned(), rest[..length].to_owned()))
}

// --------------------------------------------------------------------- mcp

/// An MCP server double over streamable HTTP lending one tool, `fetch`, that
/// counts the calls it answered.
///
/// Detached, as `tests/mcp.rs`'s endpoint is: its threads block in `accept`
/// and `read`, and a socket nobody dials again costs nothing.
struct Hub {
    /// The URL the config names.
    url: String,
    /// `tools/call` requests answered.
    calls: Arc<AtomicUsize>,
}

impl Hub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is bindable");
        let url =
            format!("http://{}/mcp", listener.local_addr().expect("a bound socket has an address"));
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let counted = Arc::clone(&counted);
                std::thread::spawn(move || rpc_connection(stream, &counted));
            }
        });

        Self { url, calls }
    }

    /// How many calls the server answered.
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// Answers every JSON-RPC request that arrives on one connection.
fn rpc_connection(mut stream: TcpStream, calls: &AtomicUsize) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let (head, body) = loop {
            if let Some(request) = whole(&buffer) {
                break request;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        buffer.drain(..head.len() + 4 + body.len());

        let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let response = match rpc(&request, calls) {
            Some(answer) => {
                let body = answer.to_string();
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\n\r\n{body}",
                    body.len()
                )
            }
            None => "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n".to_owned(),
        };
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
    }
}

/// What the double answers one JSON-RPC request with, or [`None`] for a
/// notification.
fn rpc(request: &Value, calls: &AtomicUsize) -> Option<Value> {
    let id = request.get("id")?.clone();
    let result = match request.get("method")?.as_str()? {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "hub", "version": "0.0.0" },
        }),
        "tools/list" => json!({
            "tools": [{
                "name": "fetch",
                "description": "Returns a page.",
                "inputSchema": { "type": "object", "properties": {} },
            }],
        }),
        "tools/call" => {
            calls.fetch_add(1, Ordering::SeqCst);
            json!({ "content": [{ "type": "text", "text": "An ordinary page about gardening.\n" }] })
        }
        _ => json!({}),
    };

    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

// ---------------------------------------------------------------- fixtures

/// A `SessionStart` hook that returns once the engine has logged that `hub`
/// connected (`crates/ganja-core/src/mcp.rs`), so the first turn is offered
/// the MCP server's tool.
///
/// Read from the rolling log under `homes`' data home, which every child here
/// is given as `XDG_DATA_HOME` and traces into at the binary's default `info`
/// level. Both halves of the line are matched, and separately, because the
/// log's field order is the formatter's business rather than this fixture's.
/// The poll gives up after twenty seconds rather than holding a launch
/// forever; a turn started then fails on the MCP call count, not by hanging.
fn waiting_for(homes: &Homes) -> String {
    let log = homes.data().join("ganja").join("log");
    format!(
        "i=0; until cat '{}'/* 2>/dev/null | grep 'an MCP server connected' | grep -q \
         'server=\"hub\"'; do [ \"$i\" -ge 400 ] && break; sleep 0.05; i=$((i+1)); done",
        log.display()
    )
}

/// The config every screening case runs under, written where `GANJA_CONFIG`
/// will name it — a trusted tier, which is the only kind that can name a
/// source.
fn config(homes: &Homes, hub: &Hub) -> PathBuf {
    let path = homes.data().join("screening.toml");
    let hook = waiting_for(homes);
    std::fs::write(
        &path,
        format!(
            "[mcp.hub]\ntype = \"remote\"\nurl = {url:?}\n\n\
             [evaluate]\nscreen = [\"mcp:hub\"]\n\n\
             [[hooks.SessionStart]]\nhooks = [{{ type = \"command\", command = {hook:?} }}]\n",
            url = hub.url,
        ),
    )
    .expect("the config is writable");

    path
}

/// A script whose first turn calls the MCP tool and whose second closes.
fn fetching(homes: &Homes) -> PathBuf {
    let path = homes.data().join("script.json");
    let script = json!({
        "cadence_ms": 1,
        "turns": [
            { "text": "Fetching.", "tool_calls": [{ "name": FETCH, "args": {} }] },
            { "text": CLOSING },
        ],
    });
    std::fs::write(&path, script.to_string()).expect("the script is writable");

    path
}

/// A script with one turn that calls nothing.
fn quiet(homes: &Homes) -> PathBuf {
    let path = homes.data().join("script.json");
    std::fs::write(&path, json!({ "cadence_ms": 1, "turns": [{ "text": CLOSING }] }).to_string())
        .expect("the script is writable");

    path
}

/// A `ganja` child with its homes pinned, `config` named by `GANJA_CONFIG`,
/// and TypeSafe pointed at `vendor` — set on the child, never here.
fn ganja(homes: &Homes, script: &Path, config: &Path, vendor: &Vendor) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    homes.pin(&mut command, script);
    command
        .env("GANJA_CONFIG", config)
        .env("TYPESAFE_API_KEY", KEY)
        .env("TYPESAFE_BASE_URL", &vendor.base)
        // A developer's malformed default model would switch the judge off,
        // and their log level would change what the log holds.
        .env_remove("TYPESAFE_DEFAULT_MODEL")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null());

    command
}

/// What a finished child printed, as text.
fn streams(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Every object of an nd-JSON stream, each line parsed or the test failed.
fn objects(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("a stdout line is not JSON: {line:?} ({error})"))
        })
        .collect()
}

/// The settled `FETCH` call's state, from a `--format json` stream.
fn fetch_state(objects: &[Value]) -> Value {
    objects
        .iter()
        .filter(|object| object["type"] == "tool_use" && object["part"]["tool"] == FETCH)
        .map(|object| object["part"]["state"].clone())
        .next_back()
        .unwrap_or_else(|| panic!("the stream carries no settled {FETCH} call: {objects:?}"))
}

/// Everything the child traced into its data home's rolling log.
fn log(homes: &Homes) -> String {
    let directory = homes.data().join("ganja").join("log");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&directory)
        .map(|entries| entries.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect())
        .unwrap_or_default();
    files.sort();

    files.iter().filter_map(|file| std::fs::read_to_string(file).ok()).collect()
}

// ------------------------------------------------------------------- cases

/// **Criterion 37.** A local run screens, and says so on stderr alone: the
/// vendor is asked, the launch line names the source, the host and who it
/// covers, no stdout line carries it, and every stdout line is still one of
/// the stream's six objects.
#[test]
fn a_screening_run_discloses_on_stderr_and_its_json_stream_stays_whole() {
    let homes = Homes::new();
    let vendor = Vendor::start();
    let hub = Hub::start();
    let config = config(&homes, &hub);
    let script = fetching(&homes);

    let output = ganja(&homes, &script, &config, &vendor)
        .args(["run", "--auto", "--format", "json", "fetch the page"])
        .output()
        .expect("the binary runs");
    let (stdout, stderr) = streams(&output);

    assert!(output.status.success(), "the run exited {:?}\nstderr:\n{stderr}", output.status);
    assert_eq!(hub.calls(), 1, "the turn called the MCP tool once\nstderr:\n{stderr}");
    assert!(vendor.requests() >= 1, "the result was sent to be screened\nstderr:\n{stderr}");
    let disclosed = format!("{DISCLOSURE} mcp:hub via 127.0.0.1 {COVERED}");
    assert!(stderr.contains(&disclosed), "stderr carries {disclosed:?}:\n{stderr}");
    assert!(!stderr.contains("allow_private"), "nothing here allows private fetches:\n{stderr}");
    for line in stdout.lines() {
        assert!(!line.contains(DISCLOSURE), "a stdout line carries the disclosure: {line}");
        assert!(!line.contains(COVERED), "a stdout line carries the disclosure: {line}");
    }
    let objects = objects(&stdout);
    for object in &objects {
        let kind = object["type"].as_str().unwrap_or_default();
        assert!(TYPES.contains(&kind), "an object carried a type outside the set: {object}");
    }
    let state = fetch_state(&objects);
    let title = state["title"].as_str().unwrap_or_default();
    assert!(title.ends_with(SCREENED), "the screened call's title says so: {title:?}");
    assert!(
        state["metadata"]["screen"]["segments"]["issued"].as_u64().is_some_and(|n| n >= 1),
        "the call records what was sent: {state}"
    );
    assert!(stdout.contains(CLOSING), "the whole turn ran:\n{stdout}");
}

/// **Criterion 37**, the default format's half: the ` · screened` a call's
/// title gains is part of the account of the turn and is printed on stdout by
/// design, while the launch line stays on stderr.
#[test]
fn the_readable_stream_shows_the_screened_title_and_never_the_disclosure() {
    let homes = Homes::new();
    let vendor = Vendor::start();
    let hub = Hub::start();
    let config = config(&homes, &hub);
    let script = fetching(&homes);

    let output = ganja(&homes, &script, &config, &vendor)
        .args(["run", "--auto", "fetch the page"])
        .output()
        .expect("the binary runs");
    let (stdout, stderr) = streams(&output);

    assert!(output.status.success(), "the run exited {:?}\nstderr:\n{stderr}", output.status);
    assert!(vendor.requests() >= 1, "the result was sent to be screened\nstderr:\n{stderr}");
    assert!(stdout.contains(SCREENED), "the call's line carries the suffix:\n{stdout}");
    assert!(!stdout.contains(DISCLOSURE), "stdout carries the disclosure:\n{stdout}");
    assert!(stderr.contains(DISCLOSURE), "stderr carries the disclosure:\n{stderr}");
}

/// **Criterion 40.** A listed `mcp:nowhere` that no configured or plugin
/// server answers to is one warning naming it, and the launch goes on.
#[test]
fn a_listed_server_nobody_answers_to_is_one_warning_and_the_run_goes_on() {
    let homes = Homes::new();
    let vendor = Vendor::start();
    let config = homes.data().join("nowhere.toml");
    std::fs::write(&config, "[evaluate]\nscreen = [\"mcp:nowhere\"]\n")
        .expect("the config is writable");
    let script = quiet(&homes);

    let output = ganja(&homes, &script, &config, &vendor)
        .args(["run", "say something"])
        .output()
        .expect("the binary runs");
    let (stdout, stderr) = streams(&output);

    assert!(output.status.success(), "the run exited {:?}\nstderr:\n{stderr}", output.status);
    assert!(stdout.contains(CLOSING), "the turn ran:\n{stdout}");
    let traced = log(&homes);
    let warned: Vec<&str> = traced
        .lines()
        .filter(|line| line.contains("WARN") && line.contains("mcp:nowhere"))
        .collect();
    assert_eq!(warned.len(), 1, "exactly one warning names the entry:\n{traced}");
    assert!(stderr.contains(&format!("{DISCLOSURE} mcp:nowhere")), "{stderr}");
    assert_eq!(vendor.requests(), 0, "nothing was screened, so nothing was sent");
}

/// **Criteria 38 and 39.** Under the config and the settings a local run
/// screens with, a served engine sends nothing, and a run attached to it
/// assembles nothing and says nothing about screening.
///
/// Not vacuous: the served turn really calls the MCP tool — the double
/// counts it and the attached stream carries the settled call — and its
/// title carries no ` · screened`, because no text of it left the machine.
#[cfg(unix)]
#[test]
fn a_served_engine_screens_nothing_and_an_attached_run_discloses_nothing() {
    let homes = Homes::new();
    let vendor = Vendor::start();
    let hub = Hub::start();
    let config = config(&homes, &hub);
    let script = fetching(&homes);
    let config_dir = homes.data().join("config");
    let vendor_base = vendor.base.clone();
    let config_path = config.display().to_string();
    let shared = [
        ("GANJA_PROVIDER", "fake"),
        ("GANJA_DISABLE_MODELS_FETCH", "1"),
        ("GANJA_CONFIG", config_path.as_str()),
        ("TYPESAFE_API_KEY", KEY),
        ("TYPESAFE_BASE_URL", vendor_base.as_str()),
        // The level the `SessionStart` hook reads the connected line at,
        // whatever a developer exported: `ganja` removes the variable from
        // the local children, and a server is handed values, not removals.
        ("RUST_LOG", "info"),
    ];

    let mut server = served_child::spawn_with(
        homes.project(),
        homes.data(),
        &config_dir,
        homes.data(),
        &script,
        &shared,
    );
    let line = server.announcement("the server's address line");
    let url = line
        .trim()
        .rsplit_once(' ')
        .map(|(_, url)| url.to_owned())
        .unwrap_or_else(|| panic!("the address line ends with a URL: {line:?}"));

    let elsewhere = ganja_testkit::temp_dir();
    let client_data = ganja_testkit::temp_dir();
    let mut client = Command::new(env!("CARGO_BIN_EXE_ganja"));
    client
        .current_dir(elsewhere.path())
        .env("XDG_DATA_HOME", client_data.path())
        .env("XDG_CONFIG_HOME", client_data.path().join("config"))
        .env("HOME", client_data.path())
        .env_remove("TYPESAFE_DEFAULT_MODEL")
        .stdin(Stdio::null());
    for name in served_child::UNINHERITED {
        client.env_remove(name);
    }
    client.envs(shared);
    let output = client
        .args(["run", "--attach", &url, "--auto", "--format", "json", "fetch the page"])
        .output()
        .expect("the attached run runs");
    let (stdout, stderr) = streams(&output);
    assert!(
        output.status.success(),
        "the attached run exited {:?}\nstderr:\n{stderr}\n{}",
        output.status,
        server.state()
    );

    let killed =
        Command::new("kill").args(["-TERM", &server.id().to_string()]).status().expect("kill runs");
    assert!(killed.success(), "the signal was delivered");
    let status = server.wait_for_exit("the server to exit on SIGTERM");
    assert!(status.success(), "a clean shutdown exits 0: {status}");
    let served = server.diagnostics();

    assert_eq!(hub.calls(), 1, "the served turn called the MCP tool once\n{served}");
    let state = fetch_state(&objects(&stdout));
    assert_eq!(state["status"], "completed", "the call settled: {state}");
    let title = state["title"].as_str().unwrap_or_default();
    assert!(!title.ends_with(SCREENED), "nothing of the served call left: {title:?}");
    assert!(state["metadata"].get("screen").is_none(), "the served call was not screened: {state}");
    assert_eq!(vendor.requests(), 0, "a served engine sends nothing to be screened");
    for (who, said) in [("the attached run", &stderr), ("the server", &served)] {
        assert!(!said.contains(DISCLOSURE), "{who} disclosed a screen it does not run:\n{said}");
        assert!(!said.contains(COVERED), "{who} disclosed a screen it does not run:\n{said}");
    }
}
