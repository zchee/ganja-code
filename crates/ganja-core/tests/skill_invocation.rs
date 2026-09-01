//! `$name` skill invocation: the engine-side expansion **D491** records.
//!
//! What is proved here is the seam `session.rs`'s `skill_parts` owns: a
//! prompt's or steer's `skills` list becomes the same `<skill_content>` block
//! a `skill` tool call returns — **byte for byte**, checked against the
//! actual tool's output rather than against the shared function, so a fork of
//! either side fails here — a name nothing answers to becomes the tool's own
//! not-found sentence and the turn proceeds, and a `$word` the frontend did
//! not validate is simply text. The token scan itself is unit-tested where it
//! lives (`ganja-tool`'s `skill::requested_in`); the frontends' use of it is
//! theirs. No environment is touched: every root is an explicit temporary
//! directory, which is exactly the property `Engine::with_skill_roots`
//! exists to preserve.

use std::sync::Arc;

use ganja_core::Engine;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Command, Event, PermissionReply, Role};
use ganja_core::provider::ChatRequest;
use ganja_core::tool::skill::{Roots, SkillTool};
use ganja_core::tool::{Credentials, FileTimes, Registry, Tool as _, ToolCtx};
use ganja_testkit::{ScriptedProvider, drain, says, tool_call};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Model every engine here asks for; nothing depends on its family.
const MODEL: &str = "invocation-model";

/// Writes a skill at `<root>/<name>/SKILL.md`.
fn plant(root: &std::path::Path, name: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("the fixture's directories are creatable");
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: a fixture.\n---\n{body}"),
    )
    .expect("the fixture is writable");
}

/// What the `skill` tool itself hands back for `name` over `roots` — the
/// reference every injected part is compared against.
async fn tool_output(roots: Roots, name: &str) -> String {
    let ctx = ToolCtx {
        cwd: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        call_id: "call-reference".to_owned(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: None,
        postbox: None,
        tasks: None,
        ask: None,
        switch: None,
        jobs: None,
    };

    SkillTool::over(roots)
        .run(json!({ "name": name }), &ctx)
        .await
        .expect("the reference load succeeds")
        .output
}

/// Every text part on the user side of `request`, one entry per part.
fn user_parts(request: &ChatRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| part.as_text().map(str::to_owned))
        .collect()
}

/// An invoked skill reaches the request as its own part, byte-identical to
/// what a `skill` tool call over the same roots returns — and nothing about
/// the expansion asks anyone anything.
#[tokio::test]
async fn an_invocation_is_the_tools_own_rendering_byte_for_byte() {
    let dir = ganja_testkit::temp_dir();
    plant(dir.path(), "porting", "Read the upstream file first.");
    let roots = Roots::none().with_paths([dir.path().to_path_buf()]);

    let (provider, requests) = ScriptedProvider::new(vec![says("loaded")]);
    let engine =
        Engine::new(provider, MODEL, Arc::new(Registry::new(Vec::new())), Permissions::default())
            .with_skill_roots(roots.clone());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "explain $porting now, and leave $PATH alone".to_owned(),
            mentions: Vec::new(),
            skills: vec!["porting".to_owned()],
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    let expected = tool_output(roots, "porting").await;
    let requests = requests.lock().expect("the request log is never poisoned");
    let parts = user_parts(&requests[0]);
    assert_eq!(parts.len(), 2, "the prompt text and exactly one skill part: {parts:?}");
    assert_eq!(
        parts[0], "explain $porting now, and leave $PATH alone",
        "the token stays in the text the model reads"
    );
    assert_eq!(parts[1], expected, "the injected part is the tool's own rendering, byte for byte");
    assert!(
        !seen.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
        "an invocation crosses no permission dialog"
    );
}

