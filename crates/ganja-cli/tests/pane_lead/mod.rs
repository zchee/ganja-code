//! A real `ganja` lead in a tmux pane of its own, driven from outside.
//!
//! Shared by every pane binary in this directory — `teammate_permission.rs`,
//! `teammate_env.rs`, `teammate_pane.rs`, `team_tasks_pane.rs` and
//! `team_continuation_pane.rs` — which are the end-to-end half of what
//! `ganja-teammate-local/tests/pane_support` pins with a fake pane child: here **both**
//! processes are the shipped binary — the lead is the terminal UI running
//! inside a private tmux server, and the pane is whatever that lead's `/teammate
//! spawn w1 --backend ganja` split off — and the test reaches them the way a
//! person would, through `send-keys` and `capture-pane`.
//!
//! # Two ways in, and why both
//!
//! [`Lead`] is born **as** the server's first window, which is what a drill
//! wants when the question is about the environment that window was born
//! from. [`Tmux`] instead starts the server on an idle shell and splits the
//! lead into a pane of its own, which is what a drill wants when the lead has
//! to hold something the server does not — its own fake-provider script, so a
//! pane it later spawns cannot play its conversation. The rest — the names a
//! server is born without, the two environments ([`server_env`], [`lead_env`]),
//! the log reads and the team file — is shared by both shapes.
//!
//! # Where the environment goes
//!
//! Everything the fixture puts on the tmux **client's** environment when the
//! server is born becomes the server's global environment, which every pane
//! it ever splits inherits (§10.10): the fake provider and its script, the
//! data and cache homes, the models-fetch kill switch. What a test wants the
//! *lead alone* to hold — a config home the server predates, a credential a
//! pane must never see on its command line — is put on the lead's own process
//! by an `env NAME=value` prefix in the window command, which touches neither
//! of tmux's environment tables. Whether the pane sees it afterwards is then
//! exactly the question the test asks.
//!
//! # No `set_var`
//!
//! Nothing here mutates this process's environment: the server, the lead and
//! the pane are children, and every variable travels to them. So a binary
//! over this fixture may hold more than one test; `teammate_permission.rs`
//! happens to hold one.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ganja_core::team::TeamFile;
pub use ganja_testkit::Homes;
use ganja_testkit::tmux::{PrivateServer, require_tmux};

/// The composer's placeholder, which the first frame of an idle lead draws —
/// pinned to `ganja_tui::component::editor`. Once it is on screen raw mode is
/// on, and a key sent to the pane is a key the app reads; later in a drill it
/// is the sign that the next line typed reaches the composer rather than an
/// overlay that owns the keyboard.
///
/// It is **not** the sign that no turn is running: the placeholder is drawn
/// whenever the buffer is empty, a streaming reply included, so a drill that
/// reads it as "idle" types its next line into a turn that has not ended —
/// which the engine takes as a steer, not a prompt. [`idle`] is that sign.
pub const COMPOSER: &str = "Ask ganja something";

/// The status bar's word for a lead with no turn running — pinned to
/// `ganja_tui::component::status::Activity::Ready`'s label, which the frontend
/// sets in the same event handler that clears its own turn flag. A bar that
/// reads it is a lead whose next typed line starts a turn rather than steering
/// one.
pub const READY: &str = "ready";

/// What the status bar joins its segments with — pinned to
/// `ganja_tui::component::status`.
const SEPARATOR: &str = " \u{b7} ";

/// The head of what the lead says right after a spawn — `<name> started`,
/// with `\u{b7} prompt persisted in cleartext at <path>` following it
/// (`ganja_tui::component::team::Spawned::notice`). The notice names the
/// teammate, so waiting on `<name> started` cannot read an earlier teammate's
/// line as this one's.
///
/// Only the head, and that is a finding rather than a shortcut: a pane
/// teammate's column takes 65% of the width by default
/// (`ganja_teammate_local::pane::DEFAULT_SHARE`), and a lead at the remaining
/// 35% has no room on one status line for the path — these drills watch a
/// **real** terminal. That the sentence itself is whole is pinned where a
/// width can be chosen — `ganja-tui`'s own
/// `a_team_spawn_is_reaped_by_the_tick_and_says_where_the_prompt_landed`.
/// What is asserted here is the half only a real lead can show: that it says
/// anything at all. An in-process teammate opens no column, and the head is
/// still all a drill needs of the line.
pub const SPAWN_NOTICE: &str = "started";

