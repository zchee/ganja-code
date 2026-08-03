//! Golden transcripts: ganja's agent loop against upstream opencode's.
//!
//! The port's claim is behavioural, not textual — that a session driven by the
//! same model does the same things. Nothing that reads only ganja can check
//! that, so this runs both agents: upstream opencode v1.18.11 out of
//! `.omc/reference/`, and this crate's [`Engine`] over
//! [`Registry::with_builtins`], and compares the one thing a user would notice
//! if the port drifted — the ordered list of tool calls each side actually
//! *executed*, and the arguments each ran with.
//!
//! # Why a replay server rather than a model
//!
//! A real model would make the comparison a judgement about what it felt like
//! saying that day. Instead both legs talk to a loopback endpoint speaking
//! OpenAI chat completions, which answers with a fixed script of streamed tool
//! calls; the scripts live in `tests/fixtures/golden/*.json`. What is under
//! test is therefore everything downstream of the wire: frame decoding,
//! argument assembly across chunk boundaries, the permission gate, tool
//! dispatch, and the order all of it happens in.
//!
//! The endpoint is a real socket serving real HTTP, for the reason
//! `tests/http.rs` gives: mocking the client would skip the request that is
//! actually built and the frames it is actually split into.
//!
//! # Which request gets which script
//!
//! Scripts are handed out to *agent* requests only — the ones carrying a
//! `tools` array. Upstream opens a session by asking the same endpoint for a
//! conversation title, with no tools offered; ganja has no such request. Were
//! responses handed out by arrival order, that one extra call would shift
//! upstream's whole script by one and the two legs would be compared against
//! different transcripts. Discriminating on `tools` is what keeps the two legs
//! reading the same script, and it covers any other toolless bookkeeping call
//! a future upstream adds.
//!
//! # Process-wide state
//!
//! One test, one binary, on purpose. The engine captures the working directory
//! at construction, so each task's engine has to be built while the process
//! sits in that task's directory, and `cargo test` runs the tests inside a
//! binary on parallel threads. `XDG_DATA_HOME` is redirected for the same
//! reason it is in `tests/credentials_env.rs`: nothing here may read or write
//! the real user's stored permissions or spilled tool output.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Command, Engine, Event, FinishReason, PartBody, PermissionReply, Permissions, Registry,
    ToolState, provider::OpenAiProvider,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};

/// Provider id the generated `opencode.json` declares, and the half of
/// `--model provider/model` upstream resolves against it.
const PROVIDER: &str = "golden";

/// Model id both legs ask for.
///
/// Deliberately free of `gpt-`. Upstream's registry swaps `edit` and `write`
/// for `apply_patch` whenever the model id looks like a current OpenAI model
/// (`tool/registry.ts`: `modelID.includes("gpt-") && !includes("oss") &&
/// !includes("gpt-4")`), so asking for `gpt-test` would offer upstream a tool
/// set this port does not have and the comparison would be about the fixture's
/// choice of name rather than about either agent.
const MODEL: &str = "golden-model";

/// The credential both legs authenticate with. The endpoint never checks it;
/// it exists because neither client will send a request without one.
const KEY: &str = "dummy-key";

/// What every argument value's leading directory is rewritten to before the
/// two legs are compared. Each leg runs in a temp directory of its own, so an
/// absolute path is equal only up to its root.
const ROOT: &str = "<CWD>";

/// How long a single upstream `run` is given. Generous: it is a cold `bun`
/// start plus an agent loop, and a timeout here should mean "hung", not "slow
/// machine".
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(180);

/// Environment variable pointing at the upstream checkout, for a tree that
/// vendored it somewhere else.
const CHECKOUT_ENV: &str = "GANJA_OPENCODE_DIR";

/// One tool call the endpoint scripts.
#[derive(Clone, Debug, Deserialize)]
struct Call {
    /// Tool the model names, in upstream's spelling.
    name: String,
    /// Arguments it streams, with upstream's camelCase keys.
    arguments: Value,
}

/// One model request's worth of answer.
#[derive(Clone, Debug, Deserialize)]
struct Step {
    /// Text streamed before the calls.
    #[serde(default)]
    text: String,
    /// Calls streamed after it. A step with none ends the turn.
    #[serde(default)]
    calls: Vec<Call>,
}

/// One canned task: a seeded directory, a prompt, and the answers.
#[derive(Debug, Deserialize)]
struct Task {
    /// What the user asks for. Both legs send it verbatim.
    prompt: String,
    /// Files the task's directory starts with, by relative path.
    #[serde(default)]
    seed: BTreeMap<String, String>,
    /// The scripted answers, in order.
    steps: Vec<Step>,
}

