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
//! GANJA_LIVE_TEST=1 cargo test -p ganja-teammate-local --test teammate_claude_live -- --ignored
//! ```
//!
//! It lives in `ganja-core`'s tests beside the two `ganja` pane binaries
//! rather than in `ganja-cli`'s: the whole claim is about a `ganja-core`
//! backend, and the CLI adds nothing to it.
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
//! # What a private config home takes away with it
//!
//! `CLAUDE_CONFIG_DIR` does not only move the teams directory. A real `claude`
//! derives the **name of its credential store** from it as well — on macOS the
//! keychain service is `Claude Code-credentials` under the default home and
//! `Claude Code-credentials-<eight hex of the path>` under any other — which is
//! how that one variable serves several accounts at once. So a config home a
//! test invents is a config home nobody has ever logged into: the pane starts,
//! reads its inbox, addresses its lead correctly and then answers *"Anthropic
//! profile login expired"* instead of taking its turn. That is an authentication
//! failure wearing an interop failure's clothes, and it is the whole reason this
//! test spent its first real run timing out on an inbox that was never going to
//! be written to.
//!
//! [`SECURE_STORAGE_ENV`] is claude's own door out of it: set — the empty string
//! counts as set — it, rather than `CLAUDE_CONFIG_DIR`, is what the store's
//! identity comes from, and empty selects the default store. Setting it empty
//! here buys exactly the distinction this test needs, between a private teams
//! **directory** and a private **login**: the first is what a shared inbox under
//! a temporary root means, the second was only ever an accident of asking for
//! the first.
//!
//! It is test scaffolding and nothing else. `ganja` neither sets this variable
//! nor carries it — it is not in `claude::carried_env`, and D502's list stays
//! closed — because in production the pane rides the user's own config home,
//! whose store is the one that user logged into.
//!
//! # The second claim this test used to make, and why it cannot
//!
//! Open Question 3 — whether registration writes `worker-2` or `worker2` —
//! was once asked here, by telling the pane to register a teammate of its own
//! under the name it already had. **A teammate cannot answer that question**,
//! and not because of a timeout: Claude Code forbids it. The 2.1.233 binary was
//! observed to drop the `name` parameter from the Agent tool's own description
//! — leaving "teammates cannot spawn teammates" in its place — for exactly a
//! process launched with **both** `--agent-id` and `--team-name`, which is
//! [`claude::ClaudePane`]'s launch line, every time. A pane teammate therefore
//! has no `name` parameter to spawn *with*, only anonymous subagents that never
//! earn a member record, so no second member was ever going to appear in the
//! team file. The leg is gone rather than given a longer timeout.
//!
//! A lead `claude` could answer it, and cannot be made to: a lead mints its own
//! `session-<eight hex>` team from its own session id, which is not the team
//! this directory holds.
//!
//! It is settled anyway, and by better evidence than a live run — the same
//! binary that refuses the witness performs the registration itself, observed
//! directly. See [`ganja_team::COLLISION_SEPARATOR`], which records what it does.

use std::sync::Arc;
use std::time::Duration;

use ganja_core::teammate::TeammateRegistry;
use ganja_core::{Storage, Teammates};
use ganja_team::{MemberName, TeamName, TeamsRoot, mailbox};
use ganja_teammate_local::claude::{self, CONFIG_DIR_ENV};
use ganja_testkit::tmux::PrivateServer;
use ganja_testkit::{AllowSpawn, LEAD_SESSION_ID, caller, eventually, spawn_with_prompt};

/// The opt-in every live test in this workspace shares.
const LIVE: &str = "GANJA_LIVE_TEST";

/// The variable that decides which credential store a real `claude` reads,
/// independently of its config home — see the module doc for why this test has
/// to say anything about it at all.
const SECURE_STORAGE_ENV: &str = "CLAUDE_SECURESTORAGE_CONFIG_DIR";

