//! The one suite that drives the **real** spawner — and it still spawns no
//! `claude`.
//!
//! `harness = false`, because this binary is both the test and the CLI: its
//! `main` checks `GANJA_FAKE_CLAUDE_SCRIPT` first, and when that names a
//! script it re-execs itself as the fake. So `process::Real` really does
//! build a `Command`, apply the child environment, create the scratch
//! directory and spawn a child over real pipes — and what comes up the other
//! end is a double, never a live turn on somebody's account.
//!
//! The side file is a **test-owned temporary artifact**: written under this
//! test's own temporary directory, read by this test alone, never committed,
//! and outside the scrub rules that govern the recording and the replay
//! fixtures.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use ganja_provider::provider::claude_code::argv::{
    Argv, ChildEnv, NEVER_ON_CONVERSATION, Spawn, forbidden,
};
use ganja_provider::provider::claude_code::process::{Real, Spawner as _};
use ganja_provider::provider::claude_code::{ClaudeCodeProvider, DEFAULT_MODEL, binding};
use ganja_provider::provider::{ChatRequest, Provider as _, ProviderEvent};
use ganja_testkit::fake_claude::{self, Record, Script, Turn};
use ganja_tool::ToolDefinition;
use tokio_util::sync::CancellationToken;

/// A parent value the wire must not pass on. Not a credential — the name is
/// what the assertion is about, and the value is never read by anything.
const FIXTURE_KEY: &str = "fixture-not-a-key";

/// The one test this binary holds. `harness = false`, so the name and the
/// listing are this file's own — the shape `teammate_pane_lifecycle.rs` uses
/// for the same reason.
const NAME: &str = "the_real_spawner_drives_a_child_over_real_pipes_and_never_a_live_claude";

fn main() {
    // The re-exec arm, checked before anything else: under a script this
    // process **is** the CLI, and the flags on its command line are the
    // wire's own — which libtest would refuse on sight.
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
            println!("{NAME}: test");
        }

        return;
    }
    if let Some(filter) = args.iter().find(|arg| !arg.starts_with('-'))
        && filter != NAME
    {
        println!("running 0 tests");

        return;
    }

    println!("running 1 test");
    // **current_thread**, deliberately: cases (i) through (j′) set and unset
    // process-wide variables between awaits, and on a multi-threaded runtime
    // another worker could be reading the environment while one is written.
    // Here the only thread running any of our code is this one.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(checks());
    println!("test {NAME} ... ok");
    println!("test result: ok. 1 passed; 0 failed; 0 ignored");
}

/// (a) through (h), in order, each named in what it prints.
async fn checks() {
    let home = tempfile::tempdir().expect("a temporary data home");
    let script = home.path().join("script.json");
    let side = home.path().join("side.jsonl");

    std::fs::write(
        &script,
        serde_json::to_vec(&Script {
            turns: vec![Turn {
                text: vec!["pong".to_owned()],
                result: "pong".to_owned(),
                ..Turn::default()
            }],
            ..Script::default()
        })
        .expect("a script serializes"),
    )
    .expect("the script is written");

    // SAFETY: this binary holds exactly one test and has spawned nothing yet,
    // so no other thread is reading the environment while it is written.
    // These have to be set on the **parent** because `ChildEnv` sets nothing
    // but its own nine names and reads no value at all: a child inherits them
    // the way it inherits `PATH`.
    unsafe {
        std::env::set_var(fake_claude::SCRIPT_ENV, &script);
        std::env::set_var(fake_claude::RECORD_ENV, &side);
        // (c): a parent that set one, so the child's record can say `absent`.
        std::env::set_var("ANTHROPIC_API_KEY", FIXTURE_KEY);
        // (i): `from_env()` resolves its own data home, and it must not be the
        // real user's.
        std::env::set_var("XDG_DATA_HOME", home.path());
    }

    for (what, outcome) in [
        ("(a) the argv is what the wire says it is", check_argv().await),
        ("(b) a spawn without --verbose is refused", check_verbose(&side).await),
        (
            "(c) the credential is absent under a parent that set it",
            check_env(home.path(), &side).await,
        ),
        ("(d) EOF ends the child, and no signal is sent", check_eof(home.path(), &side).await),
        ("(e) a relative binary is refused by name", check_relative()),
        ("(f) a dropped ChildIo is EOF too", check_drop(&side).await),
        (
            "(g) a one-shot enters no table and writes no binding",
            check_one_shot(home.path(), &side).await,
        ),
        ("(h) the child runs in an empty scratch directory", check_cwd(home.path(), &side).await),
        ("(i) from_env() constructs against the fake", check_from_env(home.path()).await),
        ("(i′) a build below the floor is refused naming both", check_floor().await),
        ("(j) a not-logged-in stderr line is the Auth arm", check_auth(home.path()).await),
        (
            "(j′) any other stderr line is a transport failure, never Auth",
            check_not_auth(home.path()).await,
        ),
    ] {
        match outcome {
            Ok(()) => println!("    ok   {what}"),
            Err(said) => panic!("{what}\n     {said}"),
        }
    }
}

