//! A lead session's socket, end to end through the real binary (**D505**,
//! "one socket per session"): a lead in a pty binds its socket in the
//! directory it was told, the socket answers health with the lead's own
//! session id, `/new` moves the socket to the new id, exit unlinks it, a pane
//! member binds nothing, and a directory the binder must refuse costs the
//! session its socket and nothing else.
//!
//! No upstream counterpart: opencode's TUI binds no socket. The spec is D-12
//! and the plan's "one socket per session"; the binder itself is
//! `ganja-cli/src/binder.rs` over `ganja-tui/src/binder.rs`'s seam.
//!
//! **The witness is `ganja sessions --live --socket-dir <dir>`** — the same
//! binary, reading the same directory through `ganja-client`'s socket form —
//! so what is asserted is the wire a peer would use, not a file's existence.
//! Which session the socket names is cross-checked against `ganja sessions`,
//! the stored listing, once a prompt has earned the lead a row: the two
//! listings agreeing is what says the socket is *this* session's and not one
//! minted beside it.
//!
//! Every process here runs the fake provider — a two-turn script, one word
//! each — in a project, data home and config home of its own, with
//! `TMUX`/`TMUX_PANE` withheld so a lead started from inside a developer's
//! tmux sweeps and spawns nothing real, and the socket directory is a private
//! one under the fixture — never the developer's `/tmp/ganja-<uid>/`, which
//! `sessions --live` would otherwise list and prune. Nothing calls
//! `std::env::set_var`.
//!
//! **A turn's end is read off a `Stop` hook**, not off the screen: the
//! project config appends a line to a ledger at every `Stop`, and a turn is
//! over when the ledger has grown — which is what makes the `/new` after it
//! land on an idle engine rather than be refused `Busy` while the fake reply
//! is still settling. The screen is used only to see the reply arrive, and
//! it is **drained while anything else is waited for**: a process whose
//! frames nobody reads blocks on its own stdout, and a lead blocked there
//! handles no key and rebinds nothing — which is not a finding about the
//! binder.
//!
//! Two sessions minted in one 65-second UUIDv7 bucket share their first
//! eight hex digits, so after `/new` the *file name* may be the same as
//! before; what the rebind is asserted on is the id the socket answers with,
//! which is the id `ganja sessions` stores.

#![cfg(unix)]

