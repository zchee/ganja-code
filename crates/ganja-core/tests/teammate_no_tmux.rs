//! A pane spawn in a session with no tmux is refused, and refused readably
//! (**AC-16**, **D501** enforced against the two pane values, D-6).
//!
//! Spec: Claude Code's teammates — §10.2's closing decision, "with no `$TMUX`,
//! either refuse readably or self-host a detached session", settled as *refuse*
//! by D-6. Upstream opencode has no teammates and no counterpart.
//!
//! What this pins is the sentence, not the error kind, because the sentence is
//! the useful half: a person who asked for a window and was told
//! `Unsupported` would still not know whether their session or their build was
//! the problem. And it pins that the refusal is a refusal — the in-process
//! backend spawns in the very same session, so nothing here silently
//! substitutes one for the other in either direction: a `pane` request does not
//! become an in-process teammate, and an `in-process` request is not refused
//! for lack of a window it never wanted.
//!
//! One test, because it mutates `TMUX` (and `TMUX_PANE`, which would otherwise
//! name a pane of whatever server the developer is running the suite in), and
//! a binary that mutates process-wide state holds exactly one — a plain `cargo
//! test` runs a binary's tests on threads of one process.
//!
//! Every spawn goes through [`Teammates::start`], the one door onto the
//! registry, so the refusal asserted is the one production answers with.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use ganja_core::{
    Backends, Caller, SpawnAsk, SpawnAsker, Storage, Teammates,
    permission::Permissions,
    protocol::PermissionReply,
    provider::FakeProvider,
    teammate::{
        InProcess, TeammateRegistry,
        claude::ClaudePane,
        pane::GanjaPane,
        tmux::{self, REFUSED_NO_TMUX},
    },
    tool::{Registry, task::TeammateSpawn},
};
use ganja_team::{MemberName, TeamFile, TeamName, TeamsRoot, mailbox};

/// The task every spawn here is started with. Nothing reads it; what matters
/// is that a refused spawn left none of it behind.
const TASK: &str = "have a look at the parser";

/// Says yes to everything, and is asked nothing by any spawn here: every spawn
/// works inside its own project and asks for no bypass, so the gate answers
/// `Allow` on its own. Saying yes is the answer that cannot mask a failure.
#[derive(Debug)]
struct Yes;

#[async_trait::async_trait]
impl SpawnAsker for Yes {
    async fn ask(&self, _request: SpawnAsk) -> PermissionReply {
        PermissionReply::Once
    }
}

/// A spawn of `name` on `backend`, with everything else the same.
fn request(name: &str, backend: Option<&str>) -> TeammateSpawn {
    TeammateSpawn {
        name: name.to_owned(),
        backend: backend.map(str::to_owned),
        agent_type: "general".to_owned(),
        prompt: TASK.to_owned(),
    }
}

/// The **teammates** the team file records, or the empty account of a file
/// that was never written.
fn recorded(root: &TeamsRoot, team: &TeamName) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.config_path(team)) else {
        return Vec::new();
    };
    let file: TeamFile =
        serde_json::from_str(&text).expect("the team file this build wrote decodes");

    file.members
        .into_iter()
        .map(|member| member.name)
        .filter(|name| name != ganja_team::LEAD)
        .collect()
}

#[tokio::test]
async fn a_pane_spawn_without_tmux_is_refused_readably() {
    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written. Both
    // variables go, because tmux exports both into a pane and either one left
    // behind would name the developer's own server.
    unsafe {
        std::env::remove_var(tmux::TMUX);
        std::env::remove_var(tmux::TMUX_PANE);
    }
    assert!(!tmux::hosted(), "the premise: this process is outside tmux");

    let home = ganja_testkit::temp_dir();
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-abcd1234").expect("a team name");
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-0000-7000-8000-000000000000",
        home.path(),
    ));
    let door = Teammates::new(
        Arc::clone(&registry),
        Backends {
            in_process: Arc::new(InProcess::new(
                Arc::new(FakeProvider::new("on it", Duration::ZERO)),
                Arc::new(Registry::new(Vec::new())),
                Storage::open(home.path().join("storage")),
                |_| Permissions::default(),
            )),
            pane: Arc::new(GanjaPane),
            claude: Arc::new(ClaudePane),
        },
    );
    let caller = Caller {
        model: "recorder-model".to_owned(),
        cwd: home.path().to_path_buf(),
        permissions: Arc::new(Mutex::new(Permissions::default())),
        project_root: home.path().to_path_buf(),
    };

    // The assertion is on the words: the variable that is missing, and the way
    // out — which is not "wait for another phase". Both pane values are in the
    // loop because both have real bodies now, and the claim is that they
    // refuse in **one** sentence: a `claude` spawn that said something else
    // about a missing session would be two behaviours wearing one argument.
    for backend in ["pane", "claude"] {
        let refused = door
            .start(request("worker", Some(backend)), &caller, &Yes)
            .await
            .expect_err("a session outside tmux has no pane to give");
        assert!(
            refused.reason.ends_with(REFUSED_NO_TMUX),
            "{backend} refuses in the sentence that names the session as what is missing: \
             {}",
            refused.reason
        );
        assert!(
            refused.reason.contains("$TMUX") && refused.reason.contains("in-process"),
            "{backend}'s refusal names the variable and the alternative: {}",
            refused.reason
        );
        assert!(
            refused.reason.contains(backend),
            "and still says which surface was asked for: {}",
            refused.reason
        );
    }
    // A refused spawn leaves nothing behind: no member, no teammate, and — the
    // half that would otherwise still be there tomorrow — no task sitting in a
    // mailbox nothing will ever read.
    assert!(
        recorded(&root, &team).is_empty(),
        "a refused spawn joined nobody to the team"
    );
    assert_eq!(
        registry.running(),
        0,
        "and nothing was quietly started instead"
    );
    let inbox = root.inbox_path(&team, &MemberName::parse("worker").expect("a member name"));
    assert!(
        mailbox::read(&inbox)
            .expect("the inbox reads")
            .valid
            .is_empty(),
        "a refused spawn left its prompt in an inbox"
    );

    // The other direction of "no silent fallback": the backend that needs no
    // window still runs in this very session.
    let started = door
        .start(request("worker", None), &caller, &Yes)
        .await
        .expect("an in-process teammate needs no tmux");
    assert_eq!(started.backend, "in-process");
    assert_eq!(registry.running(), 1);
    assert_eq!(recorded(&root, &team), vec!["worker".to_owned()]);

    registry.shutdown().await;
    assert_eq!(registry.running(), 0);
}
