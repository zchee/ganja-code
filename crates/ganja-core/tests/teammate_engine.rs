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

use std::{sync::Arc, time::Duration};

use futures::StreamExt as _;
use ganja_core::{
    Caller, Engine, SpawnAsk, SpawnAsker, Storage,
    permission::Permissions,
    protocol::Command,
    provider::ChatRequest,
    teammate::TeammateRegistry,
    tool::{Registry, send_message, task::TeammateSpawn},
};
use ganja_team::{TeamFile, TeamName, TeamsRoot, mailbox};
use ganja_testkit::{ScriptedProvider, says, tool_call};

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

/// How long the request log has to stand still before nobody is asking.
const QUIET: Duration = Duration::from_millis(400);

/// Waits until nothing has asked the provider for [`QUIET`], and answers how
/// many requests there were by then.
///
/// The teammate's turn is started by its runner rather than by this test, so
/// "it has finished" is not a thing that can be awaited — but "it has stopped
/// asking" is, and that is the property the script push below needs.
async fn quiet(requests: &Arc<std::sync::Mutex<Vec<ChatRequest>>>) -> usize {
    let deadline = tokio::time::Instant::now() + EVENTUALLY;
    loop {
        let seen = requests
            .lock()
            .expect("the request log is never poisoned")
            .len();
        tokio::time::sleep(QUIET).await;
        let after = requests
            .lock()
            .expect("the request log is never poisoned")
            .len();
        // Something asked, and then nothing did: the first half is what says
        // the runner really woke up, the second that it is done.
        if after > 0 && after == seen {
            return after;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the teammate should have taken its spawn turn and stopped by now"
        );
    }
}

/// The calling turn, as the spawn gate reads it: a teammate inherits the
/// model and the directory of the turn that started it, and the rules a spawn
/// is judged by are the **lead's** own.
///
/// `cwd` and `project_root` are the same directory here, which is the ordinary
/// case and the one that asks nobody anything: a teammate working inside the
/// project discloses no directory and raises no dialog.
fn caller(home: &std::path::Path) -> Caller {
    Caller {
        model: "recorder-model".to_owned(),
        cwd: home.to_path_buf(),
        permissions: Arc::new(std::sync::Mutex::new(Permissions::default())),
        project_root: home.to_path_buf(),
    }
}

/// A person who says yes. Nothing below asks it anything — a spawn inside the
/// project with no bypass has nothing to ask about — so this exists to prove
/// the gate's *absence* rather than its answer.
#[derive(Debug)]
struct Allowing;

