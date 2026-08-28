//! Which surfaces there are, how one is named, what each can promise about a
//! delivery, and what two spawns racing for one name get (**D501**,
//! P25's **AC-27**).
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

use std::sync::Arc;
use std::time::Duration;

use ganja_core::Storage;
use ganja_core::permission::Permissions;
use ganja_core::protocol::team::MemberBackend;
use ganja_core::provider::FakeProvider;
use ganja_core::teammate::agy::Agy;
use ganja_core::teammate::claude::ClaudePane;
use ganja_core::teammate::codex::Codex;
use ganja_core::teammate::grok::Grok;
use ganja_core::teammate::pane::GanjaPane;
use ganja_core::teammate::shim::ShimBackend;
use ganja_core::teammate::shim_tui::ShimTui;
use ganja_core::teammate::{
    BACKENDS, DEFAULT_BACKEND, Delivery, InProcess, SpawnSpec, TeammateBackend, backend_name,
    parse_backend, posture_line,
};
use ganja_core::tool::Registry;
use ganja_team::{MemberName, TeamName, TeamsRoot};
use ganja_testkit::{AllowSpawn, caller, spawn, team, teammates_recorded};

/// P25's **AC-27**, and P27's **AC-1**. A value outside the six is refused *by
/// name*, and the refusal carries the list — because the useful half of "no
/// such backend" is which ones there are, and a typo is the only way anybody
/// reaches this.
///
/// The list is walked rather than spelled, so a seventh backend joins this
/// assertion by existing.
#[test]
fn an_unknown_backend_value_is_refused_naming_the_six() {
    let refused = parse_backend("tmux").expect_err("there is no backend called tmux");

    assert_eq!(refused.value, "tmux");
    let sentence = refused.to_string();
    assert!(sentence.contains("tmux"), "{sentence}");
    for backend in BACKENDS {
        assert!(sentence.contains(backend), "the refusal must list {backend}: {sentence}");
    }

    // A near-miss is refused too: the argument is a value, not a prefix.
    assert!(parse_backend("in process").is_err());
    assert!(parse_backend("").is_err());
    assert!(parse_backend("Claude").is_err());
}

/// The argument's vocabulary and the document's are the same six words.
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

/// **AC-3, as Dv-1 amends it.** Neither door infers a backend, and the one
/// they fall back to is a pane of ganja's own: a teammate with a window is
/// what a spawn that said nothing gets.
///
/// The value and its spelling are both pinned, because the default is read
/// two ways — as a `MemberBackend` by the door, and as a word by every
/// sentence that has to name it.
///
/// That the default is *refused* rather than silently downgraded in a session
/// with no tmux is `teammate_no_tmux.rs`'s to assert: it owns `TMUX`, and this
/// binary deliberately starts no pane backend at all.
#[test]
fn the_default_backend_is_a_pane_of_its_own() {
    assert_eq!(DEFAULT_BACKEND, MemberBackend::Ganja);
    assert_eq!(backend_name(DEFAULT_BACKEND), "ganja");
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
    assert_eq!(GanjaPane.backend(), MemberBackend::Ganja);
    assert_eq!(ClaudePane.backend(), MemberBackend::Claude);

    // **AC-2.** All three shims acknowledge, and the reason is the same one
    // the in-process backend acknowledges for: the shim itself reads the
    // inbox and takes the message onto a turn *in this process*, so the
    // acknowledgement is the shim's own read and the lead can retire its
    // queue entry on having watched it. `ClaudePane` is the odd one out
    // precisely because a foreign process reads at its own pace and marks a
    // message read when it reads it, not when a turn takes it on.
    //
    // Asserted over the three **real** backends as of Dv-7, which is what the
    // waves were for: the answer this table pins was settled at W1 against
    // stubs precisely so that filling in the children could not change what
    // the lead's queue strip does, and here is the same answer from the
    // children themselves. All three are one `ShimBackend` over one driver
    // now — agy's slot stopped being the exception when it stopped refusing.
    let shims: [(MemberBackend, Arc<dyn TeammateBackend>); 3] = [
        (MemberBackend::Codex, Arc::new(ShimBackend::new(Arc::new(Codex::new())))),
        (MemberBackend::Agy, Arc::new(ShimBackend::new(Arc::new(Agy::new())))),
        (MemberBackend::Grok, Arc::new(ShimBackend::new(Arc::new(Grok::new())))),
    ];
    for (backend, shim) in shims {
        assert_eq!(shim.backend(), backend);
        assert_eq!(
            shim.delivery(),
            Delivery::Acknowledged,
            "{} reads its own inbox, so its read is the acknowledgement",
            backend_name(backend)
        );
    }
}

