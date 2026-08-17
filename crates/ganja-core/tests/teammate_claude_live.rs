//! A real `claude` pane, spawned and answered over the shared inbox
//! (**AC-13**).
//!
//! Spec: Claude Code's teammates — §4.1's spawn sequence with its own flags,
//! §2.1's `$CLAUDE_CONFIG_DIR/teams` layout, §3's mailbox and §5.5.1's "the
//! lead is addressed by name". Upstream opencode has no teammates and no
//! counterpart to any of it.
//!
//! # Why this one is live-gated, and what that costs
//!
//! Every other claim in this landing is checked against documents *this build*
//! writes, which proves the shape and proves nothing about interop: the only
//! witness that a real `claude` reads what `ganja-team` writes is a real
//! `claude`. That binary is proprietary, versioned outside this repository and
//! absent from CI, so this test is `#[ignore]`d **and** inert unless
//! `GANJA_LIVE_TEST=1` — the same two-lock shape `tests/live.rs` uses for a
//! paid provider. A machine without `claude` or without `tmux` therefore never
//! runs it, and a green suite never claims it did.
//!
//! Run it deliberately:
//!
//! ```sh
//! GANJA_LIVE_TEST=1 cargo test -p ganja-core --test teammate_claude_live -- --ignored
//! ```
//!
//! It lives in `ganja-core`'s tests beside the two `ganja` pane binaries rather
//! than in `ganja-cli`'s, where the plan's ownership table first placed it: the
//! whole claim is about a `ganja-core` backend, the CLI adds nothing to it, and
//! `async-trait` — which any [`SpawnAsker`] needs — is a dependency of this
//! crate and not of that one.
//!
//! # The shared inbox
//!
//! A `ganja` lead reads `<ganja config home>/teams`; a real `claude` reads
//! `$CLAUDE_CONFIG_DIR/teams` and will not be talked out of it. So this test
//! points **both** at one directory — the lead's [`TeamsRoot`] is exactly what
//! `teammate::claude::teams_root()` answers under a `CLAUDE_CONFIG_DIR` set to
//! a temporary directory — which is the one configuration in which the two are
//! members of the same team, and is what this test's name means. Nothing here
//! touches the developer's own `~/.claude`.
//!
//! # The second claim: which separator a colliding name gets
//!
//! Open Question 3 asks whether Claude Code's registration writes `worker-2` or
//! `worker2`, and **nothing in CI can settle it** — §1.1 says only "appends an
//! incrementing counter starting at 2". This test is its only witness, so it
//! takes one: with `worker` already in the shared team file, the pane is asked
//! to register a teammate of its own by that same name, and whatever appears
//! beside `worker` in the team file is what a real `claude` does. It is
//! asserted against [`COLLISION_SEPARATOR`] rather than merely printed,
//! because a witness that only whispers is not one.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use ganja_core::{
    Backends, Caller, SpawnAsk, SpawnAsker, Storage, Teammates,
    permission::Permissions,
    protocol::PermissionReply,
    provider::FakeProvider,
    teammate::{
        InProcess, TeammateRegistry,
        claude::{self, CONFIG_DIR_ENV},
        pane::GanjaPane,
        tmux,
    },
    tool::{Registry, task::TeammateSpawn},
};
use ganja_team::{
    COLLISION_SEPARATOR, LEAD, MailboxMessage, MemberName, TeamName, TeamsRoot, mailbox, record,
};

/// The opt-in every live test in this workspace shares.
const LIVE: &str = "GANJA_LIVE_TEST";

/// The lead's session, and therefore the team: `session-01998ad0`.
const SESSION_ID: &str = "01998ad0-0000-7000-8000-000000000000";
const TEAM: &str = "session-01998ad0";

/// The teammate the lead spawns, and the name the collision probe re-asks for.
const WORKER: &str = "worker";

/// The word the pane is asked to send back, chosen so it cannot occur in any
/// preamble, refusal or apology the model might produce instead.
const TOKEN: &str = "GANJA-AC13-ROUNDTRIP-OK";

/// How long the pane is given to start, read its inbox and answer.
///
/// Generous: a cold `claude` start plus one real model turn. The test fails on
/// the timeout rather than hanging, because a live test nobody can interrupt is
/// a live test nobody runs twice.
const REPLY: Duration = Duration::from_secs(240);

