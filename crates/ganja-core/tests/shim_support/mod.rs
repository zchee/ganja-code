//! A CLI that is not one: the fakes the shim core and the real drivers are
//! driven against.
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
//! this side of a `fork`. So the fakes are POSIX shell scripts on a `PATH` the
//! test owns, and everything they were handed is recorded to a file the test
//! reads back.
//!
//! Two families live here. The **shape** fakes ([`FakeCli`]) are driven by the
//! fixture drivers below and assert the shim *mechanism*, one per [`Shape`].
//! The **per-CLI** fakes ([`FakeCodex`], [`FakeGrok`]) are driven by the real
//! drivers and assert those vendors' own flags and wire shapes. All of them
//! share [`Fake`], which is the directory, the log and the five readers over
//! it.
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

/// The posture flag the shape fakes read, standing in for whatever D508(a) pins
/// per CLI — those fakes assert the *mechanism*, and each real posture is
/// asserted against its own vendor's fake instead.
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
    /// Answer part of the turn and then report that it stopped — the shape a
    /// CLI's own account of an unfinished turn takes.
    Stopped,
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
            Self::Stopped => "stopped",
        }
    }
}

/// A `PATH` of a test's own, holding whichever fakes were installed on it, and
/// the log everything they were handed is appended to.
///
/// **One fixture for every fake CLI in this tree** (W5's hoist): the five
/// accessors below were duplicated verbatim between the shape fixture and the
/// per-CLI ones, which is one place for the two to drift about what a record
/// line looks like. What stays per CLI is what actually differs — the script,
/// and whatever narrow reader that CLI's own argv needs.
#[derive(Debug)]
pub struct Fake {
    directory: tempfile::TempDir,
    /// Where everything the fakes were handed is appended.
    pub log: PathBuf,
}

impl Fake {
    /// Writes every `(name, body)` script and the log they append to.
    ///
    /// `@LOG@` and `@MODE@` are substituted into each body, which is how a fake
    /// driven by a **real** driver is told anything at all: the argv is that
    /// vendor's, so there is no flag of the fixture's own to carry them. A
    /// script that takes both on flags instead simply has neither marker, and
    /// the substitution is then a no-op.
    pub fn install(scripts: &[(&str, &str)], mode: &str) -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::Builder::new()
            .prefix("fake-cli-")
            .tempdir()
            .expect("a directory for the fake CLIs");
        let log = directory.path().join("received.log");
        std::fs::write(&log, "").expect("the fake CLI log");

        for (name, body) in scripts {
            let path = directory.path().join(name);
            std::fs::write(
                &path,
                body.replace("@LOG@", &log.display().to_string())
                    .replace("@MODE@", mode),
            )
            .expect("a fake CLI script");
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

    /// The records of one kind — `argv`, `env`, `stdin`, `line` or `file`.
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

    /// Everything one invocation was handed, grouped — the `argv:` record that
    /// opens it and every record written before the next one.
    ///
    /// The flat reader above cannot answer "which prompt did *that* command
    /// line carry", which is exactly what an assertion about two members
    /// holding conversations of their own has to ask.
    pub fn grouped(&self) -> Vec<Vec<String>> {
        let mut invocations: Vec<Vec<String>> = Vec::new();
        for line in self.received() {
            if line.starts_with("argv:") {
                invocations.push(Vec::new());
            }
            if let Some(current) = invocations.last_mut() {
                current.push(line);
            }
        }

        invocations
    }

    /// Where the scripts live, for a test that wants to name one.
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }
}

/// The two shape fakes — one per [`Shape`] — driven by the fixture drivers
/// below rather than by any real one.
#[derive(Debug)]
pub struct FakeCli(Fake);

impl FakeCli {
    /// Writes both scripts and the log they append to.
    pub fn install() -> Self {
        // Both take their log and their mode on flags of their own, so the mode
        // passed here reaches neither: it is the fixture drivers that carry it.
        Self(Fake::install(
            &[
                ("fake-per-message", PER_MESSAGE),
                ("fake-resident", RESIDENT),
            ],
            Mode::Answer.word(),
        ))
    }
}

impl std::ops::Deref for FakeCli {
    type Target = Fake;

