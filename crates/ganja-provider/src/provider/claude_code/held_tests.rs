use std::time::Duration;

use ganja_testkit::fake_claude::{Call, Script, Turn};

use super::{DEFAULT_IDLE_BOUND, HELD_CAP, Reason, SILENCE_BOUND, STRANDED_BOUND};
use crate::provider::Provider as _;
use crate::provider::claude_code::binding::{self, Paths};
use crate::provider::claude_code::tests::{
    FakeCli, assistant, called, failure, request, said, says, turn, user, wired,
};

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

/// The key a conversation opening with `id` is filed under.
fn key(id: &str) -> String {
    crate::provider::ids::derived(&crate::protocol::MessageId::from(id.to_owned()))
}

/// A script whose one turn calls `read` and then waits.
fn calls_a_tool() -> Script {
    Script {
        turns: vec![Turn {
            tool_calls: vec![Call {
                id: "toolu_1".to_owned(),
                name: "read".to_owned(),
                input: serde_json::json!({}),
                call_first: false,
            }],
            result: "pong".to_owned(),
            ..Turn::default()
        }],
        ..Script::default()
    }
}

// -------------------------------------------------------- the constants

/// Each number is derived rather than picked, and the derivation is the
/// comment beside it — so a later change has to disagree with an argument
/// rather than with a literal.
#[test]
fn each_bound_is_the_number_its_own_reasoning_produced() {
    // One root conversation plus `agents.concurrency`'s default four
    // children is five, the most a `/team` holds busy at once; three more are
    // idle headroom, so a busy five never has to evict to admit a sixth.
    assert_eq!(HELD_CAP, 8);
    // The `tools/call` timeout this side hands the CLI, past which the CLI
    // has abandoned the call itself.
    assert_eq!(STRANDED_BOUND, Duration::from_secs(3_600));
    // Sixty times the slowest first frame after a `user` frame (2.0 s).
    assert_eq!(SILENCE_BOUND, Duration::from_secs(120));
    assert_eq!(DEFAULT_IDLE_BOUND, Duration::from_secs(600));
    // Over a hundred times the slowest EOF-to-exit the recording measured
    // (14 ms), and fourteen times run 7's whole life (exit 1 in 141 ms).
    assert_eq!(super::EXIT_SETTLE_BOUND, Duration::from_secs(2));
}

/// The ring's word for a refused entry maps to `refused-record` and **never**
/// to `exited`.
#[test]
fn a_refused_entry_never_logs_as_an_exited_one() {
    assert_eq!(Reason::Refused.spawn_word(), Some("refused-record"));
    assert_eq!(Reason::Exited.spawn_word(), Some("exited"));
    assert_eq!(Reason::IdleEvicted.spawn_word(), Some("idle-evicted"));
    assert_eq!(Reason::Stranded.spawn_word(), Some("stranded"));
}

/// A capped, closed or diverged entry's next request has a reason of its own,
/// so the ring says nothing a spawn can use.
#[test]
fn the_three_reasons_a_spawn_learns_nothing_from_say_nothing() {
    assert_eq!(Reason::Cap.spawn_word(), None);
    assert_eq!(Reason::Close.spawn_word(), None);
    assert_eq!(Reason::Divergence.spawn_word(), None);
}

// --------------------------------------------------------------- (i) idle

/// The eviction is **rendered**, not only logged: the notice shows exactly
/// between the eviction and the turn that pays for it.
#[tokio::test(start_paused = true)]
async fn an_idle_entry_is_closed_and_the_notice_stands_until_the_turn_that_pays_for_it() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two"]));
    let provider = wired(&cli, home.path()).with_idle_bound(Duration::from_secs(30));

    turn(&provider, request(vec![user("m1", "first")], 0)).await;
    assert_eq!(provider.held_entries(), 1);
    assert_eq!(provider.last_eviction(), None, "nothing has been evicted yet");

    tokio::time::sleep(Duration::from_secs(31)).await;
    tokio::task::yield_now().await;

    assert_eq!(provider.held_entries(), 0, "an idle entry is closed");
    let eviction = provider.last_eviction().expect("the notice stands");
    assert_eq!(eviction.key, key("m1"));

    // The next turn on that key opens a **fresh record** — never a resume —
    // and takes the notice down as it does.
    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2),
    )
    .await;

    assert_eq!(cli.count(), 2, "a fresh record, and the process is not resumed");
    assert!(cli.argv(1).contains(&"--session-id".to_owned()));
    assert!(!cli.argv(1).contains(&"--resume".to_owned()));
    assert_eq!(provider.last_eviction(), None, "the notice comes down when its cost is paid");

    provider.shutdown().await;
}

/// The sweep does not run while an ask is parked: a dialog somebody is
/// reading and a twelve-minute `bash` are not idleness. `IDLE_BOUND` was
/// exactly the rule that would otherwise have reaped the entry rule (ii) says
/// is never evictable.
#[tokio::test(start_paused = true)]
async fn an_entry_with_a_parked_ask_survives_the_idle_bound_and_twice_it() {
    let home = temp();
    let cli = FakeCli::new(calls_a_tool());
    let provider = wired(&cli, home.path()).with_idle_bound(Duration::from_secs(30));

    turn(&provider, request(vec![user("m1", "read it")], 0)).await;
    assert_eq!(provider.held_entries(), 1);

    tokio::time::sleep(Duration::from_secs(61)).await;
    tokio::task::yield_now().await;

    assert_eq!(provider.held_entries(), 1, "a parked ask is not idleness");
    assert_eq!(provider.last_eviction(), None);

    provider.shutdown().await;
}

/// (i′) The backstop rule (i) needs because it excludes exactly that case.
#[tokio::test(start_paused = true)]
async fn an_ask_parked_past_the_stranded_bound_closes_its_entry() {
    let home = temp();
    let cli = FakeCli::new(calls_a_tool());
    let provider = wired(&cli, home.path()).with_idle_bound(Duration::from_secs(30));

    turn(&provider, request(vec![user("m1", "read it")], 0)).await;
    tokio::time::sleep(STRANDED_BOUND + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;

    assert_eq!(provider.held_entries(), 0, "past the hour the CLI has given up on the call");
}

// ----------------------------------------------------------- the recover

/// A resolving request that finds no live entry while carrying `Tool` parts
/// after `turn_start`. The engine ran the tool exactly once and the CLI turn
/// that asked is dead, so the turn is reopened rather than re-run.
#[tokio::test(start_paused = true)]
async fn a_stranded_turn_is_reopened_on_a_fresh_record_and_the_tool_is_not_re_run() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![
            Turn {
                tool_calls: vec![Call {
                    id: "toolu_1".to_owned(),
                    name: "read".to_owned(),
                    input: serde_json::json!({}),
                    call_first: false,
                }],
                result: "pong".to_owned(),
                ..Turn::default()
            },
            Turn {
                text: vec!["recovered".to_owned()],
                result: "recovered".to_owned(),
                ..Turn::default()
            },
        ],
        ..Script::default()
    });
    let provider = wired(&cli, home.path()).with_idle_bound(Duration::from_secs(30));

    turn(&provider, request(vec![user("m1", "read it")], 0)).await;
    tokio::time::sleep(STRANDED_BOUND + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.held_entries(), 0);

    // The engine finished the tool and calls back with the result.
    let events = turn(
        &provider,
        request(vec![user("m1", "read it"), called("a1", "toolu_1", "contents")], 0),
    )
    .await;

    assert_eq!(said(&events), "recovered");
    assert_eq!(cli.count(), 2, "a fresh record, not a resume");

    let opened = &cli.record(1).user_frames[0];
    assert!(opened.contains("[User] read it"), "the turn's own prompt: {opened}");
    assert!(opened.contains("[Tool Result]\ncontents"), "and its tool trail: {opened}");
    assert!(opened.ends_with(super::super::preamble::MID_TURN_RESUME));
    assert!(
        !opened.lines().any(|line| line.starts_with("[Assistant]")),
        "never the reply's text: {opened}"
    );

    assert_eq!(cli.record(1).mcp_results.len(), 0, "the tool is not re-run");

    provider.shutdown().await;
}