/// A tool call an agent executed: the tool's name and the arguments it ran
/// with, normalized.
type Executed = (String, Value);

/// A loopback endpoint speaking OpenAI chat completions from a script.
struct Replay {
    /// Base URL a client is pointed at, including the `/v1` both clients
    /// append `/chat/completions` to.
    url: String,
    /// How many agent requests it has answered, which is also how many script
    /// steps it has handed out.
    served: Arc<AtomicUsize>,
    /// Kept so the endpoint outlives the leg talking to it.
    _server: tokio::task::JoinHandle<()>,
}

/// Serves `steps` to agent requests, in order, until dropped.
async fn replay(steps: Vec<Step>) -> Replay {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let address: SocketAddr = listener
        .local_addr()
        .expect("a bound socket has an address");
    let served = Arc::new(AtomicUsize::new(0));

    let steps = Arc::new(steps);
    let cursor = Arc::clone(&served);
    let server = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            // Per connection, because a client is free to open one before it
            // has anything to say and answering them in lockstep would let
            // that stall the agent's next request.
            tokio::spawn(answer(socket, Arc::clone(&steps), Arc::clone(&cursor)));
        }
    });

    Replay {
        url: format!("http://{address}/v1"),
        served,
        _server: server,
    }
}

/// Reads one request off `socket` and writes the answer it earns.
async fn answer(mut socket: TcpStream, steps: Arc<Vec<Step>>, cursor: Arc<AtomicUsize>) {
    let Some((head, body)) = request(&mut socket).await else {
        return;
    };

    let response = if head.starts_with("POST") {
        let body: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
        // A request offering no tools is not the agent asking what to do next;
        // it is bookkeeping — upstream's title generator — and answering it out
        // of the script would shift every later step by one.
        let step = match body.get("tools").and_then(Value::as_array) {
            Some(tools) if !tools.is_empty() => {
                let index = cursor.fetch_add(1, Ordering::SeqCst);
                // Past the end the endpoint keeps ending the turn, so an agent
                // that asks one more time than the script expects produces a
                // length mismatch in the diff rather than a hang.
                steps.get(index).cloned().unwrap_or_else(|| Step {
                    text: "Done.".to_owned(),
                    calls: Vec::new(),
                })
            }
            _ => Step {
                text: "Golden Replay Session".to_owned(),
                calls: Vec::new(),
            },
        };

        http("200 OK", "text/event-stream", &sse(&step))
    } else {
        // Neither client asks for anything else, and a refusal that says so is
        // easier to read in a failure than a socket that simply closed.
        http("404 Not Found", "text/plain", "only POST is served here")
    };

    let _ = socket.write_all(&response).await;
    let _ = socket.flush().await;
}

/// Reads one whole HTTP request off `socket`: the head, then as many body
/// bytes as it declared. [`None`] when the connection died first.
async fn request(socket: &mut TcpStream) -> Option<(String, String)> {
    let mut seen = Vec::new();
    let mut chunk = [0_u8; 4096];

    loop {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => seen.extend_from_slice(&chunk[..read]),
        }

        let Some(end) = seen.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&seen[..end]).into_owned();
        let length: usize = head
            .lines()
            .find_map(|line| {
                let (field, value) = line.split_once(':')?;
                field
                    .trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())?
            })
            .unwrap_or_default();

        if seen.len() >= end + 4 + length {
            let body = String::from_utf8_lossy(&seen[end + 4..end + 4 + length]).into_owned();
            return Some((head, body));
        }
    }
}

/// A complete, close-delimited HTTP response.
fn http(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// One `data:` frame carrying a chat-completions chunk.
fn frame(delta: Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-golden",
            "object": "chat.completion.chunk",
            "created": 1_770_000_000_u64,
            "model": MODEL,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    )
}

/// `step` as the event stream a chat-completions client reads.
///
/// Every call's arguments are split across two frames, at a boundary chosen
/// without regard for the JSON, because assembling them back into one object
/// is the client's job and a fixture that never splits them would not ask it
/// to do that job.
fn sse(step: &Step) -> String {
    let mut out = String::new();

    if !step.text.is_empty() {
        out.push_str(&frame(
            json!({"role": "assistant", "content": step.text}),
            None,
        ));
    }

    for (index, call) in step.calls.iter().enumerate() {
        let arguments = call.arguments.to_string();
        let (head, tail) = arguments.split_at(split_point(&arguments));

        out.push_str(&frame(
            json!({"tool_calls": [{
                "index": index,
                "id": format!("call_{}", index + 1),
                "type": "function",
                "function": {"name": call.name, "arguments": head},
            }]}),
            None,
        ));
        out.push_str(&frame(
            json!({"tool_calls": [{
                "index": index,
                "function": {"arguments": tail},
            }]}),
            None,
        ));
    }

    let finish = if step.calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    out.push_str(&frame(json!({}), Some(finish)));
    out.push_str(&format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-golden",
            "object": "chat.completion.chunk",
            "created": 1_770_000_000_u64,
            "model": MODEL,
            "choices": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        })
    ));
    out.push_str("data: [DONE]\n\n");

    out
}

