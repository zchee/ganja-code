//! A CLI that is not one: the fake `codex`/`agy` the shim core is driven
//! against.
//!
//! Not a test binary of its own — cargo does not discover `tests/*/mod.rs` as
//! one — but a module the shim binaries declare, so the scripts and the drivers
//! over them are written once. The `pane_support` shape, for the same reason:
//! several suites need one fixture and none of them should own it.
//!
//! # Why a script rather than a mock
//!
//! Every claim W2 makes is about a **process**: that its argv carries flags and
//! never a prompt, that its environment is exactly an enumeration, that its
//! process group dies with it, that a deadline reaches it. A double behind the
//! [`Driver`] trait would assert none of those, because none of them happen on
//! this side of a `fork`. So the fakes are two POSIX shell scripts on a
//! `PATH` the test owns, and everything they were handed is recorded to a file
//! the test reads back.
//!
//! # Everything travels in argv, and that is deliberate
//!
//! The log path and the behaviour switch are **flags**, not environment
//! variables, and not because flags are prettier: a test that had to export
//! `FAKE_LOG` would be mutating process-wide state, which in this tree costs a
//! test binary of its own. Flags cost nothing and exercise the real rule at the
//! same time — argv is for flags, and the prompt is the one thing that must
//! never be in it.

// Each shim binary compiles this module separately and uses a different part of
// it, so the unused half differs per binary and a targeted allow cannot name
// it.
#![allow(dead_code)]

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use ganja_core::{
    Backends, Storage, Teammates,
    teammate::{
        SpawnSpec, TeammateRegistry,
        claude::ClaudePane,
        pane::GanjaPane,
        shim::{Door, Driver, Read, Reply, Shape, ShimBackend, Turn},
    },
};
use ganja_protocol::team::MemberBackend;
use ganja_team::{ShimCli, TeamName, TeamsRoot};

/// The lead's session, and therefore the team's name.
pub const SESSION_ID: &str = "01998ad0-0000-7000-8000-000000000000";

/// The flag the fakes take their log path on.
pub const LOG: &str = "--fake-log";

/// The flag the fakes take their behaviour on.
pub const MODE: &str = "--fake-mode";

/// The posture flag the fakes read, standing in for whatever D508(a) pins per
/// CLI — this module asserts the *mechanism*, and each real posture arrives
/// with its own wave.
pub const SANDBOX: &str = "--sandbox";

/// The value of it that makes the fake refuse a would-write instruction.
pub const READ_ONLY: &str = "read-only";

/// How a fake behaves this turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Answer the turn.
    Answer,
    /// Exit non-zero with a sentence on stderr.
    Fail,
    /// Exit cleanly having written something no driver can read.
    Garbage,
    /// Never answer, so the per-turn deadline is the only thing that ends it.
    Hang,
    /// Refuse to start at all, the way a vendor's own startup gate does.
    Refuse,
}

impl Mode {
    /// What the script switches on.
    pub const fn word(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Fail => "fail",
            Self::Garbage => "garbage",
            Self::Hang => "hang",
            Self::Refuse => "refuse",
        }
    }
}

/// A `PATH` of a test's own, holding the two fakes.
#[derive(Debug)]
pub struct FakeCli {
    directory: tempfile::TempDir,
    /// Where everything the fakes were handed is appended.
    pub log: PathBuf,
}

impl FakeCli {
    /// Writes both scripts and the log they append to.
    pub fn install() -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::Builder::new()
            .prefix("fake-cli-")
            .tempdir()
            .expect("a directory for the fake CLIs");
        let log = directory.path().join("received.log");
        std::fs::write(&log, "").expect("the fake CLI log");

