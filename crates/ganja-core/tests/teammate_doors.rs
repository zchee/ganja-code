//! The `task` tool's teammate door, end to end (**AC-14**, the `task` half).
//!
//! Spec: Claude Code's teammates — §4.1's spawn sequence. Upstream opencode
//! has no teammates and no counterpart to any of it.
//!
//! AC-14's claim is "both doors, one sequence", and it is pinned in three
//! places because no one binary can see all three: this file holds the `task`
//! door's engine-side half, `crates/ganja-tui/src/component/team.rs` holds the
//! `/team spawn` dialog's half, and `teammate_backends.rs` holds the
//! per-backend refusals on their own. What this file has to show is that the
//! **door** reaches [`Teammates::start`] and that a refusal propagates
//! through it in exactly the sentence the other door refuses in — one door
//! must not spawn where the other refuses.
//!
//! One test, because it redirects `XDG_DATA_HOME` and **withdraws `TMUX`**, and
//! a binary that mutates process-wide state holds exactly one — a plain `cargo
//! test` runs a binary's tests on threads of one process. `TMUX` goes because
//! since P25b the pane backends are real: with it set, the `pane` request below
//! would split a pane of this test harness into whatever tmux the developer is
//! running the suite in, and the claim here is about the *door*, not about a
//! window.
//!
//! # What sits behind the seam here, and why
//!
//! The context this drives carries a test-local [`Subagents`] whose
//! `spawn_teammate` is the engine's own [`Teammates`] and whose `delegate` is
//! not exercised. That is one forwarding call short of what a session installs
//! — `ganja_core`'s `subagent::Spawn` reads the same value off its `Host` —
//! and it is short of it because a `Host` needs a whole engine, a provider, an
//! agent roster and a live parent turn to exist, none of which this claim is
//! about. Everything past the seam is the real thing: the real registry, the
//! real backends, the real team file.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{PermissionReply, Role};
use ganja_core::provider::FakeProvider;
use ganja_core::teammate::tmux::{self, REFUSED_NO_TMUX};
use ganja_core::tool::task::{
    Delegated, Delegation, NotSpawned, Offered, Subagents, TaskTool, TeammateSpawn, Teammated,
    Unanswered,
};
use ganja_core::tool::{Credentials, FileTimes, Registry, Tool as _, ToolCtx, ToolError};
use ganja_core::{Caller, SpawnAsk, SpawnAsker, Storage, Teammates};
use ganja_testkit::{TEAM, caller, team_file, team_with};
use tokio_util::sync::CancellationToken;

/// How long the teammate's own provider takes to answer.
///
/// Long enough that a door which *waited* for the teammate would find that
/// teammate's assistant reply already stored when it came back: the runner's
/// first pass alone is half a second behind the spawn, and the turn it
/// starts is this much more.
const TEAMMATE_TURN: Duration = Duration::from_secs(2);

/// The engine-side seam a `task` call reaches, with only the teammate half
/// wired: what a delegation does is `task_tool.rs`'s claim, not this one.
#[derive(Debug)]
struct Door {
    teammates: Arc<Teammates>,
    caller: Caller,
    /// Every spawn a person was asked about, which for a teammate working
    /// inside the project should be none at all.
    asked: Arc<Mutex<Vec<SpawnAsk>>>,
}

#[async_trait]
impl Subagents for Door {
    async fn delegate(
        &self,
        _request: Delegation,
        _cancel: CancellationToken,
    ) -> Result<Delegated, Unanswered> {
        Err(Unanswered::Unknown)
    }

    async fn spawn_teammate(&self, request: TeammateSpawn) -> Result<Teammated, NotSpawned> {
        self.teammates.start(request, &self.caller, self).await
    }
}

#[async_trait]
impl SpawnAsker for Door {
    async fn ask(&self, request: SpawnAsk) -> PermissionReply {
        self.asked.lock().expect("no panic").push(request);

        PermissionReply::Once
    }
}

/// What the model calls the tool with to start a teammate on `backend`.
fn args(backend: &str) -> serde_json::Value {
    serde_json::json!({
        "description": "hold the fort",
        "prompt": "watch the build and say what breaks",
        "subagent_type": "general",
        "name": "worker",
        "backend": backend,
    })
}

