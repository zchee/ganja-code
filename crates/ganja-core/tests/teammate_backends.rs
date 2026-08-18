//! Which surfaces there are, how one is named, what each can promise about a
//! delivery, and what two spawns racing for one name get (**D501**,
//! **AC-27**).
//!
//! Every root is handed in, and nothing here **writes** process-wide state, so
//! this binary holds several tests. It does read some — `tempfile` resolves
//! `TMPDIR` for the directories below — which is a read every test in the
//! workspace makes and no test here can disturb.
//!
//! What it deliberately does *not* hold, and why:
//!
//! - the door-equivalence claim — that a `task` call and `/team spawn` build
//!   the same request is asserted where each door lives, because a core test
//!   binary cannot see the TUI one;
//! - **anything that starts a pane backend.** Since P25b the two pane values
//!   really spawn when `$TMUX` is set, so a spawn of either from a binary that
//!   cannot control that variable would split a pane of *this test harness*
//!   into whatever tmux the developer is running the suite in. The per-value
//!   refusal outside tmux is `teammate_no_tmux.rs` (AC-16), which owns
//!   `TMUX`; the real spawn on a private server is
//!   `teammate_pane_lifecycle.rs` (AC-11's engine-side leg). Both pane values
//!   are still constructed here, for what they say
//!   about themselves without being asked to spawn.
//!
//! Everything that starts a teammate here goes through
//! [`ganja_core::Teammates::start`], which is the **only** door onto the
//! registry's spawn: the entry beneath it is crate-internal precisely so that
//! nothing can reach a spawn the permission gate never saw, and a test calling
//! past the gate would be a test of a path production has not got.

use std::{sync::Arc, time::Duration};

use ganja_core::{
    Storage,
    permission::Permissions,
    protocol::team::MemberBackend,
    provider::FakeProvider,
    teammate::{
        BACKENDS, DEFAULT_BACKEND, Delivery, InProcess, TeammateBackend, backend_name,
        claude::ClaudePane, pane::GanjaPane, parse_backend,
    },
    tool::Registry,
};
use ganja_team::MemberName;
use ganja_testkit::{AllowSpawn, caller, spawn, team, teammates_recorded};

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
    // Bound rather than used inline: a temporary directory dropped at the end
    // of the statement takes the tree out from under the store handle that is
    // about to be asked questions.
    let home = ganja_testkit::temp_dir();
    let in_process = InProcess::new(
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        Arc::new(Registry::new(Vec::new())),
        Storage::open(home.path().join("storage")),
        |_| Permissions::default(),
    );

    assert_eq!(in_process.delivery(), Delivery::Acknowledged);
    assert_eq!(GanjaPane.delivery(), Delivery::Acknowledged);
    assert_eq!(ClaudePane.delivery(), Delivery::FireAndForget);

    assert_eq!(in_process.backend(), MemberBackend::InProcess);
    assert_eq!(GanjaPane.backend(), MemberBackend::Pane);
    assert_eq!(ClaudePane.backend(), MemberBackend::Claude);
}

/// **Two spawns of one name at once are two teammates, not one and a ghost.**
///
/// The window is real and it is wide: a spawn crosses four awaits between
/// reading which names are taken and registering itself, and `task` bodies run
/// up to `agents.concurrency` at a time. Without a synchronous reservation both
/// spawns resolve to `worker`, share one inbox, and the second registration
/// evicts the first from the member map — leaving a teammate with its own
/// engine and its own turn that nothing holds, and therefore that
/// [`TeammateRegistry::shutdown`] never ends.
///
/// The two run in a [`tokio::task::JoinSet`], and on this runtime that makes
/// the race **deterministic** rather than likely: the taken-names read hops to
/// the blocking pool, so the first spawn is guaranteed to yield there and the
/// second is guaranteed to read the same snapshot it did.
///
/// What this shape reaches is the *overlapping* window — both spawns still in
/// flight. The one it cannot stage is a spawn that begins and **finishes**
/// inside another claimer's snapshot, which would need that claimer's task to
/// be woken and then starved for the length of a whole spawn. That case is
/// closed by the reservation set never shrinking on success, pinned beside the
/// code in `teammate.rs`'s `a_name_a_completed_spawn_took_stays_claimed`.
#[tokio::test]
async fn two_spawns_of_one_name_at_once_get_two_teammates() {
    let home = ganja_testkit::temp_dir();
    let (root, team, registry, door) = team(home.path());
    let caller = caller(home.path());

    let mut spawning = tokio::task::JoinSet::new();
    for _ in 0..2 {
        let door = Arc::clone(&door);
        let caller = caller.clone();
        spawning.spawn(async move {
            door.start(spawn("worker", None), &caller, &AllowSpawn)
                .await
        });
    }

    let mut names = Vec::new();
    while let Some(joined) = spawning.join_next().await {
        let started = joined
            .expect("neither spawn panicked")
            .expect("both spawns start: a taken name is resolved, never refused");
        names.push(started.name);
    }
    names.sort();

    assert_eq!(names.len(), 2);
    assert_ne!(
        names[0], names[1],
        "two teammates cannot answer to one name: {names:?}"
    );
    assert!(
        names.contains(&"worker".to_owned()),
        "one of them is still the name that was asked for: {names:?}"
    );

    // Both are real: two records on disk, two inboxes, and — the assertion the
    // orphan fails — two teammates the registry still holds.
    assert_eq!(
        teammates_recorded(&root, &team),
        names,
        "each teammate wrote its own member record"
    );
    for name in &names {
        let member = MemberName::parse(name).expect("a resolved name is a member name");
        assert!(
            root.inbox_path(&team, &member).exists(),
            "{name} was given a mailbox of its own"
        );
    }
    assert_eq!(
        registry.running(),
        2,
        "a teammate the map forgot is a teammate nothing can shut down"
    );

    registry.shutdown().await;
    assert_eq!(
        registry.running(),
        0,
        "and both of them really went when the team did"
    );
}

/// The plan-approval wait is a seam something can actually reach.
///
/// It answers a real defect rather than a hypothetical: the runner's loop used
/// to **consume** the value it ran on, so nothing outlived it holding one — and
/// every approval the lead ever sent would have been ignored as answering
/// nothing, while the method saying otherwise sat there uncallable. What is
/// still absent is the asking side, which is why this pins reachability rather
/// than a round trip.
#[tokio::test]
async fn a_running_teammate_can_be_told_which_plan_approval_it_is_waiting_on() {
    let home = ganja_testkit::temp_dir();
    let (_root, _team, registry, door) = team(home.path());

    let started = door
        .start(spawn("worker", None), &caller(home.path()), &AllowSpawn)
        .await
        .expect("an in-process teammate starts");

    assert!(
        registry.awaiting_plan_approval(&started.name, "plan-request-1"),
        "a running in-process teammate has a loop to tell"
    );
    assert!(
        !registry.awaiting_plan_approval("nobody", "plan-request-1"),
        "a name this team never had is nobody to tell"
    );

    registry.shutdown().await;
}