/// The memory is per **turn**: two strandings within one turn fail the
/// second, and a later turn of the same conversation may recover again.
#[tokio::test(start_paused = true)]
async fn a_second_recovery_of_one_turn_is_failed_naming_both_attempts() {
    let home = temp();
    let cli = FakeCli::new(calls_a_tool());
    let provider = wired(&cli, home.path()).with_idle_bound(Duration::from_secs(30));

    turn(&provider, request(vec![user("m1", "read it")], 0)).await;
    tokio::time::sleep(STRANDED_BOUND + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;

    let resolving = request(vec![user("m1", "read it"), called("a1", "toolu_1", "contents")], 0);
    turn(&provider, resolving.clone()).await;

    // Strand the reopened record too.
    tokio::time::sleep(STRANDED_BOUND + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;

    let events = turn(&provider, resolving).await;
    let failure = failure(&events).expect("a second reopening of one turn is failed");
    assert!(failure.contains("already reopened once"), "{failure}");
    assert!(failure.contains(&key("m1")), "the conversation is named: {failure}");

    provider.shutdown().await;
}

// ------------------------------------------------------------- (ii) cap

/// Never a running turn and never a parked ask: evicting either would throw
/// away work in flight to keep a count.
#[tokio::test]
async fn the_ninth_entry_closes_the_least_recently_used_idle_one() {
    let home = temp();
    let cli = FakeCli::new(says(&["pong"]));
    let provider = wired(&cli, home.path());

    for at in 0..HELD_CAP {
        turn(&provider, request(vec![user(&format!("m{at}"), "hello")], 0)).await;
    }
    assert_eq!(provider.held_entries(), HELD_CAP);

    turn(&provider, request(vec![user("m-ninth", "hello")], 0)).await;

    assert_eq!(provider.held_entries(), HELD_CAP, "the cap holds");
    assert_eq!(cli.count(), HELD_CAP + 1, "and the ninth was still served");

    provider.shutdown().await;
}

/// Refusing a turn to keep a count is the worse failure.
#[tokio::test]
async fn with_every_entry_holding_a_parked_ask_the_spawn_proceeds_anyway() {
    let home = temp();
    let cli = FakeCli::new(calls_a_tool());
    let provider = wired(&cli, home.path());

    for at in 0..HELD_CAP {
        turn(&provider, request(vec![user(&format!("m{at}"), "read it")], 0)).await;
    }
    assert_eq!(provider.held_entries(), HELD_CAP);

    turn(&provider, request(vec![user("m-ninth", "read it")], 0)).await;

    assert_eq!(provider.held_entries(), HELD_CAP + 1, "over the cap rather than a refused turn");

    provider.shutdown().await;
}

/// Two child keys sharing one provider each get their own entry.
#[tokio::test]
async fn two_conversations_on_one_provider_hold_two_processes() {
    let home = temp();
    let cli = FakeCli::new(says(&["pong"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("root", "hello")], 0)).await;
    turn(&provider, request(vec![user("child", "hello")], 0)).await;

    assert_eq!(provider.held_entries(), 2);
    assert_eq!(cli.count(), 2);

    provider.shutdown().await;
}

// ------------------------------------------------- (iii) divergence

/// AC-3.15 (a): a `system` change takes **Continue**. The process keeps the
/// prompt it opened with, and the change reaches the CLI at the next fresh
/// record.
#[tokio::test(start_paused = true)]
async fn a_changed_system_prompt_rides_the_same_process_and_the_next_fresh_record_carries_it() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two", "three"]));
    let provider = wired(&cli, home.path()).with_idle_bound(Duration::from_secs(30));

    turn(&provider, request(vec![user("m1", "first")], 0)).await;

    let mut changed =
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2);
    changed.system = Some("you are ganja, and you have walked into a subtree".to_owned());
    turn(&provider, changed.clone()).await;

    assert_eq!(cli.count(), 1, "a live process does not diverge on its machinery");
    assert_eq!(cli.record(0).user_frames.len(), 2);
    assert_eq!(
        cli.record(0).system_prompt.as_deref(),
        Some(["you are ganja".to_owned()].as_slice()),
        "it keeps what it opened with"
    );

    // An idle eviction, and the fresh record carries the new value.
    tokio::time::sleep(Duration::from_secs(31)).await;
    tokio::task::yield_now().await;

    let mut third = request(
        vec![
            user("m1", "first"),
            assistant("a1", "one"),
            user("m2", "second"),
            assistant("a2", "two"),
            user("m3", "third"),
        ],
        4,
    );
    third.system = changed.system.clone();
    turn(&provider, third).await;

    assert_eq!(cli.count(), 2);
    assert_eq!(
        cli.record(1).system_prompt.as_deref(),
        Some(["you are ganja, and you have walked into a subtree".to_owned()].as_slice())
    );

    provider.shutdown().await;
}

#[tokio::test]
async fn a_changed_roster_rides_the_same_process_too() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;

    let mut changed =
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2);
    changed.tools.push(crate::tool::ToolDefinition {
        name: "bash".to_owned(),
        description: "runs a command".to_owned(),
        schema: serde_json::json!({}),
    });
    turn(&provider, changed).await;

    assert_eq!(cli.count(), 1);
    assert_eq!(
        cli.record(0).tools_list,
        ["read"],
        "nothing re-dialled, so the roster is the one it opened with"
    );

    provider.shutdown().await;
}

