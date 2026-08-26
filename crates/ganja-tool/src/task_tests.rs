use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{
    BACKEND_WITHOUT_NAME, DESCRIPTION, Delegated, Delegation, NAME_WITH_TASK_ID, NO_TEAM,
    NotSpawned, Offered, ROSTER_HEADER, STARTED, Subagents, TaskTool, TeammateSpawn, Teammated,
    Unanswered, render,
};
use crate::{Tool as _, ToolCtx, ToolError};

/// A seam that records what it was asked and answers a teammate spawn from
/// a script. Delegation is not what these tests are about, so it is the
/// one thing this double refuses.
#[derive(Debug)]
struct Fake {
    started: Mutex<Vec<TeammateSpawn>>,
    answer: Result<Teammated, NotSpawned>,
}

impl Fake {
    fn answering(answer: Result<Teammated, NotSpawned>) -> Arc<Self> {
        Arc::new(Self {
            started: Mutex::new(Vec::new()),
            answer,
        })
    }

    fn spawning() -> Arc<Self> {
        Self::answering(Ok(Teammated {
            name: "worker-2".to_owned(),
            agent_id: "worker-2@session-abcd1234".to_owned(),
            backend: "in-process".to_owned(),
            note: "it reads this through its mailbox".to_owned(),
        }))
    }
}

#[async_trait]
impl Subagents for Fake {
    async fn delegate(
        &self,
        _request: Delegation,
        _cancel: CancellationToken,
    ) -> Result<Delegated, Unanswered> {
        Err(Unanswered::Unknown)
    }

    async fn spawn_teammate(&self, request: TeammateSpawn) -> Result<Teammated, NotSpawned> {
        self.started
            .lock()
            .expect("the spawn log is never poisoned")
            .push(request);

        self.answer.clone()
    }
}

/// A context whose only interesting field is the seam under test.
fn ctx(spawn: Option<Arc<dyn Subagents>>) -> ToolCtx {
    let mut ctx = ToolCtx::fixture(std::env::temp_dir());
    ctx.spawn = spawn;
    ctx
}

fn tool() -> TaskTool {
    TaskTool::new(&[Offered {
        name: "general".to_owned(),
        description: None,
    }])
}