/// Whether `screen` shows a lead with no turn running: its status bar — the
/// bottom row, and the last row that says anything while no Ctrl+T inspector
/// is open to hide it — carries [`READY`] as a segment of its own.
///
/// A segment rather than a substring, because the bar's tail is the notice
/// lane and a notice is free prose: a finished spawn names a cleartext path
/// there, and a path is the kind of text that can hold a five-letter word by
/// accident. The activity segment reads the bare label only before any turn
/// or after one that completed — `<spinner> streaming`, `tool: <name>` and
/// `waiting on permission` while one runs, `stopped` or `failed` after one
/// that did not complete — so nothing but an idle lead answers `true`, and a
/// drill whose turn was cancelled or failed times out here, with the screen
/// and the log printed, rather than failing at a count.
///
/// The default bar only: a configured `tui.statusline` roster joins its
/// elements with ` | ` and draws only the ones it names, so a drill that sets
/// one needs a sign of its own.
pub fn idle(screen: &str) -> bool {
    screen
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|bar| bar.split(SEPARATOR).any(|segment| segment.trim() == READY))
}

/// The permission dialog's options line — pinned to
/// `ganja_tui::component::permission`, as `pty_smoke.rs` pins it.
pub const DIALOG_OPTIONS: &str = "[y] allow once   [a] always allow   [n]/[Esc] reject";

/// How long any one thing here — the lead's first frame, the pane's exec, a
/// dialog, a file — is waited for. Generous, because a cold debug binary is
/// what tmux is starting.
pub const DEADLINE: Duration = Duration::from_secs(30);

/// Between looks.
const POLL: Duration = Duration::from_millis(100);

/// The pane teammate every test here spawns.
pub const TEAMMATE: &str = "w1";

/// How many lines of a lead's log a timed-out wait quotes.
const LOG_TAIL: usize = 80;

/// What a private server is born **without**, so nothing a pane inherits can
/// be this developer's rather than the fixture's (§10.10). One spelling,
/// because a list that drifted between two drills would be two different
/// experiments wearing one name.
const WITHHELD: &[&str] = &[
    "GANJA_CONFIG_HOME",
    "GANJA_CONFIG",
    "GANJA_MODEL",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "TMUX",
    "TMUX_PANE",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
];

/// The environment a private server is born from, and so what every pane it
/// makes inherits: the shell a person started tmux from, pinned to `homes` the
/// way the other pty drills pin theirs.
///
/// No config home and no XDG base — those are the lead's to be given and the
/// launch's to carry. `script` is the fake provider's script the **panes** get,
/// which a drill either has (its teammate's) or has not; a lead needing one of
/// its own is handed it by [`lead_env`] instead, on its own process, so the two
/// conversations cannot play each other's turns.
pub fn server_env(homes: &Homes, script: Option<&Path>) -> Vec<(&'static str, String)> {
    let mut env =
        vec![("HOME", homes.data().display().to_string()), ("GANJA_PROVIDER", "fake".to_owned())];
    if let Some(script) = script {
        env.push(("GANJA_FAKE_SCRIPT", script.display().to_string()));
    }
    env.push(("GANJA_DISABLE_MODELS_FETCH", "1".to_owned()));

    env
}

/// What a lead's own pane is additionally given (`-e`): where its things are,
/// and the `script` it plays in place of whatever the server was born holding.
///
/// Given to the lead's process alone rather than to the server, which is what
/// keeps a pane the lead later spawns from inheriting the lead's own
/// conversation. A drill whose lead is content with the server's own script
/// passes none.
pub fn lead_env(homes: &Homes, script: Option<&Path>) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("GANJA_CONFIG_HOME", homes.config_home().display().to_string()),
        ("XDG_DATA_HOME", homes.data().display().to_string()),
        ("XDG_CONFIG_HOME", homes.data().join("config").display().to_string()),
        ("XDG_CACHE_HOME", homes.data().join("cache").display().to_string()),
    ];
    if let Some(script) = script {
        env.push(("GANJA_FAKE_SCRIPT", script.display().to_string()));
    }
    // What every other pty drill sets (**D517**): a private server born
    // from `-f /dev/null` is a terminal whose answer to the kitty query
    // depends on which tmux the host ships, and the probe blocks up to
    // two seconds where there is none.
    env.push(("GANJA_DISABLE_TERM_PROBE", "1".to_owned()));

    env
}