    fn deref(&self) -> &Fake {
        &self.0
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
        // Optional, and read rather than hard-coded to `None`: *what* a CLI
        // says when it stops is a per-CLI fact, but the **ordering** the runner
        // owes such a turn — session stored, words mailed, then the report — is
        // the shim core's, so the shape fixtures have to be able to produce one.
        refused: value
            .get("refused")
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
        // The two slots this lead is *not* pointing at its fake hold the real
        // backends made harmless the way `ganja_testkit::backends` makes them
        // harmless, and for the same reason: no stub is truthful any more, and
        // a real backend on this process's own `PATH` would spawn the
        // developer's own CLI. The two that search get an empty search path;
        // agy searches nothing and refuses every spawn already.
        codex: Arc::new(
            ShimBackend::new(Arc::new(ganja_core::teammate::codex::Codex::new()))
                .searching(OsString::new()),
        ),
        agy: Arc::new(ganja_core::teammate::agy::Agy::new()),
        grok: Arc::new(
            ShimBackend::new(Arc::new(ganja_core::teammate::grok::Grok::new()))
                .searching(OsString::new()),
        ),
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
/// the script at install time by [`Fake::install`], which is the same trick
/// under a different roof: a fixture written per test into a directory of its
/// own.
///
/// It answers two doors, because the driver knocks on two: `codex login status`
/// for the spawn pre-check, and `codex exec [resume <id>]` for a turn.
#[derive(Debug)]
pub struct FakeCodex(Fake);

impl FakeCodex {
    /// A fake that is logged in and behaves as `mode` says.
    pub fn install(mode: Mode) -> Self {
        Self(Fake::install(&[("codex", CODEX)], mode.word()))
    }

    /// A fake whose `login status` answers non-zero — **AC-10**'s other arm.
    ///
    /// A mode word that is not a [`Mode`], because it is not a *shape* of turn
    /// this fixture family has: it is one CLI's own pre-check answering no.
    pub fn logged_out() -> Self {
        Self(Fake::install(&[("codex", CODEX)], "logged-out"))
    }

    /// The argv records of turns only, with the pre-check's own left out.
    ///
    /// `codex login status` is an invocation this fixture records like any
    /// other, and a posture assertion that counted it would be asserting about
    /// a command that takes no turn. codex's alone, because it is the only one
    /// of these CLIs with a door that is not a turn.
    pub fn turns(&self) -> Vec<String> {
        self.records("argv")
            .into_iter()
            .filter(|argv| !argv.starts_with("login "))
            .collect()
    }
}

impl std::ops::Deref for FakeCodex {
    type Target = Fake;

    fn deref(&self) -> &Fake {
        &self.0
    }
}

/// A fake `grok`, shaped like the real one (**W5**).
///
/// [`FakeCodex`]'s shape and for its reason — it is driven by the **real**
/// [`ganja_core::teammate::grok::Grok`] driver, so the argv is that vendor's
/// and the log path is baked in — differing in the two ways that vendor's
/// surface differs: the prompt arrives in the file `--prompt-file` names rather
/// than on stdin, and the answer is NDJSON in the Anthropic Messages wire
/// format rather than codex's own event stream.
///
/// Every invocation of it is a turn: unlike codex there is no pre-check
/// subcommand to filter out of the argv records, which is why this one has no
/// `turns()` of its own.
#[derive(Debug)]
pub struct FakeGrok(Fake);

impl FakeGrok {
    /// A fake that behaves as `mode` says.
    pub fn install(mode: Mode) -> Self {
        Self(Fake::install(&[("grok", GROK)], mode.word()))
    }

    /// A fake whose turn ends with a tool named and no answer — what an
    /// unapproved tool ask costs on the probed version.
    ///
    /// A mode word that is not a [`Mode`], for [`FakeCodex::logged_out`]'s
    /// reason: it is this one CLI's own posture rather than a shape of turn.
    pub fn cancelling() -> Self {
        Self(Fake::install(&[("grok", GROK)], "cancel"))
    }

    /// The session id each turn was told to use, in turn order — a `--resume`
    /// value where the turn resumed and a `--session-id` value where it minted.
    ///
    /// Read off the fake's own argv records rather than off its answers,
    /// because what **AC-6** and **AC-19** assert is what was *composed*.
    pub fn sessions(&self) -> Vec<String> {
        self.records("argv")
            .iter()
            .map(|argv| session_of(argv))
            .collect()
    }

    /// The session id of the one turn whose prompt mentions `needle`.
    ///
    /// **AC-19**'s reader: two members' turns interleave in one log, and the
    /// only thing that tells them apart is what each was asked to do.
    pub fn session_for(&self, needle: &str) -> Option<String> {
        self.grouped().into_iter().find_map(|invocation| {
            let carried = invocation
                .iter()
                .any(|line| line.starts_with("file:") && line.contains(needle));
            let argv = invocation.first()?.strip_prefix("argv:")?;

            carried.then(|| session_of(argv))
        })
    }
}

/// The value after `--resume` or `--session-id` on one composed line.
fn session_of(argv: &str) -> String {
    let tokens: Vec<&str> = argv.split(' ').collect();
    tokens
        .iter()
        .position(|token| *token == "--resume" || *token == "--session-id")
        .and_then(|at| tokens.get(at + 1))
        .map_or_else(String::new, |id| (*id).to_owned())
}

impl std::ops::Deref for FakeGrok {
    type Target = Fake;

    fn deref(&self) -> &Fake {
        &self.0
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

/// The fake `grok`.
///
/// Prints the NDJSON a probed `grok 1.0.6` actually printed under
/// `--output-format streaming-messages-json --include-partial-messages`: a
/// `system`/`init` record naming the session, `stream_event` records wrapping
/// Anthropic Messages events, one whole `assistant` message, and a terminal
/// `result`. Not a shape invented here, which is what makes the parser's unit
/// tests and this fixture agree about the same vendor.
///
/// **It echoes the session id it was given** rather than minting one of its
/// own, because for this CLI the id is the shim's to choose: a first turn is
/// told `--session-id <uuid>` and a later one `--resume <uuid>`, and echoing is
/// how the driver learns the child accepted it (**AC-6**, **AC-19**).
///
/// Its `refuse` mode prints that vendor's own could-not-apply sentence and
/// exits 1 **before reading the prompt file**, which is the order the real one
/// refuses in: the sandbox is applied at process entry, before the headless
/// branch is even computed. Its `cancel` mode prints what a probed 1.0.6
/// printed for a turn whose tool ask nothing approved: the tool named in the
/// partial stream, then a terminal `result` carrying `stop_reason: "cancelled"`
/// and a one-word `errors: ["cancelled"]`, on a **zero** exit.
const GROK: &str = r#"#!/bin/sh
log='@LOG@'
mode='@MODE@'
args="$*"
printf 'argv:%s\n' "$args" >> "$log"
printf 'env:%s\n' "$(env | cut -d= -f1 | sort | tr '\n' ' ')" >> "$log"

prompt=""
session=""
resume=""
sandbox=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prompt-file) prompt="$2"; shift 2 ;;
    --session-id) session="$2"; shift 2 ;;
    --resume) resume="$2"; shift 2 ;;
    --sandbox) sandbox="$2"; shift 2 ;;
    *) shift ;;
  esac
