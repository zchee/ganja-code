//! The base half of the system prompt, across a model that moves.
//!
//! The half an agent replaces is chosen by the model's **family** — Anthropic's
//! prompt, OpenAI's, or the one for everything else — so a session that
//! switches from a `claude` model to a `gpt` one and keeps the prompt it
//! launched with spends the rest of the conversation running the new model
//! under another family's instructions. Its environment block, meanwhile,
//! already names the new model: the prompt would describe two different
//! sessions at once.
//!
//! Everything here composes through the **real** `instruction::base_prompt`,
//! never a stand-in, so what is asserted is the text a model would actually
//! read. The engine is given no environment half, which leaves the system
//! prompt equal to the base alone and every assertion below an equality rather
//! than a search for a needle.
//!
//! The suffix half's own recomposition is pinned in `agents.rs`; this file is
//! its sibling, and the two are deliberately separate binaries so that a
//! regression in one half names the half it is in.

use std::sync::Arc;

use ganja_core::{
    AgentConfig, Config, Engine, instruction,
    permission::Permissions,
    protocol::{Command, Event, PermissionReply},
    tool::Registry,
};
use ganja_testkit::{ScriptedProvider, agent_registry, drain, drain_answering, says, tool_call};
use serde_json::json;

/// A model id in Anthropic's family, spelled so `base_prompt`'s substring
/// match lands there.
const CLAUDE: &str = "claude-fixture";

/// The same for OpenAI's, which is the other arm a cross-family switch has to
/// reach.
const GPT: &str = "gpt-fixture";