/// Where a session under `homes` traces — the data home's rolling log, one
/// file per local date. A member's pane inherits the same data home (D502) and
/// writes the same file, which is what a failing spawn needs read anyway: both
/// halves of it, in one order.
pub fn log_dir(homes: &Homes) -> PathBuf {
    homes.data().join("ganja").join("log")
}

/// Everything traced under `dir` so far, oldest file first — nothing where
/// there is no log yet, which is every moment before the first session opens
/// one.
pub fn log_text(dir: &Path) -> String {
    let Ok(entries) = fs::read_dir(dir) else {
        return String::new();
    };
    let mut files: Vec<PathBuf> =
        entries.filter_map(Result::ok).map(|entry| entry.path()).collect();
    files.sort();

    files.iter().filter_map(|path| fs::read_to_string(path).ok()).collect()
}

/// The last [`LOG_TAIL`] lines of what `dir` holds, named and counted, for a
/// wait that timed out.
///
/// Names the directory because the most common way this reads empty is that
/// nothing ever opened a log there — which is a different failure from a lead
/// that ran and traced nothing.
pub fn log_tail(dir: &Path) -> String {
    let text = log_text(dir);
    let held: Vec<&str> = text.lines().collect();
    let kept = held.len().saturating_sub(LOG_TAIL);

    format!(
        "last {} of {} lines from {}:\n{}",
        held.len() - kept,
        held.len(),
        dir.display(),
        held[kept..].join("\n")
    )
}