/// Names a file to install as the config home's `.claude.json` before the
/// pane starts — the CI lane's door into a machine nobody ever logged into.
///
/// A runner has no keychain and no stored profile, so its pane authenticates
/// with `ANTHROPIC_API_KEY` — which a real `claude` only *takes* once the key
/// is pre-approved in the config home's own state file, Claude Code's
/// `customApiKeyResponses` mechanism (approval is the key's sha256, first
/// twenty hex, plus `hasCompletedOnboarding` so a fresh home does not stop to
/// ask). The workflow computes that hash and writes this file, so the
/// arithmetic lives beside the secret it hashes; this test only copies it
/// into the config home it mints. The key itself travels as ambient process
/// environment into the tmux server (§10.10) and from there into the pane —
/// `claude::carried_env` does not name it, and D502's list stays closed.
///
/// Unset — every local run — nothing is seeded and this test is exactly what
/// it always was: the keychain lane, on the developer's own login. The two
/// lanes were separated empirically: under a developer shell that is itself a
/// Claude Code session, ambient `CLAUDE_*`/`ANTHROPIC_*` state routes the
/// pane to profile auth no matter what this seed says, so the CI lane can
/// only be exercised where the environment is clean — which a runner is.
const SEED: &str = "GANJA_LIVE_CLAUDE_SEED";

/// The team, spelled from [`LEAD_SESSION_ID`]'s first eight hex the way a
/// lead's implicit session team is.
const TEAM: &str = "session-01998ad0";

/// The teammate the lead spawns.
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

/// The whole of AC-13: a real `claude` is spawned into a pane, reads the task
/// out of the shared inbox, and answers into the lead's.
#[tokio::test]
#[ignore = "needs a real `claude` binary and a real `tmux`; opt in with GANJA_LIVE_TEST=1"]
async fn a_real_claude_pane_round_trips_over_the_shared_inbox() {
    if std::env::var(LIVE).ok().as_deref() != Some("1") {
        eprintln!("{LIVE} is not 1, so this test is inert; nothing was checked");
        return;
    }

    let home = ganja_testkit::temp_dir();
    let config_dir = home.path().join("claude");
    if let Some(seed) = std::env::var_os(SEED) {
        std::fs::create_dir_all(&config_dir).expect("the config home is made");
        std::fs::copy(&seed, config_dir.join(".claude.json"))
            .expect("the workflow's auth seed installs as the pane's state file");
    }

    // SAFETY: as below — one test in this binary, so nothing else here is
    // reading the environment. It is written *before* the server rather than
    // beside the others because a pane inherits the tmux **server's**
    // environment (§10.10) and this one travels no other way: it is not in
    // `claude::carried_env`, so no `-e` carries it, and a value written after
    // the next line would reach nothing.
    unsafe {
        std::env::set_var(SECURE_STORAGE_ENV, "");
    }

    let server = PrivateServer::start(&["/bin/sh", "-s"], &[], &[]);

    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written. All three
    // are process-wide by necessity: the backend reads `$TMUX`/`$TMUX_PANE` —
    // `enter` sets both — to find the server to split, and both this side and
    // the pane read `CLAUDE_CONFIG_DIR` to find the same teams directory —
    // which is the whole of what "the shared inbox" means.
    unsafe {
        std::env::set_var(CONFIG_DIR_ENV, &config_dir);
        server.enter();
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

    let registry =
        Arc::new(TeammateRegistry::new(root.clone(), team.clone(), LEAD_SESSION_ID, home.path()));
    // The in-process slot is never reached: every spawn below names `claude`.
    // Present because the door takes all three, and the default backends'
    // fake provider is what keeps it from needing a credential.
    let door = Teammates::new(
        Arc::clone(&registry),
        ganja_testkit::backends(Storage::open(home.path().join("storage"))),
    );

    let started = door
        .start(
            spawn_with_prompt(
                WORKER,
                Some("claude"),
                &format!("Reply to your lead with exactly this one word and nothing else: {TOKEN}"),
            ),
            &caller(home.path()),
            &AllowSpawn,
        )
        .await
        .unwrap_or_else(|refused| panic!("a claude pane could not be started: {}", refused.reason));
    assert_eq!(started.backend, "claude");
    assert_eq!(started.name, WORKER, "the spawn keeps the name it asked for");

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
    let answer = eventually(REPLY, "the pane's reply in the lead's inbox", async || {
        mailbox::read(&lead_inbox)
            .expect("the lead's inbox reads")
            .valid
            .into_iter()
            .find(|message| message.text.contains(TOKEN))
    })
    .await;
    assert_eq!(answer.from, WORKER, "a teammate answers as itself: {:?}", answer.from);

    registry.shutdown().await;
    // Explicit rather than left to `Drop` order, so the panes are gone before
    // the temporary directory their store lives in is.
    drop(server);
}
