//! **W4's acceptance drill.** A real `ganja`, in a pane, auto-continues while
//! its team holds work — and stops after five.
//!
//! The guard is engine-native and unit-tested at three heights already; what
//! only a whole binary can show is that it survives everything between the
//! agent loop and a person's terminal: the turn really keeps going, the
//! session really becomes idle again afterwards rather than spinning, and the
//! composer comes back. A guard that quietly never fired — or one that never
//! stopped — would pass every in-process test and be unusable.
//!
//! Spec: this port's own team-orchestration behavior specification (decision
//! 11). Neither upstream has an auto-continuation of any kind, so there is
//! nothing of either's to port.
//!
//! **Hard-fails without tmux**, for `team_tasks_pane.rs`'s reason: a pane test
//! that skipped where there is no tmux would be green on exactly the machines
//! where nothing was tested.
//!
//! # What is asserted, in order
//!
//! 1. The lead files a task and its turn **ends** — a session leading nobody
//!    is not a team with work stranded in it.
//! 2. A teammate joins, so the registry holds somebody running.
//! 3. The next turn would end the same way and does not: the guard continues
//!    it, five times, each one saying so once in the log.
//! 4. The sixth is refused, the turn ends, and the composer takes a line
//!    again — the session is back with the person rather than talking to
//!    itself.
//!
//! # One script, entries that do not care where they land
//!
//! A fake-provider script is played one entry per **request**, and a session
//! makes requests this drill never asks for — a title, at a moment of its own
//! choosing, and the teammate's own turn shares this process's provider. So
//! only the first entry does anything, and every entry after it is the same
//! plain reply: whichever request lands on whichever, the answer is a step
//! with no tool calls, which is exactly the step the guard is about. Pinning
//! an index would be pinning a race (W3's finding, kept).

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ganja_testkit::Homes;
use ganja_testkit::tmux::{PrivateServer, require_tmux};
use serde_json::json;

/// How long each stage is given: a debug `ganja` starting cold in a pane, then
/// seven provider round trips against a scripted provider.
const DEADLINE: Duration = Duration::from_secs(60);

/// How many lines of the lead's log a timed-out wait quotes.
const LOG_TAIL: usize = 80;

/// What the engine traces once per auto-continuation.
const CONTINUED: &str = "the team still holds work, so the turn continues";

/// What it traces when the breaker trips, exactly once.
const STOPPED: &str = "the turn stopped auto-continuing and handed the session back";

/// The constant the breaker is set to. Restated here rather than imported
/// because this drill is about the shipped binary's behavior, and a test that
/// read the same constant the code reads could not notice the two coming
/// apart.
const MAX_CONTINUATIONS: usize = 5;

/// The teammate's name.
const MEMBER: &str = "w1";

/// What the lead files.
const SUBJECT: &str = "port the parser";

/// What the composer draws when nothing else owns the screen.
const COMPOSER: &str = "Ask ganja something";

/// The lead's first prompt: the one turn that files the work.
const FILE_PROMPT: &str = "file the work";

/// Its second, after the teammate has joined. Answered with a plain reply, so
/// the only reason the turn does not end here is the guard.
const CARRY_ON: &str = "anything else";

/// The script both conversations play.
const SCRIPT: &str = "turns.json";

/// What the private server is born **without**, so nothing a pane inherits can
/// be this developer's rather than the fixture's.
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

/// The homes, the script, and this drill's reads of what the lead wrote.
struct Fixture {
    homes: Homes,
}

impl Fixture {
    fn new() -> Self {
        let homes = Homes::new();
        // Entry one files the task. Everything after it is the same plain
        // reply — a step with no tool calls, which is the step a turn ends on
        // and therefore the step the guard has an opinion about. Generous in
        // number: the run needs one opening step, five continuations and a
        // title, and an entry too many costs nothing while an entry too few
        // would end the turn for the wrong reason.
        let mut turns = vec![json!({
            "text": "Filing it.",
            "tool_calls": [{"name": "task_create", "args": {
                "subject": SUBJECT,
                "description": "start from the spec",
            }}],
        })];
        turns.extend(std::iter::repeat_n(json!({"text": "Nothing more from me."}), 24));
        homes.script(SCRIPT, json!(turns));

        Self { homes }
    }

    fn config_home(&self) -> PathBuf {
        self.homes.config_home()
    }

    fn log_dir(&self) -> PathBuf {
        self.homes.data().join("ganja").join("log")
    }