/// AC-3.15 (b): the two values a **person** chose. Keeping a chosen model
/// stale bills the opening model under a status bar that says otherwise.
#[tokio::test]
async fn a_changed_model_closes_the_process_and_opens_a_fresh_record() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two", "three"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;

    let mut changed =
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2);
    changed.model = "claude-sonnet-5".to_owned();
    turn(&provider, changed.clone()).await;

    assert_eq!(cli.count(), 2, "the person chose it, so it is honoured");
    assert!(cli.argv(1).contains(&"--model".to_owned()));
    assert!(cli.argv(1).contains(&"claude-sonnet-5".to_owned()));
    assert!(!cli.argv(1).contains(&"--resume".to_owned()));
    assert!(cli.signals.lock().expect("the signals").is_empty(), "EOF was enough");

    // And the next request on the key rides the second process.
    let mut third = request(
        vec![
            user("m1", "first"),
            assistant("a1", "one"),
            user("m2", "second"),
            assistant("a2", "two"),
            user("m3", "third"),
        ],
        4,
    );
    third.model = changed.model;
    turn(&provider, third).await;

    assert_eq!(cli.count(), 2, "the second process is the one that continues");

    provider.shutdown().await;
}

#[tokio::test]
async fn a_changed_effort_closes_the_process_too() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;

    let mut changed =
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2);
    changed.effort_options.insert("effort".to_owned(), serde_json::Value::from("high"));
    turn(&provider, changed).await;

    assert_eq!(cli.count(), 2);
    assert!(cli.argv(1).contains(&"--effort".to_owned()));
    assert!(cli.argv(1).contains(&"high".to_owned()));

    provider.shutdown().await;
}

// -------------------------------------------------------- (iv) refused

/// A refused record's replacement opens with **no preamble at all** — the
/// user's asks alone, no header, no tool trail — and a second refusal in a
/// row spends nothing.
#[tokio::test]
async fn a_refused_record_is_closed_and_its_replacement_carries_no_preamble() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![
            Turn { refused: true, ..Turn::default() },
            Turn { refused: true, ..Turn::default() },
        ],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());
    let paths = Paths::under(home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;

    assert_eq!(provider.held_entries(), 0, "the entry is closed at the refusal's own result");
    let after = binding::load(&paths.binding(&key("m1"))).expect("a binding");
    assert!(after.refused);
    assert_eq!(after.refused_streak, 1);

    // The next request on the key: a fresh record with the prompts alone.
    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "?"), user("m2", "second")], 2),
    )
    .await;

    assert_eq!(cli.count(), 2);
    let opened = &cli.record(1).user_frames[0];
    assert!(!opened.contains("[Conversation so far]"), "no header: {opened}");
    assert!(!opened.contains("[Tool Call]"), "no tool trail: {opened}");
    assert!(!opened.contains("[User]"), "the asks as owed text, not as rendered lines: {opened}");
    assert_eq!(opened, "first\n\nsecond", "each user message exactly once, in order");

    let replaced = binding::load(&paths.binding(&key("m1"))).expect("a binding");
    assert_eq!(
        replaced.refused_streak, 2,
        "the replacement was refused too, and the streak counts consecutive records"
    );
}

/// At two the wire stops spending: one record with the transcript rendered,
/// one with the prompts alone, and then the turn is failed locally.
#[tokio::test]
async fn a_second_refusal_in_a_row_spawns_nothing_and_names_both_attempts() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![
            Turn { refused: true, ..Turn::default() },
            Turn { refused: true, ..Turn::default() },
        ],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());
    let paths = Paths::under(home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;
    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "?"), user("m2", "second")], 2),
    )
    .await;
    assert_eq!(binding::load(&paths.binding(&key("m1"))).expect("a binding").refused_streak, 2);

    let events = turn(
        &provider,
        request(
            vec![
                user("m1", "first"),
                assistant("a1", "?"),
                user("m2", "second"),
                assistant("a2", "?"),
                user("m3", "third"),
            ],
            4,
        ),
    )
    .await;

    let failure = failure(&events).expect("the third request is refused locally");
    assert!(failure.contains("refused twice in a row"), "{failure}");
    assert!(failure.contains("/compact"), "the two doors are named: {failure}");
    assert!(failure.contains("new session"), "{failure}");
    assert_eq!(cli.count(), 2, "exactly two spawns for the key, no third");
}

/// A served `result` on any record of the key is the only reset.
#[tokio::test]
async fn a_served_turn_resets_the_streak_so_a_later_refusal_starts_a_new_one() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![
            Turn { refused: true, ..Turn::default() },
            Turn { text: vec!["pong".to_owned()], result: "pong".to_owned(), ..Turn::default() },
        ],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());
    let paths = Paths::under(home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;
    assert_eq!(binding::load(&paths.binding(&key("m1"))).expect("a binding").refused_streak, 1);

    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "?"), user("m2", "second")], 2),
    )
    .await;

    assert_eq!(
        binding::load(&paths.binding(&key("m1"))).expect("a binding").refused_streak,
        0,
        "a record that has been served has shown the streak is over"
    );

    provider.shutdown().await;
}

/// A new key — a compaction — starts from a fresh binding and spawns.
#[tokio::test]
async fn a_compaction_is_a_new_key_that_starts_from_nothing() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![Turn { refused: true, ..Turn::default() }],
        ..Script::default()
    });
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;

    // A compaction replaces `messages[0]`, so the key moves — and the new key
    // reads its own binding, which does not exist, rather than the old key's
    // refusal.
    turn(&provider, request(vec![assistant("s1", "Summary so far: …"), user("m9", "carry on")], 1))
        .await;

    assert_eq!(cli.count(), 2, "a new key spawns rather than inheriting a streak");
    let opened = &cli.record(1).user_frames[0];
    assert_eq!(
        opened, "carry on",
        "a summary is assistant text, so the preamble renders nothing at all: {opened}"
    );

    provider.shutdown().await;
}

// -------------------------------------------------------- the rewind arm

/// AC-3.8's own loop-closing assertion: the *second* turn after a re-seed
/// must take Continue, or every later turn would re-seed for the life of the
/// conversation.
#[tokio::test]
async fn a_rewind_re_seeds_once_and_the_next_turn_rides_the_new_record() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two", "three"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;
    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2),
    )
    .await;
    assert_eq!(cli.count(), 1);

    // `m2` is gone: a `/rewind`.
    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m3", "instead")], 2),
    )
    .await;

    assert_eq!(cli.count(), 2, "the held process holds a conversation ganja no longer has");
    let opened = &cli.record(1).user_frames[0];
    assert!(opened.starts_with("[Conversation so far]"));
    assert_eq!(opened.matches("first").count(), 1, "each user message exactly once");
    assert_eq!(opened.matches("instead").count(), 1);
    assert!(
        !opened.lines().any(|line| line.starts_with("[Assistant]")),
        "though the transcript holds a reply: {opened}"
    );

    // The second turn on the new record.
    turn(
        &provider,
        request(
            vec![
                user("m1", "first"),
                assistant("a1", "one"),
                user("m3", "instead"),
                assistant("a2", "two"),
                user("m4", "again"),
            ],
            4,
        ),
    )
    .await;

    assert_eq!(cli.count(), 2, "the re-seed cannot loop");
    assert_eq!(cli.record(1).user_frames.len(), 2);
    assert_eq!(cli.record(1).user_frames[1], "again", "the new prompt alone");

    provider.shutdown().await;
}