        for (name, body) in [
            ("fake-per-message", PER_MESSAGE),
            ("fake-resident", RESIDENT),
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, body).expect("a fake CLI script");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("a fake CLI is executable");
        }

        Self { directory, log }
    }

    /// The search path a [`ShimBackend`] is pointed at, and the child's own
    /// `PATH`.
    ///
    /// `/usr/bin:/bin` travels beside the fakes because the scripts are `sh`
    /// scripts: the kernel needs the interpreter, and a `PATH` holding only the
    /// fixture would be testing that `sh` cannot be found.
    pub fn path(&self) -> OsString {
        OsString::from(format!("{}:/usr/bin:/bin", self.directory.path().display()))
    }

    /// Everything the fakes have been handed so far, one record per line.
    pub fn received(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The records of one kind — `argv`, `stdin`, `line` or `file`.
    pub fn records(&self, kind: &str) -> Vec<String> {
        let prefix = format!("{kind}:");
        self.received()
            .into_iter()
            .filter_map(|line| {
                line.strip_prefix(&prefix)
                    .map(std::borrow::ToOwned::to_owned)
            })
            .collect()
    }

    /// Whether anything the fakes ever saw contains `needle` — the assertion
    /// behind "no frame JSON and no prompt ever reached the CLI".
    pub fn ever_saw(&self, needle: &str) -> bool {
        self.received().iter().any(|line| line.contains(needle))
    }

    /// Where the scripts live, for a test that wants to name one.
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }
}

/// A per-message fake driver: one child per inbox message, prompt in a `0600`
/// file whose path the argv names.
#[derive(Debug)]
pub struct PerMessage {
    log: PathBuf,
    mode: Mode,
}

impl PerMessage {
    /// The driver over `cli`'s log, behaving as `mode` says.
    pub fn new(log: &Path, mode: Mode) -> Self {
        Self {
            log: log.to_path_buf(),
            mode,
        }
    }
}

impl Driver for PerMessage {
    fn cli(&self) -> ShimCli {
        ShimCli::Codex
    }

    fn backend(&self) -> MemberBackend {
        MemberBackend::Codex
    }

    fn binary(&self) -> &str {
        "fake-per-message"
    }

    fn shape(&self) -> Shape {
        Shape::PerMessage
    }

    fn door(&self) -> Door {
        Door::File
    }

    fn argv(&self, turn: &Turn<'_>) -> Vec<OsString> {
        let mut argv = vec![
            OsString::from(LOG),
            OsString::from(&self.log),
            OsString::from(MODE),
            OsString::from(self.mode.word()),
            // The posture, on **every** turn rather than only the first, which
            // is the shape D508(a) pins per CLI.
            OsString::from(SANDBOX),
            OsString::from(READ_ONLY),
        ];
        if let Some(session) = turn.session {
            argv.push(OsString::from("--resume"));
            argv.push(OsString::from(session));
        }
        if let Some(prompt) = turn.prompt {
            argv.push(OsString::from("--prompt-file"));
            argv.push(OsString::from(prompt));
        }

        argv
    }

    fn reply(&self, stdout: &str) -> Result<Reply, String> {
        decode(stdout)
    }
}

/// A resident fake driver: one child for the member's life, one JSON line per
/// turn on its stdin.
#[derive(Debug)]
pub struct Resident {
    log: PathBuf,
    mode: Mode,
}

impl Resident {
    /// The driver over `cli`'s log, behaving as `mode` says.
    pub fn new(log: &Path, mode: Mode) -> Self {
        Self {
            log: log.to_path_buf(),
            mode,
        }
    }
}

impl Driver for Resident {
    fn cli(&self) -> ShimCli {
        ShimCli::Agy
    }

    fn backend(&self) -> MemberBackend {
        MemberBackend::Agy
    }

    fn binary(&self) -> &str {
        "fake-resident"
    }

    fn shape(&self) -> Shape {
        Shape::Resident
    }

    fn door(&self) -> Door {
        Door::Stdin
    }

    fn argv(&self, _turn: &Turn<'_>) -> Vec<OsString> {
        vec![
            OsString::from(LOG),
            OsString::from(&self.log),
            OsString::from(MODE),
            OsString::from(self.mode.word()),
            OsString::from(SANDBOX),
            OsString::from(READ_ONLY),
        ]
    }