/// The middle of `text`, moved back to a character boundary.
fn split_point(text: &str) -> usize {
    let middle = text.len() / 2;

    (0..=middle)
        .rev()
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or_default()
}

/// Writes `task`'s seed files into `directory`.
fn seed(directory: &Path, task: &Task) {
    for (relative, contents) in &task.seed {
        let path = directory.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the seed's directories are creatable");
        }
        std::fs::write(&path, contents).expect("the seed is writable");
    }
}

/// Every tool call this crate's engine executed running `task`, in order.
///
/// The process's working directory has to be `directory` before this is called:
/// the engine captures it at construction and every relative path in the script
/// resolves against it.
async fn ganja(task: &Task) -> Vec<Executed> {
    let endpoint = replay(task.steps.clone()).await;
    let provider = OpenAiProvider::new(KEY)
        .expect("a client builds")
        .with_base_url(&endpoint.url);
    let engine = Engine::new(
        Arc::new(provider),
        MODEL,
        Arc::new(Registry::with_builtins()),
        // Not `Permissions::load`: the defaults are what upstream's `--auto`
        // faces too, and loading would reach for a store on disk.
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: task.prompt.clone(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let executed = collect(&engine, &mut events).await;
    assert!(
        endpoint.served.load(Ordering::SeqCst) > 0,
        "the engine never reached the replay endpoint"
    );

    executed
}

/// Drains `events` to the end of the turn, allowing every permission it asks
/// for and recording every call that reached execution.
///
/// A call is counted when it enters [`ToolState::Running`], which is the moment
/// the loop has parsed its arguments, cleared the permission gate and handed it
/// to the tool. A call that never gets that far never ran, and upstream would
/// not report it either.
async fn collect(engine: &Engine, events: &mut BoxStream<'static, Event>) -> Vec<Executed> {
    let mut executed = Vec::new();

    loop {
        let event = events
            .next()
            .await
            .expect("the turn should finish before the stream ends");

        match event {
            Event::PermissionRequested { id, .. } => {
                engine
                    .send(Command::ReplyPermission {
                        id,
                        reply: PermissionReply::Once,
                    })
                    .await
                    .expect("a reply is always accepted");
            }
            Event::PartUpdated { part, .. } => {
                if let PartBody::Tool {
                    tool,
                    state: ToolState::Running { input, .. },
                    ..
                } = part.body
                {
                    executed.push((tool, input));
                }
            }
            Event::MessageFinished { reason, error, .. } => {
                assert_eq!(
                    reason,
                    FinishReason::Completed,
                    "the scripted turn should complete: {error:?}"
                );
                return executed;
            }
            _ => {}
        }
    }
}

/// The upstream checkout, or the reason there is nothing to compare against.
///
/// A missing checkout or a missing `bun` is a failure rather than a skip: this
/// suite exists to hold the port to upstream's behaviour, and a green run that
/// silently compared ganja against nothing would be worth less than no run.
fn checkout() -> PathBuf {
    let directory = std::env::var_os(CHECKOUT_ENV).map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.omc/reference/opencode-v1.18.11")
                .to_owned()
        },
        PathBuf::from,
    );

    let entry = directory.join("packages/opencode/src/index.ts");
    assert!(
        entry.is_file(),
        "the upstream checkout is not where this expects it: {} does not exist. \
         Vendor opencode v1.18.11 there, or point {CHECKOUT_ENV} at it.",
        entry.display()
    );
    assert!(
        directory.join("node_modules").is_dir(),
        "the upstream checkout has no dependencies installed. Run `bun install` in {}.",
        directory.display()
    );

    directory
}