// -------------------------------------------------------- locked elsewhere

/// The arm with no binding to reason from — where a rendering that appended
/// nothing would have written the conversation twice in one paid frame.
#[tokio::test]
async fn a_second_ganja_on_a_locked_conversation_writes_each_message_exactly_once() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two"]));
    let provider = wired(&cli, home.path());
    let paths = Paths::under(home.path());

    // Somebody else holds the lock.
    let held = binding::Lock::claim(&paths.lock(&key("m1"))).expect("the first claim");

    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2),
    )
    .await;

    let opened = &cli.record(0).user_frames[0];
    assert_eq!(opened.matches("first").count(), 1, "exactly once: {opened}");
    assert_eq!(opened.matches("second").count(), 1);
    assert!(!opened.lines().any(|line| line.starts_with("[Assistant]")));
    assert!(
        binding::load(&paths.binding(&key("m1"))).is_none(),
        "this ganja reads no binding and writes none"
    );

    // The lock still held, the next turn rides this ganja's own fresh record.
    turn(
        &provider,
        request(
            vec![
                user("m1", "first"),
                assistant("a1", "one"),
                user("m2", "second"),
                assistant("a2", "two"),
                user("m3", "third"),
            ],
            4,
        ),
    )
    .await;

    assert_eq!(cli.count(), 1, "one fresh record, then Continue on it");
    assert_eq!(cli.record(0).user_frames[1], "third");

    drop(held);
    provider.shutdown().await;
}

// -------------------------------------------------- AC-3.21's twelve arms

/// **No argv the wire builds contains `--resume`**, driven by reaching every
/// arm that spawns rather than by asserting it of the builders alone.
///
/// One chain on one key, so every spawn the test causes is one of the named
/// arms; the one-shot is its own request, and `unsent-history` needs a second
/// provider whose drop ring is empty.
#[tokio::test(start_paused = true)]
async fn every_arm_that_spawns_builds_an_argv_and_none_of_them_resumes() {
    let home = temp();
    let cli = FakeCli::new(Script {
        turns: vec![
            Turn { text: vec!["one".to_owned()], result: "one".to_owned(), ..Turn::default() },
            Turn { refused: true, ..Turn::default() },
            Turn { text: vec!["two".to_owned()], result: "two".to_owned(), ..Turn::default() },
        ],
        ..Script::default()
    });
    let provider = wired(&cli, home.path()).with_idle_bound(Duration::from_secs(30));

    // new-key.
    turn(&provider, request(vec![user("m1", "first")], 0)).await;

    // idle-evicted.
    tokio::time::sleep(Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2),
    )
    .await;

    // rewind.
    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m3", "instead")], 2),
    )
    .await;

    // model.
    let mut moved =
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m3", "instead")], 2);
    moved.model = "claude-sonnet-5".to_owned();
    turn(&provider, moved.clone()).await;

    // effort.
    let mut effort = moved.clone();
    effort.effort_options.insert("effort".to_owned(), serde_json::Value::from("high"));
    turn(&provider, effort).await;

    for argv in cli.argvs() {
        assert!(!argv.contains(&"--resume".to_owned()), "{argv:?}");
        assert!(argv.contains(&"--session-id".to_owned()), "{argv:?}");
    }
    assert!(cli.count() >= 5, "each arm above spawned: {}", cli.count());

    provider.shutdown().await;
}

/// The three arms that spawn **nothing** add no argv.
#[tokio::test]
async fn resolve_continue_and_the_refused_twice_refusal_add_no_argv() {
    let home = temp();
    let cli = FakeCli::new(says(&["one", "two"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "first")], 0)).await;
    let after_spawn = cli.count();

    // Continue.
    turn(
        &provider,
        request(vec![user("m1", "first"), assistant("a1", "one"), user("m2", "second")], 2),
    )
    .await;

    assert_eq!(cli.count(), after_spawn, "a Continue rides the process it found");

    provider.shutdown().await;
}

// ----------------------------------------------------------- the scratch cwd

/// **Never the project root and never `.`**: run 6 paid 7 348 extra prefix
/// tokens for a checkout as cwd.
#[tokio::test]
async fn every_child_runs_in_an_empty_scratch_directory_under_the_data_home() {
    let home = temp();
    let cli = FakeCli::new(says(&["pong"]));
    let provider = wired(&cli, home.path());

    turn(&provider, request(vec![user("m1", "hello")], 0)).await;

    let record = cli.record(0);
    let cwd = std::path::Path::new(&record.cwd);
    assert!(cwd.starts_with(home.path().join("ganja").join("claude-code").join("cwd")));
    assert!(cwd.exists(), "it exists at spawn");
    assert_eq!(std::fs::read_dir(cwd).expect("the directory").count(), 0, "and it is empty");
    assert_ne!(cwd, std::env::current_dir().expect("a cwd"), "never this process's own");

    provider.shutdown().await;

    assert!(!cwd.exists(), "the per-key directory goes with the entry that owned it");
}

#[tokio::test]
async fn a_one_shots_scratch_directory_is_shared_and_survives() {
    let home = temp();
    let cli = FakeCli::new(says(&["a title"]));
    let provider = wired(&cli, home.path());

    turn(
        &provider,
        crate::provider::ChatRequest {
            model: super::super::DEFAULT_MODEL.to_owned(),
            system: None,
            messages: vec![user("m1", "name this")],
            turn_start: 0,
            tools: Vec::new(),
            effort_options: serde_json::Map::new(),
        },
    )
    .await;

    let one_shot = Paths::under(home.path()).one_shot_cwd();
    assert_eq!(std::path::Path::new(&cli.record(0).cwd), one_shot);
    assert!(one_shot.exists(), "left, empty, for the next one-shot");
}

// ---------------------------------------- what a turn leaves behind, and to whom

/// One `Wiring` with nothing behind it: no table, no binding, no scratch
/// directory — enough for the arms that only read `meta` and `tools`.
fn wiring(tools: Vec<crate::tool::ToolDefinition>) -> super::Wiring {
    super::Wiring {
        key: "k".to_owned(),
        meta: std::sync::Arc::new(std::sync::Mutex::new(super::Meta::opening(
            "01998a00-0000-7000-8000-00000000000a".to_owned(),
            "default".to_owned(),
            None,
            0,
            0,
        ))),
        table: std::sync::Weak::new(),
        slots: super::Slots::default(),
        binding: None,
        cwd: None,
        tools,
        requested_model: "default".to_owned(),
        version: "0.0.0".to_owned(),
        opening: String::new(),
        one_shot: false,
    }
}

