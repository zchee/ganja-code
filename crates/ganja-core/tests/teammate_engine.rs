//! What an engine that leads a team is wired to (**D498**, **D500**,
//! **D501**, **D-5**).
//!
//! The pieces are each somebody else's — the registry, the backends, the
//! postboxes, the dialog channel — and what this pins is the assembly: that
//! [`Engine::with_teammates`] really hands the in-process backend this
//! session's own provider, tool set and *store*, that both engines end up
//! offering `send_message` described against the right roster, and that the
//! dialog queue is claimable exactly once.
//!
//! The witness is the provider. Every request either engine makes is recorded
//! by the same scripted double, so what the lead is offered and what its
//! teammate is offered are read off the wire rather than off a private field —
//! which is also the only way to see the teammate's set at all, since a
//! teammate engine is reachable by shared reference and its registry is the
//! backend's.
//!
//! Every root is handed in and nothing here reads or writes the environment,
//! so this binary may hold more than one test.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use ganja_core::permission::Permissions;
use ganja_core::protocol::Command;
use ganja_core::provider::{ChatRequest, Provider};
use ganja_core::teammate::TeammateRegistry;
use ganja_core::tool::task::Teammated;
use ganja_core::tool::{Registry, send_message};
use ganja_core::{Engine, Storage};
use ganja_team::{TeamName, TeamsRoot, mailbox};
use ganja_testkit::{
    LEAD_SESSION_ID, RecordedSpawns, ScriptedProvider, TEAM, caller, eventually, says,
    spawn_with_prompt, team_file, tool_call,
};

/// How long the two engines are given to have asked the provider. Generous
/// against a loaded machine: the teammate's runner polls, so this is waiting
/// on a poll interval rather than on a machine.
const EVENTUALLY: Duration = Duration::from_secs(20);

/// The lead's own prompt, appearing nowhere else, so its request can be picked
/// out of what both engines asked.
const LEAD_PROMPT: &str = "the lead's own turn, zarquon";

/// The spawn prompt, which reaches the teammate through its mailbox and comes
/// back to us as the first thing that teammate's own engine asks about.
const TEAMMATE_PROMPT: &str = "the teammate's instructions, zarquon";

/// What the teammate writes to the lead, appearing nowhere else.
const REPORT: &str = "reporting in, zarquon";

/// What the lead writes to the teammate, appearing nowhere else.
const MEMO: &str = "look at the build, zarquon";

/// How many requests one teammate's spawn accounts for: the turn it takes on
/// the task in its mailbox, and the title a persistent engine asks for once
/// that turn has a message to name.
///
/// A fact about the script and the engine, not about the machine — which is
/// what lets the wait below be a count rather than a stillness.
const SPAWN_REQUESTS: usize = 2;

/// Waits until the teammate has asked its [`SPAWN_REQUESTS`], and answers how
/// many requests there were by then.
///
/// The teammate's turn is started by its runner rather than by this test, so
/// "it has finished" is not a thing that can be awaited. Waiting for the log
/// to *stand still* was the previous shape and it was a sleep in disguise: it
/// passed on a fast machine and would have started failing on a loaded one for
/// reasons having nothing to do with the code under test. Counting works
/// because the double records a request **before** it pops the script for it,
/// so by the time the second request is logged both scripts are spent and the
/// next push can only be the lead's.
async fn spawn_turn_taken(requests: &Arc<std::sync::Mutex<Vec<ChatRequest>>>) -> usize {
    eventually(EVENTUALLY, "the teammate to have taken its spawn turn", async || {
        let seen = requests.lock().expect("the request log is never poisoned").len();

        (seen >= SPAWN_REQUESTS).then_some(seen)
    })
    .await
}

/// The lead's side of every test here: a persistent engine over its own
/// store, wired to a team, its birth queue drained.
struct Lead {
    home: tempfile::TempDir,
    root: TeamsRoot,
    team: TeamName,
    registry: Arc<TeammateRegistry>,
    engine: Engine,
    asker: RecordedSpawns,
}

async fn lead(provider: Arc<dyn Provider>) -> Lead {
    let home = ganja_testkit::temp_dir();
    let storage = Storage::open(home.path().join("storage"));
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse(TEAM).expect("a team name");
    let registry =
        Arc::new(TeammateRegistry::new(root.clone(), team.clone(), LEAD_SESSION_ID, home.path()));

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

    Lead { home, root, team, registry, engine, asker: RecordedSpawns::default() }
}

