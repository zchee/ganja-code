//! The four task tools, as an engine that leads a team really serves them.
//!
//! What this pins is the wiring: that a lead is **offered** `task_create`,
//! `task_update`, `task_list` and `task_get`; that calling them moves the
//! documents in the team's own directory; that a teammate is lent the same
//! four and claims through them under **its own** name; and that the lead's
//! listing afterwards is the work its teammate did. The store's own guarantees
//! are `ganja-team`'s to pin — this is the seam above them.
//!
//! The provider double is [`ganja_testkit::Director`], which answers by what
//! it was asked rather than by a position in a queue — its own doc gives the
//! reason, and this suite is the one that had it first: a lead, its in-process
//! teammate and a title request all reach the one provider here.
//!
//! Every root is handed in and nothing here reads or writes the environment,
//! so this binary may hold more than one test.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use ganja_core::Engine;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{PartBody, ToolState};
use ganja_core::provider::{ChatRequest, ProviderEvent};
use ganja_core::teammate::TeammateRegistry;
use ganja_core::tool::Registry;
use ganja_protocol::FinishReason;
use ganja_team::task::{Store, TaskId, TaskStatus};
use ganja_team::{TeamName, TeamsRoot};
use ganja_testkit::{
    RecordedSpawns, caller, eventually, says, spawn_with_prompt, tool_call, transcript,
};
use serde_json::json;

mod lead;

/// How long the two engines are given. Generous against a loaded machine: a
/// teammate's runner polls its mailbox, so this waits on a poll rather than on
/// a machine.
const EVENTUALLY: Duration = Duration::from_secs(20);

/// The lead's first prompt, appearing nowhere else.
const FILE_PROMPT: &str = "file the work, zarquon";

/// The lead's second prompt, appearing nowhere else.
const LIST_PROMPT: &str = "how is it going, zarquon";

/// The prompt the teammate is spawned with, which reaches it through its
/// mailbox and comes back as the first thing its own engine asks about.
const WORK_PROMPT: &str = "take the task and finish it, zarquon";

/// The teammate's name, which is also the owner a claim writes.
const WORKER: &str = "worker-1";

/// What the filed task is called.
const SUBJECT: &str = "port the parser";

/// What the teammate says about it, so the comment can be found by its text
/// alone.
const NOTE: &str = "the lexer was the hard half, zarquon";

/// Whether this conversation has already called `tool`.
fn called(request: &ChatRequest, tool: &str) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .any(|part| matches!(&part.body, PartBody::Tool { tool: called, .. } if called == tool))
}

/// What to answer this request with.
///
/// The arms are the conversations, and within a conversation the step is read
/// off what the previous call answered rather than off a counter, so two
/// engines interleaving cannot take each other's turn.
fn script(request: &ChatRequest) -> Vec<ProviderEvent> {
    // A title request carries no tools at all, and is the one kind of request
    // neither conversation asked for.
    if request.tools.is_empty() {
        return says("a title");
    }
    let said = transcript(request);

    if said.contains(WORK_PROMPT) {
        // The teammate: claim it and start, then finish it with a word about
        // what it did, then report.
        if !called(request, "task_update") {
            return tool_call(
                "task_update",
                json!({"task_id": "1", "owner": WORKER, "status": "in_progress"}),
            );
        }
        if !said.contains("[completed]") {
            return tool_call(
                "task_update",
                json!({"task_id": "1", "status": "completed", "add_comment": NOTE}),
            );
        }

        return says("done");
    }
    if said.contains(LIST_PROMPT) {
        // The lead, asking what became of it.
        if !called(request, "task_list") {
            return tool_call("task_list", json!({}));
        }

        return says("the team finished it");
    }
    if said.contains(FILE_PROMPT) {
        // The lead, filing the work before anybody exists to do it.
        if !called(request, "task_create") {
            return tool_call(
                "task_create",
                json!({"subject": SUBJECT, "description": "start from the spec"}),
            );
        }

        return says("filed");
    }

    vec![ProviderEvent::Finish(FinishReason::Completed)]
}

/// The lead: a persistent engine over its own store, wired to a team, its
/// birth queue drained.
struct Lead {
    root: TeamsRoot,
    team: TeamName,
    registry: Arc<TeammateRegistry>,
    engine: Engine,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    asker: RecordedSpawns,
    /// Declared last so it is dropped last: the engine's storage lives under
    /// it, and taking the directory away while the engine still holds it is
    /// the reverse of the safe order.
    home: tempfile::TempDir,
}

impl Lead {
    async fn new() -> Self {
        let (root, team, registry, storage, home) = lead::ground();
        let (provider, requests) = ganja_testkit::Director::answering(script);

        let engine = Engine::persistent(
            provider,
            "recorder-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
            storage,
        )
        .with_teammates(Arc::clone(&registry), ganja_testkit::externals());
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        tokio::spawn(async move { while events.next().await.is_some() {} });

        Self { root, team, registry, engine, requests, asker: RecordedSpawns::default(), home }
    }

    /// The documents this team's list is kept in.
    fn store(&self) -> Store {
        lead::store(&self.root, &self.team)
    }