/// The engine under test: no tools, no environment half, and the base half
/// composed for whatever model is active.
fn engine(provider: Arc<ScriptedProvider>, model: &str) -> Engine {
    Engine::new(
        provider,
        model,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_base_for_model()
}

/// Sends `text` and waits for the turn it starts to finish.
async fn ask(engine: &Engine, events: &mut futures::stream::BoxStream<'static, Event>) {
    engine
        .send(Command::SendPrompt {
            text: "anything".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(events).await;
}

/// The system prompt of every request the provider was asked, in order.
fn systems(requests: &std::sync::Mutex<Vec<ganja_core::provider::ChatRequest>>) -> Vec<String> {
    requests
        .lock()
        .expect("the request log is never poisoned")
        .iter()
        .map(|request| {
            request
                .system
                .clone()
                .expect("every request carries the prompt it was built with")
        })
        .collect()
}

/// Asserts the two ids really are in different families, which is the whole
/// premise of every cross-family test here: a fixture whose two ids happened to
/// select the same prompt would pass on a build that never recomposed anything.
fn families_differ() {
    assert_ne!(
        instruction::base_prompt(CLAUDE),
        instruction::base_prompt(GPT),
        "the fixture only proves anything while the two ids land in different families"
    );
}

/// The defect: a cross-family switch moved the environment block and left the
/// base prompt behind, so the model was told which model it was in one half and
/// instructed as a different family in the other.
#[tokio::test]
async fn switching_to_a_model_of_another_family_recomposes_the_base_prompt() {
    families_differ();

    let (provider, seen) = ScriptedProvider::new(vec![says("one"), says("two")]);
    let engine = engine(provider, CLAUDE);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events).await;

    engine
        .send(Command::SwitchModel {
            model: GPT.to_owned(),
        })
        .await
        .expect("a provider the catalog does not cover takes the model at its word");

    ask(&engine, &mut events).await;

    let systems = systems(&seen);
    assert_eq!(systems.len(), 2, "one request per prompt");
    assert_eq!(
        systems[0],
        instruction::base_prompt(CLAUDE),
        "the session launched on a claude model and was told so"
    );
    assert_eq!(
        systems[1],
        instruction::base_prompt(GPT),
        "and the request after the switch carries the other family's prompt"
    );
}

/// The same half, moved by the other route a model moves: an agent that prefers
/// one switches the model with it, so it has to switch the family's prompt too.
#[tokio::test]
async fn switching_to_an_agent_that_prefers_another_familys_model_recomposes_the_base_prompt() {
    families_differ();

    let mut agent = std::collections::BTreeMap::new();
    agent.insert(
        "scribe".to_owned(),
        AgentConfig {
            model: Some(format!("recorder/{GPT}")),
            ..AgentConfig::default()
        },
    );
    let config = Config {
        agent,
        ..Config::default()
    };

    let (provider, seen) = ScriptedProvider::new(vec![says("one"), says("two")]);
    let engine = Engine::new(
        provider,
        CLAUDE,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(agent_registry(&config))
    .with_base_for_model();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events).await;

    engine
        .send(Command::SwitchAgent {
            name: "scribe".to_owned(),
        })
        .await
        .expect("a config agent is selectable");
    assert_eq!(
        engine.model(),
        GPT,
        "the fixture only proves anything while the agent really moves the model"
    );

    ask(&engine, &mut events).await;

    let systems = systems(&seen);
    assert_eq!(systems.len(), 2, "one request per prompt");
    assert_eq!(
        systems[0],
        instruction::base_prompt(CLAUDE),
        "the session launched under the launch model's family"
    );
    assert_eq!(
        systems[1],
        instruction::base_prompt(GPT),
        "and the agent that brought a model brought its family's prompt with it"
    );
}

/// The launch composition, by both routes a caller can chain it.
///
/// `with_agents` moves the model when the default agent prefers one, so the
/// base half a session starts on has to be *that* agent's family and not the
/// family of the model the process was started with. The two builders therefore
/// have an order between them, and neither order may lose: the frontend chains
/// `with_agents` first and relies on `with_base_for_model` composing against
/// the model active by then, while the reverse order relies on `with_agents`'
/// own recomposition. Both are asserted here — which is also what keeps that
/// second call from being the dead branch it would otherwise look like.
#[tokio::test]
async fn a_default_agent_that_prefers_another_familys_model_decides_the_launch_prompt() {
    families_differ();

    let mut agent = std::collections::BTreeMap::new();
    agent.insert(
        "scribe".to_owned(),
        AgentConfig {
            model: Some(format!("recorder/{GPT}")),
            ..AgentConfig::default()
        },
    );
    let config = Config {
        agent,
        default_agent: Some("scribe".to_owned()),
        ..Config::default()
    };

    for agents_first in [true, false] {
        let (provider, seen) = ScriptedProvider::new(vec![says("one")]);
        let bare = Engine::new(
            provider,
            CLAUDE,
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        );
        let engine = if agents_first {
            bare.with_agents(agent_registry(&config))
                .with_base_for_model()
        } else {
            bare.with_base_for_model()
                .with_agents(agent_registry(&config))
        };
        assert_eq!(
            engine.model(),
            GPT,
            "the fixture only proves anything while the default agent really moves the model"
        );

        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        ask(&engine, &mut events).await;

        assert_eq!(
            systems(&seen),
            vec![instruction::base_prompt(GPT).to_owned()],
            "chained with agents first = {agents_first}: the session starts on the \
             default agent's family, not on the launch model's"
        );
    }
}

/// A subagent runs on the parent's model, so it has to be handed the parent's
/// base prompt **as it now stands** — the family in force — and not the one the
/// parent launched with.
///
/// One ordered script drives both loops: the parent delegates, the child
/// answers, the parent answers. The child's own request is therefore the second
/// the provider was asked, and its system prompt is what is under test.
#[tokio::test]
async fn a_subagent_is_handed_the_base_prompt_of_the_family_in_force() {
    families_differ();

    let (provider, seen) = ScriptedProvider::new(vec![
        tool_call(
            "task",
            json!({
                "description": "find the thing",
                "prompt": "go and find the thing",
                "subagent_type": "general",
            }),
        ),
        // The child's own turn, which ends without calling anything.
        says("what the child found"),
        // And the parent's answer, once the delegation came back.
        says("what the parent made of it"),
    ]);
    let engine = Engine::new(
        provider,
        CLAUDE,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(agent_registry(&Config::default()))
    .with_base_for_model();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SwitchModel {
            model: GPT.to_owned(),
        })
        .await
        .expect("a provider the catalog does not cover takes the model at its word");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_answering(&engine, &mut events, PermissionReply::Once).await;

    // Which request is the child's is not taken on trust: a subagent is offered
    // this build's tools *minus* the one that spawns subagents, and `task` is
    // the only tool this engine has, so the middle request is the one with no
    // tools at all and the two around it are the parent's.
    {
        let requests = seen.lock().expect("the request log is never poisoned");
        let offered: Vec<Vec<&str>> = requests
            .iter()
            .map(|request| {
                request
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect()
            })
            .collect();
        assert_eq!(
            offered,
            vec![vec!["task"], Vec::new(), vec!["task"]],
            "the middle request is the child's, and it really is a second loop"
        );
    }

    let systems = systems(&seen);
    assert_eq!(
        systems.len(),
        3,
        "one ordered script drives both loops: parent, child, parent"
    );
    // `general` deliberately has no prompt of its own, which is what makes the
    // child fall through to the base half this test is about.
    assert_eq!(
        systems[1],
        instruction::base_prompt(GPT),
        "the child inherits the family in force, not the one the parent launched on"
    );
    assert_ne!(
        systems[1],
        instruction::base_prompt(CLAUDE),
        "and nothing of the launch family is left in what the child was told"
    );
}

/// Recomposition is invisible where the family does not move: a switch within
/// one family leaves the prompt exactly as it was, byte for byte.
#[tokio::test]
async fn a_switch_within_one_family_leaves_the_base_prompt_exactly_as_it_was() {
    const OTHER_CLAUDE: &str = "claude-fixture-two";

    let (provider, seen) = ScriptedProvider::new(vec![says("one"), says("two")]);
    let engine = engine(provider, CLAUDE);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events).await;

    engine
        .send(Command::SwitchModel {
            model: OTHER_CLAUDE.to_owned(),
        })
        .await
        .expect("a provider the catalog does not cover takes the model at its word");

    ask(&engine, &mut events).await;

    let systems = systems(&seen);
    assert_eq!(systems.len(), 2, "one request per prompt");
    assert_eq!(
        systems[0],
        instruction::base_prompt(CLAUDE),
        "the launch model's family"
    );
    assert_eq!(
        systems[1], systems[0],
        "and a sibling of that family is told exactly the same thing"
    );
}

/// An engine nobody asked to follow the model keeps the base it was handed,
/// however far the model moves — which is what every scripted and golden run
/// depends on, and what stops this whole mechanism from being on by default.
#[tokio::test]
async fn a_base_nobody_asked_to_follow_the_model_survives_a_cross_family_switch() {
    const FIXED: &str = "you are a fixture";

    let (provider, seen) = ScriptedProvider::new(vec![says("one"), says("two")]);
    let engine = Engine::new(
        provider,
        CLAUDE,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_system_parts(Some(FIXED.to_owned()), None);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    ask(&engine, &mut events).await;

    engine
        .send(Command::SwitchModel {
            model: GPT.to_owned(),
        })
        .await
        .expect("a provider the catalog does not cover takes the model at its word");

    ask(&engine, &mut events).await;

    assert_eq!(
        systems(&seen),
        vec![FIXED.to_owned(), FIXED.to_owned()],
        "a literal base is a literal base on both sides of a model switch"
    );
}