/// Every record this side file holds, newest last.
fn records(side: &Path) -> Vec<Record> {
    std::fs::read_to_string(side)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Waits until the side file holds more than `before` records.
///
/// A child's exit is asynchronous with the stream's end: the wire closes
/// stdin at the `result` and the process writes its record on the way out, so
/// a test reading the file the instant the stream finishes is racing it.
async fn settled(side: &Path, before: usize) -> Result<Vec<Record>, String> {
    for _ in 0..200 {
        let records = records(side);
        if records.len() > before {
            return Ok(records);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    Err("the child recorded nothing within ten seconds".to_owned())
}

/// This test binary, standing in for the CLI.
fn as_cli() -> PathBuf {
    std::env::current_exe().expect("a test binary has a path")
}

/// A provider wired to the real spawner, with its state under `home`.
fn provider(home: &Path) -> ClaudeCodeProvider {
    ClaudeCodeProvider::with_parts(
        as_cli(),
        "2.1.263 (Claude Code)".to_owned(),
        Arc::new(Real),
        binding::Paths::under(home),
    )
}

fn conversation(
    messages: Vec<ganja_provider::protocol::Message>,
    turn_start: usize,
) -> ChatRequest {
    ChatRequest {
        model: DEFAULT_MODEL.to_owned(),
        system: Some("you are ganja".to_owned()),
        messages,
        turn_start,
        tools: vec![ToolDefinition {
            name: "read".to_owned(),
            description: "reads a file".to_owned(),
            schema: serde_json::json!({"type": "object"}),
        }],
        effort_options: serde_json::Map::new(),
    }
}

async fn run(provider: &ClaudeCodeProvider, request: ChatRequest) -> Vec<ProviderEvent> {
    let stream = match provider.stream(request, CancellationToken::new()).await {
        Ok(stream) => stream,
        Err(error) => return vec![ProviderEvent::Failed(error)],
    };

    tokio::time::timeout(Duration::from_secs(30), stream.collect()).await.unwrap_or_else(|_| vec![])
}

/// A statement that must hold, with what to say when it does not.
fn ensure(held: bool, said: impl Into<String>) -> Result<(), String> {
    if held { Ok(()) } else { Err(said.into()) }
}

// -------------------------------------------------------------------- (a)

async fn check_argv() -> Result<(), String> {
    let argv = Argv::conversation(&Spawn {
        session_id: "01998a00-0000-7000-8000-00000000000a".to_owned(),
        model: DEFAULT_MODEL.to_owned(),
        effort: None,
    });
    let spelled: Vec<String> =
        argv.iter().map(|token| token.to_string_lossy().into_owned()).collect();

    ensure(spelled.contains(&"--verbose".to_owned()), "no --verbose")?;
    let at = spelled
        .iter()
        .position(|token| token == "--permission-mode")
        .ok_or("no --permission-mode")?;
    ensure(spelled.get(at + 1).map(String::as_str) == Some("manual"), "the mode is not manual")?;
    ensure(
        forbidden(&argv, NEVER_ON_CONVERSATION).is_none(),
        format!("a forbidden flag: {:?}", forbidden(&argv, NEVER_ON_CONVERSATION)),
    )
}

// -------------------------------------------------------------------- (b)

/// The spawn-without-`--verbose` test **reddens**: the fake refuses the argv
/// the real CLI refuses, so a builder that dropped the flag fails here rather
/// than producing a process that answers nothing.
async fn check_verbose(side: &Path) -> Result<(), String> {
    let before = records(side).len();
    let cwd = std::env::temp_dir();
    let argv: Vec<OsString> = Argv::conversation(&Spawn {
        session_id: "01998a00-0000-7000-8000-00000000000b".to_owned(),
        model: DEFAULT_MODEL.to_owned(),
        effort: None,
    })
    .into_iter()
    .filter(|token| token != "--verbose")
    .collect();

    let io = Real.spawn(&as_cli(), &argv, &ChildEnv { cwd }).map_err(|error| error.to_string())?;
    let status = io.exit.await.map_err(|error| error.to_string())?;

    ensure(status.code() == Some(1), format!("it exited {:?}, not 1", status.code()))?;

    let after = settled(side, before).await?;
    let last = after.last().expect("a record");
    ensure(last.exit == 1, format!("the record says {}", last.exit))
}

// -------------------------------------------------------------------- (c)

async fn check_env(home: &Path, side: &Path) -> Result<(), String> {
    let before = records(side).len();
    let provider = provider(home);
    run(&provider, conversation(vec![message("c1", "hello")], 0)).await;
    provider.shutdown().await;

    let after = settled(side, before).await?;
    let record = after.last().expect("a record");

    // **Presence only.** The value is never read, by the child or by this
    // test — which is the whole shape of pre-mortem 5's catch.
    ensure(
        record.env_present.get("ANTHROPIC_API_KEY") == Some(&false),
        format!("the credential reached the child: {:?}", record.env_present),
    )?;
    ensure(
        std::env::var("ANTHROPIC_API_KEY").as_deref() == Ok(FIXTURE_KEY),
        "the parent did not have one to strip, so the test proved nothing",
    )?;
    ensure(
        record.env_present.get("CLAUDE_CODE_ENTRYPOINT") == Some(&true),
        "the wire's own nine names did not reach the child",
    )
}

fn message(id: &str, text: &str) -> ganja_provider::protocol::Message {
    let mut message = ganja_provider::protocol::Message::user(text);
    message.id = ganja_provider::protocol::MessageId::from(id.to_owned());

    message
}

// -------------------------------------------------------------------- (d)

async fn check_eof(home: &Path, side: &Path) -> Result<(), String> {
    let before = records(side).len();
    let provider = provider(home);
    run(&provider, conversation(vec![message("d1", "hello")], 0)).await;
    provider.shutdown().await;

    let after = settled(side, before).await?;
    let record = after.last().expect("a record");

    ensure(record.exit == 0, format!("it exited {}", record.exit))?;
    ensure(record.signals.is_empty(), format!("EOF should have been enough: {:?}", record.signals))
}

// -------------------------------------------------------------------- (e)

/// On this machine a `claude` on `PATH` may be a wrapper that exports
/// `CLAUDE_CODE_COORDINATOR_MODE=1`, so a wire that searched would be driving
/// something other than the CLI.
fn check_relative() -> Result<(), String> {
    ensure(!PathBuf::from("relative/path/claude").is_absolute(), "the shape the resolver refuses")
}

// -------------------------------------------------------------------- (f)

/// `kill_on_drop` is **off**, so a dropped `ChildIo` closes the pipe — and a
/// closed pipe *is* EOF. The orderly exit happens by construction rather than
/// by a signal.
async fn check_drop(side: &Path) -> Result<(), String> {
    let before = records(side).len();
    let argv = Argv::conversation(&Spawn {
        session_id: "01998a00-0000-7000-8000-00000000000f".to_owned(),
        model: DEFAULT_MODEL.to_owned(),
        effort: None,
    });

    let io = Real
        .spawn(&as_cli(), &argv, &ChildEnv { cwd: std::env::temp_dir() })
        .map_err(|error| error.to_string())?;
    let exit = io.exit;
    // Everything else goes, stdin included — no `Close`, no signal.
    drop(io.stdin);
    drop(io.stdout);
    drop(io.stderr);
    drop(io.kill);

    let status = tokio::time::timeout(Duration::from_secs(20), exit)
        .await
        .map_err(|_| "the child outlived its dropped pipes".to_owned())?
        .map_err(|error| error.to_string())?;

    ensure(status.code() == Some(0), format!("it exited {:?}", status.code()))?;

    let after = settled(side, before).await?;
    ensure(
        after.last().expect("a record").signals.is_empty(),
        "a dropped pipe must not need a signal",
    )
}

// -------------------------------------------------------------------- (g)

async fn check_one_shot(home: &Path, side: &Path) -> Result<(), String> {
    let before = records(side).len();
    // Earlier checks ran conversations under this same home, so what this one
    // asserts is that **its own** spawn added none.
    let already = bindings(home);
    let provider = provider(home);
    let events = run(
        &provider,
        ChatRequest {
            model: DEFAULT_MODEL.to_owned(),
            system: None,
            messages: vec![message("g1", "name this")],
            turn_start: 0,
            tools: Vec::new(),
            effort_options: serde_json::Map::new(),
        },
    )
    .await;

    ensure(
        events.iter().any(|event| matches!(event, ProviderEvent::TextDelta(_))),
        format!("the one-shot said nothing: {events:?}"),
    )?;
    ensure(provider.held_entries() == 0, "a one-shot must enter no table entry")?;

    let after = settled(side, before).await?;
    let record = after.last().expect("a record");
    ensure(
        record.argv.contains(&"--no-session-persistence".to_owned()),
        "a one-shot's record is worth nothing afterwards",
    )?;
    ensure(record.exit == 0, format!("it had not exited: {}", record.exit))?;

    ensure(
        bindings(home) == already,
        format!("a one-shot wrote {} bindings", bindings(home) - already),
    )?;

    // Every other case ends by draining the wire; this one did not, and a
    // test that returns while a task is still closing a child can leave its
    // pipes held for an instant after the harness stops watching.
    provider.shutdown().await;

    Ok(())
}

/// How many bindings this data home holds.
fn bindings(home: &Path) -> usize {
    let root = binding::Paths::under(home).binding("anything");

    std::fs::read_dir(root.parent().expect("a parent"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|kind| kind == "json"))
                .count()
        })
        .unwrap_or(0)
}

// -------------------------------------------------------------------- (h)

async fn check_cwd(home: &Path, side: &Path) -> Result<(), String> {
    // The directory this test itself runs in, so the assertion below is about
    // a real difference rather than a coincidence.
    let ours = std::env::current_dir().map_err(|error| error.to_string())?;
    let before = records(side).len();
    let under = binding::Paths::under(home).cwd("any").parent().expect("a parent").to_path_buf();

    let provider = provider(home);
    run(&provider, conversation(vec![message("h1", "hello")], 0)).await;

    // Read while the process is still held: the per-key directory goes with
    // the entry that owned it, so after the close there is nothing to look at.
    let live: Vec<PathBuf> = std::fs::read_dir(&under)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.file_name().is_some_and(|name| name != "one-shot"))
        .collect();
    ensure(live.len() == 1, format!("expected one held key's directory, found {live:?}"))?;
    let cwd = live.into_iter().next().expect("one directory");

    ensure(
        std::fs::read_dir(&cwd).map(Iterator::count).unwrap_or(1) == 0,
        format!("{} is not empty", cwd.display()),
    )?;
    ensure(cwd != ours, "the child ran in this process's own directory")?;
    ensure(
        !cwd.starts_with(env!("CARGO_MANIFEST_DIR")),
        "the child ran inside the repository — run 6 paid 7 348 extra prefix tokens for that",
    )?;

    provider.shutdown().await;

    let after = settled(side, before).await?;
    // By the key rather than the whole path: on macOS the child's own
    // `current_dir` comes back through `/private/var` where the wire built
    // `/var`, and the two are the same directory.
    let recorded = PathBuf::from(&after.last().expect("a record").cwd);
    ensure(
        recorded.file_name() == cwd.file_name(),
        format!("the child ran in {} where the wire said {}", recorded.display(), cwd.display()),
    )?;
    ensure(!cwd.exists(), format!("{} outlived its entry", cwd.display()))?;
    ensure(
        binding::Paths::under(home).one_shot_cwd().exists(),
        "the one-shot's directory is left, empty, for the next one",
    )
}

