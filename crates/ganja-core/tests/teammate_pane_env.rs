//! What a `ganja` pane inherits from the lead, and what it must not
//! (**D502**, and the argv-secrets posture of §10.2 step 6 / §10.12-4).
//!
//! Spec: Claude Code's teammates — §10.10, "new panes inherit the *server*
//! environment, not the client's: carry the config-home variable explicitly;
//! never secrets". Upstream opencode has no teammates and no counterpart.
//!
//! # The failure the allowlist fixes
//!
//! Start a tmux server. *Then* export `GANJA_CONFIG_HOME` and start a lead in
//! it. The lead's teams root is under that home; a pane spawned with no
//! environment of its own inherits the **server's** environment, which
//! predates the export, and so joins a different team — or none. This binary
//! stages exactly that order: the private server is born **without** the
//! variable, the test sets it afterwards, and the pane must still report from
//! inside the lead's team. If `pane.rs` stopped carrying the variable, the
//! child would resolve a different home, write to a different inbox, and the
//! wait below would time out.
//!
//! # And the half that must not travel
//!
//! Two credentials are planted in the lead's process the same way — after the
//! server, before the spawn — and a canary string is put in the spawn prompt.
//! The pane's report names every variable in its environment: the two
//! credentials must not be among them, `GANJA_CONFIG_HOME` must, the argv is
//! the five flags and nothing else, and tmux's own record of the pane's
//! command line carries no canary. The prompt reached the pane's inbox and its
//! member record, verbatim (D-7) — and nothing else.
//!
//! # Its own `main`
//!
//! `harness = false`, for the reason `teammate_pane_lifecycle.rs` gives: the pane's
//! program is this binary. One test, because it sets `TMUX`, `TMUX_PANE`,
//! `GANJA_CONFIG_HOME`, `XDG_DATA_HOME` and the two canaries for the whole
//! process. Hard-fails without tmux.

mod pane_support;

use std::{sync::Arc, time::Duration};

use ganja_core::{
    config::CONFIG_HOME_ENV,
    teammate::{
        lead_inbox::LeadInbox,
        pane::{AGENT_COLOR, AGENT_ID, AGENT_NAME, PARENT_SESSION_ID, TEAM_NAME},
        tmux::{self, Server},
    },
    tool::{
        Tool as _,
        task::{Offered, TaskTool},
    },
};
use ganja_team::TeamFile;
use pane_support::{
    PrivateServer, Report, SESSION_ID, ctx, lead, pane_child_if_asked, run_one, task_args, team_of,
    wait_for,
};

/// How long the pane's process gets to start and report.
const CHILD_STARTS: Duration = Duration::from_secs(30);

/// A credential a lead might well hold, planted after the server started.
const API_KEY: &str = "ANTHROPIC_API_KEY";
/// And the other kind, which a config-home allowlist must equally never carry.
const SERVER_PASSWORD: &str = "GANJA_SERVER_PASSWORD";
/// What both are set to, and what the prompt carries: one string to grep for.
const CANARY: &str = "sk-ant-CANARY-b1a5f7-never-on-a-launch-line";

fn main() {
    pane_child_if_asked();
    run_one(
        "a_pane_joins_the_team_when_the_tmux_server_predates_the_config_home_export",
        a_pane_joins_the_team_when_the_tmux_server_predates_the_config_home_export(),
    );
}