    /// The environment the server is born from, and so what every pane
    /// inherits.
    fn server_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", self.homes.data().display().to_string()),
            ("GANJA_PROVIDER", "fake".to_owned()),
            ("GANJA_DISABLE_MODELS_FETCH", "1".to_owned()),
        ]
    }

    /// What the lead's own pane is additionally given (`-e`).
    fn lead_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("GANJA_CONFIG_HOME", self.config_home().display().to_string()),
            ("XDG_DATA_HOME", self.homes.data().display().to_string()),
            ("XDG_CONFIG_HOME", self.homes.data().join("config").display().to_string()),
            ("XDG_CACHE_HOME", self.homes.data().join("cache").display().to_string()),
            ("GANJA_FAKE_SCRIPT", self.homes.project().join(SCRIPT).display().to_string()),
            // What every other pty drill sets (**D517**): a private server born
            // from `-f /dev/null` is a terminal whose answer to the kitty query
            // depends on which tmux the host ships, and the probe blocks up to
            // two seconds where there is none.
            ("GANJA_DISABLE_TERM_PROBE", "1".to_owned()),
        ]
    }

    /// Everything the lead has traced so far.
    fn log(&self) -> String {
        let mut files: Vec<PathBuf> = match fs::read_dir(self.log_dir()) {
            Ok(entries) => entries.filter_map(Result::ok).map(|entry| entry.path()).collect(),
            Err(_) => return String::new(),
        };
        files.sort();

        files.iter().filter_map(|path| fs::read_to_string(path).ok()).collect::<Vec<_>>().join("")
    }

    /// How many times the log says `needle`.
    fn logged(&self, needle: &str) -> usize {
        self.log().matches(needle).count()
    }

    /// The tail of the log, for a wait that timed out.
    fn log_tail(&self) -> String {
        let text = self.log();
        let lines: Vec<&str> = text.lines().collect();
        let kept = lines.len().saturating_sub(LOG_TAIL);

        format!(
            "last {} of {} log lines:\n{}",
            lines.len() - kept,
            lines.len(),
            lines[kept..].join("\n")
        )
    }

    /// Whether the lead's team directory holds any task document yet.
    fn filed_anything(&self) -> bool {
        let Ok(teams) = fs::read_dir(self.config_home().join("teams")) else {
            return false;
        };

        teams
            .filter_map(Result::ok)
            .filter_map(|team| fs::read_dir(team.path().join("tasks")).ok())
            .any(|tasks| {
                tasks.filter_map(Result::ok).any(|entry| {
                    entry.path().extension().is_some_and(|extension| extension == "json")
                })
            })
    }
}

/// The private server plus the send-keys/capture glue.
struct Tmux {
    server: PrivateServer,
}

impl Tmux {
    fn start(fixture: &Fixture) -> Self {
        let env = fixture.server_env();
        let pairs: Vec<(&str, &str)> =
            env.iter().map(|(name, value)| (*name, value.as_str())).collect();

        Self { server: PrivateServer::start(&["sleep", "3600"], WITHHELD, &pairs) }
    }

    fn screen(&self, pane: &str) -> String {
        self.server.run(&["capture-pane", "-p", "-t", pane])
    }

    /// Types `text` into `pane` literally, then Enter.
    fn type_line(&self, pane: &str, text: &str) {
        self.server.run(&["send-keys", "-t", pane, "-l", "--", text]);
        self.server.run(&["send-keys", "-t", pane, "Enter"]);
    }
}

/// Polls `read` every 50ms until it answers, or panics with `what`, the lead's
/// screen and the tail of its log after [`DEADLINE`].
fn wait_for<T>(
    what: &str,
    tmux: &Tmux,
    fixture: &Fixture,
    lead: &str,
    mut read: impl FnMut() -> Option<T>,
) -> T {
    let started = Instant::now();
    loop {
        if let Some(found) = read() {
            return found;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "waited {DEADLINE:?} for {what} and it did not happen; the lead's screen:\n{}\n{}",
            tmux.screen(lead),
            fixture.log_tail(),
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A real `ganja` auto-continues while its team holds work, and the breaker
/// stops it after five.
#[test]
fn a_lead_auto_continues_for_its_team_and_the_breaker_halts_it_at_five() {
    require_tmux();
    let fixture = Fixture::new();
    let tmux = Tmux::start(&fixture);
    let env = fixture.lead_env();
    let pairs: Vec<(&str, &str)> =
        env.iter().map(|(name, value)| (*name, value.as_str())).collect();

    // The lead, in a pane of its own in the project directory. Two words on
    // purpose (`env` and the binary): a one-word command would go through the
    // login shell.
    let lead = tmux.server.split(
        Some(fixture.homes.project()),
        &pairs,
        &["/usr/bin/env", env!("CARGO_BIN_EXE_ganja")],
    );
    wait_for("the lead to draw its composer", &tmux, &fixture, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });

    // 1. The work is filed before there is anybody to do it, which is the
    // order the pipeline actually runs in — and that turn **ends**: open work
    // with nobody running is a list, not a stranded team.
    tmux.type_line(&lead, FILE_PROMPT);
    wait_for("the lead to file the task", &tmux, &fixture, &lead, || {
        fixture.filed_anything().then_some(())
    });
    wait_for("the first turn to end with no continuation", &tmux, &fixture, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    assert_eq!(
        fixture.logged(CONTINUED),
        0,
        "nobody was running, so nothing was stranded: {}",
        fixture.log_tail(),
    );

    // 2. Somebody joins. In-process rather than a second pane on purpose: what
    // the guard reads is the registry, and a second binary would only add a
    // second thing that can be slow.
    tmux.type_line(&lead, &format!("/teammate spawn {MEMBER} --backend in-process"));

    // 3. The next turn would end exactly as the first one did. It does not.
    wait_for("the composer to take the next line", &tmux, &fixture, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
    tmux.type_line(&lead, CARRY_ON);
    wait_for("the breaker to trip", &tmux, &fixture, &lead, || {
        (fixture.logged(STOPPED) > 0).then_some(())
    });

    assert_eq!(
        fixture.logged(CONTINUED),
        MAX_CONTINUATIONS,
        "five auto-continuations, then the budget is spent: {}",
        fixture.log_tail(),
    );
    assert_eq!(
        fixture.logged(STOPPED),
        1,
        "and the breaker says so once, at the moment it trips: {}",
        fixture.log_tail(),
    );

    // 4. And the session is back with the person: the turn really ended
    // rather than continuing quietly with the notice suppressed.
    wait_for("the composer to come back after the breaker", &tmux, &fixture, &lead, || {
        tmux.screen(&lead).contains(COMPOSER).then_some(())
    });
}