/// The door the `task` tool goes through, driven directly: what a call adds
/// is a name and a prompt, and everything else — the store, the provider,
/// the tools, the rules — is what the engine wired in.
async fn spawn_worker(lead: &Lead) -> Teammated {
    let started = lead
        .engine
        .teammates()
        .expect("this session leads a team")
        .start(
            spawn_with_prompt("worker", Some("in-process"), TEAMMATE_PROMPT),
            &caller(lead.home.path()),
            &lead.asker,
        )
        .await
        .expect("an in-process teammate starts on a session that has a store");
    lead.asker.asked_nobody();

    started
}

/// What one engine was offered, as the request it sent carries it.
fn offered<'a>(request: &'a ChatRequest, tool: &str) -> Option<&'a str> {
    request
        .tools
        .iter()
        .find(|definition| definition.name == tool)
        .map(|definition| definition.description.as_str())
}

/// The request whose conversation carries `needle`.
fn asked_about(requests: &[ChatRequest], needle: &str) -> Option<ChatRequest> {
    requests
        .iter()
        .find(|request| {
            request.messages.iter().any(|message| {
                message
                    .parts
                    .iter()
                    .filter_map(ganja_core::protocol::Part::as_text)
                    .any(|text| text.contains(needle))
            })
        })
        .cloned()
}

/// The first tool answer carrying `needle`, read off what a request's
/// transcript holds — a failed call travels as the error the model reads
/// next, so this is what "the sender is told" looks like on the wire.
fn tool_answer_about(requests: &[ChatRequest], needle: &str) -> Option<String> {
    use ganja_core::protocol::{PartBody, ToolState};

    requests
        .iter()
        .flat_map(|request| &request.messages)
        .flat_map(|message| &message.parts)
        .find_map(|part| match &part.body {
            PartBody::Tool { state: ToolState::Error { error, .. }, .. }
                if error.contains(needle) =>
            {
                Some(error.clone())
            }
            PartBody::Tool { state: ToolState::Completed { output, .. }, .. }
                if output.contains(needle) =>
            {
                Some(output.clone())
            }
            _ => None,
        })
}