/// A name nothing answers to becomes the tool's own not-found sentence —
/// information the model reads — and the turn still completes.
#[tokio::test]
async fn a_vanished_skill_is_reported_in_the_tools_words_and_the_turn_proceeds() {
    let dir = ganja_testkit::temp_dir();
    plant(dir.path(), "porting", "still here");
    let roots = Roots::none().with_paths([dir.path().to_path_buf()]);

    let (provider, requests) = ScriptedProvider::new(vec![says("noted")]);
    let engine =
        Engine::new(provider, MODEL, Arc::new(Registry::new(Vec::new())), Permissions::default())
            .with_skill_roots(roots);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "use $missing".to_owned(),
            mentions: Vec::new(),
            skills: vec!["missing".to_owned()],
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let parts = user_parts(&requests[0]);
    assert_eq!(
        parts[1], "Skill \"missing\" not found. Available skills: porting",
        "the miss reads exactly as the tool's refusal does: {parts:?}"
    );
    assert!(
        seen.iter().any(|event| matches!(event, Event::MessageFinished { .. })),
        "a miss is information, and the turn still finishes"
    );
}

/// Two roots claiming one name: the invocation loads what the tool would —
/// the later root's body, `discover`'s own collision rule.
#[tokio::test]
async fn a_name_collision_loads_the_same_body_the_tool_would() {
    let first = ganja_testkit::temp_dir();
    let second = ganja_testkit::temp_dir();
    plant(first.path(), "porting", "the first body");
    plant(second.path(), "porting", "the second body");
    let roots = Roots::none().with_paths([first.path().to_path_buf(), second.path().to_path_buf()]);

    let (provider, requests) = ScriptedProvider::new(vec![says("loaded")]);
    let engine =
        Engine::new(provider, MODEL, Arc::new(Registry::new(Vec::new())), Permissions::default())
            .with_skill_roots(roots.clone());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "$porting".to_owned(),
            mentions: Vec::new(),
            skills: vec!["porting".to_owned()],
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let expected = tool_output(roots, "porting").await;
    let requests = requests.lock().expect("the request log is never poisoned");
    let parts = user_parts(&requests[0]);
    assert_eq!(parts[1], expected);
    assert!(
        parts[1].contains("the second body") && !parts[1].contains("the first body"),
        "the later root wins here exactly as it does in discovery: {}",
        parts[1]
    );
}

/// A steered `$name` expands at the drain — the boundary that builds the
/// message — so the request after the boundary carries the body whole.
#[tokio::test]
async fn a_steered_invocation_expands_at_the_boundary_that_takes_it() {
    let dir = ganja_testkit::temp_dir();
    plant(dir.path(), "porting", "steered body");
    let roots = Roots::none().with_paths([dir.path().to_path_buf()]);

    let mut first = Vec::new();
    first.extend(tool_call("shell", json!({ "key": "a" })));
    let (provider, requests) = ScriptedProvider::new(vec![first, says("done")]);
    // The recorder wears the shell tool's name, which asks by default — what
    // holds the turn open long enough for a steer to land mid-turn, exactly
    // as `tests/steering.rs` holds it.
    let (gated, _calls) = ganja_testkit::RecorderTool::new("shell", "shell ran", "found it");
    let engine =
        Engine::new(provider, MODEL, Arc::new(Registry::new(vec![gated])), Permissions::default())
            .with_skill_roots(roots.clone());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "run something".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // The shell call asks by default, which is what holds the turn open long
    // enough for a steer to land mid-turn.
    let permission = loop {
        match futures::StreamExt::next(&mut events).await {
            Some(Event::PermissionRequested { id, .. }) => break id,
            Some(_) => {}
            None => panic!("the stream ended before the permission ask"),
        }
    };
    engine
        .send(Command::Steer {
            id: "steer-1".to_owned(),
            text: "also $porting".to_owned(),
            mentions: Vec::new(),
            skills: vec!["porting".to_owned()],
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a steer reaches a running turn");
    engine
        .send(Command::ReplyPermission { id: permission, reply: PermissionReply::Once })
        .await
        .expect("the reply is never refused");
    drain(&mut events).await;

    let expected = tool_output(roots, "porting").await;
    let requests = requests.lock().expect("the request log is never poisoned");
    let carried = user_parts(&requests[1]);
    assert!(
        carried.iter().any(|part| part == &expected),
        "the second request carries the steered skill whole: {carried:?}"
    );
}