use std::{
    fs,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use expectrl::{
    ControlCode, Eof, Expect as _, Session, process::unix::WaitStatus, session::OsSession,
};
use ganja_core::team::{
    MemberName, MemberRecord, Spawn, Surface, TeamFile, TeamName, TeamsRoot, record,
};
use ganja_serve::socket::EXTENSION;
use tempfile::TempDir;

/// How long a debug `ganja` is given to come up, bind, answer, or leave.
const DEADLINE: Duration = Duration::from_secs(20);

/// The escape that opens the alternate screen — the app has the terminal.
const ALT_SCREEN: &str = "\x1b[?1049h";

/// The two scripted replies, one word each and each appearing nowhere else,
/// so a fragment carries the whole word and the second cannot be mistaken
/// for a redraw of the first.
const REPLIES: [&str; 2] = ["reply-one-zarquon", "reply-two-zarquon"];

/// Where the fake script and the `Stop` ledger live, inside the project.
const SCRIPT: &str = "script.json";
const LEDGER: &str = "stops";

/// A prompt that appears nowhere else on screen.
const PROMPT: &str = "socket-drill-prompt";

/// The name and team a member is seeded under.
const MEMBER: &str = "w1";
const TEAM: &str = "session-abcd1234";
const LEAD_SESSION: &str = "01998ad0-0000-7000-8000-000000000000";

/// A project, a data home and a config home that vanish with the test, and
/// the private socket directory the binder is pointed at — created by the
/// binder itself, at `0700`, so the same code path a real lead takes is the
/// one under test.
struct Fixture {
    project: TempDir,
    home: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let project = TempDir::new().expect("a temporary directory is creatable");
        fs::create_dir(project.path().join(".git")).expect("the checkout marker is creatable");
        fs::write(
            project.path().join(SCRIPT),
            serde_json::json!({
                "cadence_ms": 1,
                "turns": REPLIES.iter().map(|text| serde_json::json!({"text": text})).collect::<Vec<_>>(),
            })
            .to_string(),
        )
        .expect("the script is writable");
        // The hook reads its envelope so the engine's write never sees a
        // closed pipe, then appends one line: the turn count, for `Ganja`.
        let ledger = project.path().join(LEDGER);
        fs::write(
            project.path().join("ganja.jsonc"),
            serde_json::json!({
                "hooks": {
                    "Stop": [{ "hooks": [{
                        "type": "command",
                        "command": format!("{{ cat >/dev/null; echo stop; }} >> {}", ledger.display()),
                    }] }],
                }
            })
            .to_string(),
        )
        .expect("the project config is writable");
        let home = TempDir::new().expect("a temporary directory is creatable");
        fs::create_dir_all(home.path().join("config").join("ganja"))
            .expect("the config home is creatable");

        Self { project, home }
    }

    fn config_home(&self) -> PathBuf {
        self.home.path().join("config").join("ganja")
    }

    fn sockets(&self) -> PathBuf {
        self.home.path().join("sockets")
    }

    /// The binary, in this fixture's project and homes, provider fake.
    fn ganja(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
        command
            .current_dir(self.project.path())
            .env("GANJA_PROVIDER", "fake")
            .env("GANJA_DISABLE_MODELS_FETCH", "1")
            .env("HOME", self.home.path())
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_CACHE_HOME", self.home.path().join("cache"))
            .env("GANJA_CONFIG_HOME", self.config_home())
            .env("GANJA_FAKE_SCRIPT", self.project.path().join(SCRIPT))
            .env_remove("GANJA_CONFIG")
            .env_remove("GANJA_MODEL")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");

        command
    }

    /// The UI as a lead, its socket bound under `sockets`.
    fn lead(&self, sockets: &Path) -> Ganja {
        let mut command = self.ganja();
        command.arg("--socket-dir").arg(sockets);

        Ganja::spawn(command, self.project.path().join(LEDGER))
    }

    /// How many turns have ended, by the `Stop` ledger.
    fn stops(ledger: &Path) -> usize {
        fs::read_to_string(ledger).map_or(0, |text| text.lines().count())
    }

    /// `ganja sessions --live --socket-dir <sockets>`: every `(session id,
    /// socket path)` the listing printed.
    fn live(&self, sockets: &Path) -> Vec<(String, PathBuf)> {
        let output = self
            .ganja()
            .args(["sessions", "--live", "--socket-dir"])
            .arg(sockets)
            .output()
            .expect("the listing runs");
        assert!(
            output.status.success(),
            "sessions --live failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);

        stdout
            .lines()
            .skip_while(|line| !line.starts_with("SESSION"))
            .skip(1)
            .filter_map(|line| {
                let (session, path) = line.split_once("  ")?;
                Some((session.trim().to_owned(), PathBuf::from(path.trim())))
            })
            .collect()
    }

    /// `ganja sessions`: the ids of this project's stored sessions.
    fn stored(&self) -> Vec<String> {
        let output = self
            .ganja()
            .arg("sessions")
            .output()
            .expect("the listing runs");
        assert!(
            output.status.success(),
            "sessions failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);

        stdout
            .lines()
            .skip_while(|line| !line.starts_with("SESSION"))
            .skip(1)
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect()
    }

    /// The socket files under `sockets` — the `.sock` entries, never the lock
    /// siblings the binder keeps beside them.
    fn socket_files(sockets: &Path) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(sockets) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some(EXTENSION))
            .collect();
        files.sort();

        files
    }
}

