//! Which surfaces there are, how one is named, and what a build that cannot
//! have one says (**D501**, **AC-27**, AC-14's P25a leg).
//!
//! Every root is handed in and nothing here reads or writes process-wide state,
//! so this binary holds several tests. What it deliberately does *not* hold is
//! the door-equivalence claim: that a `task` call and `/team spawn` build the
//! same request is asserted where each door lives, because a core test binary
//! cannot see the TUI one.

use std::{sync::Arc, time::Duration};

use ganja_core::{
    Storage,
    permission::Permissions,
    protocol::team::MemberBackend,
    provider::FakeProvider,
    teammate::{
        BACKENDS, DEFAULT_BACKEND, Delivery, InProcess, REFUSED_UNTIL_P25B, SpawnError,
        SpawnRequest, TeammateBackend, TeammateRegistry, backend_name, claude::ClaudePane,
        pane::GanjaPane, parse_backend,
    },
    tool::Registry,
};
use ganja_team::{MemberName, TeamName, TeamsRoot, mailbox};

/// A spawn of `name` on `backend`, with everything else the same, so two spawns
/// differ only where the test is looking.
fn request(name: &str, backend: MemberBackend) -> SpawnRequest {
    SpawnRequest {
        name: name.to_owned(),
        backend,
        agent_type: "general".to_owned(),
        model: "recorder-model".to_owned(),
        color: None,
        prompt: "have a look at the parser".to_owned(),
        cwd: std::env::temp_dir(),
        plan_mode_required: false,
        bypass: false,
    }
}

/// **AC-27.** A value outside the three is refused *by name*, and the refusal
/// carries the list — because the useful half of "no such backend" is which
/// ones there are, and a typo is the only way anybody reaches this.
#[test]
fn an_unknown_backend_value_is_refused_naming_the_three() {
    let refused = parse_backend("tmux").expect_err("there is no backend called tmux");

    assert_eq!(refused.value, "tmux");
    let sentence = refused.to_string();
    assert!(sentence.contains("tmux"), "{sentence}");
    for backend in BACKENDS {
        assert!(
            sentence.contains(backend),
            "the refusal must list {backend}: {sentence}"
        );
    }

    // A near-miss is refused too: the argument is a value, not a prefix.
    assert!(parse_backend("in process").is_err());
    assert!(parse_backend("").is_err());
    assert!(parse_backend("Claude").is_err());
}

/// The argument's vocabulary and the document's are the same three words.
///
/// Written out in [`BACKENDS`] and matched in [`backend_name`], then checked
/// here against what the type actually serializes as: two hand-written lists
/// that agree today are two lists that could stop agreeing, and this is what
/// says so.
#[test]
fn every_backend_value_is_spelled_the_way_it_is_serialized() {
    for name in BACKENDS {
        let backend = parse_backend(name).expect("a listed value parses");
        assert_eq!(backend_name(backend), name);
        assert_eq!(
            serde_json::to_value(backend).expect("a backend serializes"),
            serde_json::json!(name),
            "the argument and the member record must spell {name} the same"
        );
    }
}

/// Neither door infers a backend, and the one they fall back to is the one with
/// no window: an in-process teammate is what a spawn that said nothing gets.
#[test]
fn the_default_backend_is_the_one_in_this_process() {
    assert_eq!(DEFAULT_BACKEND, MemberBackend::InProcess);
    assert_eq!(backend_name(DEFAULT_BACKEND), "in-process");
}

/// What each backend can promise about a delivery (**D501**, spent by
/// **D503**).
///
/// The split is the whole reason `delivery()` is on the trait: a real `claude`
/// pane marks a message read when it reads it, not when a turn takes it on, so
/// a lead waiting for an acknowledgement from one would wait forever.
#[tokio::test]
async fn each_backend_says_what_it_can_promise_about_a_delivery() {
    let in_process = InProcess::new(
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        Arc::new(Registry::new(Vec::new())),
        Storage::open(ganja_testkit::temp_dir().path().join("storage")),
        |_| Permissions::default(),
    );

    assert_eq!(in_process.delivery(), Delivery::Acknowledged);
    assert_eq!(GanjaPane.delivery(), Delivery::Acknowledged);
    assert_eq!(ClaudePane.delivery(), Delivery::FireAndForget);

    assert_eq!(in_process.backend(), MemberBackend::InProcess);
    assert_eq!(GanjaPane.backend(), MemberBackend::Pane);
    assert_eq!(ClaudePane.backend(), MemberBackend::Claude);
}

/// **AC-14's P25a leg.** Both pane values refuse, they refuse with the same
/// sentence, and the sentence names the phase that will change the answer.
///
/// Identical on purpose: one door spawning where the other refuses would be two
/// behaviours wearing one argument, and a refusal that named neither the phase
/// nor the alternative would leave a reader guessing whether their session or
/// their build was the problem.
#[tokio::test]
async fn the_two_pane_backends_refuse_identically_until_p25b() {
    let spec_holder = ganja_testkit::temp_dir();
    let root = TeamsRoot::new(spec_holder.path().join("teams"));
    let team = TeamName::parse("session-abcd1234").expect("a team name");
    let registry = Arc::new(TeammateRegistry::new(
        root.clone(),
        team.clone(),
        "01998ad0-0000-7000-8000-000000000000",
        spec_holder.path(),
    ));

    let ganja = registry
        .spawn(Arc::new(GanjaPane), request("worker", MemberBackend::Pane))
        .await
        .expect_err("this build has no panes yet");
    let claude = registry
        .spawn(
            Arc::new(ClaudePane),
            request("worker", MemberBackend::Claude),
        )
        .await
        .expect_err("nor real claude ones");

    let (SpawnError::Unsupported(ganja), SpawnError::Unsupported(claude)) = (&ganja, &claude)
    else {
        panic!("a pane spawn failed for some other reason: {ganja:?} / {claude:?}");
    };
    assert_eq!(ganja.backend, MemberBackend::Pane);
    assert_eq!(claude.backend, MemberBackend::Claude);
    assert_eq!(
        ganja.reason, claude.reason,
        "one door must not refuse differently from the other"
    );
    assert_eq!(ganja.reason, REFUSED_UNTIL_P25B);
    assert!(ganja.reason.contains("P25b"), "{}", ganja.reason);
    assert!(
        ganja.to_string().contains("pane") && claude.to_string().contains("claude"),
        "a refusal still says which surface was asked for"
    );

    // And a refused spawn leaves nothing behind: no member, and — the half that
    // would otherwise still be there tomorrow — no task sitting in a mailbox
    // nothing will ever read.
    assert!(
        !root.config_path(&team).exists(),
        "a team file was written for a teammate that never started"
    );
    let inbox = root.inbox_path(&team, &MemberName::parse("worker").expect("a member name"));
    assert!(
        mailbox::read(&inbox)
            .expect("the inbox reads")
            .valid
            .is_empty(),
        "a refused spawn left its prompt in an inbox"
    );
}