/// How long the pane is given to register a teammate of its own.
const COLLISION: Duration = Duration::from_secs(240);

/// How often the shared directory is looked at.
const POLL: Duration = Duration::from_millis(500);

/// Says yes to everything. The spawn works inside its own directory and asks
/// for no bypass, so the gate answers on its own and this is never called;
/// saying yes is the answer that cannot mask a failure.
#[derive(Debug)]
struct Yes;

#[async_trait]
impl SpawnAsker for Yes {
    async fn ask(&self, _request: SpawnAsk) -> PermissionReply {
        PermissionReply::Once
    }
}

/// A private tmux server, so the panes this test makes never appear in — or
/// outlive — the developer's own session.
struct Tmux {
    socket: PathBuf,
}

impl Tmux {
    /// Starts a detached server on `socket` and answers with the pane a split
    /// should split from.
    fn start(socket: &Path) -> (Self, String) {
        let server = Self {
            socket: socket.to_path_buf(),
        };
        server.run(&[
            "new-session",
            "-d",
            "-x",
            "200",
            "-y",
            "50",
            "--",
            "/bin/sh",
            "-s",
        ]);
        let panes = server.run(&["list-panes", "-a", "-F", "#{pane_id}"]);
        let pane = panes
            .lines()
            .next()
            .unwrap_or_else(|| panic!("a fresh tmux server has one pane: {panes:?}"))
            .to_owned();

        (server, pane)
    }

