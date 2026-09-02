use std::path::PathBuf;

use ganja_protocol::team::MemberBackend;

use super::{
    ANY, Arc, AtomicUsize, CancellationToken, DialogSurface, Event, FOREIGN, Forwarded, Forwarding,
    Ordering, Posture, SpawnGate, Teammate, backend_name, mpsc, oneshot, permissions_for,
    spawn_gate,
};
use crate::Storage;
use crate::permission::{Action, Decision, EXTERNAL_DIRECTORY, Permissions, Rule};
use crate::protocol::{Command, PermissionId, PermissionReply, SessionId};
use crate::provider::Provider;
use crate::tool::Registry;

/// A dialog surface over `lead`, counting into a tally nobody else holds.
///
/// What a registry hands a teammate is the same value counting into *its* own
/// tally ([`crate::teammate::TeammateRegistry::dialogs_waiting`]); a test that
/// only needs somewhere for a question to go supplies its own.
fn surface(lead: mpsc::Sender<Forwarded>) -> DialogSurface {
    DialogSurface::new(lead, Arc::default())
}

fn rule(permission: &str, pattern: &str, action: Action) -> Rule {
    Rule { permission: permission.to_owned(), pattern: pattern.to_owned(), action }
}

/// A lead whose rules are `rules` and whose project is nowhere in
/// particular — every test here judges rules rather than paths, except the
/// two that pass a root explicitly.
fn lead(rules: Vec<Rule>) -> Permissions {
    let mut permissions = Permissions::default();
    permissions.set_baseline(rules);

    permissions
}

/// A teammate over its own temporary store, holding `tools`.
fn teammate(
    provider: Arc<dyn Provider>,
    tools: Arc<Registry>,
    permissions: Permissions,
) -> (tempfile::TempDir, Arc<Teammate>) {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(directory.path().join("storage"));

    (
        directory,
        Arc::new(Teammate::new("worker", provider, "recorder-model", tools, permissions, storage)),
    )
}

/// A turn that calls `write` once and then says it is done.
fn writes() -> Vec<Vec<crate::provider::ProviderEvent>> {
    vec![
        ganja_testkit::tool_call(
            "write",
            serde_json::json!({ "filePath": "notes.md", "content": "x" }),
        ),
        ganja_testkit::says("done"),
    ]
}

/// **The anti-laundering rule.** A teammate's own agent may say whatever it
/// likes; what the lead is refused, the teammate is refused. The agent rule
/// here is the strongest one an agent could write — allow everything — and
/// it changes nothing, because the lead's refusals are appended after it
/// and last-match-wins reads a baseline backwards.
#[test]
fn what_the_leads_rules_deny_a_teammates_agent_cannot_allow() {
    let lead = lead(vec![rule("bash", ANY, Action::Deny)]);
    let teammate = permissions_for(&lead, vec![rule("bash", ANY, Action::Allow)]);

    assert_eq!(
        teammate.gate("bash", &serde_json::json!({ "command": "cargo test" })).action,
        Decision::Deny,
        "an agent's allow must not outrank the lead's deny"
    );
}

/// The other direction, so the test above is not passing on a ruleset that
/// refuses everything: an agent still decides what the lead never refused.
#[test]
fn a_teammates_agent_still_decides_what_the_lead_never_refused() {
    let lead = lead(Vec::new());
    let teammate = permissions_for(&lead, vec![rule("webfetch", ANY, Action::Allow)]);
    let call = serde_json::json!({ "url": "https://example.invalid" });

    assert_eq!(
        lead.gate("webfetch", &call).action,
        Decision::Ask,
        "the lead itself would have asked"
    );
    assert_eq!(
        teammate.gate("webfetch", &call).action,
        Decision::Allow,
        "the agent's own rule is the one nothing overrides"
    );
}