    fn line(&self, turn: &Turn<'_>) -> Result<String, String> {
        serde_json::to_string(&serde_json::json!({ "prompt": turn.text }))
            .map_err(|error| error.to_string())
    }

    fn reply(&self, stdout: &str) -> Result<Reply, String> {
        decode(stdout)
    }

    fn read(&self, line: &str) -> Read {
        match decode(line) {
            Ok(reply) => Read::Done(reply),
            // Anything that is not this turn's answer is another line of the
            // vendor's own chatter, which a real driver skips too.
            Err(reason) if line.contains("\"done\"") => Read::Refused(reason),
            Err(_) => Read::Ignored,
        }
    }
}

/// What both fakes answer a turn with, decoded.
fn decode(text: &str) -> Result<Reply, String> {
    let value: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|error| format!("the answer was not this fake's shape: {error}"))?;
    let messages: Vec<String> = value
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "the answer named no messages".to_owned())?
        .iter()
        .filter_map(|message| message.as_str().map(str::to_owned))
        .collect();

    Ok(Reply {
        messages,
        session: value
            .get("session")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

/// The lead's team over `home`, with the fake installed as one shim backend.
///
/// Everything else is production's own value, deliberately: a test that
/// replaced the pane backends too would be asserting against a fixture's
/// opinion of what a refusal says.
pub fn lead(
    home: &Path,
    project: &Path,
    driver: Arc<dyn Driver>,
    path: OsString,
) -> (Arc<TeammateRegistry>, Arc<Teammates>) {
    lead_with_timeout(home, project, driver, path, None)
}

/// [`lead`], with the one config key that moves a shim's per-turn deadline.
pub fn lead_with_timeout(
    home: &Path,
    project: &Path,
    driver: Arc<dyn Driver>,
    path: OsString,
    timeout: Option<Duration>,
) -> (Arc<TeammateRegistry>, Arc<Teammates>) {
    let registry = Arc::new(
        TeammateRegistry::for_session(home, SESSION_ID, project)
            .with_shim_turn_timeout(timeout)
            // Never the real `/tmp/ganja-<uid>`: a suite that spawned against it
            // would leave one `.shims` file per test process in the directory
            // `ganja sessions --live` walks, each naming an owner that is gone
            // by the time anybody looks.
            .with_shim_directory(home.join("shims")),
    );
    let backend = driver.backend();
    let fake: Arc<dyn ganja_core::teammate::TeammateBackend> =
        Arc::new(ShimBackend::new(driver).searching(path));
    let storage = Storage::open(project.join("storage"));
    let mut backends = Backends {
        in_process: Arc::new(ganja_core::teammate::InProcess::new(
            Arc::new(ganja_core::provider::FakeProvider::new(
                "on it",
                Duration::ZERO,
            )),
            Arc::new(ganja_core::tool::Registry::new(Vec::new())),
            storage,
            |_: &SpawnSpec| ganja_core::permission::Permissions::default(),
        )),
        pane: Arc::new(GanjaPane),
        claude: Arc::new(ClaudePane),
        codex: Arc::new(ganja_core::teammate::Unbuilt::new(MemberBackend::Codex)),
        agy: Arc::new(ganja_core::teammate::Unbuilt::new(MemberBackend::Agy)),
        grok: Arc::new(ganja_core::teammate::Unbuilt::new(MemberBackend::Grok)),
    };
    match backend {
        MemberBackend::Codex => backends.codex = fake,
        MemberBackend::Agy => backends.agy = fake,
        MemberBackend::Grok => backends.grok = fake,
        other => panic!("{other:?} is not a shim backend"),
    }
    let door = Arc::new(Teammates::new(Arc::clone(&registry), backends));

    (registry, door)
}

/// Where this team's documents live.
pub fn team_of(registry: &TeammateRegistry) -> (TeamsRoot, TeamName) {
    (registry.root().clone(), registry.team().clone())
}

/// Waits until `check` answers, or gives up after `limit`.
///
/// Polled rather than slept through, so a test that would have passed in ten
/// milliseconds costs ten rather than the whole budget.
pub async fn until(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    check()
}

/// Whether `pid` is still a live process, asked the cheap way.
pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 sends nothing; it only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A fake `codex`, shaped like the real one (**W3**).
///
/// Distinct from [`FakeCli`] in the one way that matters: it is driven by the
/// **real** [`ganja_core::teammate::codex::Codex`] driver, so it cannot be told
/// where to log through a flag of its own — the real argv carries codex's flags
/// and nothing else. The log path and the behaviour are therefore baked into
/// the script at install time, which is the same trick under a different roof:
/// a fixture written per test into a directory of its own.
///
/// It answers two doors, because the driver knocks on two: `codex login status`
/// for the spawn pre-check, and `codex exec [resume <id>]` for a turn.
#[derive(Debug)]
pub struct FakeCodex {
    directory: tempfile::TempDir,
    /// Where everything it was handed is appended.
    pub log: PathBuf,
}

impl FakeCodex {
    /// A fake that is logged in and behaves as `mode` says.
    pub fn install(mode: Mode) -> Self {
        Self::write(mode.word())
    }

    /// A fake whose `login status` answers non-zero — **AC-10**'s other arm.
    pub fn logged_out() -> Self {
        Self::write("logged-out")
    }

    fn write(mode: &str) -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::Builder::new()
            .prefix("fake-codex-")
            .tempdir()
            .expect("a directory for the fake codex");
        let log = directory.path().join("received.log");
        std::fs::write(&log, "").expect("the fake codex log");
        let path = directory.path().join("codex");
        std::fs::write(
            &path,
            CODEX
                .replace("@LOG@", &log.display().to_string())
                .replace("@MODE@", mode),
        )
        .expect("the fake codex script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the fake codex is executable");

        Self { directory, log }
    }

    /// The search path a [`ShimBackend`] is pointed at, and the child's `PATH`.
    pub fn path(&self) -> OsString {
        OsString::from(format!("{}:/usr/bin:/bin", self.directory.path().display()))
    }

    /// Everything it has been handed so far, one record per line.
    pub fn received(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The records of one kind — `argv`, `env` or `stdin`.
    pub fn records(&self, kind: &str) -> Vec<String> {
        let prefix = format!("{kind}:");
        self.received()
            .into_iter()
            .filter_map(|line| {
                line.strip_prefix(&prefix)
                    .map(std::borrow::ToOwned::to_owned)
            })
            .collect()
    }

    /// The argv records of turns only, with the pre-check's own left out.
    ///
    /// `codex login status` is an invocation this fixture records like any
    /// other, and a posture assertion that counted it would be asserting about
    /// a command that takes no turn.
    pub fn turns(&self) -> Vec<String> {
        self.records("argv")
            .into_iter()
            .filter(|argv| !argv.starts_with("login "))
            .collect()
    }

    /// Whether anything it ever saw contains `needle`.
    pub fn ever_saw(&self, needle: &str) -> bool {
        self.received().iter().any(|line| line.contains(needle))
    }

    /// Where the script lives, for a test that wants to name it.
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }
}

/// The fake `codex`.
///
/// Prints the JSONL a probed `codex-cli 0.149.0-alpha.1` actually printed —
/// `thread.started` carrying `thread_id`, `item.completed` wrapping an item
/// whose own `type` is the discriminator, `turn.completed` with usage — rather
/// than a shape invented here, which is what makes the parser test and this
/// fixture agree about the same vendor.
///
/// Its thread id is minted from its own pid, so two members' first turns cannot
/// come back with one id (**AC-19**), and a `resume` echoes the id it was given.
const CODEX: &str = r#"#!/bin/sh
log='@LOG@'
mode='@MODE@'
args="$*"
printf 'argv:%s\n' "$args" >> "$log"
printf 'env:%s\n' "$(env | cut -d= -f1 | sort | tr '\n' ' ')" >> "$log"

if [ "$1" = "login" ]; then
  if [ "$mode" = "logged-out" ]; then
    printf 'Not logged in\n' >&2
    exit 1
  fi
  printf 'Logged in using ChatGPT\n'
  exit 0
fi

resume=""
if [ "$1" = "exec" ] && [ "$2" = "resume" ]; then
  resume="$3"
fi

prompt=$(cat)
printf 'stdin:%s\n' "$(printf '%s' "$prompt" | tr '\n' ' ')" >> "$log"

case "$mode" in
  refuse) printf 'error: codex refuses to start here\n' >&2; exit 1 ;;
  fail) printf 'error: the fake was not logged in\n' >&2; exit 3 ;;
  garbage) printf 'this is not the shape any driver reads\n'; exit 0 ;;
  hang) sleep 300; exit 0 ;;
