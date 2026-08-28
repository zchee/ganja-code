//! A pane spawn in a session with no tmux is refused, and refused readably
//! (**AC-16**, **D501** enforced against the two pane values, D-6).
//!
//! Spec: Claude Code's teammates — §10.2's closing decision, "with no `$TMUX`,
//! either refuse readably or self-host a detached session", settled as *refuse*
//! by D-6. Upstream opencode has no teammates and no counterpart.
//!
//! What this pins is the sentence, not the error kind, because the sentence is
//! the useful half: a person who asked for a window and was told
//! `Unsupported` would still not know whether their session or their build was
//! the problem. And it pins that the refusal is a refusal — the in-process
//! backend spawns in the very same session, so nothing here silently
//! substitutes one for the other in either direction: a `ganja` request does
//! not become an in-process teammate, and an `in-process` request is not
//! refused for lack of a window it never wanted.
//!
//! Since **Dv-1** it pins one more arm, and the one with the most room to go
//! wrong: an **unnamed** backend is `ganja`, so it is refused here too. That
//! is the case where a silent fallback would be most tempting and least
//! honest — nobody typed a surface, so nobody would notice getting a different
//! one.
//!
//! One test, because it mutates `TMUX` (and `TMUX_PANE`, which would otherwise
//! name a pane of whatever server the developer is running the suite in), and
//! a binary that mutates process-wide state holds exactly one — a plain `cargo
//! test` runs a binary's tests on threads of one process.
//!
//! Every spawn goes through [`Teammates::start`], the one door onto the
//! registry, so the refusal asserted is the one production answers with.

use ganja_team::{MemberName, mailbox};
use ganja_teammate_local::tmux::{self, REFUSED_NO_TMUX};
use ganja_testkit::{AllowSpawn, caller, spawn, team, teammates_recorded};

#[tokio::test]
async fn a_pane_spawn_without_tmux_is_refused_readably() {
    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written. Both
    // variables go, because tmux exports both into a pane and either one left
    // behind would name the developer's own server.
    unsafe {
        std::env::remove_var(tmux::TMUX);
        std::env::remove_var(tmux::TMUX_PANE);
    }
    assert!(!tmux::hosted(), "the premise: this process is outside tmux");

    let home = ganja_testkit::temp_dir();
    let (root, team, registry, door) = team(home.path());
    let caller = caller(home.path());

    // The assertion is on the words: the variable that is missing, and the way
    // out — which is not "wait for another phase". Both pane values are in the
    // loop because both have real bodies now, and the claim is that they
    // refuse in **one** sentence: a `claude` spawn that said something else
    // about a missing session would be two behaviours wearing one argument.
    for backend in ["ganja", "claude"] {
        let refused = door
            .start(spawn("worker", Some(backend)), &caller, &AllowSpawn)
            .await
            .expect_err("a session outside tmux has no pane to give");
        assert!(
            refused.reason.ends_with(REFUSED_NO_TMUX),
            "{backend} refuses in the sentence that names the session as what is missing: \
             {}",
            refused.reason
        );
        assert!(
            refused.reason.contains("$TMUX") && refused.reason.contains("in-process"),
            "{backend}'s refusal names the variable and the alternative: {}",
            refused.reason
        );
        assert!(
            refused.reason.contains(backend),
            "and still says which surface was asked for: {}",
            refused.reason
        );
    }
    // A refused spawn leaves nothing behind: no member, no teammate, and — the
    // half that would otherwise still be there tomorrow — no task sitting in a
    // mailbox nothing will ever read.
    assert!(
        teammates_recorded(&root, &team).is_empty(),
        "a refused spawn joined nobody to the team"
    );
    assert_eq!(registry.running(), 0, "and nothing was quietly started instead");
    let inbox = root.inbox_path(&team, &MemberName::parse("worker").expect("a member name"));
    assert!(
        mailbox::read(&inbox).expect("the inbox reads").valid.is_empty(),
        "a refused spawn left its prompt in an inbox"
    );

    // **AC-3, as Dv-1 amends it.** An absent backend means `ganja`, so it is
    // refused here in the same sentence naming it would have earned — the
    // whole point of the no-silent-fallback rule, now reaching the spawns that
    // named nothing. A build that quietly handed back an in-process teammate
    // would be answering a question nobody asked.
    let defaulted = door
        .start(spawn("worker", None), &caller, &AllowSpawn)
        .await
        .expect_err("an unnamed backend is `ganja`, and this session has no pane to give");
    assert!(
        defaulted.reason.ends_with(REFUSED_NO_TMUX),
        "an unnamed backend refuses in the sentence its name would have: {}",
        defaulted.reason
    );
    assert_eq!(registry.running(), 0, "and started nothing instead");

    // The other direction of "no silent fallback": the backend that needs no
    // window still runs in this very session — asked for **by name**, which
    // since Dv-1 is the only way to reach it.
    let started = door
        .start(spawn("worker", Some("in-process")), &caller, &AllowSpawn)
        .await
        .expect("an in-process teammate needs no tmux");
    assert_eq!(started.backend, "in-process");
    assert_eq!(registry.running(), 1);
    assert_eq!(teammates_recorded(&root, &team), vec!["worker".to_owned()]);

    registry.shutdown().await;
    assert_eq!(registry.running(), 0);
}