    async fn prompt(&self, text: &str) {
        lead::prompt(&self.engine, text).await;
    }

    /// Every request either engine has made so far.
    fn asked(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("the request log is never poisoned").clone()
    }

    /// The tools one of those requests was offered.
    fn offered(&self, needle: &str) -> Vec<String> {
        self.asked()
            .iter()
            .find(|request| transcript(request).contains(needle))
            .map(|request| request.tools.iter().map(|definition| definition.name.clone()).collect())
            .unwrap_or_default()
    }
}

/// The lead files the work, and the documents in the team's own directory are
/// what say it happened.
#[tokio::test]
async fn a_leads_own_call_files_a_task_in_the_teams_directory() {
    let lead = Lead::new().await;
    lead.prompt(FILE_PROMPT).await;

    let filed = eventually(EVENTUALLY, "the task to reach the team's directory", async || {
        lead.store().list().ok().filter(|listed| !listed.is_empty())
    })
    .await;

    assert_eq!(filed.len(), 1, "one call, one task: {filed:?}");
    assert_eq!(filed[0].id.to_string(), "1", "ids start at one");
    assert_eq!(filed[0].subject, SUBJECT);
    assert_eq!(filed[0].status, TaskStatus::Pending, "a filed task is pending");
    assert!(filed[0].owner.is_empty(), "and belongs to nobody yet");

    let offered = lead.offered(FILE_PROMPT);
    for tool in ["task_create", "task_update", "task_list", "task_get"] {
        assert!(offered.contains(&tool.to_owned()), "a lead is offered {tool}: {offered:?}");
    }

    lead.engine.shutdown_teammates().await;
}

/// The whole wave, end to end in one process: the lead files the work, a
/// teammate lent the same four tools claims it under **its own** name and
/// finishes it, and the lead's own listing afterwards is what its teammate
/// did.
#[tokio::test]
async fn a_teammate_claims_and_completes_the_task_its_lead_filed() {
    let lead = Lead::new().await;
    lead.prompt(FILE_PROMPT).await;
    let task = eventually(EVENTUALLY, "the lead to file the task", async || {
        lead.store().list().ok().filter(|listed| !listed.is_empty()).map(|listed| listed[0].id)
    })
    .await;

    lead.engine
        .teammates()
        .expect("this session leads a team")
        .start(
            spawn_with_prompt(WORKER, Some("in-process"), WORK_PROMPT),
            &caller(lead.home.path()),
            &lead.asker,
        )
        .await
        .expect("an in-process teammate starts on a session that has a store");

    // The teammate's turn is started by its own runner off the mailbox, so
    // this waits on the documents rather than on a call this test made.
    let finished = eventually(EVENTUALLY, "the teammate to finish the task", async || {
        lead.store().get(&task).ok().filter(|task| task.status == TaskStatus::Completed)
    })
    .await;

    assert_eq!(finished.owner, WORKER, "the claim wrote the teammate's own name");
    assert_eq!(
        finished.comments.iter().map(|comment| comment.from.as_str()).collect::<Vec<_>>(),
        [WORKER],
        "and so did the comment: an author is the list's to stamp, never an argument"
    );
    assert_eq!(finished.comments[0].text, NOTE);

    // A teammate is lent the same four, and no `task` tool: a teammate is not
    // a place to nest a second team.
    let lent = lead.offered(WORK_PROMPT);
    for tool in ["task_create", "task_update", "task_list", "task_get"] {
        assert!(lent.contains(&tool.to_owned()), "a teammate is lent {tool}: {lent:?}");
    }
    assert!(!lent.contains(&"task".to_owned()), "and not the spawn door: {lent:?}");

    // And the lead reads the work back: its own listing is what its teammate
    // did, owner and all.
    lead.prompt(LIST_PROMPT).await;
    let listing =
        eventually(EVENTUALLY, "the lead to read its list back", async || {
            lead.asked().iter().find_map(|request| {
                request.messages.iter().flat_map(|message| &message.parts).find_map(|part| {
                    match &part.body {
                        PartBody::Tool {
                            tool, state: ToolState::Completed { output, .. }, ..
                        } if tool == "task_list" => Some(output.clone()),
                        _ => None,
                    }
                })
            })
        })
        .await;

    assert_eq!(
        listing,
        format!("1 [completed] owner {WORKER} — {SUBJECT}"),
        "the lead's listing carries the status and the owner its teammate wrote"
    );

    lead.engine.shutdown_teammates().await;
}

/// A session that leads **nobody** still has a list, and that is the ordinary
/// case rather than an edge one: a lead files the work before it spawns
/// whoever will do it, so a list withheld until the first member would be
/// withheld at exactly the moment it is used first.
#[tokio::test]
async fn a_session_that_leads_nobody_is_still_offered_the_list() {
    let lead = Lead::new().await;
    lead.prompt(FILE_PROMPT).await;
    eventually(EVENTUALLY, "the task to be filed with no teammate anywhere", async || {
        lead.store().get(&TaskId::parse("1").expect("an id")).ok()
    })
    .await;

    assert!(lead.registry.leads_nobody(), "nobody was ever spawned into this team");

    lead.engine.shutdown_teammates().await;
}