/// Polls `check` until it answers, or fails naming `what`, draining
/// `session`'s pty between polls so the process under test never blocks on
/// a frame nobody read.
fn wait_for<T>(session: &mut Ganja, what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        session.drain();
        if let Some(found) = check() {
            return found;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "gave up waiting for {what} after {DEADLINE:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// A `ganja` process in a pty, reaped however the test that owns it ends.
struct Ganja {
    session: Option<OsSession>,
    /// The `Stop` ledger its turns append to.
    ledger: PathBuf,
    /// How many turns this process has taken, which is also the index of the
    /// scripted reply the next one gets.
    turns: usize,
}

impl Ganja {
    fn spawn(command: Command, ledger: PathBuf) -> Self {
        let mut session = Session::spawn(command).expect("failed to spawn `ganja` in a pty");
        session.set_expect_timeout(Some(DEADLINE));
        session
            .get_process_mut()
            .set_window_size(80, 40)
            .expect("failed to size the pty");
        session
            .expect(ALT_SCREEN)
            .expect("`ganja` never took the terminal over");

        Self {
            session: Some(session),
            ledger,
            turns: 0,
        }
    }

    /// Reads whatever the process has drawn since the last read, and drops
    /// it: the screen is not what this suite asserts on, but a frame left
    /// unread is a process blocked on its stdout.
    fn drain(&mut self) {
        let mut buffer = [0u8; 8192];
        while let Ok(read) = self.try_read(&mut buffer) {
            if read == 0 {
                break;
            }
        }
    }

    /// Types `PROMPT`, submits it, waits for the scripted reply and then for
    /// the turn's `Stop` — a whole turn, over, which is what earns the
    /// session its stored row and leaves the engine idle for what follows.
    fn take_a_turn(&mut self) {
        let before = Fixture::stops(&self.ledger);
        let reply = REPLIES[self.turns];
        self.turns += 1;
        self.send(PROMPT).expect("failed to type the prompt");
        self.send("\r").expect("failed to send Enter");
        self.expect(reply)
            .expect("the scripted reply never reached the transcript");
        let ledger = self.ledger.clone();
        wait_for(self, "the turn's Stop hook", || {
            (Fixture::stops(&ledger) > before).then_some(())
        });
    }

    /// `/new`, submitted, until the socket under `sockets` answers with a
    /// session other than `previous` — which is also the rebind assertion.
    ///
    /// Submitted more than once when it has to be: the `Stop` ledger says a
    /// turn's hooks ran, and the slot releases a few awaits after that, so a
    /// `/new` typed on the ledger's word alone can still be refused `Busy`
    /// (a refusal is a status-bar sentence, and nothing here reads the
    /// bar). Each submission is given a window in which a rebind, if it
    /// happened, is certainly observable before the next is typed; a second
    /// `/new` that lands anyway only mints a third session, which every
    /// assertion after this tolerates by reading the *live* id back rather
    /// than trusting the one returned here.
    fn new_session_until_moved(
        &mut self,
        fixture: &Fixture,
        sockets: &Path,
        previous: &str,
    ) -> (String, PathBuf) {
        let start = Instant::now();
        loop {
            self.send("/new").expect("failed to type /new");
            self.send("\r").expect("failed to send Enter");
            let window = Instant::now();
            while window.elapsed() < Duration::from_secs(3) {
                self.drain();
                let live = fixture.live(sockets);
                if live.len() == 1 && live[0].0 != previous {
                    return live[0].clone();
                }
                thread::sleep(Duration::from_millis(100));
            }
            assert!(
                start.elapsed() < DEADLINE,
                "gave up waiting for /new to move the socket after {DEADLINE:?}"
            );
        }
    }

    fn quit_and_assert_clean_exit(mut self) {
        let mut session = self
            .session
            .take()
            .expect("a session is only ever taken once");
        session
            .send(ControlCode::EndOfText)
            .expect("failed to send Ctrl-C");
        session
            .expect(Eof)
            .expect("`ganja` did not exit within the deadline");
        let status = session
            .get_process()
            .wait()
            .expect("failed to reap the `ganja` process");
        assert!(
            matches!(status, WaitStatus::Exited(_, 0)),
            "expected a clean exit, got {status:?}"
        );
    }
}

impl Deref for Ganja {
    type Target = OsSession;

    fn deref(&self) -> &Self::Target {
        self.session.as_ref().expect("the session outlives its use")
    }
}

impl DerefMut for Ganja {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.session.as_mut().expect("the session outlives its use")
    }
}

impl Drop for Ganja {
    fn drop(&mut self) {
        if let Some(mut session) = self.session.take() {
            let _ = session.get_process_mut().exit(true);
        }
    }
}

/// The lead binds under its own id, `/new` moves the socket to the new id
/// (the old one gone, exactly one socket at every point), and exit unlinks
/// what was bound.
#[test]
fn a_lead_binds_its_session_socket_rebinds_on_new_and_unlinks_on_exit() {
    let fixture = Fixture::new();
    let sockets = fixture.sockets();
    let mut lead = fixture.lead(&sockets);

    // Bound at startup: one socket, answering health with an id.
    let (first, first_path) = wait_for(&mut lead, "the lead's socket to answer health", || {
        let live = fixture.live(&sockets);
        (live.len() == 1).then(|| live[0].clone())
    });
    assert_eq!(
        Fixture::socket_files(&sockets),
        vec![first_path.clone()],
        "one socket file, and it is the one that answered"
    );

    // Whose id: after a turn the stored listing holds exactly this session.
    lead.take_a_turn();
    let stored = wait_for(&mut lead, "the turn's row to be stored", || {
        let stored = fixture.stored();
        (!stored.is_empty()).then_some(stored)
    });
    assert_eq!(
        stored,
        vec![first.clone()],
        "the socket answers with the id the store holds for this lead — no other"
    );

    // `/new`: the slot moves, and the socket follows it.
    let (second, second_path) = lead.new_session_until_moved(&fixture, &sockets, &first);
    assert_ne!(second, first);
    assert_eq!(
        Fixture::socket_files(&sockets),
        vec![second_path],
        "still one socket, and no file of the old bind left beside it"
    );
    // Whose id, again: the turn taken now lands on whatever session the
    // socket names *now* — read back live rather than trusted from above —
    // and that session is stored beside the first, and is not the first.
    lead.take_a_turn();
    let (current, stored) = wait_for(&mut lead, "the new session's row to be stored", || {
        let live = fixture.live(&sockets);
        let stored = fixture.stored();
        (live.len() == 1 && stored.len() >= 2).then(|| (live[0].0.clone(), stored))
    });
    assert_ne!(current, first, "the socket left the first session");
    assert!(
        stored.contains(&current),
        "the socket names the session the turn was stored under: {current} not in {stored:?}"
    );

    // Exit: the file is unlinked; the lock sibling is kept by design.
    lead.quit_and_assert_clean_exit();
    assert!(
        Fixture::socket_files(&sockets).is_empty(),
        "exit unlinks the socket: {:?}",
        Fixture::socket_files(&sockets)
    );
    assert!(
        fixture.live(&sockets).is_empty(),
        "and nothing is listed as live"
    );
}

/// A pane member is addressed through its lead's team and binds no socket
/// of its own, whatever `--socket-dir` says.
#[test]
fn a_pane_member_binds_no_socket() {
    let fixture = Fixture::new();
    let sockets = fixture.sockets();
    seed_member_record(&fixture);

    let mut command = fixture.ganja();
    command
        .args([
            "--agent-id",
            &format!("{MEMBER}@{TEAM}"),
            "--agent-name",
            MEMBER,
            "--team-name",
            TEAM,
            "--parent-session-id",
            LEAD_SESSION,
            "--socket-dir",
        ])
        .arg(&sockets);
    let mut member = Ganja::spawn(command, fixture.project.path().join(LEDGER));
    // A whole turn, so the startup — every seam a lead would bind at — is
    // provably behind us before the directory is read.
    member.take_a_turn();

    assert!(
        !sockets.exists(),
        "a member created no socket directory: {:?}",
        Fixture::socket_files(&sockets)
    );
    member.quit_and_assert_clean_exit();
    assert!(!sockets.exists(), "and still none at exit");
}

/// A socket directory the binder must refuse — a plain file where the
/// directory would be — costs the lead its socket and nothing else: the
/// session comes up, takes a turn, and leaves cleanly.
#[test]
fn a_refused_socket_directory_does_not_cost_the_session() {
    let fixture = Fixture::new();
    let sockets = fixture.sockets();
    fs::write(&sockets, b"not a directory").expect("the decoy is writable");

    let mut lead = fixture.lead(&sockets);
    lead.take_a_turn();
    let stored = wait_for(&mut lead, "the turn's row to be stored", || {
        let stored = fixture.stored();
        (!stored.is_empty()).then_some(stored)
    });
    assert_eq!(stored.len(), 1, "the session ran, unserved");
    assert_eq!(
        fs::read(&sockets).expect("the decoy is readable"),
        b"not a directory",
        "the decoy was neither replaced nor removed"
    );

    lead.quit_and_assert_clean_exit();
}

/// Writes the team file a lead would have written before launching the
/// member — the record a real member waits for first.
fn seed_member_record(fixture: &Fixture) {
    let root = TeamsRoot::new(fixture.config_home().join("teams"));
    let team = TeamName::parse(TEAM).expect("a team name");
    let name = MemberName::parse(MEMBER).expect("a member name");
    let cwd = fixture.project.path().display().to_string();
    let mut file = TeamFile::new(&team, LEAD_SESSION, cwd.clone(), record::now_millis());
    file.members.push(MemberRecord::teammate(
        &name,
        &team,
        Spawn {
            agent_type: "general".to_owned(),
            model: "fake-model".to_owned(),
            color: "blue".to_owned(),
            prompt: String::new(),
            plan_mode_required: false,
            surface: Surface::Pane {
                id: "%7".to_owned(),
            },
            cwd,
        },
        record::now_millis(),
    ));
    let path = root.config_path(&team);
    fs::create_dir_all(path.parent().expect("a team file has a directory"))
        .expect("the team directory is creatable");
    fs::write(&path, record::document(&file).expect("a team file encodes"))
        .expect("the team file is writable");
}
