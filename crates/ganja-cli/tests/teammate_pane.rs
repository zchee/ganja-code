//! `/team spawn w1 --backend pane` makes a real pane teammate, and its
//! `shutdown_approved` ends it (**AC-11**, as the spec spells it).
//!
//! Spec: Claude Code's teammates — §4.1's spawn sequence and §6.2's shutdown
//! handshake, read against this tree in §10.2–§10.4. Upstream opencode has no
//! teammates and no counterpart to any of it.
//!
//! Everything here is the real thing, end to end, and it runs where a lead
//! really runs: **inside tmux**. A private server (`tmux -S <socket>`, the
//! socket in a directory that vanishes with the test) holds a pane running the
//! real `ganja` binary as the lead — tmux itself hands it `TMUX` and
//! `TMUX_PANE`, nothing is faked into its environment — and the spec's own line
//! is typed into that pane. The lead splits a second pane, and in it the same
//! binary runs as a **member** with §4.1's five flags: it takes its seeded
//! turn against the fake provider, reports to the lead's inbox, and answers
//! the lead's shutdown request by leaving. Both screens are read back with
//! `capture-pane`, which is what makes a full-screen application's state a
//! thing a test can assert on. The engine-side half of this claim, over the
//! registry alone, is `ganja-core/tests/teammate_pane_lifecycle.rs`; this is
//! the door a person uses.
//!
//! **Hard-fails without tmux.** A pane test that skipped where there was no
//! tmux would be green on exactly the machines where nothing was tested.
//!
//! # What is asserted, in order
//!
//! 1. The lead's dialog answers the spawn (the D-7 cleartext notice), and the
//!    team file names `w1` on a `tmuxPaneId` with `backendType: "tmux"`.
//! 2. The private server lists that pane, and the process in it is `ganja` —
//!    the launch line was typed into the idle shell and `exec`'d.
//! 3. The member took the seeded task as its first turn: the fake reply is
//!    on the pane's own screen, and the `idle_notification` it wrote reached
//!    the lead — seen present in the lead's inbox while the lead is held
//!    still, then seen gone once the lead is let go and its pass has read
//!    it.
//! 4. `/team shutdown w1` — the lead asks, the member approves and leaves, the
//!    lead reads the approval: the pane is gone from tmux and `w1` from the
//!    team file, with nothing here having killed anything.
//!
//! # The environment, deliberately split in two
//!
//! The private tmux server is born from an environment that carries the
//! provider selection and the fake script — the shell a person started tmux
//! from — but **not** the config home nor any XDG base. The lead's pane is
//! given those explicitly (`-e`, the way a person's `GANJA_CONFIG_HOME=…
//! ganja` would), and they reach the member's pane only because `pane.rs`
//! carries them (D502). So the member joining the lead's team at all is the
//! D502 mechanism working through the real binary; `teammate_env.rs` pins the
//! same fact with the launch line composed by hand.
//!
//! Nothing here calls `std::env::set_var`: every variable is set on a child,
//! so the binary holds a second, tmux-free test beside the pane one: the
//! **spelling guard**. `pane.rs` composes the launch line from its own flag
//! constants and the pane child of the core lifecycle binary parses them by
//! hand, so a drift between those spellings and `Cli`'s clap names would pass
//! every test but the live pane. The guard runs the real binary on exactly
//! `pane::arguments(&spec)` — no tty, no tmux, over a team file seeded with
//! the member's record so the record wait a real member starts with is
//! satisfied at once — and reads how far it got: past clap (no usage error),
//! past `Membership::resolve` (which re-derives `<name>@<team>` from the
//! parsed flags and refuses a mismatch), past the record it found, and into
//! the terminal it has not got. That is every value on the line landing where
//! `MemberArgs` puts it, short of the colour, which nothing validates.

#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use ganja_core::{
    team::{
        MemberName, MemberRecord, Spawn, Surface, TeamFile, TeamName, TeamsRoot, mailbox, record,
    },
    teammate::{SpawnSpec, pane},
};
use ganja_protocol::team::{Frame, MemberBackend};
use serde_json::json;
use tempfile::TempDir;