#[async_trait::async_trait]
impl SpawnAsker for Allowing {
    async fn ask(&self, _request: SpawnAsk) -> ganja_core::protocol::PermissionReply {
        ganja_core::protocol::PermissionReply::Once
    }
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

/// The engine's team wiring, end to end: the door starts a teammate over this
/// session's own store, and both sides are told who they may write to.
#[tokio::test]
async fn a_team_gives_both_engines_a_postbox_and_the_teammate_a_store() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(home.path().join("storage"));
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-abcd1234").expect("a team name");
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
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-0000-7000-8000-000000000000",
        home.path(),
    ));

    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
    .with_teammates(Arc::clone(&registry));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    tokio::spawn(async move { while events.next().await.is_some() {} });

    // Claimed once, and once only: two readers would split the dialogs
    // between them.
    assert!(
        engine.teammate_dialogs().is_some(),
        "a session with a team has a dialog queue to claim"
    );
    assert!(
        engine.teammate_dialogs().is_none(),
        "and there is exactly one of it"
    );

    // The door the `task` tool goes through, driven directly: what a call adds
    // is a name and a prompt, and everything else — the store, the provider,
    // the tools, the rules — is what the engine wired in.
    let started = engine
        .teammates()
        .expect("this session leads a team")
        .start(
            TeammateSpawn {
                name: "worker".to_owned(),
                backend: None,
                agent_type: "general".to_owned(),
                prompt: TEAMMATE_PROMPT.to_owned(),
            },
            &caller(home.path()),
            &Allowing,
        )
        .await
        .expect("an in-process teammate starts on a session that has a store");
    assert_eq!(started.name, "worker");
    assert_eq!(started.backend, "in-process");

    let file: TeamFile = serde_json::from_str(
        &std::fs::read_to_string(root.config_path(&team)).expect("the team file was written"),
    )
    .expect("the team file this build wrote decodes");
    assert!(
        file.members.iter().any(|member| member.name == "worker"),
        "the spawn wrote the member record: {:?}",
        file.members
    );

    engine
        .send(Command::SendPrompt {
            text: LEAD_PROMPT.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // Both engines have to have asked before either roster can be read. The
    // teammate's own turn is started by its runner off the mailbox, which is a
    // poll rather than a call, so this waits rather than assumes.
    let deadline = tokio::time::Instant::now() + EVENTUALLY;
    let (lead, worker) = loop {
        let seen = requests
            .lock()
            .expect("the request log is never poisoned")
            .clone();
        if let Some(lead) = asked_about(&seen, LEAD_PROMPT)
            && let Some(worker) = asked_about(&seen, TEAMMATE_PROMPT)
        {
            break (lead, worker);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "both engines should have asked by now, got {} requests",
            seen.len()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // The lead is offered the tool, described against the team it leads.
    let leads = offered(&lead, send_message::ID).expect("the lead is offered send_message");
    assert!(
        leads.contains("worker"),
        "the lead's roster names the teammate it started: {leads}"
    );

    // And so is the teammate, described against the one peer that existed
    // before it did. Its *delivery* goes through a postbox of its own, which
    // is what stamps its name on what it writes; this is the half a request
    // can show.
    let works = offered(&worker, send_message::ID).expect("a teammate is offered send_message");
    assert!(
        works.contains(ganja_team::LEAD),
        "a teammate's roster names the lead: {works}"
    );
    assert!(
        offered(&worker, "task").is_none(),
        "a teammate is not a place to nest a second team"
    );

    engine.shutdown_teammates().await;
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
    let home = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(home.path().join("storage"));
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-abcd1234").expect("a team name");
    let (provider, _requests) = ScriptedProvider::new(vec![
        tool_call(
            send_message::ID,
            serde_json::json!({"to": ganja_team::LEAD, "message": REPORT}),
        ),
        says("reported"),
        says("a title"),
    ]);
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-0000-7000-8000-000000000000",
        home.path(),
    ));

    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
    .with_teammates(Arc::clone(&registry));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    tokio::spawn(async move { while events.next().await.is_some() {} });

    engine
        .teammates()
        .expect("this session leads a team")
        .start(
            TeammateSpawn {
                name: "worker".to_owned(),
                backend: None,
                agent_type: "general".to_owned(),
                prompt: TEAMMATE_PROMPT.to_owned(),
            },
            &caller(home.path()),
            &Allowing,
        )
        .await
        .expect("an in-process teammate starts");

    let inbox = registry.lead_inbox();
    let deadline = tokio::time::Instant::now() + EVENTUALLY;
    let posted = loop {
        let waiting = mailbox::read(&inbox)
            .map(|read| read.valid)
            .unwrap_or_default();
        if let Some(message) = waiting
            .iter()
            .find(|message| message.text.contains(REPORT))
            .cloned()
        {
            break message;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the teammate's message should have reached the lead's inbox by now"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert_eq!(
        posted.from, "worker",
        "a message carries the name its sender was given, not one anything typed"
    );

    engine.shutdown_teammates().await;
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
    let home = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(home.path().join("storage"));
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse("session-abcd1234").expect("a team name");
    let (provider, requests) =
        ScriptedProvider::new(vec![says("on it"), says("the teammate's title")]);
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-0000-7000-8000-000000000000",
        home.path(),
    ));

    let engine = Engine::persistent(
        Arc::clone(&provider) as Arc<dyn ganja_core::provider::Provider>,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
    .with_teammates(Arc::clone(&registry));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    tokio::spawn(async move { while events.next().await.is_some() {} });

    engine
        .teammates()
        .expect("this session leads a team")
        .start(
            TeammateSpawn {
                name: "worker".to_owned(),
                backend: None,
                agent_type: "general".to_owned(),
                prompt: TEAMMATE_PROMPT.to_owned(),
            },
            &caller(home.path()),
            &Allowing,
        )
        .await
        .expect("an in-process teammate starts");

    // The teammate has read its spawn prompt and stopped asking. Only then is
    // the lead's script pushed, so the call below is the lead's and nobody
    // else's.
    let asked = quiet(&requests).await;
    provider.push(tool_call(
        send_message::ID,
        serde_json::json!({"to": "worker", "message": MEMO}),
    ));
    provider.push(says("sent"));
    provider.push(says("the lead's title"));

    engine
        .send(Command::SendPrompt {
            text: LEAD_PROMPT.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let deadline = tokio::time::Instant::now() + EVENTUALLY;
    let delivered = loop {
        let seen = requests
            .lock()
            .expect("the request log is never poisoned")
            .clone();
        if let Some(request) = asked_about(&seen[asked..], MEMO) {
            break request;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the lead's message should have reached the teammate's turn by now"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let read: String = delivered
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_core::protocol::Part::as_text)
        .collect();
    assert!(
        read.contains(ganja_team::LEAD),
        "the teammate is told who wrote to it: {read}"
    );

    engine.shutdown_teammates().await;
}