esac

if [ -n "$resume" ]; then
  id="$resume"
else
  id="thread-$$"
fi
printf '{"type":"thread.started","thread_id":"%s"}\n' "$id"
printf '{"type":"turn.started"}\n'
printf '{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"thinking is not mail"}}\n'
case "$args:$prompt" in
  *'sandbox_mode="read-only"'*WRITE*)
    printf '{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"refused: the sandbox is read-only"}}\n'
    printf '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}\n'
    exit 0 ;;
esac
printf '{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"starting on it"}}\n'
printf '{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"done"}}\n'
printf '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}\n'
"#;

/// The per-message fake.
///
/// Records its whole argv, then the prompt file's contents where it was given
/// one — which is what makes "the prompt reached the CLI, and it reached it
/// through a file" one assertion rather than an inference.
const PER_MESSAGE: &str = r#"#!/bin/sh
log=""
mode="answer"
sandbox=""
prompt=""
args="$*"
while [ $# -gt 0 ]; do
  case "$1" in
    --fake-log) log="$2"; shift 2 ;;
    --fake-mode) mode="$2"; shift 2 ;;
    --sandbox) sandbox="$2"; shift 2 ;;
    --prompt-file) prompt="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf 'argv:%s\n' "$args" >> "$log"
printf 'env:%s\n' "$(env | cut -d= -f1 | sort | tr '\n' ' ')" >> "$log"
text=""
if [ -n "$prompt" ]; then
  text=$(tr '\n' ' ' < "$prompt")
  printf 'file:%s\n' "$text" >> "$log"