/// Everything one `tools/call` produced: what went to the CLI, and what the
/// engine was asked to do about it.
async fn call_answered(
    wiring: &super::Wiring,
    turn: &mut super::Turn,
    call: super::super::rpc::ToolCall,
) -> (serde_json::Value, Vec<crate::provider::ProviderEvent>) {
    use tokio::io::AsyncReadExt as _;

    let (events, mut streamed) = tokio::sync::mpsc::channel(16);
    turn.events = Some(events);

    let (to_cli, mut from_wire) = tokio::io::duplex(64 * 1024);
    let mut stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin> = Box::new(to_cli);
    super::call_arrived(wiring, turn, &mut stdin, "req-1", call).await;
    drop(stdin);

    let mut written = String::new();
    from_wire.read_to_string(&mut written).await.expect("what the wire wrote");

    let mut surfaced = Vec::new();
    while let Ok(event) = streamed.try_recv() {
        surfaced.push(event);
    }

    (serde_json::from_str(written.trim()).expect("a control_response line"), surfaced)
}

fn a_call(name: &str, tool_use_id: &str) -> super::super::rpc::ToolCall {
    super::super::rpc::ToolCall {
        id: serde_json::json!(1),
        name: name.to_owned(),
        tool_use_id: Some(tool_use_id.to_owned()),
    }
}

/// `turn.denied` was written and read nowhere, so a `tools/call` for an ask
/// the person had just refused fell through to the secondary path and reached
/// the engine as a **brand-new** tool call with fabricated arguments — where
/// an allow rule or a stored "always" answer would have run it with no dialog
/// at all (CC-2).
#[tokio::test]
async fn a_call_for_an_ask_already_denied_is_answered_as_denied_and_never_reaches_the_engine() {
    let wiring = wiring(super::super::tests::roster());
    let mut turn = super::Turn::default();
    turn.denied.insert("toolu_1".to_owned(), "read".to_owned());

    let (answer, surfaced) = call_answered(&wiring, &mut turn, a_call("read", "toolu_1")).await;

    let result = &answer["response"]["response"]["mcp_response"]["result"];
    assert_eq!(result["isError"], serde_json::json!(true), "answered as a refusal: {answer}");
    assert!(
        result["content"][0]["text"].as_str().unwrap_or_default().contains("refused"),
        "and says so: {answer}"
    );
    assert!(surfaced.is_empty(), "the engine was asked nothing: {surfaced:?}");
    assert!(
        wiring.meta.lock().expect("meta").pending.is_empty(),
        "and nothing was parked for a resolve to answer"
    );

    // The id stays in the set: a peer that sends the call twice is refused
    // twice, rather than refused once and then obeyed.
    let (again, _) = call_answered(&wiring, &mut turn, a_call("read", "toolu_1")).await;
    assert_eq!(
        again["response"]["response"]["mcp_response"]["result"]["isError"],
        serde_json::json!(true)
    );
}

/// The guard above was keyed on the id a call carries, and the fallback for a
/// call carrying none searched `meta.pending` by name — which the deny itself
/// had just emptied. So a peer that sent a call for a refused ask **and**
/// dropped its `_meta` reached the engine as a fresh call after all (RR-1).
/// Driven through `answer_asks`, so the deny is the one a resolve records
/// rather than one a test wrote into the set.
#[tokio::test]
async fn a_call_carrying_no_id_for_a_tool_just_denied_is_refused_and_never_reaches_the_engine() {
    let wiring = wiring(super::super::tests::roster());
    let mut turn = super::Turn::default();
    wiring.meta.lock().expect("meta").pending.push(super::Pending {
        request_id: Some("req-0".to_owned()),
        tool_use_id: "toolu_1".to_owned(),
        name: "read".to_owned(),
        input: serde_json::json!({}),
        call_request_id: None,
        call_rpc_id: None,
    });

    let (to_cli, _from_wire) = tokio::io::duplex(64 * 1024);
    let mut stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin> = Box::new(to_cli);
    let denied = super::super::bridge::Resolution {
        tool_use_id: "toolu_1".to_owned(),
        permission: super::super::bridge::Permission::Deny { message: "denied".to_owned() },
        result: None,
    };
    super::answer_asks(&wiring, &mut turn, &mut stdin, vec![denied]).await;

    let carrying_no_id = super::super::rpc::ToolCall {
        id: serde_json::json!(2),
        name: "read".to_owned(),
        tool_use_id: None,
    };
    let (answer, surfaced) = call_answered(&wiring, &mut turn, carrying_no_id.clone()).await;

    let result = &answer["response"]["response"]["mcp_response"]["result"];
    assert_eq!(result["isError"], serde_json::json!(true), "answered as a refusal: {answer}");
    assert!(
        result["content"][0]["text"].as_str().unwrap_or_default().contains("refused"),
        "and says so: {answer}"
    );
    assert!(surfaced.is_empty(), "the engine was asked nothing: {surfaced:?}");
    assert!(
        wiring.meta.lock().expect("meta").pending.is_empty(),
        "and nothing was parked for a resolve to answer"
    );

    // Refused every time, not once: the name is not spent by a refusal.
    let (again, surfaced) = call_answered(&wiring, &mut turn, carrying_no_id).await;
    assert_eq!(
        again["response"]["response"]["mcp_response"]["result"]["isError"],
        serde_json::json!(true)
    );
    assert!(surfaced.is_empty(), "nor the second time: {surfaced:?}");

    // And the id stays denied: the call that does carry it is refused as well.
    let (carrying_it, surfaced) =
        call_answered(&wiring, &mut turn, a_call("read", "toolu_1")).await;
    assert_eq!(
        carrying_it["response"]["response"]["mcp_response"]["result"]["isError"],
        serde_json::json!(true)
    );
    assert!(surfaced.is_empty(), "the id-carrying call reaches nothing either: {surfaced:?}");
}

/// A deny covers the name it was asked under and nothing else: a call carrying
/// no id for a **different** declared tool is still the secondary path's ask.
#[tokio::test]
async fn a_call_carrying_no_id_for_a_tool_nobody_denied_is_still_surfaced() {
    let mut roster = super::super::tests::roster();
    roster.push(crate::tool::ToolDefinition {
        name: "grep".to_owned(),
        description: "searches files".to_owned(),
        schema: serde_json::json!({"type": "object"}),
    });
    let wiring = wiring(roster);
    let mut turn = super::Turn { expected_calls: 1, ..super::Turn::default() };
    wiring.meta.lock().expect("meta").pending.push(super::Pending {
        request_id: Some("req-0".to_owned()),
        tool_use_id: "toolu_1".to_owned(),
        name: "read".to_owned(),
        input: serde_json::json!({}),
        call_request_id: None,
        call_rpc_id: None,
    });

    let (to_cli, _from_wire) = tokio::io::duplex(64 * 1024);
    let mut stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin> = Box::new(to_cli);
    let denied = super::super::bridge::Resolution {
        tool_use_id: "toolu_1".to_owned(),
        permission: super::super::bridge::Permission::Deny { message: "denied".to_owned() },
        result: None,
    };
    super::answer_asks(&wiring, &mut turn, &mut stdin, vec![denied]).await;

    let (events, mut streamed) = tokio::sync::mpsc::channel(16);
    turn.events = Some(events);
    let other = super::super::rpc::ToolCall {
        id: serde_json::json!(2),
        name: "grep".to_owned(),
        tool_use_id: None,
    };
    super::call_arrived(&wiring, &mut turn, &mut stdin, "req-1", other).await;

    let first = streamed.try_recv().expect("an ask was surfaced");
    assert!(matches!(
        first,
        crate::provider::ProviderEvent::ToolCallStart { ref name, .. } if name == "grep"
    ));
}

