//! What a real `agy` does under the posture this build composes (**Dv-7**'s
//! ship probe, **AC-15**, **AC-27**, **AC-29**).
//!
//! Every other agy assertion in this landing is checked against a shell script
//! that answers in the shapes a probed binary printed. That proves the driver
//! and proves nothing about the vendor. Three questions can only be answered by
//! a real one, and this file is what answered them:
//!
//! 1. **Does the resident wire work at all?** One child, one NDJSON line per
//!    turn, read until `result` — the shape agy's own `--input-format
//!    stream-json` promises. Everything about this backend rests on it.
//! 2. **Is the grant what Dv-7 says it is?** W4 measured `--sandbox` as a bound
//!    on agy's terminal and not on its filesystem; the sentence a person
//!    approves says agy may write anywhere they can. So the ship evidence is a
//!    **write that worked** — the opposite of the codex and grok probes, whose
//!    recordings are of writes being refused.
//! 3. **What does a turn cost, and does anything stop to ask?** The deadline is
//!    ordered against the vendor's own `--print-timeout`, so a real turn's
//!    wall-clock is what says the ordering is not merely arithmetic.
//!
//! So this file is `#[ignore]`d **and** inert unless `GANJA_LIVE_TEST=1`, the
//! two-lock shape `tests/live.rs`, `teammate_codex_live.rs` and
//! `teammate_grok_live.rs` already use for a surface that costs somebody's
//! quota.
//!
//! ```sh
//! GANJA_LIVE_TEST=1 cargo test -p ganja-core --test teammate_agy_live -- --ignored --nocapture
//! ```
//!
//! # One conversation, two turns, one test
//!
//! Splitting the questions would mean spending a second conversation to
//! re-measure the same child. Question 1 *is* the second turn — a per-message
//! CLI would have started a new process for it — so the turns are shared and
//! each question is asserted separately below.
//!
//! # The confound this file records rather than hides
//!
//! agy's Tool Execution Policy is a **setting**, not a flag: its four values
//! are `always-proceed`, `request-review`, `strict` and `proceed-in-sandbox`,
//! and the `init` record reports which one is in force. The machine this was
//! written on is set to `always-proceed`, so "no approval prompt appeared" is
//! measured *for that configuration* and is not a claim about a machine set to
//! `strict`. The test asserts what it saw and prints the policy, so a run on
//! another machine reports rather than silently measuring something else —
//! this is the grok symlinked-`GROK_HOME` class of finding.

mod shim_support;

use std::time::Duration;

use ganja_core::teammate::SpawnSpec;
use ganja_core::teammate::agy::Agy;
use ganja_core::teammate::shim::{self, Driver as _, Turn};
use ganja_protocol::team::MemberBackend;
use ganja_team::{MemberName, TeamName, TeamsRoot};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

/// How long one real agy turn gets before this test gives up on it.
///
/// Below the shipped deadline: a test that waited four minutes to fail would
/// be a test nobody runs. The probe's own turns took 54.0s and 20.8s.
const TURN: Duration = Duration::from_secs(180);

/// The recording this run produced, and which **AC-27** compares against.
const PROBE: &str = include_str!("fixtures/agy-posture-probe.txt");

/// Whether the two locks are both open.
fn enabled() -> bool {
    std::env::var("GANJA_LIVE_TEST").is_ok_and(|value| !value.is_empty())
}

/// A spawn spec pointed at `cwd`, which is the directory the write must land
/// in.
fn spec(cwd: &std::path::Path) -> SpawnSpec {
    SpawnSpec {
        name: MemberName::parse("w1").expect("a member name"),
        team: TeamName::parse("live").expect("a team name"),
        lead: MemberName::lead(),
        root: TeamsRoot::new(cwd.join("teams")),
        backend: MemberBackend::Agy,
        agent_type: "general".to_owned(),
        model: "live".to_owned(),
        color: "blue".to_owned(),
        prompt: String::new(),
        cwd: cwd.to_path_buf(),
        plan_mode_required: false,
        parent_session_id: "live-probe".to_owned(),
    }
}

/// One NDJSON line, as the shipped driver encodes it.
fn line(spec: &SpawnSpec, text: &str) -> String {
    Agy.line(&Turn { spec, text, prompt: None, session: None, deadline: TURN })
        .expect("a turn encodes")
}

/// One turn: read until the shipped driver says it is over.
///
/// Also picks the reported Tool Execution Policy off the `init` record on the
/// way past, because that is the confound this probe has to report rather than
/// quietly measure around.
async fn take(
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    label: &'static str,
) -> (shim::Reply, Duration, Option<String>) {
    let started = std::time::Instant::now();
    let mut policy = None;
    loop {
        let next = tokio::time::timeout(TURN, lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("{label} finished inside this test's own patience"))
            .expect("the child's output is readable");
        let Some(text) = next else {
            panic!("{label}: the child closed its output before the turn was over");
        };
        if let Some(at) = text.find("\"permission_mode\":\"") {
            let rest = &text[at + 19..];
            policy = rest.find('"').map(|end| rest[..end].to_owned());
        }
        match Agy.read(&text) {
            shim::Read::Ignored => {}
            shim::Read::Refused(reason) => panic!("{label}: {reason}"),
            shim::Read::Done(reply) => {
                eprintln!("{label}: {:.1}s {reply:?}", started.elapsed().as_secs_f64());

                return (reply, started.elapsed(), policy);
            }
        }
    }
}