/// The team directory the lead made — the only one under the config home
/// `homes` names, itself named `session-<8hex>` after a session id no drill
/// here ever sees. The config home is the variable D502 exists to carry, so
/// finding a team under it at all is that mechanism having worked.
pub fn team_dir(homes: &Homes) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(homes.config_home().join("teams"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    assert!(dirs.len() <= 1, "one lead is one team: {dirs:?}");

    dirs.into_iter().next()
}

/// The team file that directory holds, or nothing where no lead has written
/// one yet.
pub fn team_file(homes: &Homes) -> Option<TeamFile> {
    let text = fs::read_to_string(team_dir(homes)?.join("config.json")).ok()?;

    Some(serde_json::from_str(&text).expect("the team file this build wrote decodes"))
}

/// A private tmux server whose first window is a real `ganja` lead — the
/// server itself is [`ganja_testkit::tmux`]'s, killed when dropped so a
/// failing test leaves neither a server nor a pane of the binary behind.
pub struct Lead {
    server: PrivateServer,
    /// The pane the lead runs in.
    pane: String,
}

impl Lead {
    /// Starts the server with `homes`' environment and the lead in its first
    /// window.
    ///
    /// `pane_script` is what the server's global environment names as the
    /// fake provider's script — what a spawned pane will play. `withheld` is
    /// taken **out** of the server's environment, and `lead_only` is put on
    /// the lead's process alone, through the window command's `env` prefix:
    /// how a test stages "the server predates the export" and "the lead holds
    /// a secret".
    pub fn start(
        homes: &Homes,
        pane_script: &Path,
        withheld: &[&str],
        lead_only: &[(&str, &str)],
    ) -> Self {
        require_tmux();
        // The window command: the lead's own environment first, then the
        // binary. Always at least two words, which is what keeps tmux from
        // handing a one-word command to the login shell (`pane.rs`'s own
        // finding: that shell sources rc files and hands a pane whatever they
        // export).
        let mut window = String::from("env");
        for (name, value) in lead_only {
            window.push(' ');
            window.push_str(&shell_quote(&format!("{name}={value}")));
        }
        window.push(' ');
        window.push_str(&shell_quote(env!("CARGO_BIN_EXE_ganja")));

        let script = pane_script.display().to_string();
        let data = homes.data().display().to_string();
        let config = homes.data().join("config").display().to_string();
        let cache = homes.data().join("cache").display().to_string();
        let mut removed = vec![
            "GANJA_CONFIG_HOME",
            "GANJA_CONFIG",
            "GANJA_MODEL",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "OPENROUTER_API_KEY",
            "GANJA_SERVER_PASSWORD",
        ];
        removed.extend_from_slice(withheld);
        let server = PrivateServer::start_in(
            homes.project(),
            // Wide enough that the lead is still comfortable **after** a
            // teammate takes its column: since 2026-08-20 a spawn leaves the
            // lead 30% of the window, and at 160 that is 48 columns — where
            // the permission dialog's own option line wraps and a screen
            // assertion reads half of it. 240 leaves the lead 72.
            (240, 48),
            &[&window],
            &removed,
            &[
                ("GANJA_PROVIDER", "fake"),
                ("GANJA_FAKE_SCRIPT", &script),
                ("XDG_DATA_HOME", &data),
                ("HOME", &data),
                ("XDG_CONFIG_HOME", &config),
                ("XDG_CACHE_HOME", &cache),
                ("GANJA_DISABLE_MODELS_FETCH", "1"),
            ],
        );

        let pane = server.first_pane().to_owned();
        let lead = Self { server, pane };
        // The lead's first frame, for [`COMPOSER`]'s reason.
        lead.wait_for_screen(&lead.pane, |screen| screen.contains(COMPOSER));

        lead
    }

    /// The pane the lead runs in.
    pub fn pane(&self) -> &str {
        &self.pane
    }

    /// Types `line` into the lead and presses Enter.
    pub fn type_line(&self, line: &str) {
        self.server.run(&["send-keys", "-t", &self.pane, "-l", line]);
        self.server.run(&["send-keys", "-t", &self.pane, "Enter"]);
    }

    /// Presses one key in the lead.
    pub fn press(&self, key: &str) {
        self.server.run(&["send-keys", "-t", &self.pane, key]);
    }

    /// What `pane` shows right now.
    pub fn screen(&self, pane: &str) -> String {
        self.server.run(&["capture-pane", "-p", "-t", pane])
    }

    /// Waits until `pane`'s screen satisfies `wanted`, and answers the screen
    /// that did.
    pub fn wait_for_screen(&self, pane: &str, wanted: impl Fn(&str) -> bool) -> String {
        let started = Instant::now();
        loop {
            let screen = self.screen(pane);
            if wanted(&screen) {
                return screen;
            }
            assert!(
                started.elapsed() < DEADLINE,
                "pane {pane} never showed what was waited for; it shows:\n{screen}"
            );
            std::thread::sleep(POLL);
        }
    }

    /// The live panes as `(id, pid)` pairs.
    pub fn panes(&self) -> Vec<(String, u32)> {
        self.server
            .run(&["list-panes", "-a", "-F", "#{pane_id} #{pane_pid}"])
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let (id, pid) = line.trim().split_once(' ').expect("id and pid");

                (id.to_owned(), pid.parse().expect("a pid"))
            })
            .collect()
    }

    /// Waits for a second pane — the teammate's — and answers its `(id, pid)`.
    pub fn wait_for_teammate_pane(&self) -> (String, u32) {
        let started = Instant::now();
        loop {
            if let Some(pane) = self.panes().into_iter().find(|(id, _)| *id != self.pane) {
                return pane;
            }
            assert!(
                started.elapsed() < DEADLINE,
                "no teammate pane appeared; the lead shows:\n{}",
                self.screen(&self.pane)
            );
            std::thread::sleep(POLL);
        }
    }

    /// Whether the server's **global** environment — what every pane it makes
    /// inherits — holds `name`.
    pub fn global_has(&self, name: &str) -> bool {
        self.server.global_has(name)
    }
}

/// A private tmux server whose first window is an idle shell, for the drills
/// that **split** their lead into a pane rather than being born as one.
///
/// [`Lead`]'s shape cannot serve them: a lead born as the server's first
/// window holds exactly what the server holds, and these drills need the lead
/// to hold a fake-provider script the server does not.
pub struct Tmux {
    server: PrivateServer,
    /// [`log_dir`] of the homes the server was started over, kept here
    /// because [`Tmux::wait_for`] has the server and the lead's pane in hand
    /// and nothing else.
    logs: PathBuf,
    /// How long [`Tmux::wait_for`] gives any one thing — each drill's own,
    /// since what they wait on ranges from one cold binary to seven provider
    /// round trips.
    deadline: Duration,
}