/// **D514.** Every backend tells its teammate how — or whether — it answers,
/// before the task, in one frame: the two native surfaces name ganja's
/// `send_message`, a real `claude` its `SendMessage`, a CLI's native TUI in a
/// pane and a headless child alike that their answers are carried to the lead
/// as mail — all of them for codex, the last one per turn for grok and agy, as
/// each driver and each transcript reader really forwards (**D515**). Pinned per backend over the trait method the registry seeds
/// from, so a backend whose words drifted onto another's channel fails here.
#[test]
fn each_backend_tells_its_teammate_how_it_answers_before_the_task() {
    let home = ganja_testkit::temp_dir();
    let spec = SpawnSpec {
        name: MemberName::parse("worker").expect("a member name"),
        team: TeamName::parse("session-abcd1234").expect("a team name"),
        lead: MemberName::lead(),
        root: TeamsRoot::new(home.path().join("teams")),
        backend: MemberBackend::InProcess,
        agent_type: "general".to_owned(),
        model: "recorder-model".to_owned(),
        color: "blue".to_owned(),
        prompt: "have a look at the parser".to_owned(),
        cwd: home.path().to_path_buf(),
        plan_mode_required: false,
        parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
        shell: ganja_core::teammate::pane::PaneShell::default(),
        share: ganja_core::teammate::pane::PaneShare::default(),
    };
    let in_process = InProcess::new(
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        Arc::new(Registry::new(Vec::new())),
        Storage::open(home.path().join("storage")),
        |_| Permissions::default(),
    );
    let channels: Vec<(&str, Arc<dyn TeammateBackend>, &str)> = vec![
        ("in-process", Arc::new(in_process), "`send_message`"),
        ("ganja", Arc::new(GanjaPane), "`send_message`"),
        ("claude", Arc::new(ClaudePane), "`SendMessage(to: \"team-lead\")`"),
        (
            "codex (pane)",
            Arc::new(ShimTui::new(Arc::new(Codex::new()))),
            "every message you print in answer is carried to the lead as mail, in order",
        ),
        (
            "agy (pane)",
            Arc::new(ShimTui::new(Arc::new(Agy::new()))),
            "every message you print in answer is carried to the lead as mail, in order",
        ),
        (
            "grok (pane)",
            Arc::new(ShimTui::new(Arc::new(Grok::new()))),
            "only your final answer for the turn is carried to the lead",
        ),
        (
            "codex (headless)",
            Arc::new(ShimBackend::new(Arc::new(Codex::new()))),
            "every message you print in answer is carried to the lead as mail, in order",
        ),
        (
            "agy (headless)",
            Arc::new(ShimBackend::new(Arc::new(Agy::new()))),
            "only your final answer for the turn is carried to the lead",
        ),
        (
            "grok (headless)",
            Arc::new(ShimBackend::new(Arc::new(Grok::new()))),
            "only your final answer for the turn is carried to the lead",
        ),
    ];

    for (which, backend, channel) in channels {
        let text = backend.preamble(&spec);
        assert!(
            text.starts_with(
                "You are worker, a teammate on the team session-abcd1234. Your lead is team-lead."
            ),
            "{which}: every preamble opens on who and whose: {text}"
        );
        assert!(
            text.contains(channel),
            "{which}: the answering channel is this backend's own: {text}"
        );
        assert!(
            text.ends_with("Your task:\n\nhave a look at the parser"),
            "{which}: the task is what the message ends with: {text}"
        );
    }
}

/// **AC-1's other half.** A shim backend parses, gates and *then* answers —
/// and as of W5 every one of the three answers with something it measured
/// rather than with a promissory note.
///
/// The P25a shape this test was written for is finished: `pane` and `claude`
/// were values that parsed, gated and refused for one wave before their bodies
/// landed, and W3-W5 did the same for the three CLIs. What the assertion is
/// about now is that the refusals a person actually meets are the **real
/// backends'** — the name parses, the gate approves, the backend is reached,
/// and *it* is what refuses, for a reason that is either this machine's or that
/// vendor's rather than this build's own not-yet.
#[tokio::test]
async fn every_shim_backend_refuses_with_something_it_measured() {
    let home = ganja_testkit::temp_dir();
    let (root, team, registry, door) = team(home.path());
    let caller = caller(home.path());

    // All three search a `PATH`, and all three are refused by naming the
    // binary, because this fixture's lead is production on a machine with none
    // of them installed. agy joined the loop with Dv-7: its own refusal was a
    // measured sentence about a sandbox until that amendment shipped the
    // backend, and now the only thing standing between it and a child is the
    // same missing binary as the other two.
    for cli in ["codex", "agy", "grok"] {
        let refused = door
            .start(spawn("w1", Some(cli)), &caller, &AllowSpawn)
            .await
            .expect_err("this fixture's lead has no such binary on its PATH");

        assert!(refused.reason.contains(cli), "the refusal names the binary: {}", refused.reason);
        assert!(
            refused.reason.contains(ganja_core::teammate::shim::REFUSED_NO_BINARY),
            "and says what about it: {}",
            refused.reason
        );
        // The sentence W1 shipped and W5 retired. Nothing may say it again:
        // there is no unbuilt backend left, so a build claiming one would be
        // claiming a state it cannot be in.
        assert!(
            !refused.reason.contains("cannot run a teammate on another vendor's CLI yet"),
            "no backend is unbuilt any more: {}",
            refused.reason
        );
    }

    // A refused spawn leaves nothing behind: no member on disk and nothing the
    // registry would have to shut down.
    assert!(teammates_recorded(&root, &team).is_empty(), "a refused spawn records no member");
    assert_eq!(registry.running(), 0);
}