/// **The ship probe.** One resident child, two turns, a write and a read.
#[tokio::test]
#[ignore = "spends somebody's agy quota; set GANJA_LIVE_TEST=1"]
async fn a_real_agy_teammate_takes_two_turns_on_one_child_and_writes_where_it_was_told() {
    if !enabled() {
        eprintln!("GANJA_LIVE_TEST is not set, so this probe is inert");

        return;
    }

    let home = ganja_testkit::temp_dir();
    let cwd = home.path().join("scratch");
    std::fs::create_dir_all(&cwd).expect("a working directory");
    std::fs::write(cwd.join("read-me.txt"), "READ-ME-FIRST-LINE-7f3a\nsecond\n")
        .expect("something to read");
    let spec = spec(&cwd);
    let launch =
        shim::prepare(&Agy::new(), &spec, None).expect("this machine has an agy on its PATH");

    // The shipped launch line, composed by the shipped driver — not a line
    // this test wrote, which is the whole point of driving the real one.
    let argv =
        Agy.argv(&Turn { spec: &spec, text: "", prompt: None, session: None, deadline: TURN });
    let rendered: Vec<String> =
        argv.iter().map(|token| token.to_string_lossy().into_owned()).collect();
    eprintln!("argv: {}", rendered.join(" "));

    let mut child = launch
        .command(&argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("agy starts");
    let mut stdin = child.stdin.take().expect("its stdin");
    let stdout = child.stdout.take().expect("its stdout");
    let mut lines = tokio::io::BufReader::new(stdout).lines();

    let ask = "Do exactly these three things in the current working directory, then stop. (1) Use \
               your write_to_file tool to create a file named ship-probe.txt in the current \
               working directory containing exactly the single line AGY-SHIP-PROBE-OK. (2) Use \
               your view_file tool to read the file read-me.txt in the current working directory. \
               (3) Reply with the first line of read-me.txt and nothing else.";
    stdin
        .write_all(format!("{}\n", line(&spec, ask)).as_bytes())
        .await
        .expect("the first turn is written");
    stdin.flush().await.expect("and flushed");
    let (first, first_took, policy) = take(&mut lines, "turn 1").await;

    // Question 2: the grant — and the one assertion this probe must **not**
    // make. W4 recorded that agy's own argument validator sometimes refuses a
    // write to an absolute path outside its scratch directory ("is not a valid
    // artifact path") and sometimes does not: it wrote in 2 of 2 runs of the
    // shipped flag set and was refused in others, from the same prompt. The
    // first run of this very probe wrote the file; a later one was refused by
    // that validator with the read in the same turn still answered.
    //
    // That nondeterminism is the whole reason the posture sentence says what
    // it says: **self-restraint that does not always hold is not a bound**, so
    // a person consenting to an agy teammate is consenting to one that may
    // write anywhere they can. Asserting the write lands would therefore ship
    // a flaky test *and* assert the opposite of what this backend claims — so
    // the outcome is recorded, and only the shape of a refusal is asserted.
    let written = cwd.join("ship-probe.txt");
    let landed = written.is_file();
    eprintln!("write landed: {landed}");
    if landed {
        assert!(
            std::fs::read_to_string(&written)
                .expect("the file it wrote")
                .contains("AGY-SHIP-PROBE-OK")
        );
    } else {
        let why = first.refused.as_deref().expect("a write that did not land was refused out loud");
        assert!(
            why.contains("artifact path"),
            "the only thing that ever stops this write is agy's own argument \
             validator; a sandbox denial here would be a different vendor: {why}"
        );
    }
    // Either way the turn is *reported*, never silent, and whatever it managed
    // to say still travels — which is `Reply`'s words-and-reason rule holding
    // against a real vendor rather than against a script.
    assert!(
        !first.messages.is_empty() || first.refused.is_some(),
        "a turn says something or says why not"
    );
    let conversation = first.session.clone().expect("the turn named its conversation");

    // Question 1: the same child takes another turn. A per-message CLI would
    // need a second process for this, and the conversation id would change.
    stdin
        .write_all(
            format!("{}\n", line(&spec, "Reply with exactly the word SECOND and nothing else."))
                .as_bytes(),
        )
        .await
        .expect("the second turn is written");
    stdin.flush().await.expect("and flushed");
    let (second, second_took, _) = take(&mut lines, "turn 2").await;

    assert_eq!(
        second.session.as_deref(),
        Some(conversation.as_str()),
        "one child, one conversation"
    );
    assert!(
        !second.messages.is_empty() || second.refused.is_some(),
        "and it is still answering after whatever the first turn did"
    );

    // Question 3: what a turn costs, against the deadline that bounds it. The
    // shipped `--print-timeout` is derived from that deadline and must outlast
    // it, which is the ordering AC-29 is about.
    let longest = first_took.max(second_took);
    eprintln!(
        "wall-clock: turn 1 {:.1}s, turn 2 {:.1}s, policy {:?}",
        first_took.as_secs_f64(),
        second_took.as_secs_f64(),
        policy
    );
    assert!(
        longest < shim::AGY_TURN_TIMEOUT,
        "a real turn finishes well inside the shipped deadline: {longest:?}"
    );
    let composed = ganja_core::teammate::agy::print_timeout(shim::AGY_TURN_TIMEOUT);
    assert_eq!(composed, "300s");

    // The confound, printed rather than asserted away: the policy this ran
    // under is a setting of the machine's, and "nothing asked" is measured for
    // that setting only.
    assert_eq!(
        policy.as_deref(),
        Some("always-proceed"),
        "this probe's recording was made under always-proceed; a run under another policy is \
         measuring something else and should say so"
    );

    // The recording this run produced still describes it.
    assert!(PROBE.contains("always-proceed"));
    assert!(PROBE.contains("sentence: "));

    drop(stdin);
    let _ = child.wait().await;
}