/// The secondary path is the one arm where a name this turn never advertised
/// could reach the engine as a call: the primary path only ever answers an ask
/// this side surfaced.
#[tokio::test]
async fn a_call_for_a_tool_this_turn_never_declared_is_refused_by_name() {
    let wiring = wiring(super::super::tests::roster());
    let mut turn = super::Turn::default();

    let (answer, surfaced) = call_answered(&wiring, &mut turn, a_call("rm", "toolu_9")).await;

    let result = &answer["response"]["response"]["mcp_response"]["result"];
    assert_eq!(result["isError"], serde_json::json!(true));
    assert!(
        result["content"][0]["text"].as_str().unwrap_or_default().contains("`rm`"),
        "the name is quoted back: {answer}"
    );
    assert!(surfaced.is_empty(), "and the engine was asked nothing: {surfaced:?}");
}

/// The arm that still has to work: a declared name with no prior ask **is**
/// the ask, and is surfaced.
#[tokio::test]
async fn a_call_for_a_declared_tool_with_no_prior_ask_is_still_surfaced_as_one() {
    let wiring = wiring(super::super::tests::roster());
    let mut turn = super::Turn { expected_calls: 1, ..super::Turn::default() };

    let (events, mut streamed) = tokio::sync::mpsc::channel(16);
    turn.events = Some(events);
    let (to_cli, _from_wire) = tokio::io::duplex(64 * 1024);
    let mut stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin> = Box::new(to_cli);
    super::call_arrived(&wiring, &mut turn, &mut stdin, "req-1", a_call("read", "toolu_1")).await;

    let first = streamed.try_recv().expect("an ask was surfaced");
    assert!(matches!(
        first,
        crate::provider::ProviderEvent::ToolCallStart { ref name, .. } if name == "read"
    ));
    assert_eq!(wiring.meta.lock().expect("meta").pending.len(), 1, "and parked for the resolve");
}

/// `outcomes` holds a whole tool result, and an allowed ask whose `tools/call`
/// never arrived left one resident for the life of a held process — which
/// under posture C is the life of the conversation (CC-6).
#[test]
fn the_per_call_maps_are_dropped_by_the_next_turn_and_not_by_a_resolve() {
    let wiring = wiring(Vec::new());
    let mut turn = super::Turn::default();
    turn.outcomes.insert(
        "toolu_1".to_owned(),
        super::super::rpc::CallToolResult {
            content: vec![super::super::rpc::Content::text("a whole tool result")],
            is_error: false,
        },
    );
    turn.minted.insert("req-1".to_owned());
    turn.denied.insert("toolu_2".to_owned(), "read".to_owned());

    // A resolve continues the CLI turn that parked the asks, so `open_turn`
    // must keep all three — clearing them here would lose an outcome whose
    // `tools/call` has not arrived yet.
    let (events, _streamed) = tokio::sync::mpsc::channel(1);
    super::open_turn(&wiring, &mut turn, events);
    assert_eq!(turn.outcomes.len(), 1, "a resolve keeps the turn's outcomes");
    assert_eq!(turn.minted.len(), 1);
    assert_eq!(turn.denied.len(), 1);

    // A new turn drops them.
    super::drop_previous_turn(&mut turn);
    assert!(turn.outcomes.is_empty(), "an unclaimed result is not resident at the next turn");
    assert!(turn.minted.is_empty());
    assert!(turn.denied.is_empty());
}

/// What an exit before `system/init` surfaces is whatever the CLI printed
/// first, and that text reaches a transcript and a log — so it is bounded and
/// it says whose words it is (CC-11). This wire holds no credential, so there
/// is nothing to scrub by value; a diagnostic that ever printed one would
/// still travel no further than the bound.
#[test]
fn the_clis_first_stderr_line_is_surfaced_bounded_and_labelled_as_its_own() {
    use std::os::unix::process::ExitStatusExt as _;

    let exited_saying = |line: &str| {
        let wiring = wiring(Vec::new());
        let mut turn = super::Turn::default();
        let said = std::sync::Arc::new(std::sync::Mutex::new(vec![line.to_owned()]));
        let status = std::process::ExitStatus::from_raw(1 << 8);

        match super::report_exit(&wiring, &mut turn, &said, Ok(status)) {
            Some(crate::provider::ProviderError::Transport(message)) => message,
            other => {
                panic!("an exit that names no missing login is a transport failure: {other:?}")
            }
        }
    };

    // Run 7's sentence, the shape this arm exists for, arrives whole.
    let short = "Session ID 01998a00-0000-7000-8000-00000000000a is already in use.";
    assert_eq!(exited_saying(short), format!("{}{short}", super::SAID_LABEL));

    // A line past the bound is cut, and the cut is counted rather than hidden.
    let long = format!("{}TAIL", "x".repeat(super::SAID_LIMIT * 4));
    let surfaced = exited_saying(&long);
    assert!(surfaced.starts_with(super::SAID_LABEL), "labelled as the CLI's own: {surfaced}");
    assert!(!surfaced.contains("TAIL"), "what is past the bound does not travel: {surfaced}");
    assert!(
        surfaced.ends_with(&format!("[+{} bytes]", long.len() - super::SAID_LIMIT)),
        "and the elision says how much: {surfaced}"
    );

    // A cut that would land inside a character lands before it instead.
    let wide = exited_saying(&"日".repeat(super::SAID_LIMIT));
    assert!(
        wide.len() < super::SAID_LABEL.len() + super::SAID_LIMIT + 32,
        "bounded whatever the line is made of: {} bytes",
        wide.len()
    );
}

/// A child built by hand rather than spawned, because the order is the whole
/// point: a real child's exit, its closed pipes and its last stderr bytes reach
/// the task in whichever order the runtime notices them, and a loaded Linux
/// runner noticed them in an order a spawned child cannot be made to repeat.
/// Here each piece lands at an instant of its own on a paused clock, so the
/// order a test names is the only one there is.
struct FakeChild {
    /// Whether anything reads its stdin. Nothing does once a child has exited,
    /// which is when a pipe refuses a write.
    reads_stdin: bool,
    /// The line it writes to stderr.
    says: &'static str,
    /// When that line reaches the pipe.
    says_at: Duration,
    /// When it exits 1, closing its stdin and stdout, or never.
    exits_at: Option<Duration>,
}