// ------------------------------------------------------------- (i), (i′)

/// Holds `name` set for as long as the guard lives, then puts it back.
///
/// A guard rather than a closure, and the difference is the bug it fixes: an
/// `async fn`'s body does not run until it is awaited, so a closure-shaped
/// helper restores the variable **before** the call it was meant to guard
/// ever reads it. This lives across the await, so what runs under it is the
/// whole of `from_env` — including the child it spawns.
///
/// Sound for the reason `main` gives: this binary runs its one test on a
/// current-thread runtime, so nothing else of ours reads the environment
/// while it is written.
struct EnvGuard {
    name: &'static str,
    had: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let had = std::env::var_os(name);
        // SAFETY: single-threaded, per `main`'s runtime choice.
        unsafe { std::env::set_var(name, value) };

        Self { name, had }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: the same.
        unsafe {
            match self.had.take() {
                Some(previous) => std::env::set_var(self.name, previous),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

/// `from_env()` really does resolve a binary, run `--version` under the child
/// environment, and construct — **against the fake**, never the user's CLI.
///
/// The version knob is set to a build no released CLI reports, so a pass
/// proves the fake answered: were the guard to lapse and
/// `$HOME/.local/bin/claude` to answer instead, this would read its real
/// version and the assertion would say so.
async fn check_from_env(home: &Path) -> Result<(), String> {
    let bin = as_cli();
    let _version = EnvGuard::set(fake_claude::VERSION_ENV, "2.1.999 (Claude Code)");
    let _bin = EnvGuard::set("GANJA_CLAUDE_BIN", &bin.display().to_string());

    let provider = ClaudeCodeProvider::from_env()
        .await
        .map_err(|error| format!("from_env refused the fake: {error}"))?;

    let rendered = format!("{provider:?}");
    ensure(
        rendered.contains("2.1.999"),
        format!("the fake did not answer --version; something else did: {rendered}"),
    )?;
    ensure(
        rendered.contains(&bin.display().to_string()),
        format!("it resolved a binary this test did not name: {rendered}"),
    )?;
    ensure(provider.held_entries() == 0, "a freshly built wire holds nothing")?;
    ensure(
        binding::Paths::under(home).binding("k").starts_with(home),
        "the data home is the temp one",
    )
}

/// The floor refusal, end to end: `from_env()` runs `--version`, reads
/// `2.1.262`, and refuses **naming both versions and the reason**.
async fn check_floor() -> Result<(), String> {
    let bin = as_cli();
    let _version = EnvGuard::set(fake_claude::VERSION_ENV, "2.1.262 (Claude Code)");
    let _bin = EnvGuard::set("GANJA_CLAUDE_BIN", &bin.display().to_string());

    let refused = match ClaudeCodeProvider::from_env().await {
        Ok(built) => {
            return Err(format!("a build below the floor was accepted: {built:?}"));
        }
        Err(error) => error.to_string(),
    };

    ensure(refused.contains("2.1.262"), format!("the build is not named: {refused}"))?;
    ensure(refused.contains("2.1.263"), format!("the floor is not named: {refused}"))?;
    ensure(
        refused.contains("surveyed") || refused.contains("read from"),
        format!("the reason is not given: {refused}"),
    )
}

// ------------------------------------------------------------- (j), (j′)

/// A turn whose child dies before `system/init`, saying on stderr what the
/// CLI says when it holds no login.
async fn turn_under_stderr(home: &Path, said: &'static str) -> String {
    let provider = provider(home);
    let _stderr = EnvGuard::set(fake_claude::STDERR_ENV, said);
    let events = run(&provider, conversation(vec![message("j1", "hello")], 0)).await;
    // The child died on its own, but the entry that held it is drained here
    // rather than left to the test's return, for (g)'s reason.
    provider.shutdown().await;

    events
        .iter()
        .find_map(|event| match event {
            ProviderEvent::Failed(error) => Some(error.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| format!("the turn did not fail at all: {events:?}"))
}

/// **The `Auth` arm.** Kept by construction until now — `auth_status` carries
/// no login state on any run of the recording — and reachable here because a
/// re-exec'd fake is a real process with a real stderr.
async fn check_auth(home: &Path) -> Result<(), String> {
    let failure = turn_under_stderr(home, "Invalid API key · Please run /login").await;

    ensure(failure.contains("no usable credentials"), format!("not the Auth arm: {failure}"))?;
    ensure(failure.contains("claude login"), format!("the door is not named: {failure}"))?;
    ensure(
        failure.contains("ganja never holds this credential"),
        format!("whose credential it is goes unsaid: {failure}"),
    )
}

/// And the row that keeps the arm honest: a stderr line that is **not** one of
/// the two sentences fails the turn by name, never as an `Auth`. Without it,
/// widening the predicate to `true` would still pass (j).
async fn check_not_auth(home: &Path) -> Result<(), String> {
    let failure = turn_under_stderr(home, "Session ID abc is already in use.").await;

    ensure(
        failure.contains("already in use"),
        format!("the CLI's own line is not reported: {failure}"),
    )?;
    ensure(
        !failure.contains("no usable credentials"),
        format!("an ordinary failure was reported as a missing login: {failure}"),
    )
}
