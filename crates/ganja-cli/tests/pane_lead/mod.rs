//! A real `ganja` lead in a tmux window of its own, driven from outside.
//!
//! Shared by the two pane binaries in this directory (`teammate_permission.rs`,
//! `teammate_env.rs`), which are the end-to-end half of what
//! `ganja-teammate-local/tests/pane_support` pins with a fake pane child: here **both**
//! processes are the shipped binary — the lead is the terminal UI running
//! inside a private tmux server, and the pane is whatever that lead's `/team
//! spawn w1 --backend ganja` split off — and the test reaches them the way a
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
//! over this fixture may hold more than one test; `teammate_permission.rs`
//! happens to hold one.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub use ganja_testkit::Homes;
use ganja_testkit::tmux::{PrivateServer, require_tmux};

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