/// Both halves of AC-14's `task` leg: the door starts a teammate and answers
/// before the teammate has done anything, and the two pane surfaces refuse
/// through it in the sentence they refuse in everywhere else.
#[tokio::test]
async fn the_task_door_starts_a_teammate_at_once_and_refuses_a_pane_as_the_other_door_does() {
    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written.
    let data = unsafe { ganja_testkit::redirect_xdg_data_home() };
    // SAFETY: the same invariant. Outside tmux, both pane values refuse (D501)
    // instead of splitting a pane of this binary into the developer's session.
    unsafe {
        std::env::remove_var(tmux::TMUX);
        std::env::remove_var(tmux::TMUX_PANE);
    }
    let home = ganja_testkit::temp_dir();
    let storage = Storage::open(home.path().join("storage"));
    let (root, team, registry, teammates) = team_with(
        home.path(),
        Arc::new(FakeProvider::new("on it", TEAMMATE_TURN)),
        Arc::new(Registry::new(Vec::new())),
        storage.clone(),
        |_| Permissions::default(),
    );
    let asked: Arc<Mutex<Vec<SpawnAsk>>> = Arc::default();
    let door = Door {
        asked: Arc::clone(&asked),
        teammates,
        // The teammate works where the calling turn works, so the spawn gate
        // has nothing to disclose and nobody to ask.
        caller: caller(home.path()),
    };
    let tool = TaskTool::new(&[Offered { name: "general".to_owned(), description: None }]);
    let ctx = ToolCtx {
        cwd: home.path().to_path_buf(),
        cancel: CancellationToken::new(),
        call_id: "call_1".to_owned(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: Some(Arc::new(door) as Arc<dyn Subagents>),
        postbox: None,
        ask: None,
        switch: None,
        jobs: None,
    };

    assert!(
        team_file(&root, &team).is_none(),
        "a session that has not spawned anything leaves no team on disk"
    );

    let output =
        tool.run(args("in-process"), &ctx).await.expect("the door starts an in-process teammate");

    // The door answered, and it answered without the teammate having done its
    // work — which is the whole difference between this door and the other.
    // Read off the store rather than a clock: the teammate's provider sleeps
    // for [`TEAMMATE_TURN`], so a door that waited for the turn comes back to
    // an assistant reply already on disk, however loaded the machine is.
    let sessions = storage.list_sessions().expect("the store lists");
    assert!(
        sessions.iter().all(|info| {
            storage
                .load_transcript(&info.id)
                .expect("the transcript reads")
                .iter()
                .all(|message| message.role != Role::Assistant)
        }),
        "the door waited for the teammate's {TEAMMATE_TURN:?} turn: {sessions:?}"
    );
    assert_eq!(
        output.metadata.get("teammate").and_then(|to| to.as_str()),
        Some("worker"),
        "the result names the teammate a later message has to address: {output:?}"
    );
    assert_eq!(
        output.metadata.get("agent_id").and_then(|id| id.as_str()),
        Some(format!("worker@{TEAM}").as_str()),
        "and its derived identity: {output:?}"
    );
    assert_eq!(
        output.metadata.get("backend").and_then(|on| on.as_str()),
        Some("in-process"),
        "and the surface it really runs on: {output:?}"
    );

    // §4.1's member record, written by the door rather than by anything the
    // teammate itself later did.
    let file = team_file(&root, &team).expect("a spawn writes the team file");
    let member = file
        .members
        .iter()
        .find(|member| member.name == "worker")
        .unwrap_or_else(|| panic!("the teammate joined the team: {file:?}"));
    assert_eq!(
        member.agent_type, "general",
        "the record carries the agent kind the call named: {member:?}"
    );
    assert_eq!(
        member.model.as_deref(),
        Some("recorder-model"),
        "and the model the calling turn is asking: {member:?}"
    );
    assert_eq!(
        member.agent_id,
        format!("worker@{TEAM}"),
        "and the identity the tool reported: {member:?}"
    );
    assert_eq!(registry.running(), 1, "and the teammate is running");
    assert!(
        asked.lock().expect("no panic").is_empty(),
        "a teammate working inside the project asks nobody: {:?}",
        asked.lock().expect("no panic")
    );

    // A refusal propagates through this door in the sentence the other door
    // refuses in — a door that spawned where the other refused would be two
    // behaviours wearing one argument. One backend suffices for the
    // propagation claim; the per-backend sentences, and that a refused spawn
    // leaves nothing behind, are `teammate_no_tmux.rs`'s.
    let refused = match tool.run(args("ganja"), &ctx).await {
        Err(ToolError::Failed(message)) => message,
        other => panic!("expected pane to be refused, got {other:?}"),
    };
    assert!(
        refused.contains(REFUSED_NO_TMUX),
        "the door refuses in the sentence teammate_no_tmux.rs pins: {refused}"
    );

    registry.shutdown().await;
    // Held to the end so the redirected data home outlives every write a
    // teammate's turn might make on its way out.
    drop(data);
}