/// How long each stage is given: a debug `ganja` starting cold in a pane,
/// then a second one starting cold in another, then both leaving.
const DEADLINE: Duration = Duration::from_secs(45);

/// What the member's fake provider says, appearing nowhere else — so finding
/// it on the pane's screen means the member ran the seeded turn.
const REPLY: &str = "pane-teammate-reply-zarquon";

/// The script the member's fake provider plays. One turn, one word.
const SCRIPT: &str = "script.json";

/// The teammate's name, as the spec's own line spells it.
const MEMBER: &str = "w1";

/// The one thing the lead's dialog says right after a spawn (Resolution 4,
/// D-7): the prompt is on disk in cleartext at a named path. Its presence on
/// screen is the spawn having gone through the dialog.
const CLEARTEXT_NOTICE: &str = "cleartext at";

/// What the composer draws when nothing else owns the screen — the sign that
/// the dialog is closed and the next line typed reaches the composer.
const COMPOSER: &str = "Ask ganja something";

/// Refuses to run without tmux, by name.
fn require_tmux() {
    let version = Command::new("tmux").arg("-V").output();
    assert!(
        version.as_ref().is_ok_and(|output| output.status.success()),
        "AC-11 needs tmux on PATH and there is none: {version:?}"
    );
}

/// A project and a data home that both vanish with the test.
struct Fixture {
    project: TempDir,
    data: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let project = TempDir::new().expect("a temporary directory is creatable");
        // The checkout marker pins the project — and so the one store every
        // process opens — to this directory rather than to whatever the
        // temporary directory happens to sit inside.
        fs::create_dir(project.path().join(".git")).expect("the checkout marker is creatable");
        fs::write(
            project.path().join(SCRIPT),
            json!({"cadence_ms": 1, "turns": [{"text": REPLY}]}).to_string(),
        )
        .expect("the script is writable");

        Self {
            project,
            data: TempDir::new().expect("a temporary directory is creatable"),
        }
    }

    /// The config home the lead runs under, and therefore where the team
    /// lives — the variable D502 exists to carry.
    fn config_home(&self) -> PathBuf {
        self.data.path().join("config").join("ganja")
    }

    /// The environment the **server** is born from, and so what every pane
    /// inherits: the shell a person started tmux from, pinned to this
    /// fixture's directories the way the other pty tests pin theirs. No
    /// config home and no XDG base — those are the lead's to be given and the
    /// launch's to carry.
    fn server_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", self.data.path().display().to_string()),
            ("GANJA_PROVIDER", "fake".to_owned()),
            (
                "GANJA_FAKE_SCRIPT",
                self.project.path().join(SCRIPT).display().to_string(),
            ),
            ("GANJA_DISABLE_MODELS_FETCH", "1".to_owned()),
        ]
    }

    /// What the lead's own pane is additionally given (`-e`): where its
    /// things are.
    fn lead_env(&self) -> Vec<String> {
        vec![
            format!("GANJA_CONFIG_HOME={}", self.config_home().display()),
            format!("XDG_DATA_HOME={}", self.data.path().display()),
            format!(
                "XDG_CONFIG_HOME={}",
                self.data.path().join("config").display()
            ),
            format!(
                "XDG_CACHE_HOME={}",
                self.data.path().join("cache").display()
            ),
        ]
    }

    /// The team directory the lead made — the only one under the config
    /// home, named `session-<8hex>` after a session id this test never sees.
    fn team_dir(&self) -> Option<PathBuf> {
        let mut dirs: Vec<PathBuf> = fs::read_dir(self.config_home().join("teams"))
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        assert!(dirs.len() <= 1, "one lead is one team: {dirs:?}");

        dirs.into_iter().next()
    }

    fn team_file(&self) -> Option<TeamFile> {
        let text = fs::read_to_string(self.team_dir()?.join("config.json")).ok()?;

        Some(serde_json::from_str(&text).expect("the team file this build wrote decodes"))
    }

    fn lead_inbox(&self) -> Option<PathBuf> {
        Some(self.team_dir()?.join("inboxes").join("team-lead.json"))
    }
}