/// Every tool call upstream opencode executed running `task`, in order.
///
/// Upstream's own non-interactive contract is what is read here: `run
/// --format json` writes one JSON object per line, and a `tool_use` line
/// carries the tool part exactly as the session recorded it — the tool's name
/// and the input it ran with — at the moment the call finished. That is the
/// same set of calls its stored session would hold, taken from the interface
/// upstream documents rather than from its storage layout.
async fn upstream(directory: &Path, data_home: &Path, task: &Task) -> Vec<Executed> {
    let endpoint = replay(task.steps.clone()).await;
    std::fs::write(
        directory.join("opencode.json"),
        serde_json::to_string_pretty(&json!({
            "$schema": "https://opencode.ai/config.json",
            "provider": {
                PROVIDER: {
                    // The shape upstream documents for any endpoint that
                    // copied OpenAI's schema.
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "Golden Replay",
                    "options": {"baseURL": endpoint.url, "apiKey": KEY},
                    "models": {MODEL: {"name": "Golden Replay Model"}},
                },
            },
        }))
        .expect("the config is JSON by construction"),
    )
    .expect("the task directory is writable");

    let mut command = tokio::process::Command::new("bun");
    command
        .current_dir(directory)
        // Upstream reads its whole standard input whenever it is not a
        // terminal, and appends it to the prompt (`cli/cmd/run.ts`:
        // `process.stdin.isTTY ? undefined : await Bun.stdin.text()`). Under a
        // test harness that inherits an input nobody is going to close, that
        // read never returns and the run hangs before its first request.
        .stdin(std::process::Stdio::null())
        .args([
            "run",
            // Upstream's own `dev` script runs its entry point this way.
            "--conditions=browser",
        ])
        .arg(checkout().join("packages/opencode/src/index.ts"))
        .args([
            "run",
            // To standard error, and only ever read when a run failed. Upstream
            // reports a failed turn to standard output as an opaque "unexpected
            // server error" with a reference; without this a broken harness is
            // a failure with nothing in it to act on.
            "--print-logs",
            "--model",
            &format!("{PROVIDER}/{MODEL}"),
            // Answers every permission ask with "once", which is what the
            // ganja leg does by hand.
            "--auto",
            "--format",
            "json",
            &task.prompt,
        ]);
    // Upstream resolves the directory it works in from `PWD` in preference to
    // the process's own (`cli/cmd/run.ts`: `Filesystem.resolve(process.env.PWD
    // ?? process.cwd())`). A shell keeps the two in step; a spawned child that
    // only sets its working directory does not, and upstream would run against
    // whatever directory this test binary was launched from — reading that
    // project's config instead of the task's, and editing its files.
    for (variable, value) in [
        ("PWD", directory.to_path_buf()),
        ("XDG_DATA_HOME", data_home.join("data")),
        ("XDG_CONFIG_HOME", data_home.join("config")),
        ("XDG_CACHE_HOME", data_home.join("cache")),
        ("XDG_STATE_HOME", data_home.join("state")),
    ] {
        command.env(variable, value);
    }
    // Left to themselves these reach the network, which would make a run's
    // duration — and its tool set — depend on what a remote service answered.
    for variable in [
        "OPENCODE_DISABLE_AUTOUPDATE",
        "OPENCODE_DISABLE_MODELS_FETCH",
        "OPENCODE_DISABLE_LSP_DOWNLOAD",
        "OPENCODE_DISABLE_SHARE",
        "OPENCODE_DISABLE_DEFAULT_PLUGINS",
        "OPENCODE_DISABLE_AUTOCOMPACT",
    ] {
        command.env(variable, "1");
    }

    let output = tokio::time::timeout(UPSTREAM_TIMEOUT, command.output())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "upstream did not finish within {}s; it answered {} of {} scripted requests",
                UPSTREAM_TIMEOUT.as_secs(),
                endpoint.served.load(Ordering::SeqCst),
                task.steps.len()
            )
        })
        .unwrap_or_else(|error| {
            panic!("upstream could not be launched ({error}); is `bun` on PATH?")
        });

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "upstream exited {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        endpoint.served.load(Ordering::SeqCst) > 0,
        "upstream never reached the replay endpoint\n--- stdout ---\n{stdout}\n\
         --- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["type"] == "tool_use")
        .filter_map(|event| {
            let part = &event["part"];
            Some((
                part["tool"].as_str()?.to_owned(),
                part["state"]["input"].clone(),
            ))
        })
        .collect()
}