/// The posture nothing chose is the lead's dialog: a teammate nobody
/// approved anything for is a teammate whose asks are the lead's to
/// answer — and since **D513** there is no spawn that could choose
/// otherwise.
#[test]
fn the_default_posture_is_the_leads_dialog() {
    assert_eq!(Posture::default(), Posture::ForwardToLead);
}

/// §10.11-11: a teammate's directory passes the same gate any other work
/// outside the project does — judged against the **lead's** project root,
/// because judging it against its own would be the laundering move itself.
#[test]
fn a_teammate_working_outside_the_project_is_asked_about_before_it_starts() {
    let project = tempfile::tempdir().expect("a temporary directory");
    let elsewhere = tempfile::tempdir().expect("a temporary directory");

    let inside = spawn_gate(
        &lead(Vec::new()),
        project.path(),
        &project.path().join("crates"),
        MemberBackend::InProcess,
    );
    assert_eq!(inside.directory, None, "the project reaches it, so there is nothing to ask about");
    assert!(inside.directories().is_empty());

    let outside =
        spawn_gate(&lead(Vec::new()), project.path(), elsewhere.path(), MemberBackend::InProcess);
    let (named, decision) = outside.directory.clone().expect("somewhere else was named");
    assert_eq!(decision, Decision::Ask);
    assert_eq!(named, crate::permission::resolve(elsewhere.path()));
    assert_eq!(outside.directories(), vec![named.clone()], "a dialog has to say where");

    let answered = spawn_gate(
        &lead(vec![rule(EXTERNAL_DIRECTORY, &named.join(ANY).to_string_lossy(), Action::Allow)]),
        project.path(),
        elsewhere.path(),
        MemberBackend::InProcess,
    );
    assert_eq!(
        answered.action(),
        Decision::Allow,
        "an answer already given for that directory answers this too"
    );

    let refused = spawn_gate(
        &lead(vec![rule(EXTERNAL_DIRECTORY, ANY, Action::Deny)]),
        project.path(),
        elsewhere.path(),
        MemberBackend::InProcess,
    );
    assert_eq!(refused.action(), Decision::Deny);
    assert!(
        refused.refusal().is_some_and(|why| why.contains("spawn it inside the project")),
        "a refused spawn says why: {:?}",
        refused.refusal()
    );
}

/// **Dv-7's reversal**, asserted rather than left to the loop above: agy
/// raises the foreign gate, and every agy spawn asks.
///
/// W4 shipped the opposite of this test. Its measurement — `--sandbox`
/// bounds agy's terminal and not its filesystem — refused every agy spawn,
/// so `posture_line` answered [`None`] for it and this clause never fired;
/// asking a person to consent to a spawn that will certainly refuse is a
/// consent question about nothing. Dv-7's user directive ships that
/// backend anyway, at the honest posture, which puts the consent question
/// back where it belongs: a foreign agent with **no enforced filesystem
/// bound** is precisely the spawn nobody should get without being asked.
///
/// Kept as a test of its own after the reversal, and not folded into the
/// loop, because the reason it exists is historical rather than
/// structural: whichever way the ship test had gone, this file had to say
/// so explicitly, so that a build which quietly stopped asking about agy
/// would fail here instead of shipping.
///
/// **The clause that was never agy's to take away** stays true and is why
/// the W4-era sentence "the refusal comes first" was only ever about the
/// foreign clause: `external_directory` is decided without reference to
/// the backend, so it raises its dialog for every surface, agy included,
/// and did so even while its spawn refused.
#[test]
fn agy_raises_the_foreign_gate_because_it_now_spawns() {
    let directory = tempfile::tempdir().expect("a temporary directory");

    let gate =
        spawn_gate(&lead(Vec::new()), directory.path(), directory.path(), MemberBackend::Agy);

    assert_eq!(gate.foreign, Some((MemberBackend::Agy, Decision::Ask)));
    assert_eq!(gate.action(), Decision::Ask);
    assert_eq!(gate.refusal(), None, "asking is not refusing");
    assert!(
        crate::teammate::posture_line(MemberBackend::Agy).is_some(),
        "and the gate fires because there is a posture to disclose"
    );
}

