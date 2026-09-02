//! `/teammate spawn w1 --backend ganja` makes a real pane teammate, and its
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
//! registry alone, is `ganja-teammate-local/tests/teammate_pane_lifecycle.rs`; this is
//! the door a person uses.
//!
//! **Hard-fails without tmux.** A pane test that skipped where there was no
//! tmux would be green on exactly the machines where nothing was tested.
//!
//! # What is asserted, in order
//!
//! 1. The lead answers the spawn on its status bar (the D-7 cleartext
//!    notice — a typed line raises no dialog), and the team file names `w1`
//!    on a `tmuxPaneId` with `backendType: "tmux"`.
//! 2. The private server lists that pane, and the process in it is `ganja` —
//!    the launch line was typed into the idle shell and `exec`'d.
//! 3. The member took the seeded task as its first turn: the fake reply is
//!    on the pane's own screen, and the `idle_notification` it wrote reached
//!    the lead — seen present in the lead's inbox while the lead is held
//!    still, then seen gone once the lead is let go and its pass has read
//!    it.
//! 4. `/teammate shutdown w1` — the lead asks, the member approves and leaves, the
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
//! D502 mechanism working through the real binary; `teammate_session.rs`'s
//! pane leg pins the same fact with the launch line composed by hand, and
//! `teammate_env.rs` from `ps`.
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

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ganja_core::team::{MemberName, Spawn, Surface, TeamName, TeamsRoot, mailbox};
use ganja_core::teammate::SpawnSpec;
use ganja_protocol::team::{Frame, MemberBackend};
use ganja_teammate_local::pane;
use serde_json::json;
use tempfile::TempDir;

mod pane_lead;

use pane_lead::{COMPOSER, Homes, SPAWN_NOTICE, Tmux};

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

/// The shared project/data pair, plus this suite's own read of the inbox the
/// lead keeps under its config home.
struct Fixture {
    homes: Homes,
}

impl Fixture {
    fn new() -> Self {
        let homes = Homes::new();
        homes.script(SCRIPT, json!([{"text": REPLY}]));

        Self { homes }
    }

    /// The environment the **server** is born from, and so what every pane
    /// inherits: the member's script among it, since the member's pane is one
    /// the lead makes rather than one this test does.
    fn server_env(&self) -> Vec<(&'static str, String)> {
        pane_lead::server_env(&self.homes, Some(&self.homes.project().join(SCRIPT)))
    }

    /// What the lead's own pane is additionally given (`-e`): where its
    /// things are. No script of its own — the lead's own turns are not what
    /// this suite reads.
    fn lead_env(&self) -> Vec<(&'static str, String)> {
        pane_lead::lead_env(&self.homes, None)
    }

