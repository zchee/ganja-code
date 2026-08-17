//! A real `ganja` lead in a tmux window of its own, driven from outside.
//!
//! Shared by the two pane binaries in this directory (`teammate_permission.rs`,
//! `teammate_env.rs`), which are the end-to-end half of what
//! `ganja-core/tests/pane_support` pins with a fake pane child: here **both**
//! processes are the shipped binary — the lead is the terminal UI running
//! inside a private tmux server, and the pane is whatever that lead's `/team
//! spawn w1 --backend pane` split off — and the test reaches them the way a
//! person would, through `send-keys` and `capture-pane`.
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
//! over this fixture may hold more than one test — and `teammate_permission.rs`
//! holds one because the plan says so, not because it must.

#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use serde_json::json;
use tempfile::TempDir;

/// The composer's placeholder, which the first frame of an idle lead draws —
/// pinned to `ganja_tui::component::editor`. Once it is on screen raw mode is
/// on, and a key sent to the pane is a key the app reads.
pub const READY: &str = "Ask ganja something";

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

/// Refuses to run without tmux, by name: a green pane test that spawned no
/// pane would be worth nothing.
pub fn require_tmux() {
    let version = Command::new("tmux").arg("-V").output();
    assert!(
        version.as_ref().is_ok_and(|output| output.status.success()),
        "the pane tests need tmux on PATH and there is none: {version:?}"
    );
}

/// The project a lead runs in and the data home it stores under, plus the
/// scripts the fake provider plays.
pub struct Homes {
    project: TempDir,
    data: TempDir,
}

impl Homes {
    /// A project (with its checkout marker) and a data home, both gone with
    /// the test.
    pub fn new() -> Self {
        let project = TempDir::new().expect("a temporary directory is creatable");
        // The checkout marker pins the project — and so the store every
        // process opens — to this directory.
        fs::create_dir(project.path().join(".git")).expect("the checkout marker is creatable");

        Self {
            project,
            data: TempDir::new().expect("a temporary directory is creatable"),
        }
    }

    pub fn project(&self) -> &Path {
        self.project.path()
    }

    pub fn data(&self) -> &Path {
        self.data.path()
    }

    /// The config home a lead started here resolves — `$XDG_CONFIG_HOME/ganja`
    /// under this fixture's data home — and where the team is kept.
    pub fn config_home(&self) -> PathBuf {
        self.data.path().join("config").join("ganja")
    }

    /// Writes a fake-provider script under the project and answers its path.
    pub fn script(&self, name: &str, turns: serde_json::Value) -> PathBuf {
        let path = self.project.path().join(name);
        fs::write(&path, json!({"cadence_ms": 1, "turns": turns}).to_string())
            .expect("the script is writable");

        path
    }

    /// The store the lead and its panes share, found under the data home the
    /// way `teammate_session.rs` finds it — one project, one store.
    pub fn store(&self) -> ganja_core::Storage {
        let mut roots: Vec<PathBuf> = fs::read_dir(self.data.path().join("ganja").join("project"))
            .expect("a run created a project directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        roots.sort();
        assert_eq!(
            roots.len(),
            1,
            "one working directory is one project, got {roots:?}"
        );

        ganja_core::Storage::open(roots.remove(0).join("storage"))
    }
}

/// A private tmux server whose first window is a real `ganja` lead.
///
/// Killed when dropped, panics included, so a failing test leaves neither a
/// server nor a pane of the binary behind.
pub struct Lead {
    socket: PathBuf,
    /// The pane the lead runs in.
    pane: String,
    _dir: TempDir,
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
        let dir = TempDir::new().expect("a temporary directory is creatable");
        let socket = dir.path().join("tmux.sock");
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

        let mut command = Command::new("tmux");
        command
            .arg("-S")
            .arg(&socket)
            .arg("-f")
            .arg("/dev/null")
            .args([
                "new-session",
                "-d",
                "-s",
                "ganja-test",
                "-x",
                "160",
                "-y",
                "48",
            ])
            .arg("-c")
            .arg(homes.project())
            .arg(window)
            .env("GANJA_PROVIDER", "fake")
            .env("GANJA_FAKE_SCRIPT", pane_script)
            .env("XDG_DATA_HOME", homes.data())
            .env("HOME", homes.data())
            .env("XDG_CONFIG_HOME", homes.data().join("config"))
            .env("XDG_CACHE_HOME", homes.data().join("cache"))
            .env("GANJA_DISABLE_MODELS_FETCH", "1")
            .env_remove("GANJA_CONFIG_HOME")
            .env_remove("GANJA_CONFIG")
            .env_remove("GANJA_MODEL")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("GANJA_SERVER_PASSWORD")
            // A server that inherited this process's own tmux would think it
            // was nested inside it.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        for name in withheld {
            command.env_remove(name);
        }
        let started = command.output().expect("tmux starts a private server");
        assert!(
            started.status.success(),
            "the private tmux server did not start: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        let listing = tmux(&socket, &["list-panes", "-a", "-F", "#{pane_id}"]);
        let pane = listing.trim().to_owned();
        assert!(
            pane.starts_with('%'),
            "the private server has a first pane: {listing:?}"
        );

        let lead = Self {
            socket,
            pane,
            _dir: dir,
        };
        // The lead's first frame, for [`READY`]'s reason.
        lead.wait_for_screen(&lead.pane, |screen| screen.contains(READY));

        lead
    }

    /// The pane the lead runs in.
    pub fn pane(&self) -> &str {
        &self.pane
    }

    /// Types `line` into the lead and presses Enter.
    pub fn type_line(&self, line: &str) {
        tmux(&self.socket, &["send-keys", "-t", &self.pane, "-l", line]);
        tmux(&self.socket, &["send-keys", "-t", &self.pane, "Enter"]);
    }

    /// Presses one key in the lead.
    pub fn press(&self, key: &str) {
        tmux(&self.socket, &["send-keys", "-t", &self.pane, key]);
    }

    /// What `pane` shows right now.
    pub fn screen(&self, pane: &str) -> String {
        tmux(&self.socket, &["capture-pane", "-p", "-t", pane])
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
        tmux(
            &self.socket,
            &["list-panes", "-a", "-F", "#{pane_id} #{pane_pid}"],
        )
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
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(["show-environment", "-g", name])
            .output()
            .expect("tmux runs")
            .status
            .success()
    }
}

impl Drop for Lead {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

/// The command line of the process `pid`, as `ps(1)` shows it to everybody.
pub fn argv_of(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");

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

/// One tmux client call against `socket`, or a panic in tmux's own words.
fn tmux(socket: &Path, args: &[&str]) -> String {
    let output = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(args)
        .output()
        .expect("tmux runs");
    assert!(
        output.status.success(),
        "tmux {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `text`, single-quoted for `sh`.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}