/// **D508(c)**, and P27's **AC-16**: a spawn onto a foreign CLI always
/// asks, a stored deny refuses it, and a stored allow changes nothing.
///
/// The third arm is the one worth reading twice, because it looks like a
/// bug and is the mechanism. [`Permissions::inherited_by_subagent`]'s
/// filter (permission.rs:803) keeps a deny and an `external_directory`
/// rule and drops everything else, so an `allow` written for [`FOREIGN`]
/// never reaches [`decide`] at all and the clause answers its `Ask`
/// default. That is what makes "every shim spawn raises a dialog, and
/// there is no rule anybody can write to stop it" a property of the code
/// rather than a promise in a doc.
///
/// Nothing writes such a rule either — the spawn dialog discards
/// `PermissionReply::Always` — so the arm is not reachable through the UI.
/// It is asserted anyway, because a config file is a thing a person can
/// edit by hand.
#[test]
fn a_spawn_onto_a_foreign_cli_always_asks_and_only_a_deny_can_change_that() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let shims = [MemberBackend::Codex, MemberBackend::Agy, MemberBackend::Grok];

    for backend in shims {
        let name = backend_name(backend);

        // No stored rule at all: the spawn is asked about, and being asked
        // is not being refused.
        let asked = spawn_gate(&lead(Vec::new()), directory.path(), directory.path(), backend);
        assert_eq!(asked.foreign, Some((backend, Decision::Ask)), "{name}");
        assert_eq!(asked.action(), Decision::Ask, "{name}");
        assert_eq!(asked.refusal(), None, "{name}: asking is not refusing");

        // A stored allow is inert, by the filter above.
        let allowed = spawn_gate(
            &lead(vec![rule(FOREIGN, ANY, Action::Allow)]),
            directory.path(),
            directory.path(),
            backend,
        );
        assert_eq!(
            allowed.action(),
            Decision::Ask,
            "{name}: an allow cannot pre-clear a vendor's gate"
        );

        // A stored deny passes the filter and refuses, in a sentence that
        // names which surface — a refusal a person cannot act on is a
        // refusal that does not say what to change.
        let denied = spawn_gate(
            &lead(vec![rule(FOREIGN, name, Action::Deny)]),
            directory.path(),
            directory.path(),
            backend,
        );
        assert_eq!(denied.action(), Decision::Deny, "{name}");
        assert!(
            denied.refusal().is_some_and(|why| why.contains(name)),
            "{name}: a refused spawn says which backend: {:?}",
            denied.refusal()
        );

        // And the rule is read against the **backend**, not the agent
        // type: a deny stored for one CLI leaves the others asking.
        let other = spawn_gate(
            &lead(vec![rule(FOREIGN, "codex", Action::Deny)]),
            directory.path(),
            directory.path(),
            backend,
        );
        let expected = if backend == MemberBackend::Codex { Decision::Deny } else { Decision::Ask };
        assert_eq!(other.action(), expected, "{name}");
    }

    // P25's three surfaces are untouched by any of it: an in-project
    // spawn still raises nothing at all, which is `posture.rs`'s own
    // `Allow` default and the thing this clause must not have moved.
    for backend in [MemberBackend::InProcess, MemberBackend::Ganja, MemberBackend::Claude] {
        let gate = spawn_gate(
            // Even with a deny stored for every backend name there is:
            // the clause is not read for these surfaces at all.
            &lead(vec![rule(FOREIGN, ANY, Action::Deny)]),
            directory.path(),
            directory.path(),
            backend,
        );

        assert_eq!(gate.foreign, None, "{}", backend_name(backend));
        assert_eq!(gate.action(), Decision::Allow);
        assert_eq!(gate.refusal(), None);
    }
}