/// **D508(c)'s table**: what each backend discloses at spawn, and the three
/// that disclose nothing.
///
/// Byte-exact rather than substring-matched, because these sentences are the
/// description a person's consent is obtained under — the spawn dialog and the
/// registry's ring line both read this one table so that a dialog cannot come
/// to say something a ring line does not. A wording change is a change to what
/// was consented to, so it belongs in a diff somebody reads.
///
/// [`MemberBackend::Agy`]'s sentence is the one that describes an **absence**.
/// W4 measured `--sandbox` as a bound on agy's terminal and not on its
/// filesystem, and Dv-7's user directive shipped the backend anyway at that
/// honest posture rather than building a write tier for it — so the row says
/// there is no filesystem bound, in the words that amendment settled on.
#[test]
fn each_backend_discloses_the_posture_it_pins_or_says_it_pins_none() {
    // The P25 surfaces answer nothing, and the absence is the honest answer:
    // they forward their dialogs to the lead, so a person stays in the loop
    // for every call rather than consenting to a posture once at spawn.
    for backend in [MemberBackend::InProcess, MemberBackend::Ganja, MemberBackend::Claude] {
        assert_eq!(posture_line(backend), None, "{}", backend_name(backend));
    }

    // codex's sentence is **measured** as of W3, and this is the regression
    // pin rather than the measurement: the assertion that it equals what its
    // probe actually recorded compares against the recording itself, in
    // `teammate_shim_codex.rs`. Both matter — one keeps the sentence honest,
    // the other keeps it from drifting.
    assert_eq!(
        posture_line(MemberBackend::Codex),
        Some(
            "sandbox=read-only: writes denied, whole-disk read, network denied — may read any \
             file you can, including credentials, but has no network to send them over"
        )
    );
    // agy's is **measured** as of W4 and *shipped* as of Dv-7, and it is the
    // only row in this table that describes the absence of a bound rather than
    // a bound. The comparison against its own recording is in
    // `teammate_shim_agy.rs`; this is the regression pin, and it is the one
    // most worth having: a wording change here is a change to what somebody
    // consented to when they approved a teammate that can write anywhere they
    // can.
    assert_eq!(
        posture_line(MemberBackend::Agy),
        Some(
            "sandbox: terminal bounded, no enforced filesystem bound — may read any file you \
             can, including credentials, and write anywhere you can; those writes are outside \
             the snapshot chain /undo walks"
        )
    );
    // grok's is **measured** as of W5, last clause included: its gating probe
    // completed a pure-read turn and cancelled a write and a shell turn on the
    // same conversation. The comparison against the recording itself is in
    // `teammate_shim_grok.rs`; this is the regression pin.
    assert_eq!(
        posture_line(MemberBackend::Grok),
        Some(
            "sandbox=read-only: writes denied outside ~/.grok and temp, whole-disk read, no \
             network bound (macOS) — may read any file you can, including credentials, and may \
             send them anywhere; reading takes no approval, and a tool request that needs one \
             ends the turn"
        )
    );

    // Every backend that discloses a posture is one that asks nobody
    // afterwards, and every backend that does not is one that keeps asking.
    // The spawn gate reads this table for exactly that reason, so a backend
    // answering the wrong way here would move a consent dialog rather than a
    // sentence.
    for name in BACKENDS {
        let backend = parse_backend(name).expect("a listed value parses");
        let discloses =
            matches!(backend, MemberBackend::Codex | MemberBackend::Agy | MemberBackend::Grok);

        assert_eq!(posture_line(backend).is_some(), discloses, "{name}");
    }
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
            door.start(spawn("worker", Some("in-process")), &caller, &AllowSpawn).await
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
    assert_ne!(names[0], names[1], "two teammates cannot answer to one name: {names:?}");
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
        assert!(root.inbox_path(&team, &member).exists(), "{name} was given a mailbox of its own");
    }
    assert_eq!(
        registry.running(),
        2,
        "a teammate the map forgot is a teammate nothing can shut down"
    );

    registry.shutdown().await;
    assert_eq!(registry.running(), 0, "and both of them really went when the team did");
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
        .start(spawn("worker", Some("in-process")), &caller(home.path()), &AllowSpawn)
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
