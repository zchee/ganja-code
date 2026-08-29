//! Who is offered the model-facing live-session listing, and who is not
//! (**D535**, **AC-33**).
//!
//! The gate is the installed postbox's cross-session **capability**, never
//! its presence — which is deliberately a different gate from
//! `send_message`'s, and this file exists so the two cannot silently
//! converge. A pane member's `MemberPostbox` refuses a `uds:` address
//! outright and resolves a name only against its own team file, so a
//! directory of this user's sessions, their working directories and their
//! `uds:` addresses is a list of addresses that member structurally cannot
//! use.
//!
//! Read off the **wire**, as `teammate_engine.rs` reads every other tool
//! registration: what an engine offers is what its request carries, not what
//! a private field says.
//!
//! Every root is handed in and nothing here mutates the environment, so this
//! binary may hold more than one test.

#![cfg(unix)]

use std::sync::Arc;

use ganja_core::Engine;
use ganja_core::permission::Permissions;
use ganja_core::protocol::Command;
use ganja_core::provider::ChatRequest;
use ganja_core::teammate::member::MemberPostbox;
use ganja_core::tool::{Registry, list_sessions, send_message};
use ganja_team::team::MemberName;
use ganja_team::{TeamName, TeamsRoot};
use ganja_testkit::{ScriptedProvider, TEAM, says, team};

/// Whether `request` carried `tool` in its offered set.
fn offers(request: &ChatRequest, tool: &str) -> bool {
    request.tools.iter().any(|definition| definition.name == tool)
}

/// Takes one turn on `engine` and answers what it offered the model.
async fn offered(
    engine: &Engine,
    requests: &Arc<std::sync::Mutex<Vec<ChatRequest>>>,
) -> ChatRequest {
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "what is running".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
            session_mentions: Vec::new(),
        })
        .await
        .expect("a prompt starts a turn");
    ganja_testkit::drain(&mut events).await;

    requests
        .lock()
        .expect("the request log is never poisoned")
        .last()
        .expect("the turn asked once")
        .clone()
}

/// **AC-33**, every assembly this build has in one test, so a new condition
/// cannot be added without reddening it — and the companion pin beside it: a
/// member pane keeps `send_message`, so the two gates are visibly different
/// rather than accidentally divergent.
///
/// Three, not the four this test held until **D543** (2026-08-30): the
/// fourth was "an interactive non-member session on the solo arm", and the
/// solo arm turned out to be a postbox no shipped binary installed. Such a
/// session leads a team of nobody and holds the lead postbox of case (i),
/// which is why the two rows collapsed into one rather than one of them
/// being dropped as untested.
#[tokio::test]
async fn the_listing_reaches_the_postboxes_that_can_cross_a_session_and_no_others() {
    // (i) A lead — of nobody here, which since D543 is the same assembly
    // a session that has spawned no teammate really runs.
    {
        let (provider, requests) = ScriptedProvider::new(vec![says("ok")]);
        let home = ganja_testkit::temp_dir();
        let (_root, _team, registry, _door) = team(home.path());
        let engine = Engine::new(
            provider,
            "recorder-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        )
        .with_teammates(Arc::clone(&registry), ganja_testkit::externals());

        let request = offered(&engine, &requests).await;
        assert!(
            offers(&request, list_sessions::ID),
            "a lead's postbox crosses sessions, so it is offered the listing"
        );
        assert!(
            offers(&request, send_message::ID),
            "and it keeps the tool the listing is paired with"
        );
    }

    // (ii) A member pane.
    {
        let (provider, requests) = ScriptedProvider::new(vec![says("ok")]);
        let home = ganja_testkit::temp_dir();
        let root = TeamsRoot::new(home.path().join("teams"));
        let team_name = TeamName::parse(TEAM).expect("a team name");
        let engine = Engine::new(
            provider,
            "recorder-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        )
        .with_postbox(Arc::new(MemberPostbox::new(
            MemberName::parse("worker").expect("a member name"),
            team_name,
            root,
        )));

        let request = offered(&engine, &requests).await;
        assert!(
            !offers(&request, list_sessions::ID),
            "a member cannot act on the listing, so it does not get one"
        );
        assert!(
            offers(&request, send_message::ID),
            "but it keeps `send_message` from its first turn — the two gates \
             are different on purpose"
        );
    }

    // (iii) A headless run: no postbox at all.
    {
        let (provider, requests) = ScriptedProvider::new(vec![says("ok")]);
        let engine = Engine::new(
            provider,
            "recorder-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        );

        let request = offered(&engine, &requests).await;
        assert!(
            !offers(&request, list_sessions::ID),
            "a session with no postbox has nobody to address and nothing to list"
        );
        assert!(!offers(&request, send_message::ID), "and it has no `send_message` either");
    }
}

/// The predicate is asserted **by name** rather than only by its effects: a
/// postbox installed over a cross-session-capable one takes the listing away
/// again, so the flag really is kept in lockstep with what is installed.
#[tokio::test]
async fn installing_a_member_postbox_over_a_lead_takes_the_listing_away() {
    let (provider, requests) = ScriptedProvider::new(vec![says("ok"), says("ok")]);
    let home = ganja_testkit::temp_dir();
    let (_root, _team, registry, _door) = team(home.path());
    let root = TeamsRoot::new(home.path().join("teams"));
    let team_name = TeamName::parse(TEAM).expect("a team name");
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_teammates(Arc::clone(&registry), ganja_testkit::externals());

    assert!(
        offers(&offered(&engine, &requests).await, list_sessions::ID),
        "the lead is offered it first"
    );

    let engine = engine.with_postbox(Arc::new(MemberPostbox::new(
        MemberName::parse("worker").expect("a member name"),
        team_name,
        root,
    )));

    assert!(
        !offers(&offered(&engine, &requests).await, list_sessions::ID),
        "and a postbox that cannot cross a session takes it away again"
    );
}