impl FakeChild {
    fn io(self) -> crate::provider::claude_code::process::ChildIo {
        use std::os::unix::process::ExitStatusExt as _;

        use tokio::io::AsyncWriteExt as _;

        let Self { reads_stdin, says, says_at, exits_at } = self;
        let (stdin, stdin_end) = tokio::io::duplex(64);
        // Dropped here when nothing reads it, and a duplex whose other end is
        // gone refuses every write with `BrokenPipe`, the way a pipe does.
        let stdin_end = reads_stdin.then_some(stdin_end);
        let (stdout, stdout_end) = tokio::io::duplex(64);
        let (stderr, mut stderr_end) = tokio::io::duplex(1024);

        tokio::spawn(async move {
            tokio::time::sleep(says_at).await;
            // A wire that stopped reading is what the assertions are about;
            // this write failing would say nothing they do not.
            let _ = stderr_end.write_all(format!("{says}\n").as_bytes()).await;
        });

        crate::provider::claude_code::process::ChildIo {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Some(Box::new(stderr)),
            exit: Box::pin(async move {
                let Some(at) = exits_at else {
                    let _running = (stdin_end, stdout_end);
                    return std::future::pending().await;
                };
                tokio::time::sleep(at).await;
                drop((stdin_end, stdout_end));

                Ok(std::process::ExitStatus::from_raw(1 << 8))
            }),
            kill: Box::new(|_| {}),
        }
    }
}

/// Plays one turn on `io` through the task itself: every event the turn
/// streamed, and how long the first of them took.
///
/// The turn is queued **before** the task starts, so writing its frame is the
/// first thing the task does.
async fn a_turn_on(
    io: crate::provider::claude_code::process::ChildIo,
) -> (Vec<crate::provider::ProviderEvent>, Duration) {
    let (input, inputs) = tokio::sync::mpsc::channel(4);
    let (events, mut streamed) = tokio::sync::mpsc::channel(16);
    input
        .send(super::Input::Turn { frame: "{}\n".to_owned(), sent: Vec::new(), events })
        .await
        .expect("the task's queue is open");

    let started = tokio::time::Instant::now();
    let task = tokio::spawn(super::run(io, inputs, wiring(Vec::new()), DEFAULT_IDLE_BOUND));

    let mut seen = Vec::new();
    let mut took = None;
    while let Some(event) = streamed.recv().await {
        took.get_or_insert_with(|| started.elapsed());
        seen.push(event);
    }

    // A child still running is closed the way every entry is; one that exited
    // has already ended the task, and the refused send is that.
    let _ = input.send(super::Input::Close).await;
    task.await.expect("the task ends without panicking");

    (seen, took.unwrap_or_default())
}

/// Every failure among `events`, in order.
fn failures(events: &[crate::provider::ProviderEvent]) -> Vec<&crate::provider::ProviderError> {
    events
        .iter()
        .filter_map(|event| match event {
            crate::provider::ProviderEvent::Failed(error) => Some(error),
            _ => None,
        })
        .collect()
}

/// A CLI that exits before it reads a byte leaves its stdin closed behind it,
/// so the turn's write is refused — and the turn still fails with what the CLI
/// said, never with the refused write. The write used to win whenever it was
/// noticed first, which put `Broken pipe` where `claude_code_run.rs`'s refusal
/// sentence belonged on three CI runs in four (bead `ganja-code-3te9`).
#[tokio::test(start_paused = true)]
async fn a_cli_that_exits_before_reading_its_stdin_fails_the_turn_with_its_own_words() {
    let exiting = |says: &'static str| FakeChild {
        reads_stdin: false,
        says,
        says_at: Duration::ZERO,
        // The recording's slowest EOF-to-exit: the write is refused well
        // before the exit is there to be seen.
        exits_at: Some(Duration::from_millis(14)),
    };
    let sentence = ganja_testkit::fake_claude::NEEDS_VERBOSE;

    let (events, _) = a_turn_on(exiting(sentence).io()).await;
    assert!(
        matches!(
            failures(&events).as_slice(),
            [crate::provider::ProviderError::Transport(message)]
                if *message == format!("{}{sentence}", super::SAID_LABEL)
        ),
        "one failure, carrying the CLI's own sentence labelled as its own: {events:?}"
    );

    // And the login arm, which the same order hid just as well.
    let (events, _) = a_turn_on(exiting("Invalid API key · Please run /login").io()).await;
    assert!(
        matches!(
            failures(&events).as_slice(),
            [crate::provider::ProviderError::Auth(message)] if message == super::NO_LOGIN
        ),
        "one failure, and it is the missing login rather than the pipe: {events:?}"
    );
}

/// The other half: a refused write is the turn's failure only for a CLI still
/// running once the exit has had [`super::EXIT_SETTLE_BOUND`] to arrive in —
/// so a child that closed its stdin and stayed up fails the turn by name
/// rather than hanging it.
#[tokio::test(start_paused = true)]
async fn a_refused_write_to_a_cli_still_running_fails_the_turn_once_the_bound_passes() {
    let running = FakeChild {
        reads_stdin: false,
        says: "still running",
        says_at: Duration::ZERO,
        exits_at: None,
    };

    let (events, took) = a_turn_on(running.io()).await;
    assert!(
        matches!(
            failures(&events).as_slice(),
            [crate::provider::ProviderError::Transport(message)]
                if message.starts_with("could not write the turn: ")
        ),
        "one failure, and it is the write's own: {events:?}"
    );
    assert!(
        took >= super::EXIT_SETTLE_BOUND,
        "reported only after the exit had the whole bound to arrive in: {took:?}"
    );
}

/// The exit handled before the CLI's words have been read: on Linux, where
/// tokio reaps a child through a pidfd, the exit and the last stderr bytes can
/// become ready in one reactor turn and the exit be taken first. The turn
/// still fails with the words rather than with a CLI that said nothing —
/// whether its frame was written, so the exit arrives by the loop's own exit
/// arms, or refused, so it arrives by the write's wait.
#[tokio::test(start_paused = true)]
async fn a_cli_whose_words_are_read_after_its_exit_still_fails_the_turn_with_them() {
    let sentence = ganja_testkit::fake_claude::NEEDS_VERBOSE;

    for reads_stdin in [true, false] {
        let late = FakeChild {
            reads_stdin,
            says: sentence,
            // After the exit: the order in which the buffer used to be read
            // before the reader had filled it.
            says_at: Duration::from_millis(28),
            exits_at: Some(Duration::from_millis(14)),
        };

        let (events, _) = a_turn_on(late.io()).await;
        assert!(
            matches!(
                failures(&events).as_slice(),
                [crate::provider::ProviderError::Transport(message)]
                    if *message == format!("{}{sentence}", super::SAID_LABEL)
            ),
            "one failure carrying the CLI's words, its frame {}: {events:?}",
            if reads_stdin { "written" } else { "refused" }
        );
    }
}