/// A tmux server of this test's own, on a socket nobody else knows, killed
/// when dropped — panics included — so a failing test leaves no server
/// holding a `ganja` open.
struct Tmux {
    socket: PathBuf,
    _dir: TempDir,
}

impl Tmux {
    /// Starts a detached server from `env` less `remove` (§10.10: what every
    /// pane inherits), whose first pane sleeps so the server outlives every
    /// pane the test watches.
    fn start(env: &[(&str, String)], remove: &[&str]) -> Self {
        let dir = TempDir::new().expect("a temporary directory is creatable");
        let socket = dir.path().join("tmux.sock");
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
                "200",
                "-y",
                "50",
            ])
            .args(["sleep", "3600"]);
        for (name, value) in env {
            command.env(name, value);
        }
        for name in remove {
            command.env_remove(name);
        }
        let started = command.output().expect("tmux starts a private server");
        assert!(
            started.status.success(),
            "the private tmux server did not start: {}",
            String::from_utf8_lossy(&started.stderr)
        );

        Self { socket, _dir: dir }
    }

    /// One client call, or a panic in tmux's own words.
    fn run(&self, args: &[&str]) -> String {
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
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

    /// Splits a pane in `cwd` running `argv` with `env` added, and returns
    /// its id. `argv` is at least two words, for the reason `pane.rs` gives.
    fn split(&self, cwd: &str, env: &[String], argv: &[&str]) -> String {
        let mut args: Vec<&str> = vec!["split-window", "-d", "-P", "-F", "#{pane_id}", "-c", cwd];
        for pair in env {
            args.push("-e");
            args.push(pair);
        }
        args.push("--");
        args.extend_from_slice(argv);

        self.run(&args).trim().to_owned()
    }

    /// The live pane ids.
    fn panes(&self) -> Vec<String> {
        self.run(&["list-panes", "-a", "-F", "#{pane_id}"])
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The pid of the pane's process — for the lead's pane, the lead.
    fn pane_pid(&self, pane: &str) -> String {
        self.run(&["display-message", "-p", "-t", pane, "#{pane_pid}"])
            .trim()
            .to_owned()
    }

    /// The name of the process in the pane's foreground, as tmux sees it.
    fn current_command(&self, pane: &str) -> String {
        self.run(&[
            "display-message",
            "-p",
            "-t",
            pane,
            "#{pane_current_command}",
        ])
        .trim()
        .to_owned()
    }

    /// The pane's screen, as text.
    fn screen(&self, pane: &str) -> String {
        self.run(&["capture-pane", "-p", "-t", pane])
    }

    /// Types `text` into `pane` literally, then Enter.
    fn type_line(&self, pane: &str, text: &str) {
        self.run(&["send-keys", "-t", pane, "-l", "--", text]);
        self.run(&["send-keys", "-t", pane, "Enter"]);
    }

    /// Presses one named key in `pane`.
    fn key(&self, pane: &str, name: &str) {
        self.run(&["send-keys", "-t", pane, name]);
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

/// Polls `read` every 50ms until it answers, or panics with `what` and the
/// lead's screen after [`DEADLINE`].
fn wait_for<T>(what: &str, tmux: &Tmux, lead: &str, mut read: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(found) = read() {
            return found;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "waited {DEADLINE:?} for {what} and it did not happen; the lead's screen:\n{}",
            tmux.screen(lead)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Every frame the lead's inbox holds right now. An inbox that is not there
/// yet reads as empty (`mailbox::read` swallows `ENOENT`, §2.5), which is
/// why no negative below stands on its own: each is preceded by seeing the
/// frame *present*.
fn lead_holds(inbox: &Path) -> Vec<Frame> {
    mailbox::read(inbox)
        .expect("the lead's inbox reads")
        .valid
        .iter()
        .filter_map(|message| message.frame())
        .collect()
}

/// Sends `signal` to `pid` through the system's own `kill`, so the test needs
/// no `libc` of its own for two lines of job control.
fn signal(pid: &str, signal: &str) {
    let status = Command::new("kill")
        .args([&format!("-{signal}"), pid])
        .status()
        .expect("kill runs");
    assert!(status.success(), "kill -{signal} {pid} failed: {status}");
}

/// A process held still (`SIGSTOP`), let go again however the test ends.
///
/// The release has to be a [`Drop`] rather than a line at the end of the
/// bracket: every `wait_for` between the two signals panics on a timeout, and a
/// trailing `kill -CONT` would then never run. A **stopped** process does not
/// act on the `SIGHUP` a `kill-server` sends it either, so the lead, its pane's
/// `ganja` and the tmux server would all outlive the run — a failing test
/// leaving three processes behind, which is how a suite starts wedging the
/// machine it runs on.
struct Held {
    pid: String,
}

impl Held {
    /// Stops `pid` now, and answers with what will let it go.
    fn stop(pid: &str) -> Self {
        signal(pid, "STOP");

        Self {
            pid: pid.to_owned(),
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // Deliberately not [`signal`]: that one asserts, and a `Drop` that
        // panics while the test is already panicking aborts the process and
        // takes the failure message with it. A `kill` that did not land is
        // nothing this can do anything about anyway.
        let _ = Command::new("kill").args(["-CONT", &self.pid]).status();
    }
}

/// **AC-11.** `/team spawn w1 --backend pane` in a real lead makes a real pane
/// teammate on a private tmux server; the member runs its seeded task and
/// reports; `/team shutdown w1` ends in the lead reading the approval and the
/// pane being gone.
#[test]
fn a_pane_teammate_spawned_with_backend_pane_is_created_and_killed_on_shutdown_approved() {
    require_tmux();
    let fixture = Fixture::new();
    let tmux = Tmux::start(
        &fixture.server_env(),
        &[
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
        ],
    );

    // The lead, in a pane of its own in the project directory — so tmux
    // gives it `TMUX` and `TMUX_PANE` itself. Two words on purpose (`env` and
    // the binary): a one-word command would go through the login shell.
    let project = fixture.project.path().display().to_string();
    let lead = tmux.split(
        &project,
        &fixture.lead_env(),
        &["/usr/bin/env", env!("CARGO_BIN_EXE_ganja")],
    );
    wait_for("the lead to draw its composer", &tmux, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });

    // 1. The spec's own line, typed. The dialog opens and says where the
    // prompt went; the team file names the member on its pane.
    tmux.type_line(&lead, &format!("/team spawn {MEMBER} --backend pane"));
    wait_for("the dialog to report the spawn", &tmux, &lead, || {
        tmux.screen(&lead).contains(CLEARTEXT_NOTICE).then_some(())
    });
    let member = wait_for("the member record", &tmux, &lead, || {
        fixture
            .team_file()?
            .member(MEMBER)
            .cloned()
            .filter(|member| member.tmux_pane_id.starts_with('%'))
    });
    assert_eq!(
        member.backend_type.as_deref(),
        Some("tmux"),
        "the record says it is a pane: {member:?}"
    );
    let pane = member.tmux_pane_id.clone();
    assert_ne!(pane, lead, "the member's pane is not the lead's");

    // 2. tmux agrees, and the pane's process is this binary: the launch line
    // was typed into the idle shell and `exec`'d.
    wait_for("the pane to be listed", &tmux, &lead, || {
        tmux.panes().contains(&pane).then_some(())
    });
    wait_for("the launch line to reach the pane", &tmux, &lead, || {
        (tmux.current_command(&pane) == "ganja").then_some(())
    });

    // 3. The member is a teammate: the seed became its first turn (the fake
    // reply is on its own screen), and the idle_notification it wrote reached
    // the lead — seen **arriving**, then seen **read**. The lead's pass prunes
    // a frame within a second of its arrival, so a poll racing that pass would
    // miss it now and then; instead the lead is held still (SIGSTOP) while the
    // member finishes, the frame is asserted present in the lead's inbox with
    // no lead running to take it, and only then is the lead let go (SIGCONT)
    // and asserted to have read it — the frame gone from an inbox that held
    // it. Two facts in sequence, neither of them a race.
    let lead_pid = tmux.pane_pid(&lead);
    let held = Held::stop(&lead_pid);
    wait_for("the member's seeded turn", &tmux, &lead, || {
        tmux.screen(&pane).contains(REPLY).then_some(())
    });
    let lead_inbox = fixture.lead_inbox().expect("the team exists by now");
    wait_for(
        "the idle notification to reach the lead's inbox",
        &tmux,
        &lead,
        || {
            lead_holds(&lead_inbox)
                .iter()
                .any(|frame| matches!(frame, Frame::IdleNotification(_)))
                .then_some(())
        },
    );
    drop(held);
    wait_for(
        "the lead to read the idle notification",
        &tmux,
        &lead,
        || {
            (!lead_holds(&lead_inbox)
                .iter()
                .any(|frame| matches!(frame, Frame::IdleNotification(_))))
            .then_some(())
        },
    );

    // 4. The handshake through the lead: it asks, the member approves and
    // leaves, and reading the approval is what kills the pane and retires the
    // record. Nothing here touches tmux to make that so. The dialog is closed
    // first — it owns every key while it is up — and the composer is waited
    // for before the next line is typed.
    tmux.key(&lead, "Escape");
    wait_for("the dialog to close", &tmux, &lead, || {
        let screen = tmux.screen(&lead);
        (!screen.contains(CLEARTEXT_NOTICE) && screen.contains(COMPOSER)).then_some(())
    });
    tmux.type_line(&lead, &format!("/team shutdown {MEMBER}"));
    wait_for(
        "the pane to be killed on the approval",
        &tmux,
        &lead,
        || (!tmux.panes().contains(&pane)).then_some(()),
    );
    wait_for("the record to be retired", &tmux, &lead, || {
        fixture
            .team_file()
            .is_some_and(|file| file.member(MEMBER).is_none())
            .then_some(())
    });
    // The record's retirement above is the proof the lead read the approval —
    // nothing else takes a member out of the team file. What is left in the
    // inbox afterwards is the same fact from the other side, and it is waited
    // for rather than asserted at once: the lead's pass retires inside its
    // loop and prunes what it read after it, in a write of its own, so the
    // record can be gone a moment before the frame is.
    wait_for(
        "the lead to prune the approval it read",
        &tmux,
        &lead,
        || {
            (!lead_holds(&lead_inbox)
                .iter()
                .any(|frame| matches!(frame, Frame::ShutdownApproved(_))))
            .then_some(())
        },
    );

    // The lead leaves cleanly, with nothing left to shut down: its pane
    // closes, and only the server's own sleeping pane remains.
    tmux.key(&lead, "Escape");
    wait_for("the dialog to close again", &tmux, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    tmux.key(&lead, "C-c");
    wait_for("the lead to leave", &tmux, &lead, || {
        (!tmux.panes().contains(&lead)).then_some(())
    });
    assert_eq!(
        tmux.panes().len(),
        1,
        "only the server's first pane is left"
    );
    drop(tmux);
}

/// The team the guard's member is spawned into.
const GUARD_TEAM: &str = "session-abcd1234";
/// The lead's session id in that team.
const GUARD_SESSION: &str = "01998ad0-0000-7000-8000-000000000000";

/// The spawn `pane.rs` would compose the launch line from, over `root`.
fn guard_spec(root: &TeamsRoot, cwd: &Path, bypass: bool) -> SpawnSpec {
    SpawnSpec {
        name: MemberName::parse(MEMBER).expect("a member name"),
        team: TeamName::parse(GUARD_TEAM).expect("a team name"),
        lead: MemberName::lead(),
        root: root.clone(),
        backend: MemberBackend::Pane,
        agent_type: "general".to_owned(),
        model: "recorder-model".to_owned(),
        color: "blue".to_owned(),
        prompt: String::new(),
        cwd: cwd.to_path_buf(),
        plan_mode_required: false,
        bypass,
        parent_session_id: GUARD_SESSION.to_owned(),
    }
}

/// Writes the team file a lead would have written before launching `spec`'s
/// member — the record a real member waits for first — so the guard's run
/// gets past that wait and on to the terminal instead of sitting out the
/// wait's whole bound twice.
fn seed_record(spec: &SpawnSpec) {
    let mut file = TeamFile::new(
        &spec.team,
        GUARD_SESSION,
        spec.cwd.display().to_string(),
        record::now_millis(),
    );
    file.members.push(MemberRecord::teammate(
        &spec.name,
        &spec.team,
        Spawn {
            agent_type: spec.agent_type.clone(),
            model: spec.model.clone(),
            color: spec.color.clone(),
            prompt: spec.prompt.clone(),
            plan_mode_required: spec.plan_mode_required,
            surface: Surface::Pane {
                id: "%7".to_owned(),
            },
            cwd: spec.cwd.display().to_string(),
        },
        record::now_millis(),
    ));
    let path = spec.root.config_path(&spec.team);
    fs::create_dir_all(path.parent().expect("a team file has a directory"))
        .expect("the team directory is creatable");
    fs::write(&path, record::document(&file).expect("a team file encodes"))
        .expect("the team file is writable");
}

/// **The spelling guard.** The line `pane.rs` composes is the line `Cli`
/// parses, both without and with `--auto`: the real binary, handed exactly
/// those words and no terminal, gets past clap, past `Membership::resolve`,
/// past the record wait (its record is seeded), and stops only at the
/// terminal it has not got.
///
/// Read in the negative on purpose: clap's usage error is exit code 2 and
/// names the word it did not know; a resolve refusal names the flag it
/// refused; a member nobody recorded says so within its bound. None may
/// appear, and the terminal failure must — which is the one line that can
/// only be reached with every flag parsed and consistent.
#[test]
fn the_launch_line_pane_composes_is_the_line_the_binary_parses() {
    let data = TempDir::new().expect("a temporary directory is creatable");
    let config_home = data.path().join("config").join("ganja");
    let root = TeamsRoot::new(config_home.join("teams"));
    for bypass in [false, true] {
        let spec = guard_spec(&root, data.path(), bypass);
        seed_record(&spec);
        let argv = pane::arguments(&spec);
        let output = Command::new(env!("CARGO_BIN_EXE_ganja"))
            .args(&argv)
            .current_dir(data.path())
            .stdin(std::process::Stdio::null())
            .env("HOME", data.path())
            .env("XDG_DATA_HOME", data.path().join("data"))
            .env("XDG_CONFIG_HOME", data.path().join("config"))
            .env("GANJA_CONFIG_HOME", &config_home)
            .env("GANJA_PROVIDER", "fake")
            .env("GANJA_DISABLE_MODELS_FETCH", "1")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("GANJA_CONFIG")
            .env_remove("GANJA_MODEL")
            .output()
            .expect("the binary runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = argv
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");

        assert_ne!(
            output.status.code(),
            Some(2),
            "clap refused the launch line `{line}`:\n{stderr}"
        );
        for refusal in [
            "unexpected argument",
            "unrecognized",
            "is refused",
            "does not name",
            "no lead wrote a record",
        ] {
            assert!(
                !stderr.contains(refusal),
                "the launch line `{line}` was refused ({refusal}):\n{stderr}"
            );
        }
        assert!(
            stderr.contains("terminal"),
            "the launch line `{line}` should have parsed and resolved and then wanted a \
             terminal; instead:\n{stderr}"
        );
    }
}