done

if [ "$mode" = "refuse" ]; then
  printf "error: could not apply the 'read-only' sandbox profile; see the warning above for the cause. Refusing to start with its protections missing.\n" >&2
  exit 1
fi

text=""
if [ -n "$prompt" ]; then
  text=$(tr '\n' ' ' < "$prompt")
  printf 'file:%s\n' "$text" >> "$log"
fi

case "$mode" in
  fail) printf 'error: the fake grok was not logged in\n' >&2; exit 3 ;;
  garbage) printf 'this is not the shape any driver reads\n'; exit 0 ;;
  hang) sleep 300; exit 0 ;;
esac

id="$resume"
if [ -z "$id" ]; then id="$session"; fi
printf '{"type":"system","subtype":"init","session_id":"%s","apiKeySource":"oauth","model":"grok-4.6","permissionMode":"dontAsk"}\n' "$id"

if [ "$mode" = "cancel" ]; then
  printf '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"write"}}}\n'
  printf '{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["cancelled"],"stop_reason":"cancelled","num_turns":1}\n'
  exit 0
fi

printf '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}}\n'
printf '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thinking is not mail"}}}\n'
case "$sandbox:$text" in
  read-only:*WRITE*)
    printf '{"type":"assistant","message":{"content":[{"type":"text","text":"refused: the sandbox is read-only"}],"stop_reason":"end_turn"}}\n'
    printf '{"type":"result","subtype":"success","is_error":false,"result":"refused: the sandbox is read-only","stop_reason":"end_turn"}\n'
    exit 0 ;;
esac
printf '{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answered"}}}\n'
printf '{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"thinking is not mail","signature":"x"},{"type":"text","text":"answered"}],"stop_reason":"end_turn"}}\n'
printf '{"type":"result","subtype":"success","is_error":false,"duration_ms":12,"num_turns":1,"result":"answered","stop_reason":"end_turn"}\n'
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
  stopped) printf '{"session":"fake-session-1","messages":["half an answer"],"refused":"the fake stopped part-way"}\n'; exit 0 ;;
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