    fn lead_inbox(&self) -> Option<PathBuf> {
        Some(pane_lead::team_dir(&self.homes)?.join("inboxes").join("team-lead.json"))
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
    let status =
        Command::new("kill").args([&format!("-{signal}"), pid]).status().expect("kill runs");
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

        Self { pid: pid.to_owned() }
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

/// **D520.** `teammates.shell` names the idle shell a pane holds until its
/// launch line arrives. With bash named in the lead's own config, the typed
/// line is still read and exec'd — the pane's process becomes this binary
/// exactly as it does under the default `/bin/sh -s`.
#[test]
fn a_configured_pane_shell_still_execs_the_launch_line() {
    let fixture = Fixture::new();
    let config_home = fixture.homes.config_home();
    fs::create_dir_all(&config_home).expect("the config home is creatable");
    fs::write(config_home.join("ganja.toml"), "[teammates]\nshell = \"/bin/bash\"\n")
        .expect("the config is writable");
    let tmux = Tmux::start(&fixture.homes, &fixture.server_env(), DEADLINE);

    let lead = tmux.lead(&fixture.homes, &fixture.lead_env());

    tmux.type_line(&lead, &format!("/teammate spawn {MEMBER} --backend ganja"));
    let member = tmux.wait_for("the member record", &lead, || {
        pane_lead::team_file(&fixture.homes)?
            .member(MEMBER)
            .cloned()
            .filter(|member| member.tmux_pane_id.starts_with('%'))
    });
    let pane = member.tmux_pane_id.clone();
    // On the member's pane, since that is the process this waits on: a shell
    // that read the line but did not exec it is only visible there. The
    // lead's screen is quoted beside it anyway.
    tmux.wait_for("the launch line to reach the pane through bash", &pane, || {
        (tmux.current_command(&pane) == "ganja").then_some(())
    });
}

/// **AC-11.** `/teammate spawn w1 --backend ganja` in a real lead makes a real pane
/// teammate on a private tmux server; the member runs its seeded task and
/// reports; `/teammate shutdown w1` ends in the lead reading the approval and the
/// pane being gone.
#[test]
fn a_pane_teammate_spawned_with_backend_ganja_is_created_and_killed_on_shutdown_approved() {
    let fixture = Fixture::new();
    let tmux = Tmux::start(&fixture.homes, &fixture.server_env(), DEADLINE);

    // The lead, in a pane of its own in the project directory — so tmux gives
    // it `TMUX` and `TMUX_PANE` itself.
    let lead = tmux.lead(&fixture.homes, &fixture.lead_env());

    // 1. The spec's own line, typed. The bar says where the prompt went — no
    // dialog is raised for a line that already said what it wanted — and the
    // team file names the member on its pane.
    tmux.type_line(&lead, &format!("/teammate spawn {MEMBER} --backend ganja"));
    tmux.wait_for("the spawn to be reported", &lead, || {
        tmux.screen(&lead).contains(&format!("{MEMBER} {SPAWN_NOTICE}")).then_some(())
    });
    let member = tmux.wait_for("the member record", &lead, || {
        pane_lead::team_file(&fixture.homes)?
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
    // The listing wait stays on the lead: a pane that never appeared has no
    // screen to quote, and what would explain it is the lead's bar.
    tmux.wait_for("the pane to be listed", &lead, || tmux.panes().contains(&pane).then_some(()));
    tmux.wait_for("the launch line to reach the pane", &pane, || {
        (tmux.current_command(&pane) == "ganja").then_some(())
    });

    // The teammate opens **beside** the lead rather than under it: the same
    // top row, further right. Asserted on tmux's geometry rather than on the
    // `-h` in an argv, because the flag reads backwards and a test that
    // repeated it would agree with a mistake as readily as with the layout.
    let (lead_left, lead_top) = tmux.corner(&lead);
    let (member_left, member_top) = tmux.corner(&pane);
    assert_eq!(member_top, lead_top, "a teammate shares the lead's top row: | lead | {MEMBER} |");
    assert!(
        member_left > lead_left,
        "and sits to its right: lead at column {lead_left}, {MEMBER} at {member_left}"
    );

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
    tmux.wait_for("the member's seeded turn", &pane, || {
        tmux.screen(&pane).contains(REPLY).then_some(())
    });
    let lead_inbox = fixture.lead_inbox().expect("the team exists by now");
    tmux.wait_for("the idle notification to reach the lead's inbox", &lead, || {
        lead_holds(&lead_inbox)
            .iter()
            .any(|frame| matches!(frame, Frame::IdleNotification(_)))
            .then_some(())
    });
    drop(held);
    tmux.wait_for("the lead to read the idle notification", &lead, || {
        (!lead_holds(&lead_inbox).iter().any(|frame| matches!(frame, Frame::IdleNotification(_))))
            .then_some(())
    });

    // 4. The handshake through the lead: it asks, the member approves and
    // leaves, and reading the approval is what kills the pane and retires the
    // record. Nothing here touches tmux to make that so. Nothing is dismissed
    // first: a typed `/teammate spawn` raises no dialog, so the composer never
    // stopped owning the keyboard — it is waited for all the same, because
    // typing the next line into a frame that has not caught up is a race.
    tmux.wait_for("the composer to take the next line", &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    tmux.type_line(&lead, &format!("/teammate shutdown {MEMBER}"));
    tmux.wait_for("the pane to be killed on the approval", &lead, || {
        (!tmux.panes().contains(&pane)).then_some(())
    });
    tmux.wait_for("the record to be retired", &lead, || {
        pane_lead::team_file(&fixture.homes)
            .is_some_and(|file| file.member(MEMBER).is_none())
            .then_some(())
    });
    // The record's retirement above is the proof the lead read the approval —
    // nothing else takes a member out of the team file. What is left in the
    // inbox afterwards is the same fact from the other side, and it is waited
    // for rather than asserted at once: the lead's pass retires inside its
    // loop and prunes what it read after it, in a write of its own, so the
    // record can be gone a moment before the frame is.
    tmux.wait_for("the lead to prune the approval it read", &lead, || {
        (!lead_holds(&lead_inbox).iter().any(|frame| matches!(frame, Frame::ShutdownApproved(_))))
            .then_some(())
    });

    // The lead leaves cleanly, with nothing left to shut down: its pane
    // closes, and only the server's own sleeping pane remains.
    tmux.wait_for("the composer to come back", &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    tmux.key(&lead, "C-c");
    tmux.wait_for("the lead to leave", &lead, || (!tmux.panes().contains(&lead)).then_some(()));
    assert_eq!(tmux.panes().len(), 1, "only the server's first pane is left");
}

/// The team the guard's member is spawned into.
const GUARD_TEAM: &str = "session-abcd1234";
/// The lead's session id in that team.
const GUARD_SESSION: &str = "01998ad0-0000-7000-8000-000000000000";

/// The spawn `pane.rs` would compose the launch line from, over `root`.
fn guard_spec(root: &TeamsRoot, cwd: &Path) -> SpawnSpec {
    SpawnSpec {
        name: MemberName::parse(MEMBER).expect("a member name"),
        team: TeamName::parse(GUARD_TEAM).expect("a team name"),
        lead: MemberName::lead(),
        root: root.clone(),
        backend: MemberBackend::Ganja,
        agent_type: "general".to_owned(),
        model: "recorder-model".to_owned(),
        color: "blue".to_owned(),
        prompt: String::new(),
        cwd: cwd.to_path_buf(),
        plan_mode_required: false,
        parent_session_id: GUARD_SESSION.to_owned(),
    }
}

/// Writes the team file a lead would have written before launching `spec`'s
/// member — the record a real member waits for first — so the guard's run
/// gets past that wait and on to the terminal instead of sitting out the
/// wait's whole bound twice.
fn seed_record(spec: &SpawnSpec) {
    ganja_testkit::seed_team_file(
        &spec.root,
        &spec.team,
        GUARD_SESSION,
        &spec.cwd,
        &[(
            spec.name.clone(),
            Spawn {
                agent_type: spec.agent_type.clone(),
                model: spec.model.clone(),
                color: spec.color.clone(),
                prompt: spec.prompt.clone(),
                plan_mode_required: spec.plan_mode_required,
                surface: Surface::Pane { id: "%7".to_owned() },
                cwd: spec.cwd.display().to_string(),
            },
        )],
    );
}

/// Types one `/teammate spawn <name> --backend ganja` at the lead and answers with
/// the pane the team file names for it, once the lead has reported the spawn
/// **finished**.
///
/// The bar's notice first, and only then the record. The record alone is too
/// early: the registry writes it *before* the launch, and the lead's own
/// spawn task is still running — and being polled by the tick — after it is
/// on disk. A second `/teammate spawn` typed inside that window is refused as
/// busy by design (`App::spawn_teammate`), which on a loaded runner is exactly
/// where the next line landed: the second teammate never existed, and the
/// first one's notice, arriving a tick later, overwrote the refusal. The
/// notice names the teammate, so waiting on `<name> started` cannot read the
/// previous teammate's line as this one's.
fn spawn_pane(tmux: &Tmux, lead: &str, fixture: &Fixture, name: &str) -> String {
    tmux.type_line(lead, &format!("/teammate spawn {name} --backend ganja"));

    tmux.wait_for(&format!("the spawn of {name} to be reported"), lead, || {
        tmux.screen(lead).contains(&format!("{name} {SPAWN_NOTICE}")).then_some(())
    });

    tmux.wait_for(&format!("the record for {name}"), lead, || {
        pane_lead::team_file(&fixture.homes)?
            .member(name)
            .cloned()
            .filter(|member| member.tmux_pane_id.starts_with('%'))
    })
    .tmux_pane_id
}

/// **The teammates' column.** One column beside the lead, filling downwards.
///
/// Two teammates rather than one, because the second is the half worth
/// proving: opening a column is what a lone `-h` does by itself, while putting
/// the *next* pane inside that column is what a wrong target would get wrong
/// silently — by opening a second column, which still looks like a split.
///
/// Asserted on tmux's geometry rather than on the argv, for the reason the
/// suite above gives: a test that repeated the flags would agree with a
/// mistake as readily as with the layout.
#[test]
fn teammates_stack_in_one_column_beside_the_lead() {
    let fixture = Fixture::new();
    let tmux = Tmux::start(&fixture.homes, &fixture.server_env(), DEADLINE);
    let lead = tmux.lead(&fixture.homes, &fixture.lead_env());

    let first = spawn_pane(&tmux, &lead, &fixture, "w1");
    let second = spawn_pane(&tmux, &lead, &fixture, "w2");

    let (lead_left, lead_top) = tmux.corner(&lead);
    let (first_left, first_top) = tmux.corner(&first);
    let (second_left, second_top) = tmux.corner(&second);

    assert!(
        first_left > lead_left,
        "the column opens right of the lead: lead at column {lead_left}, w1 at {first_left}"
    );
    assert_eq!(first_top, lead_top, "and level with it: | lead | w1 |");
    assert_eq!(
        second_left, first_left,
        "the second teammate joins that column instead of opening another: \
         w1 at column {first_left}, w2 at {second_left}"
    );
    assert!(
        second_top > first_top,
        "and stacks under the first: w1 at row {first_top}, w2 at {second_top}"
    );
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
    let spec = guard_spec(&root, data.path());
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
    let line =
        argv.iter().map(|word| word.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");

    assert_ne!(output.status.code(), Some(2), "clap refused the launch line `{line}`:\n{stderr}");
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