/// `value` with every mention of `root` replaced by [`ROOT`].
///
/// Each leg runs in a directory of its own, so a path argument is equal across
/// the two only once the directory it sits in stops being part of it. Both the
/// directory as given and its canonical form are rewritten, because macOS
/// hands out temporary directories under a symlink and a tool that resolves
/// its arguments reports the target.
fn normalize(value: &Value, roots: &[String]) -> Value {
    match value {
        Value::String(text) => {
            let mut text = text.clone();
            for root in roots {
                text = text.replace(root, ROOT);
            }
            Value::String(text)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| normalize(item, roots))
                .collect::<Vec<_>>(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, field)| (key.clone(), normalize(field, roots)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The forms `directory` can appear in inside a tool argument, longest first so
/// that rewriting one cannot leave a fragment of another behind.
fn roots(directory: &Path) -> Vec<String> {
    let mut roots = vec![directory.display().to_string()];
    if let Ok(canonical) = directory.canonicalize() {
        roots.push(canonical.display().to_string());
    }
    roots.sort_by_key(|root| std::cmp::Reverse(root.len()));
    roots.dedup();

    roots
}

/// One executed call per line, for an assertion someone has to read.
fn render(calls: &[Executed]) -> Vec<String> {
    calls
        .iter()
        .map(|(tool, input)| format!("{tool} {input}"))
        .collect()
}

/// The argument names each call ran with, which is the coarser check the
/// value comparison is allowed to assume has already passed.
fn keys(calls: &[Executed]) -> Vec<(String, Vec<String>)> {
    calls
        .iter()
        .map(|(tool, input)| {
            let mut names: Vec<String> = input
                .as_object()
                .map(|fields| fields.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();

            (tool.clone(), names)
        })
        .collect()
}

/// Loads the task in `tests/fixtures/golden/{name}.json`.
fn task(name: &str) -> Task {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/golden")
        .join(format!("{name}.json"));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the fixture {} is readable: {error}", path.display()));

    serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("the fixture {} parses: {error}", path.display()))
}

/// Runs `name` on both agents.
///
/// Reports what the fixture scripted, then what upstream executed, then what
/// this crate did — the third compared against the second, and both anchored
/// to the first so that two agents failing the same way cannot pass.
async fn differential(name: &str, home: &Path) -> (Vec<String>, Vec<Executed>, Vec<Executed>) {
    let fixture = task(name);
    let scripted: Vec<String> = fixture
        .steps
        .iter()
        .flat_map(|step| step.calls.iter().map(|call| call.name.clone()))
        .collect();

    // Two directories, because both legs write into theirs and a shared one
    // would let the first leg's edits decide what the second one finds.
    let ours = home.join(format!("{name}-ganja"));
    let theirs = home.join(format!("{name}-upstream"));
    for directory in [&ours, &theirs] {
        std::fs::create_dir_all(directory).expect("a task directory is creatable");
        seed(directory, &fixture);
    }

    let upstream_calls = upstream(&theirs, &home.join(format!("{name}-home")), &fixture).await;

    // Set last and read at construction: the engine resolves every relative
    // path in the script against the directory the process is in when it is
    // built.
    std::env::set_current_dir(&ours).expect("the task directory is enterable");
    let ganja_calls = ganja(&fixture).await;

    let normalize_all = |calls: Vec<Executed>, root: &Path| -> Vec<Executed> {
        let roots = roots(root);
        calls
            .into_iter()
            .map(|(tool, input)| (tool, normalize(&input, &roots)))
            .collect()
    };

    (
        scripted,
        normalize_all(upstream_calls, &theirs),
        normalize_all(ganja_calls, &ours),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn ganja_executes_the_same_tool_calls_as_upstream_opencode() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently. It is set before
    // anything constructs a `Permissions` or spills a truncated tool output,
    // both of which resolve under the XDG data directory.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", home.path().join("xdg"));
    }

    // Serial, not concurrent: the working directory is process-wide, and each
    // engine has to be built while the process sits in its own task's
    // directory.
    for name in ["task-read-edit", "task-write-run", "task-search-read"] {
        let (scripted, upstream_calls, ganja_calls) = differential(name, home.path()).await;

        let (upstream_names, ganja_names): (Vec<&str>, Vec<&str>) = (
            upstream_calls
                .iter()
                .map(|(tool, _)| tool.as_str())
                .collect(),
            ganja_calls.iter().map(|(tool, _)| tool.as_str()).collect(),
        );

        // Against the fixture first. Two agents that both stopped after the
        // first step would agree with each other perfectly, and a comparison
        // that only ever looked at the two of them would call that a pass.
        assert_eq!(
            upstream_names, scripted,
            "{name}: upstream did not run the whole script"
        );

        assert_eq!(
            ganja_names, upstream_names,
            "{name}: the two agents ran different tools, or ran them in a \
             different order"
        );

        assert_eq!(
            keys(&ganja_calls),
            keys(&upstream_calls),
            "{name}: the same tools ran with differently shaped arguments"
        );

        assert_eq!(
            render(&ganja_calls),
            render(&upstream_calls),
            "{name}: the same tools ran with different argument values"
        );
    }
}
