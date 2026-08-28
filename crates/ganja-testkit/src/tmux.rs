//! A tmux server of a test's own, on a socket nobody else knows.
//!
//! Every pane suite in the workspace — `ganja-core`'s two pane binaries and
//! its live `claude` round trip, `ganja-cli`'s lead-in-a-window and
//! split-pane fixtures — needs the same server: detached, configured from
//! `/dev/null` so nobody's `~/.tmux.conf` shapes a test, born **outside**
//! whatever tmux is running the suite, and killed when the test ends however
//! it ends. What varies per suite is only what the first window runs, where,
//! and which variables the server is or is not born with — all parameters
//! here rather than forks.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Refuses to run without tmux, by name: a green pane test that spawned no
/// pane would be worth nothing.
pub fn require_tmux() {
    let version = Command::new("tmux").arg("-V").output();
    assert!(
        version.as_ref().is_ok_and(|output| output.status.success()),
        "the pane tests need tmux on PATH and there is none: {version:?}"
    );
}

/// One tmux client call against `socket`, or a panic in tmux's own words.
pub fn tmux(socket: &Path, args: &[&str]) -> String {
    let output = Command::new("tmux").arg("-S").arg(socket).args(args).output().expect("tmux runs");
    assert!(
        output.status.success(),
        "tmux {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A private tmux server, killed when dropped — panics included — so a
/// failing test leaves no server behind holding a pane open.
pub struct PrivateServer {
    socket: PathBuf,
    /// The pane the server was born with.
    first_pane: String,
    _dir: TempDir,
}

impl PrivateServer {
    /// Starts a detached 200×50 server whose first window runs `window`, with
    /// `withheld` taken **out** of the environment the server is born with —
    /// how a test stages "the server predates the export" — and `env` put
    /// into it, which every pane the server ever makes inherits (§10.10).
    ///
    /// `$TMUX` and `$TMUX_PANE` are always withheld: a server that inherited
    /// this process's own tmux would refuse to start as nested, and either
    /// variable reaching a pane would name the developer's server instead of
    /// this one.
    pub fn start(window: &[&str], withheld: &[&str], env: &[(&str, &str)]) -> Self {
        Self::server(None, (200, 50), window, withheld, env)
    }

    /// [`PrivateServer::start`] with the first window's directory and the
    /// window size named — the lead-in-the-first-window shape, where the
    /// window command is the binary under test and the screen is asserted on.
    pub fn start_in(
        cwd: &Path,
        size: (u16, u16),
        window: &[&str],
        withheld: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        Self::server(Some(cwd), size, window, withheld, env)
    }

    fn server(
        cwd: Option<&Path>,
        (width, height): (u16, u16),
        window: &[&str],
        withheld: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        let dir = crate::temp_dir();
        let socket = dir.path().join("tmux.sock");
        let mut command = Command::new("tmux");
        command
            .arg("-S")
            .arg(&socket)
            .arg("-f")
            .arg("/dev/null")
            .args(["new-session", "-d", "-s", "ganja-test"])
            .args(["-x", &width.to_string(), "-y", &height.to_string()]);
        if let Some(cwd) = cwd {
            command.arg("-c").arg(cwd);
        }
        command.args(window);
        for (name, value) in env {
            command.env(name, value);
        }
        command.env_remove("TMUX").env_remove("TMUX_PANE");
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
        let first_pane = listing.trim().to_owned();
        assert!(first_pane.starts_with('%'), "the private server has a first pane: {listing:?}");

        Self { socket, first_pane, _dir: dir }
    }

    /// The socket, for `Server::at` and for `$TMUX`.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The pane the server was born with — what a lead "runs in", or the
    /// lead itself in the [`PrivateServer::start_in`] shape.
    pub fn first_pane(&self) -> &str {
        &self.first_pane
    }

    /// Points this process at the private server the way tmux would have:
    /// `$TMUX` and `$TMUX_PANE`.
    ///
    /// # Safety
    ///
    /// Mutates process-wide environment; the calling binary holds one test.
    pub unsafe fn enter(&self) {
        // SAFETY: the caller's binary holds exactly one test.
        unsafe {
            std::env::set_var("TMUX", format!("{},0,0", self.socket.display()));
            std::env::set_var("TMUX_PANE", &self.first_pane);
        }
    }

    /// One client call against this server, or a panic in tmux's own words.
    pub fn run(&self, args: &[&str]) -> String {
        tmux(&self.socket, args)
    }

    /// Splits a pane running `argv` — in `cwd` when one is named, with `env`
    /// added through tmux's own `-e` door — and answers its id. `argv` is at
    /// least two words, for the reason production's `pane.rs` gives: tmux
    /// hands a one-word command to the login shell.
    pub fn split(&self, cwd: Option<&Path>, env: &[(&str, &str)], argv: &[&str]) -> String {
        let mut args: Vec<String> =
            ["split-window", "-d", "-P", "-F", "#{pane_id}"].map(str::to_owned).into();
        if let Some(cwd) = cwd {
            args.push("-c".to_owned());
            args.push(cwd.to_string_lossy().into_owned());
        }
        for (name, value) in env {
            args.push("-e".to_owned());
            args.push(format!("{name}={value}"));
        }
        args.push("--".to_owned());
        args.extend(argv.iter().map(|word| (*word).to_owned()));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let id = self.run(&refs).trim().to_owned();
        assert!(id.starts_with('%'), "a split answers a pane id: {id:?}");

        id
    }

    /// The live pane ids.
    pub fn panes(&self) -> Vec<String> {
        self.run(&["list-panes", "-a", "-F", "#{pane_id}"])
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// Whether the server's **global** environment — what every pane it makes
    /// inherits — holds `name`. Asked directly rather than through [`tmux`],
    /// because `show-environment` answers an absent name with a failure this
    /// is precisely here to read.
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

    /// The pane's title, as `select-pane -T` set it.
    pub fn title(&self, pane_id: &str) -> String {
        self.run(&["display-message", "-p", "-t", pane_id, "#{pane_title}"]).trim().to_owned()
    }

    /// The command a pane was started with, as tmux itself records it.
    pub fn start_command(&self, pane_id: &str) -> String {
        self.run(&["display-message", "-p", "-t", pane_id, "#{pane_start_command}"])
    }
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux").arg("-S").arg(&self.socket).arg("kill-server").output();
    }
}
