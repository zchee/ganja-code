//! Two processes, one unowned task, and exactly one owner (AC: the claim is
//! atomic under the lock).
//!
//! Threads would prove nothing here, and would prove *less* than nothing: the
//! lock has an in-process half (`lock.rs`'s `Local`) that would serialize two
//! threads of one process before the on-disk protocol under test ever ran, so
//! a threaded version of this test would pass with the `mkdir` deleted. The
//! claimants are therefore real processes — this very binary, re-executed with
//! a role in its environment, which is `tests/contention.rs`'s trick and is
//! why each test below opens by asking whether it is the parent or one of the
//! children.
//!
//! Both children are handed the same wall-clock instant to start at, **and a
//! further instant per round**, so they arrive at each task together rather
//! than one running ahead of the other and taking every uncontended lock in
//! turn. That is what makes the race real; nothing *asserted* here depends on
//! the timing, so a child descheduled past its instant costs the run some
//! contention and never a false failure.
//!
//! The children's env is set on the `Command`, never on this process, so
//! nothing here mutates process-wide state.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::{env, fs, thread};

use ganja_team::record::now_millis;
use ganja_team::task::{NewTask, Store, TaskError, TaskId};

/// How many tasks the two claimants race over. One would demonstrate the
/// property; eight gives the interleaving eight chances to go wrong.
const TASKS: u64 = 8;

/// How many processes create tasks at once in the id test, and how many each
/// creates — every one of them a whole read-bump-write under the counter's own
/// hold.
const CREATORS: u64 = 4;

/// How many tasks each creator files.
const EACH: u64 = 5;

/// How long a child is given to start before the first agreed instant.
/// Generous rather than tight: it buys contention, and nothing asserted here
/// needs it.
const RUN_UP: Duration = Duration::from_millis(700);

/// How far apart the per-round instants are.
///
/// Long enough that a hold taken in one round is released before the next one
/// opens — the whole ladder is ≈655 ms, but an uncontended hold here is a
/// sub-millisecond rewrite of a small document — and short enough that eight
/// rounds cost a fifth of a second.
const SPACING: u64 = 20;

/// Set on a child, absent in the parent: which claimant or creator this is.
const ROLE: &str = "GANJA_TEAM_TASK_RACE_ROLE";

/// The task list every child drives.
const TASKS_DIR: &str = "GANJA_TEAM_TASK_RACE_TASKS";

/// Where a child writes what happened to it.
const OUTCOME: &str = "GANJA_TEAM_TASK_RACE_OUTCOME";

/// Unix milliseconds every child waits for before its first claim.
const START: &str = "GANJA_TEAM_TASK_RACE_START";

#[test]
fn two_processes_racing_one_claim_leave_exactly_one_owner() {
    if let Ok(role) = env::var(ROLE) {
        return claim_as(&role);
    }

    let (_home, root, team) = support::root("session-224cbeab");
    let store = Store::new(root.tasks_dir(&team));
    for _ in 0..TASKS {
        store.create(NewTask::new("race", "two claimants, one owner")).expect("a task is created");
    }

    let roles = ["worker-1", "worker-2"];
    let outcomes =
        run(store.dir(), &roles, "two_processes_racing_one_claim_leave_exactly_one_owner");

    // What each child says happened to it, id by id.
    let mut won: BTreeMap<u64, String> = BTreeMap::new();
    let mut lost: BTreeMap<u64, Vec<(String, String)>> = BTreeMap::new();
    for (role, outcome) in &outcomes {
        for line in outcome.lines() {
            let mut words = line.split(' ');
            let id: u64 = words.next().expect("a line opens with an id").parse().expect("an id");
            match words.next().expect("a line says what happened") {
                "won" => assert!(
                    won.insert(id, role.clone()).is_none(),
                    "task {id} was claimed by two processes at once",
                ),
                "lost" => {
                    let owner = words.next().expect("a refusal names the owner").to_owned();
                    lost.entry(id).or_default().push((role.clone(), owner));
                }
                other => panic!("a child said something unaccounted for: {other}"),
            }
        }
    }

    for id in 1..=TASKS {
        let winner = won.get(&id).unwrap_or_else(|| panic!("nobody claimed task {id}"));
        let losers = lost.get(&id).unwrap_or_else(|| panic!("nobody was refused task {id}"));
        assert_eq!(losers.len(), 1, "one winner and one loser: {losers:?}");
        let (loser, observed) = &losers[0];
        assert_ne!(loser, winner, "a process cannot be refused its own claim's task twice");
        assert_eq!(
            observed, winner,
            "the loser of task {id} was told {observed:?} holds it, and {winner:?} does",
        );

        // And the document agrees with both of them, which is the property the
        // lock exists for: the refused write never landed.
        let filed = store.get(&TaskId::parse(&id.to_string()).expect("an id")).expect("it reads");
        assert_eq!(&filed.owner, winner, "task {id}'s document names its winner");
    }

    // Evidence rather than an assertion: how the eight actually split tells
    // whether the run raced or merely ran, and a distribution assertion would
    // be a flake on a busy machine.
    let split: BTreeMap<&String, usize> = won.values().fold(BTreeMap::new(), |mut counts, role| {
        *counts.entry(role).or_default() += 1;

        counts
    });
    println!("claims won, by process: {split:?}");
}