/// Nothing asked, nothing to answer.
#[test]
fn a_spawn_with_nothing_to_gate_gates_nothing() {
    assert_eq!(SpawnGate::default().action(), Decision::Allow);
    assert_eq!(SpawnGate::default().refusal(), None);
}

/// **ForwardToLead, end to end.** The teammate's turn asks, the question
/// reaches the lead's side naming the teammate, the answer travels back by
/// the request's own id, and the call runs.
#[tokio::test]
async fn a_teammates_question_reaches_the_lead_and_its_answer_comes_back() {
    let (tool, calls) = ganja_testkit::RecorderTool::new("write", "wrote", "written");
    let (provider, _) = ganja_testkit::ScriptedProvider::named("fake", writes());
    let (_directory, teammate) = teammate(
        provider,
        Arc::new(Registry::new(vec![tool])),
        permissions_for(&lead(Vec::new()), Vec::new()),
    );

    let (sender, mut inbox) = mpsc::channel(4);
    let forwarding = Forwarding::new(Arc::clone(&teammate), Some(surface(sender)));
    let cancel = CancellationToken::new();
    let carrying = tokio::spawn(forwarding.run(cancel.clone()));
    let mut events = teammate.engine().subscribe().await.expect("the first subscriber wins");

    let lead_side = tokio::spawn(async move {
        let forwarded = inbox.recv().await.expect("the teammate asked something");
        let who = forwarded.teammate.clone();
        forwarded.reply.send(PermissionReply::Once).expect("the forwarding is still waiting");

        who
    });

    teammate
        .engine()
        .send(Command::SendPrompt {
            text: "write the note".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    ganja_testkit::drain(&mut events).await;

    assert_eq!(
        lead_side.await.expect("the lead side answered"),
        "worker",
        "the dialog says whose it is"
    );
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "the answer let the call through"
    );

    cancel.cancel();
    carrying.await.expect("the forwarding ends with its token");
}

/// A teammate whose lead has no dialog surface is refused rather than left
/// waiting for an answer nobody can give.
#[tokio::test]
async fn a_teammate_with_nowhere_to_ask_is_refused_rather_than_left_hanging() {
    let (tool, calls) = ganja_testkit::RecorderTool::new("write", "wrote", "written");
    let (provider, _) = ganja_testkit::ScriptedProvider::named("fake", writes());
    let (_directory, teammate) = teammate(
        provider,
        Arc::new(Registry::new(vec![tool])),
        permissions_for(&lead(Vec::new()), Vec::new()),
    );

    let cancel = CancellationToken::new();
    let carrying = tokio::spawn(Forwarding::new(Arc::clone(&teammate), None).run(cancel.clone()));
    let mut events = teammate.engine().subscribe().await.expect("the first subscriber wins");
    teammate
        .engine()
        .send(Command::SendPrompt {
            text: "write the note".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    ganja_testkit::drain(&mut events).await;

    assert!(
        calls.lock().expect("the call log is never poisoned").is_empty(),
        "a question nobody could see is a refusal"
    );

    cancel.cancel();
    carrying.await.expect("the forwarding ends with its token");
}

/// **What the lead's turn loop reads.** A question carried to the lead is
/// counted for exactly as long as somebody could still be answering it.
#[test]
fn a_carried_dialog_is_counted_until_whoever_waits_for_it_lets_go() {
    let waiting: Arc<AtomicUsize> = Arc::default();
    let (sender, _inbox) = mpsc::channel(4);
    let surface = DialogSurface::new(sender, Arc::clone(&waiting));
    assert_eq!(waiting.load(Ordering::Relaxed), 0, "nothing has been asked yet");

    let raised = surface.hand_over(occupying()).expect("the slot was free");
    assert_eq!(waiting.load(Ordering::Relaxed), 1, "the question is in front of the person");

    drop(raised);
    assert_eq!(waiting.load(Ordering::Relaxed), 0, "and it has been answered");
}

/// A question that was refused rather than carried was never in front of
/// anybody, so it is not counted — the property that keeps the count from
/// drifting upwards on a lead that stopped draining.
#[test]
fn a_dialog_the_lead_could_not_be_offered_is_never_counted() {
    let waiting: Arc<AtomicUsize> = Arc::default();
    let (sender, _held) = mpsc::channel(1);
    let surface = DialogSurface::new(sender, Arc::clone(&waiting));
    let occupying = surface.hand_over(occupying()).expect("the one slot was free");

    surface.hand_over(occupying_from("late")).expect_err("the queue is full");
    assert_eq!(waiting.load(Ordering::Relaxed), 1, "only the one that was really carried");

    drop(occupying);
    assert_eq!(waiting.load(Ordering::Relaxed), 0);
}

/// A dialog already sitting in the lead's one slot.
///
/// Never read by anything: what it *is* does not matter, and that it is
/// **there** is the whole of it — a queue with its only slot spent is what
/// a lead that stopped draining looks like from this side.
fn occupying() -> Forwarded {
    occupying_from("somebody-else")
}

/// The same, from a named teammate, for the one test that needs to tell two
/// of them apart in a sentence.
fn occupying_from(teammate: &str) -> Forwarded {
    Forwarded {
        teammate: teammate.to_owned(),
        request: Event::PermissionRequested {
            session_id: SessionId::ascending(),
            id: PermissionId::ascending(),
            call_id: "a call".to_owned(),
            tool: "write".to_owned(),
            title: "a question nobody answered".to_owned(),
            args: serde_json::Value::Null,
            directories: Vec::new(),
        },
        reply: oneshot::channel().0,
    }
}

/// **A full queue answers exactly as no queue does.** The lead's receiver
/// is claimed and then never drained, which is what a wedged frontend looks
/// like from here; the teammate's ask must come back refused rather than
/// wait behind a question nobody is going to read.
///
/// The channel is filled *first*, so the ask meets a full queue rather than
/// racing to fill one. And this test's real assertion is that it finishes
/// at all: before the handover stopped waiting, the turn below never ended
/// and the only thing that noticed was the harness's own timeout.
#[tokio::test]
async fn a_teammate_whose_lead_never_reads_is_refused_rather_than_left_waiting() {
    let (tool, calls) = ganja_testkit::RecorderTool::new("write", "wrote", "written");
    let (provider, _) = ganja_testkit::ScriptedProvider::named("fake", writes());
    let (_directory, teammate) = teammate(
        provider,
        Arc::new(Registry::new(vec![tool])),
        permissions_for(&lead(Vec::new()), Vec::new()),
    );

    // `_held` keeps the receiver alive, so the handover below meets `Full`
    // rather than `Closed` — the two arms answer alike, and this is the one
    // a test could otherwise pass without ever reaching.
    let (sender, _held) = mpsc::channel(1);
    sender.try_send(occupying()).expect("the one slot was free");

    let cancel = CancellationToken::new();
    let carrying = tokio::spawn(
        Forwarding::new(Arc::clone(&teammate), Some(surface(sender))).run(cancel.clone()),
    );
    let mut events = teammate.engine().subscribe().await.expect("the first subscriber wins");
    teammate
        .engine()
        .send(Command::SendPrompt {
            text: "write the note".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    ganja_testkit::drain(&mut events).await;

    assert!(
        calls.lock().expect("the call log is never poisoned").is_empty(),
        "a question that could not be handed over is a refusal"
    );

    cancel.cancel();
    carrying.await.expect("the forwarding ends with its token");
}

/// A directory named by a wildcard-bearing path is compared, not globbed —
/// the pattern a directory becomes is the directory's own name with `*`
/// appended, which is what the permission engine stores for it.
#[test]
fn the_directory_pattern_is_the_one_an_always_answer_would_have_stored() {
    assert_eq!(
        super::covering(&PathBuf::from("/tmp/scratch")),
        PathBuf::from("/tmp/scratch").join(ANY).to_string_lossy()
    );
}