async fn a_pane_joins_the_team_when_the_tmux_server_predates_the_config_home_export() {
    // The server first, born without the variable and without the credentials
    // — whatever this process inherited of them is kept out of it.
    let server = PrivateServer::start(&[CONFIG_HOME_ENV, API_KEY, SERVER_PASSWORD]);
    let config_home = ganja_testkit::temp_dir();
    let project = ganja_testkit::temp_dir();
    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written.
    unsafe {
        server.enter();
        std::env::set_var(CONFIG_HOME_ENV, config_home.path());
        std::env::set_var("XDG_DATA_HOME", project.path().join("data"));
        std::env::set_var(API_KEY, CANARY);
        std::env::set_var(SERVER_PASSWORD, CANARY);
    }
    assert!(tmux::hosted());
    // The premise, checked rather than assumed: the server really does not
    // have the home the lead is about to use.
    assert!(
        !server.global_has(CONFIG_HOME_ENV),
        "the private server was born before the config home was exported"
    );
    assert!(!server.global_has(API_KEY) && !server.global_has(SERVER_PASSWORD));

    let (registry, door) = lead(config_home.path(), project.path());
    let (root, team) = team_of(&registry);
    let door = Arc::new(door);
    let tool = TaskTool::new(&[Offered {
        name: "general".to_owned(),
        description: None,
    }]);
    let ctx = ctx(project.path(), Arc::clone(&door));

    let prompt = format!("watch the build; the key is {CANARY}");
    let output = tool
        .run(task_args("worker", "pane", &prompt), &ctx)
        .await
        .expect("the door spawns a pane teammate inside tmux");
    assert_eq!(
        output.metadata.get("backend").and_then(|on| on.as_str()),
        Some("pane")
    );
    let file: TeamFile = serde_json::from_str(
        &std::fs::read_to_string(root.config_path(&team)).expect("the team file is written"),
    )
    .expect("the team file decodes");
    let member = file
        .member("worker")
        .unwrap_or_else(|| panic!("the pane teammate joined the team: {file:?}"))
        .clone();
    let pane_id = member.tmux_pane_id.clone();
    assert!(pane_id.starts_with('%'), "{member:?}");

    // The pane reports from *inside the lead's team*: it resolved the lead's
    // home, not the server's absence of one.
    let inbox = LeadInbox::new(Arc::clone(&registry));
    let report = wait_for(
        CHILD_STARTS,
        "the pane's report to reach the lead",
        async || {
            let pass = inbox.poll().await;
            pass.messages
                .into_iter()
                .find(|message| message.from == "worker")
                .map(|message| {
                    serde_json::from_str::<Report>(&message.body).unwrap_or_else(|error| {
                        panic!("the pane wrote a report: {error} in {message:?}")
                    })
                })
        },
    )
    .await;
    assert_eq!(
        report.config_home.as_deref(),
        Some(config_home.path().to_str().expect("utf-8")),
        "the pane reads the lead's config home, exported after the server started"
    );

    // What travelled, by name: the carried variable, and neither credential.
    assert!(
        report.env_names.iter().any(|name| name == CONFIG_HOME_ENV),
        "the config home is in the pane's environment: {:?}",
        report.env_names
    );
    for secret in [API_KEY, SERVER_PASSWORD] {
        assert!(
            !report.env_names.iter().any(|name| name == secret),
            "{secret} was in the lead's environment and must not be in the pane's: {:?}",
            report.env_names
        );
    }

    // What is on the line: the five flags, and no canary anywhere tmux or the
    // pane can see it.
    assert_eq!(
        report.argv,
        [
            AGENT_ID,
            &format!("worker@{}", team.as_str()),
            AGENT_NAME,
            "worker",
            TEAM_NAME,
            team.as_str(),
            AGENT_COLOR,
            member.color.as_deref().expect("a spawn assigns a colour"),
            PARENT_SESSION_ID,
            SESSION_ID,
        ]
    );
    let line = server.start_command(&pane_id);
    assert!(
        !line.contains(CANARY),
        "the prompt rode the mailbox and never the command line: {line}"
    );
    assert!(
        !line.contains(config_home.path().to_str().expect("utf-8")),
        "the environment rode tmux's own door, not the command line: {line}"
    );
    // Where the prompt did go, verbatim: the record (D-7) and the inbox.
    assert_eq!(member.prompt.as_deref(), Some(prompt.as_str()));

    // The way out through the registry: shutdown kills the pane it made.
    registry.shutdown().await;
    let after = Server::at(server.socket(), None)
        .panes()
        .await
        .expect("the private server still lists");
    assert!(
        !after.iter().any(|live| live.id == pane_id),
        "the registry's shutdown ended the pane: {after:?}"
    );

    drop(server);
    drop(config_home);
    drop(project);
}