fi
case "$mode" in
  refuse) printf 'error: this fake refuses to start\n' >&2; exit 1 ;;
  fail) printf 'error: the fake was not logged in\n' >&2; exit 3 ;;
  garbage) printf 'this is not the shape any driver reads\n'; exit 0 ;;
  hang) sleep 300; exit 0 ;;
esac
case "$sandbox:$text" in
  read-only:*WRITE*)
    printf '{"session":"fake-session-1","messages":["refused: the sandbox is read-only"]}\n'
    exit 0 ;;
esac
printf '{"session":"fake-session-1","messages":["answered"]}\n'
"#;

/// The resident fake: one process, one line in, one line out.
const RESIDENT: &str = r#"#!/bin/sh
log=""
mode="answer"
sandbox=""
args="$*"
while [ $# -gt 0 ]; do
  case "$1" in
    --fake-log) log="$2"; shift 2 ;;
    --fake-mode) mode="$2"; shift 2 ;;
    --sandbox) sandbox="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf 'argv:%s\n' "$args" >> "$log"
printf 'env:%s\n' "$(env | cut -d= -f1 | sort | tr '\n' ' ')" >> "$log"
if [ "$mode" = "refuse" ]; then
  printf 'error: this fake refuses to start\n' >&2
  exit 1
fi
while IFS= read -r line; do
  printf 'line:%s\n' "$line" >> "$log"
  case "$mode" in
    hang) sleep 300 ;;
    garbage) printf '{"done":true,"nothing":"here"}\n' ;;
    *) printf '{"done":true,"session":"fake-session-1","messages":["answered"]}\n' ;;
  esac
done
"#;