/// The engine's team wiring, end to end: the door starts a teammate over this
/// session's own store, and both sides are told who they may write to.
#[tokio::test]
async fn a_team_gives_both_engines_a_postbox_and_the_teammate_a_store() {
    // Enough scripts for both engines' turns and the titles a persistent
    // engine asks for; the double completes rather than improvises once they
    // run out, so a script that goes to the "wrong" turn costs nothing here —
    // every assertion below is keyed on what a request *carried*.
    let (provider, requests) = ScriptedProvider::new(vec![
        says("the lead is done"),
        says("the teammate is done"),
        says("a title"),
        says("another title"),
    ]);
    let lead = lead(provider).await;

    // Claimed once, and once only: two readers would split the dialogs
    // between them.
    assert!(
        lead.engine.teammate_dialogs().is_some(),
        "a session with a team has a dialog queue to claim"
    );
    assert!(lead.engine.teammate_dialogs().is_none(), "and there is exactly one of it");

    let started = spawn_worker(&lead).await;
    assert_eq!(started.name, "worker");
    assert_eq!(started.backend, "in-process");

    let file = team_file(&lead.root, &lead.team).expect("the team file was written");
    assert!(
        file.members.iter().any(|member| member.name == "worker"),
        "the spawn wrote the member record: {:?}",
        file.members
    );

    lead.engine
        .send(Command::SendPrompt {
            text: LEAD_PROMPT.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // Both engines have to have asked before either roster can be read. The
    // teammate's own turn is started by its runner off the mailbox, which is a
    // poll rather than a call, so this waits rather than assumes.
    let (lead_request, worker_request) =
        eventually(EVENTUALLY, "both engines to have asked", async || {
            let seen = requests.lock().expect("the request log is never poisoned").clone();
            match (asked_about(&seen, LEAD_PROMPT), asked_about(&seen, TEAMMATE_PROMPT)) {
                (Some(lead), Some(worker)) => Some((lead, worker)),
                _ => None,
            }
        })
        .await;

    // The lead is offered the tool, described against the team it leads.
    let leads = offered(&lead_request, send_message::ID).expect("the lead is offered send_message");
    assert!(leads.contains("worker"), "the lead's roster names the teammate it started: {leads}");

    // And so is the teammate, described against the one peer that existed
    // before it did. Its *delivery* goes through a postbox of its own, which
    // is what stamps its name on what it writes; this is the half a request
    // can show.
    let works =
        offered(&worker_request, send_message::ID).expect("a teammate is offered send_message");
    assert!(works.contains(ganja_team::LEAD), "a teammate's roster names the lead: {works}");
    assert!(
        offered(&worker_request, "task").is_none(),
        "a teammate is not a place to nest a second team"
    );

    lead.engine.shutdown_teammates().await;
}

/// **D526**, the in-team half: the admission gate deliberately does not
/// gate roster mail, so a teammate pouring into a full lead inbox is bounded
/// by the *write* — refused by name at the ceiling — and reads that refusal
/// back as the failed delivery `send_message` already answers with. The
/// inbox is byte-identical after: the backlog nobody drained is not reshaped
/// by the message that failed to join it.
#[tokio::test]
async fn a_send_into_a_full_inbox_reports_the_named_refusal_and_changes_nothing() {
    let (provider, requests) = ScriptedProvider::new(vec![
        tool_call(send_message::ID, serde_json::json!({"to": ganja_team::LEAD, "message": REPORT})),
        says("told about it"),
        says("a title"),
    ]);
    let lead = lead(provider).await;
    let inbox = lead.registry.lead_inbox();
    let planted = ganja_testkit::flooded_inbox(&inbox);

    spawn_worker(&lead).await;

    let refusal = eventually(EVENTUALLY, "the sender to read the named refusal", async || {
        let seen = requests.lock().expect("the request log is never poisoned").clone();

        tool_answer_about(&seen, "past its ceiling")
    })
    .await;
    assert!(
        refusal.contains("could not be written"),
        "the sender is told the write failed, in the delivery arm's own words: {refusal}"
    );
    assert!(!refusal.contains("xxxx"), "a refusal carries counts, never a body");

    assert_eq!(
        std::fs::read_to_string(&inbox).expect("the inbox is readable"),
        planted,
        "a refused append leaves the lead's inbox byte-identical"
    );

    lead.engine.shutdown_teammates().await;
}

/// A teammate's own `send_message` really posts, and posts as itself.
///
/// The lead takes no turn here, on purpose: both engines ask the same scripted
/// double, so the only way to hand a script to the teammate deterministically
/// is to be the only one asking. What that buys is the half a roster cannot
/// show — the postbox the registry installs on a teammate engine, and the name
/// it stamps, which is the teammate's own and not one the model could type.
#[tokio::test]
async fn a_teammates_message_reaches_the_lead_stamped_with_the_teammates_own_name() {
    let (provider, _requests) = ScriptedProvider::new(vec![
        tool_call(send_message::ID, serde_json::json!({"to": ganja_team::LEAD, "message": REPORT})),
        says("reported"),
        says("a title"),
    ]);
    let lead = lead(provider).await;

    spawn_worker(&lead).await;

    let inbox = lead.registry.lead_inbox();
    let posted =
        eventually(EVENTUALLY, "the teammate's message to reach the lead's inbox", async || {
            mailbox::read(&inbox)
                .map(|read| read.valid)
                .unwrap_or_default()
                .iter()
                .find(|message| message.text.contains(REPORT))
                .cloned()
        })
        .await;

    assert_eq!(
        posted.from, "worker",
        "a message carries the name its sender was given, not one anything typed"
    );

    lead.engine.shutdown_teammates().await;
}

/// The other direction: what the lead's own `send_message` writes is what its
/// teammate reads next.
///
/// Asserted on the **teammate's request** rather than on its inbox, and that is
/// what makes it deterministic rather than a race: the runner prunes an entry
/// when it takes it into a turn, so an inbox is a thing that empties, where the
/// record of what was asked only ever grows.
///
/// The two engines share one scripted double, so the teammate is let go quiet
/// first — its spawn turn and its title — and the script the lead needs is
/// pushed only once nothing else is asking.
#[tokio::test]
async fn what_the_lead_sends_is_what_its_teammate_reads_next() {
    let (provider, requests) =
        ScriptedProvider::new(vec![says("on it"), says("the teammate's title")]);
    let lead = lead(Arc::clone(&provider) as Arc<dyn Provider>).await;

    spawn_worker(&lead).await;

    // The teammate has read its spawn prompt and spent both of its scripts.
    // Only then is the lead's pushed, so the call below is the lead's and
    // nobody else's.
    let asked = spawn_turn_taken(&requests).await;
    provider
        .push(tool_call(send_message::ID, serde_json::json!({"to": "worker", "message": MEMO})));
    provider.push(says("sent"));
    provider.push(says("the lead's title"));

    lead.engine
        .send(Command::SendPrompt {
            text: LEAD_PROMPT.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let delivered =
        eventually(EVENTUALLY, "the lead's message to reach the teammate's turn", async || {
            let seen = requests.lock().expect("the request log is never poisoned").clone();

            asked_about(&seen[asked..], MEMO)
        })
        .await;

    let read: String = delivered
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_core::protocol::Part::as_text)
        .collect();
    assert!(read.contains(ganja_team::LEAD), "the teammate is told who wrote to it: {read}");

    lead.engine.shutdown_teammates().await;
}