/// Runs one call against `spawn` and reports what the model would read of
/// a refusal. [`ToolError`] carries no [`PartialEq`], so the sentence is
/// what a test compares — which is the half that is the contract anyway.
async fn refusal(spawn: Arc<dyn Subagents>, args: serde_json::Value) -> String {
    match tool().run(args, &ctx(Some(spawn))).await {
        Err(ToolError::Failed(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The teammate door: the arguments cross whole, and what comes back says
/// the work has *started* rather than finished.
#[tokio::test]
async fn a_call_that_names_a_teammate_starts_one_and_says_so() {
    let spawn = Fake::spawning();
    let output = tool()
        .run(
            serde_json::json!({
                "description": "spin up a worker",
                "prompt": "hold the fort",
                "subagent_type": "general",
                "name": "worker",
                "backend": "in-process",
            }),
            &ctx(Some(Arc::clone(&spawn) as Arc<dyn Subagents>)),
        )
        .await
        .expect("a teammate starts");

    assert_eq!(
        *spawn.started.lock().expect("no panic"),
        vec![TeammateSpawn {
            name: "worker".to_owned(),
            backend: Some("in-process".to_owned()),
            agent_type: "general".to_owned(),
            prompt: "hold the fort".to_owned(),
        }]
    );
    assert!(
        output.output.starts_with(STARTED),
        "the result says a teammate started: {}",
        output.output
    );
    assert!(
        output.output.contains("worker-2"),
        "and under the name the team really gave it: {}",
        output.output
    );
}

/// A refusal is the far side's sentence, unwrapped.
#[tokio::test]
async fn a_refused_spawn_reads_back_the_far_sides_own_sentence() {
    let spawn = Fake::answering(Err(NotSpawned {
        reason: "no backend named \"tmux\"".to_owned(),
    }));
    let read = refusal(
        spawn as Arc<dyn Subagents>,
        serde_json::json!({
            "description": "spin up a worker",
            "prompt": "hold the fort",
            "subagent_type": "general",
            "name": "worker",
            "backend": "tmux",
        }),
    )
    .await;

    assert_eq!(read, "no backend named \"tmux\"");
}

/// A `backend` with no `name` is the argument that was meant to carry one:
/// refused by name rather than delegated to a subagent in silence.
#[tokio::test]
async fn a_surface_named_without_a_teammate_is_refused() {
    let spawn = Fake::spawning();
    let read = refusal(
        Arc::clone(&spawn) as Arc<dyn Subagents>,
        serde_json::json!({
            "description": "spin up a worker",
            "prompt": "hold the fort",
            "subagent_type": "general",
            "backend": "ganja",
        }),
    )
    .await;

    assert_eq!(read, BACKEND_WITHOUT_NAME);
    assert!(
        spawn.started.lock().expect("no panic").is_empty(),
        "and nothing was started"
    );
}

/// Continuing a delegation and starting a teammate are two calls, not one.
#[tokio::test]
async fn continuing_a_delegation_and_starting_a_teammate_are_not_one_call() {
    let spawn = Fake::spawning();
    let read = refusal(
        Arc::clone(&spawn) as Arc<dyn Subagents>,
        serde_json::json!({
            "description": "spin up a worker",
            "prompt": "hold the fort",
            "subagent_type": "general",
            "name": "worker",
            "task_id": "01998ad0-0000-7000-8000-000000000000",
        }),
    )
    .await;

    assert_eq!(read, NAME_WITH_TASK_ID);
    assert!(spawn.started.lock().expect("no panic").is_empty());
}

/// A seam that runs subagents and leads no team refuses in the tool's own
/// sentence, which is what the trait's default answers with.
#[tokio::test]
async fn a_seam_that_leads_no_team_refuses_a_teammate() {
    /// Nothing but [`Subagents::delegate`]; the teammate door is the
    /// trait's default.
    #[derive(Debug)]
    struct Delegator;

    #[async_trait]
    impl Subagents for Delegator {
        async fn delegate(
            &self,
            _request: Delegation,
            _cancel: CancellationToken,
        ) -> Result<Delegated, Unanswered> {
            Err(Unanswered::Unknown)
        }
    }

    let read = refusal(
        Arc::new(Delegator) as Arc<dyn Subagents>,
        serde_json::json!({
            "description": "spin up a worker",
            "prompt": "hold the fort",
            "subagent_type": "general",
            "name": "worker",
        }),
    )
    .await;

    assert_eq!(read, NO_TEAM);
}

/// A call with neither argument is the call this tool has always taken,
/// and it still reaches the delegation seam.
#[tokio::test]
async fn a_call_that_names_no_teammate_still_delegates() {
    let spawn = Fake::spawning();
    let read = refusal(
        Arc::clone(&spawn) as Arc<dyn Subagents>,
        serde_json::json!({
            "description": "find the main",
            "prompt": "where is it",
            "subagent_type": "nobody",
        }),
    )
    .await;

    assert_eq!(
        read, "Unknown agent type: nobody is not a valid agent type",
        "it reached delegate, not spawn_teammate"
    );
    assert!(spawn.started.lock().expect("no panic").is_empty());
}

/// A teammate's row is named by the teammate, since that is what a person
/// watching the team and the next message both address it by.
#[test]
fn a_teammate_row_is_named_by_the_teammate() {
    assert_eq!(
        tool().describe(&serde_json::json!({
            "subagent_type": "general",
            "description": "hold the fort",
            "name": "worker",
        })),
        "task: teammate worker — hold the fort"
    );
    assert_eq!(
        tool().describe(&serde_json::json!({
            "subagent_type": "general",
            "description": "find the main",
        })),
        "task: general — find the main"
    );
}

/// The order agents are handed over in is nobody's business but this
/// function's: upstream sorts them, and a registry does not promise one.
#[test]
fn a_roster_is_listed_in_name_order_however_it_was_handed_over() {
    let tool = TaskTool::new(&[
        Offered {
            name: "general".to_owned(),
            description: Some("does the general thing".to_owned()),
        },
        Offered {
            name: "explore".to_owned(),
            description: Some("finds things".to_owned()),
        },
    ]);
    let described = tool.description();

    assert!(
        described.starts_with(DESCRIPTION),
        "upstream's text comes first, unedited"
    );
    // Only the tail past the header is the roster: upstream's own text
    // carries `- ` bullets of its own.
    let (_, listed) = described
        .split_once(ROSTER_HEADER)
        .expect("the roster header is appended");
    let roster: Vec<&str> = listed
        .lines()
        .filter(|line| line.starts_with("- "))
        .collect();
    assert_eq!(
        roster,
        vec![
            "- explore: finds things",
            "- general: does the general thing"
        ]
    );
}

/// An agent that describes itself nowhere is still offered, under
/// upstream's stand-in line.
#[test]
fn a_subagent_with_nothing_to_say_for_itself_gets_upstreams_line() {
    let tool = TaskTool::new(&[Offered {
        name: "quiet".to_owned(),
        description: None,
    }]);

    assert!(
        tool.description()
            .ends_with("\n- quiet: This subagent should only be called manually by the user."),
        "got {}",
        tool.description()
    );
}

/// The exact bytes the parent model reads a delegated answer in. Upstream's
/// `renderOutput`, and the thing a frontend has no other way to recover.
#[test]
fn a_result_is_wrapped_in_upstreams_xml() {
    assert_eq!(
        render("ses_1", "completed", "task_result", "it holds a main"),
        "<task id=\"ses_1\" state=\"completed\">\n<task_result>\nit holds a main\n</task_result>\n</task>"
    );
    assert_eq!(
        render("ses_1", "error", "task_error", "no credentials"),
        "<task id=\"ses_1\" state=\"error\">\n<task_error>\nno credentials\n</task_error>\n</task>"
    );
}