    fn run(&self, arguments: &[&str]) -> String {
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(arguments)
            .output()
            .unwrap_or_else(|error| panic!("tmux {arguments:?} could not be run: {error}"));
        assert!(
            output.status.success(),
            "tmux {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}

impl Drop for Tmux {
    /// The server goes with the test, whether it passed or panicked: a live
    /// test that leaves a `claude` running in a pane nobody can see is worse
    /// than one that fails.
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

/// Waits until `read` answers, or gives up after `limit` and says what it was
/// waiting for.
async fn until<T>(what: &str, limit: Duration, mut read: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(found) = read() {
            return found;
        }
        assert!(
            started.elapsed() < limit,
            "gave up after {limit:?} waiting for {what}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Every teammate named in the shared team file, the lead excluded.
fn teammates(root: &TeamsRoot, team: &TeamName) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.config_path(team)) else {
        return Vec::new();
    };
    let Ok(file) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };

    file.get("members")
        .and_then(serde_json::Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(|member| member.get("name").and_then(serde_json::Value::as_str))
                .filter(|name| *name != LEAD)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The whole of AC-13: a real `claude` is spawned into a pane, reads the task
/// out of the shared inbox, and answers into the lead's — and, with that
/// proved, is asked for the one fact only it can give about a colliding name.
#[tokio::test]
#[ignore = "needs a real `claude` binary and a real `tmux`; opt in with GANJA_LIVE_TEST=1"]
async fn a_real_claude_pane_round_trips_over_the_shared_inbox() {
    if std::env::var(LIVE).ok().as_deref() != Some("1") {
        eprintln!("{LIVE} is not 1, so this test is inert; nothing was checked");
        return;
    }

    let home = ganja_testkit::temp_dir();
    let config_dir = home.path().join("claude");
    let socket = home.path().join("tmux.sock");
    let (server, pane) = Tmux::start(&socket);

    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written. All three
    // are process-wide by necessity: the backend reads `$TMUX`/`$TMUX_PANE` to
    // find the server to split, and both this side and the pane read
    // `CLAUDE_CONFIG_DIR` to find the same teams directory — which is the whole
    // of what "the shared inbox" means.
    unsafe {
        std::env::set_var(CONFIG_DIR_ENV, &config_dir);
        std::env::set_var(tmux::TMUX, format!("{},0,0", socket.display()));
        std::env::set_var(tmux::TMUX_PANE, &pane);
    }

    let root = claude::teams_root().expect("a CLAUDE_CONFIG_DIR resolves a teams root");
    assert_eq!(
        root,
        TeamsRoot::new(config_dir.join("teams")),
        "the lead reads the very directory the pane will write"
    );
    let team = TeamName::parse(TEAM).expect("a team name");
    let worker = MemberName::parse(WORKER).expect("a member name");
    let lead_inbox = root.inbox_path(&team, &MemberName::lead());

    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        SESSION_ID,
        home.path(),
    ));
    let door = Teammates::new(
        Arc::clone(&registry),
        Backends {
            // Never reached: every spawn below names `claude`. Present because
            // the door takes all three, and a fake provider is what keeps this
            // one from needing a credential.
            in_process: Arc::new(InProcess::new(
                Arc::new(FakeProvider::new("unused", Duration::ZERO)),
                Arc::new(Registry::new(Vec::new())),
                Storage::open(home.path().join("storage")),
                |_| Permissions::default(),
            )),
            pane: Arc::new(GanjaPane),
            claude: Arc::new(claude::ClaudePane),
        },
    );
    let caller = Caller {
        model: "recorder-model".to_owned(),
        cwd: home.path().to_path_buf(),
        permissions: Arc::new(Mutex::new(Permissions::default())),
        project_root: home.path().to_path_buf(),
    };

    let started = door
        .start(
            TeammateSpawn {
                name: WORKER.to_owned(),
                backend: Some("claude".to_owned()),
                agent_type: "general".to_owned(),
                prompt: format!(
                    "Reply to your lead with exactly this one word and nothing else: {TOKEN}"
                ),
            },
            &caller,
            &Yes,
        )
        .await
        .unwrap_or_else(|refused| panic!("a claude pane could not be started: {}", refused.reason));
    assert_eq!(started.backend, "claude");
    assert_eq!(started.name, WORKER, "the first `worker` is `worker`");

    // §4.1 step 5, from the pane's side of the directory: the task is in the
    // inbox the pane reads, and it was never on the command line.
    let seeded = mailbox::read(&root.inbox_path(&team, &worker)).expect("the inbox reads");
    // The message carrying the task carries §5.5.1's preamble too, which is the
    // property `TeammateBackend::owns_inbox` buys: with the two roots collapsed
    // the way this test collapses them, a registry that seeded here as well put
    // the bare task in as a *second* message ahead of the preamble, so the first
    // thing a real `claude` read was the one that does not tell it how to address
    // its lead. Asserted as a property of one entry rather than as a count of
    // them: a live `claude` sharing this directory may leave entries of its own,
    // and this test's claim is about what it reads, not about how many.
    assert!(
        seeded
            .valid
            .iter()
            .any(|message| message.text.contains(TOKEN)
                && message.text.contains("Do **not** address")),
        "the task was seeded into the shared inbox, behind its preamble: {:?}",
        seeded.valid.len()
    );

    // The round trip: the pane read its inbox, took a turn, and answered the
    // lead **by name** (§5.5.1) into the inbox beside its own.
    let answer = until("the pane's reply in the lead's inbox", REPLY, || {
        mailbox::read(&lead_inbox)
            .expect("the lead's inbox reads")
            .valid
            .into_iter()
            .find(|message| message.text.contains(TOKEN))
    })
    .await;
    assert_eq!(
        answer.from, WORKER,
        "a teammate answers as itself: {:?}",
        answer.from
    );

    // Open Question 3's only witness. `worker` is already in the shared team
    // file, so a `claude` registering that name again has to make it unique,
    // and how it does that is the fact nothing else here can observe.
    mailbox::write(
        &root.inbox_path(&team, &worker),
        MailboxMessage::new(
            LEAD,
            format!(
                "Now spawn one teammate of your own, asking for the name \"{WORKER}\" — the same \
                 name you have. Then tell me the name it was actually given."
            ),
            record::now_iso8601(),
        ),
    )
    .expect("the follow-up is written");

    let registered = until(
        "a second teammate in the shared team file",
        COLLISION,
        || {
            teammates(&root, &team)
                .into_iter()
                .find(|name| name != WORKER)
        },
    )
    .await;
    assert_eq!(
        registered,
        format!("{WORKER}{COLLISION_SEPARATOR}2"),
        "a real claude made the colliding name {registered:?}; if this is the only failure, \
         Open Question 3 is settled the other way and `COLLISION_SEPARATOR` is the one line to \
         change"
    );

    registry.shutdown().await;
    // Explicit rather than left to `Drop` order, so the panes are gone before
    // the temporary directory their store lives in is.
    drop(server);
}