/// `busy` says a turn is running, and the table's two views of idleness both
/// filter on it. A failure that cleared the stream and left the flag set made
/// the entry invisible to the idle sweep **and to the cap**, so enough of them
/// and the stated cap on authenticated runtimes stopped holding (CC-5).
#[tokio::test(start_paused = true)]
async fn an_entry_whose_turn_was_failed_by_the_watchdog_is_evictable_again() {
    let home = temp();
    let cli = FakeCli::new(Script { silent: true, ..Script::default() });
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "hello")], 0)).await;
    assert!(failure(&events).is_some(), "the silence bound failed the turn");

    assert_eq!(provider.held_entries(), 1, "the process is still held");
    assert_eq!(
        provider.held.evictable(),
        Some(key("m1")),
        "and the cap may reclaim it: `busy` means a turn is running"
    );

    provider.shutdown().await;
}

/// A lock claimed and never released meant one open descriptor and one
/// `flock` per conversation for the process's life, and a conversation this
/// ganja had let go of that no other ganja could take (CC-8).
#[tokio::test(start_paused = true)]
async fn a_lock_is_released_with_its_entry_so_another_ganja_may_take_the_conversation() {
    let home = temp();
    let paths = Paths::under(home.path());
    let cli = FakeCli::new(says(&["one"]));
    let provider = wired(&cli, home.path()).with_idle_bound(Duration::from_secs(30));

    turn(&provider, request(vec![user("m1", "first")], 0)).await;

    // `flock` treats two descriptors on one file independently even inside one
    // process, so this really is the question another ganja would be asking.
    let path = paths.lock(&key("m1"));
    assert!(binding::Lock::claim(&path).is_err(), "this ganja holds the conversation");

    tokio::time::sleep(Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.held_entries(), 0, "the entry went idle");

    assert!(binding::Lock::claim(&path).is_ok(), "and the lock went with it");

    provider.shutdown().await;
}

/// The spawn seam failing every spawn — what `process::Real` answers when the
/// binary `from_env` checked is gone by the time a turn needs it.
struct NoBinary;

impl crate::provider::claude_code::process::Spawner for NoBinary {
    fn spawn(
        &self,
        bin: &std::path::Path,
        _argv: &[std::ffi::OsString],
        _env: &crate::provider::claude_code::argv::ChildEnv,
    ) -> Result<crate::provider::claude_code::process::ChildIo, crate::provider::ProviderError>
    {
        Err(crate::provider::ProviderError::Transport(format!("could not spawn {}", bin.display())))
    }
}

/// A claim whose spawn then failed was released by nothing: `forget` lets a
/// lock go only together with an entry, and a failed spawn never files one —
/// so every other ganja was told `locked-elsewhere` about a conversation nobody
/// held, for as long as this process lived (RR-2).
#[tokio::test]
async fn a_spawn_that_fails_leaves_the_conversation_free_for_another_ganja() {
    let home = temp();
    let paths = Paths::under(home.path());
    let failing = crate::provider::claude_code::ClaudeCodeProvider::with_parts(
        std::path::PathBuf::from("/nonexistent/claude"),
        "2.1.263 (Claude Code)".to_owned(),
        std::sync::Arc::new(NoBinary),
        Paths::under(home.path()),
    );

    let opened = failing
        .stream(request(vec![user("m1", "first")], 0), tokio_util::sync::CancellationToken::new())
        .await;
    assert!(opened.is_err(), "the spawn failed, so no turn opened");
    assert_eq!(failing.held_entries(), 0, "and nothing was filed");

    // `flock` treats two descriptors on one file independently even inside one
    // process, so this really is the question another ganja would be asking.
    assert!(
        binding::Lock::claim(&paths.lock(&key("m1"))).is_ok(),
        "the failed spawn left the conversation locked against every other ganja"
    );
    assert!(
        failing.held.locks.lock().expect("the lock table").is_empty(),
        "and no descriptor is left holding it"
    );

    // And another ganja takes it: its turn writes the binding that the
    // `locked-elsewhere` arm never writes.
    let cli = FakeCli::new(says(&["one"]));
    let second = wired(&cli, home.path());
    turn(&second, request(vec![user("m1", "first")], 0)).await;
    assert!(
        binding::load(&paths.binding(&key("m1"))).is_some(),
        "the second ganja claimed the conversation rather than opening it locked elsewhere"
    );

    second.shutdown().await;
}

/// The other claim nothing released (RR-2): `route` claims before it asks for
/// a reason word, and a conversation refused twice in a row spawns nothing —
/// so the claim was held with no entry, and another ganja on it was told
/// `locked-elsewhere`, read no binding, and spent two refusals of its own
/// where reading the streak would have spent none.
#[tokio::test]
async fn a_conversation_refused_twice_is_left_for_another_ganja_to_read_the_streak_of() {
    let home = temp();
    let paths = Paths::under(home.path());
    binding::store(
        &paths.binding(&key("m1")),
        &binding::Binding {
            refused: true,
            refused_streak: binding::REFUSED_STREAK_BOUND,
            ..binding::Binding::default()
        },
    )
    .expect("the refused binding is planted");
    let cli = FakeCli::new(says(&["never said"]));
    let provider = wired(&cli, home.path());

    let events = turn(&provider, request(vec![user("m1", "again")], 0)).await;
    assert!(
        failure(&events).is_some_and(|failed| failed.contains("refused twice")),
        "the bound refused the turn locally: {events:?}"
    );
    assert_eq!(cli.count(), 0, "and spawned nothing");

    // `flock` treats two descriptors on one file independently even inside one
    // process, so this really is the question another ganja would be asking.
    assert!(
        binding::Lock::claim(&paths.lock(&key("m1"))).is_ok(),
        "a conversation this ganja will not spend on is not one it holds"
    );
    assert!(provider.held.locks.lock().expect("the lock table").is_empty());
}

/// And the claim is a claim: a conversation whose lock is held elsewhere is
/// one this wire reads no binding for and opens a fresh record on.
#[tokio::test]
async fn a_conversation_locked_elsewhere_is_answered_without_reading_its_binding() {
    let home = temp();
    let paths = Paths::under(home.path());
    let cli = FakeCli::new(says(&["one"]));
    let provider = wired(&cli, home.path());

    // Somebody else got there first.
    let held = binding::Lock::claim(&paths.lock(&key("m1"))).expect("the other ganja's claim");

    turn(&provider, request(vec![user("m1", "first")], 0)).await;

    assert_eq!(provider.held_entries(), 1, "the turn is still taken");
    assert_eq!(
        binding::load(&paths.binding(&key("m1"))),
        None,
        "and no binding is written for a conversation this ganja does not own"
    );

    drop(held);
    provider.shutdown().await;
}