impl Tmux {
    /// Starts the server with `env` on its global environment — what every
    /// pane it ever makes inherits — and `WITHHELD` taken out of it, tracing
    /// under `homes` and giving each [`Tmux::wait_for`] `deadline`.
    pub fn start(homes: &Homes, env: &[(&'static str, String)], deadline: Duration) -> Self {
        require_tmux();

        Self {
            server: PrivateServer::start(&["sleep", "3600"], WITHHELD, &borrowed(env)),
            logs: log_dir(homes),
            deadline,
        }
    }

    /// Splits `argv` into a pane of its own under `cwd`, with `env` on that
    /// process alone, and answers the pane's id. `argv` is at least two words,
    /// for the reason `pane.rs` gives.
    pub fn split(&self, cwd: &Path, env: &[(&'static str, String)], argv: &[&str]) -> String {
        self.server.split(Some(cwd), &borrowed(env), argv)
    }

    /// The live pane ids.
    pub fn panes(&self) -> Vec<String> {
        self.server.panes()
    }

    /// Where a pane's top-left corner sits in the window, as tmux reports
    /// it — the only honest way to ask which side of the lead a teammate
    /// opened on, since what a person sees is the layout rather than the
    /// argv that produced it.
    pub fn corner(&self, pane: &str) -> (u16, u16) {
        let reported =
            self.server.run(&["display-message", "-p", "-t", pane, "#{pane_left} #{pane_top}"]);
        let mut columns = reported.split_whitespace().map(|word| {
            word.parse()
                .unwrap_or_else(|_| panic!("tmux reports a corner as two numbers: {reported:?}"))
        });
        let left = columns.next().expect("a left column");
        let top = columns.next().expect("a top row");

        (left, top)
    }

    /// The pid of the pane's process — for the lead's pane, the lead.
    pub fn pane_pid(&self, pane: &str) -> String {
        self.server.run(&["display-message", "-p", "-t", pane, "#{pane_pid}"]).trim().to_owned()
    }

    /// The name of the process in `pane`'s foreground, as tmux sees it.
    pub fn current_command(&self, pane: &str) -> String {
        self.server
            .run(&["display-message", "-p", "-t", pane, "#{pane_current_command}"])
            .trim()
            .to_owned()
    }

    /// What `pane` shows right now.
    pub fn screen(&self, pane: &str) -> String {
        self.server.run(&["capture-pane", "-p", "-t", pane])
    }

    /// Types `text` into `pane` literally, then Enter.
    pub fn type_line(&self, pane: &str, text: &str) {
        self.server.run(&["send-keys", "-t", pane, "-l", "--", text]);
        self.server.run(&["send-keys", "-t", pane, "Enter"]);
    }

    /// Presses one key in `pane`.
    pub fn key(&self, pane: &str, name: &str) {
        self.server.run(&["send-keys", "-t", pane, name]);
    }

    /// Polls `read` every 50ms until it answers, or panics with `what`, the
    /// lead's screen and the tail of the log the lead — and any member sharing
    /// its data home — traced, after this server's deadline.
    ///
    /// The log beside the screen because the screen alone has already been read
    /// twice on CI (runs 33261878445 and 33368776921) and each time it showed a
    /// lead that looked idle with a line it never acted on; what the lead
    /// *traced* in that window is the half of the picture the screen cannot give.
    pub fn wait_for<T>(&self, what: &str, lead: &str, mut read: impl FnMut() -> Option<T>) -> T {
        let started = Instant::now();
        loop {
            if let Some(found) = read() {
                return found;
            }
            assert!(
                started.elapsed() < self.deadline,
                "waited {:?} for {what} and it did not happen; the lead's screen:\n{}\n{}",
                self.deadline,
                self.screen(lead),
                log_tail(&self.logs)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// `env`'s values borrowed, which is the shape [`PrivateServer`] takes.
fn borrowed<'a>(env: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    env.iter().map(|(name, value)| (*name, value.as_str())).collect()
}

/// The command line of the process `pid`, as `ps(1)` shows it to everybody.
pub fn argv_of(pid: u32) -> String {
    let output =
        Command::new("ps").args(["-o", "args=", "-p", &pid.to_string()]).output().expect("ps runs");

    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Waits until `wanted` holds, and answers what it last saw.
pub fn wait_for<T>(what: &str, mut look: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(found) = look() {
            return found;
        }
        assert!(started.elapsed() < DEADLINE, "{what} never happened");
        std::thread::sleep(POLL);
    }
}

/// `text` as one `sh` word, by the same crate the production launch line
/// rides.
fn shell_quote(text: &str) -> String {
    shlex::try_quote(text).expect("no NUL rides a test's window command").into_owned()
}