#[test]
fn concurrent_creates_issue_every_id_exactly_once() {
    if let Ok(role) = env::var(ROLE) {
        return create_as(&role);
    }

    let (_home, root, team) = support::root("session-224cbeab");
    let store = Store::new(root.tasks_dir(&team));

    let roles: Vec<String> = (0..CREATORS).map(|nth| format!("creator-{nth}")).collect();
    let roles: Vec<&str> = roles.iter().map(String::as_str).collect();
    let outcomes = run(store.dir(), &roles, "concurrent_creates_issue_every_id_exactly_once");

    let mut issued: Vec<u64> = Vec::new();
    for (_, outcome) in &outcomes {
        for line in outcome.lines() {
            issued.push(line.parse().expect("a creator writes the ids it was given"));
        }
    }
    let distinct: BTreeSet<u64> = issued.iter().copied().collect();

    assert_eq!(issued.len() as u64, CREATORS * EACH, "every create answered with an id");
    assert_eq!(distinct.len(), issued.len(), "and no id was handed out twice: {issued:?}");
    assert_eq!(
        distinct,
        (1..=CREATORS * EACH).collect::<BTreeSet<_>>(),
        "the ids are the first {} numbers, in one unbroken run",
        CREATORS * EACH,
    );
    assert_eq!(store.list().expect("the list reads").len() as u64, CREATORS * EACH);
}

/// Starts one child per role, waits for all of them, and answers with what
/// each wrote.
///
/// The agreed start instant is computed once and handed to every child, which
/// is what turns N processes into N racers rather than a queue.
fn run(tasks: &std::path::Path, roles: &[&str], test: &str) -> Vec<(String, String)> {
    let binary = env::current_exe().expect("a test binary knows its own path");
    let outcomes = tasks.parent().expect("a tasks directory sits inside a team directory");
    fs::create_dir_all(outcomes).expect("the outcome directory is creatable");
    let start = now_millis() + u64::try_from(RUN_UP.as_millis()).expect("a small number");

    let children: Vec<_> = roles
        .iter()
        .map(|role| {
            let outcome = outcomes.join(format!("outcome-{role}.txt"));
            let child = Command::new(&binary)
                .args([test, "--exact", "--test-threads=1"])
                .env(ROLE, role)
                .env(TASKS_DIR, tasks)
                .env(OUTCOME, &outcome)
                .env(START, start.to_string())
                .spawn()
                .expect("a child process starts");

            ((*role).to_owned(), outcome, child)
        })
        .collect();

    children
        .into_iter()
        .map(|(role, outcome, mut child)| {
            let status = child.wait().expect("a child process is waitable");
            assert!(status.success(), "{role} failed: {status}");

            (role, fs::read_to_string(&outcome).expect("a child wrote its outcome"))
        })
        .collect()
}

/// One claimant's whole job: every task in turn, from the agreed instant.
fn claim_as(role: &str) {
    let store = Store::new(told(TASKS_DIR));
    let start = told_when();

    let mut outcome = String::new();
    for id in 1..=TASKS {
        // Every claimant reaches task `id` at the same instant, so each of the
        // eight is genuinely contended rather than merely sequential.
        wait_until(start + id * SPACING);
        let id = TaskId::parse(&id.to_string()).expect("an id");
        match store.claim(&id, role) {
            Ok(task) => {
                assert_eq!(task.owner, role, "a claim answers with what it wrote");
                outcome.push_str(&format!("{id} won\n"));
            }
            Err(TaskError::AlreadyOwned { owner, .. }) => {
                outcome.push_str(&format!("{id} lost {owner}\n"));
            }
            Err(error) => panic!("{role} could not claim {id}: {error}"),
        }
    }
    fs::write(told(OUTCOME), outcome).expect("a child writes its outcome");
}

/// One creator's whole job: [`EACH`] tasks, from the agreed instant, each
/// through the counter's own hold.
fn create_as(role: &str) {
    let store = Store::new(told(TASKS_DIR));
    let start = told_when();

    let mut outcome = String::new();
    for round in 0..EACH {
        wait_until(start + round * SPACING);
        let task = store
            .create(NewTask::new(format!("{role}-{round}"), "one of many at once"))
            .unwrap_or_else(|error| panic!("{role} could not create round {round}: {error}"));
        outcome.push_str(&format!("{}\n", task.id));
    }
    fs::write(told(OUTCOME), outcome).expect("a child writes its outcome");
}

/// A path a child was handed.
fn told(variable: &str) -> PathBuf {
    PathBuf::from(env::var_os(variable).unwrap_or_else(|| panic!("a child is told its {variable}")))
}

/// The instant a child's first round opens at.
fn told_when() -> u64 {
    env::var(START).expect("a child is told when to start").parse().expect("milliseconds")
}

/// Parks until `instant`, so every child arrives at a round together.
///
/// Sleeps most of the way and spins the last few milliseconds: a sleep alone
/// scatters the wake-ups by the timer's own granularity, which is the scatter
/// this is trying not to have. An instant already past returns immediately,
/// which is what a child descheduled through a whole round does.
fn wait_until(instant: u64) {
    loop {
        let now = now_millis();
        if now >= instant {
            return;
        }
        if instant - now > 5 {
            thread::sleep(Duration::from_millis(1));
        } else {
            std::hint::spin_loop();
        }
    }
}
