//! What a real `grok` does under the posture this build composes (**W5**'s
//! gating probe, **AC-15**, **AC-27**, **AC-29**).
//!
//! Every other grok assertion in this landing is checked against a shell script
//! that answers in the shapes a probed binary printed. That proves the driver
//! and proves nothing about the vendor. The three questions here can only be
//! answered by a real one:
//!
//! 1. **Gating — can a grok teammate work at all under the pinned posture, and
//!    what does an unapproved tool ask cost?** The ordinary path for a
//!    read-and-answer teammate is reading, so if reads raise asks then a grok
//!    teammate cancels essentially every turn and the row ships as
//!    not-recommended.
//! 2. **Does `--resume <uuid>` compose with `--prompt-file`?** Both are
//!    documented single-turn doors; their combination is not.
//! 3. **Drift.** Is the sandbox still applied on the resume line, and does
//!    `--permission-mode dontAsk` still take the cancel arm rather than having
//!    been wired into the permission engine at 1.0.6?
//!
//! So this file is `#[ignore]`d **and** inert unless `GANJA_LIVE_TEST=1`, the
//! two-lock shape `tests/live.rs` and `teammate_codex_live.rs` already use for
//! a surface that costs somebody's quota.
//!
//! ```sh
//! GANJA_LIVE_TEST=1 cargo test -p ganja-core --test teammate_grok_live -- --ignored --nocapture
//! ```
//!
//! # Why the three questions are one ladder rather than three tests
//!
//! Questions 2 and 3 are questions *about the resume line*, and question 1's
//! own instrument is a resume: (a) reads, and (b) and (c) resume (a)'s
//! conversation. Splitting them into three test functions would mean spending
//! two more conversations to re-measure the same line — somebody's quota spent
//! to re-prove a fact this ladder already recorded. Each question is asserted
//! separately and labelled below; what is shared is the turns.
//!
//! # What this file does *not* drive through
//!
//! The gating turns compose their argv with the shipped
//! [`Grok::argv`](ganja_core::teammate::grok::Grok) and run it through the
//! shipped [`shim::Launch`], but they read the child's **stdout directly**
//! rather than going through the mailbox. That is not a shortcut: the guard
//! this probe carries — *a recorded answer only if the stream shows a read tool
//! call reaching terminal status* — is a claim about the stream, and the
//! mailbox is exactly the seam that turns a stream into one sentence. The last
//! test in this file drives the whole chain instead, so both are witnessed.
//!
//! # The precondition this machine failed
//!
//! `--sandbox read-only` installs a write-deny hook that **refuses a symlinked
//! `GROK_HOME`**, and refuses to start rather than run with its protections
//! missing. The machine this was written on has `~/.grok` symlinked, so every
//! grok turn there refuses — correctly, and as
//! [`shim::Failure::Exit`](ganja_core::teammate::shim::Failure) mail naming the
//! vendor's own sentence. The probe is therefore run with a `HOME` whose
//! `.grok` is a real directory, and the tests below say so rather than
//! reporting a vendor refusal as a measurement of anything else. `HOME` is a
//! variable the shim already carries, so nothing about the shipped enumeration
//! is bent to make this run.

mod shim_support;

use std::{path::PathBuf, sync::Arc, time::Duration};

use ganja_core::teammate::{
    SpawnSpec,
    grok::Grok,
    shim::{self, Driver as _, Prompt, Turn},
};
use ganja_protocol::team::MemberBackend;
use ganja_team::{MailboxMessage, MemberName, TeamName, TeamsRoot, mailbox, record};
use ganja_testkit::AllowSpawn;
use shim_support::until;

/// How long one real grok turn gets before this test gives up on it.
///
/// Far below the shipped deadline: a test that waited fifteen minutes to fail
/// would be a test nobody runs.
const TURN: Duration = Duration::from_secs(300);

/// Whether the two locks are both open.
fn enabled() -> bool {
    std::env::var("GANJA_LIVE_TEST").is_ok_and(|value| !value.is_empty())
}

/// Whether this machine can apply the pinned profile at all.
///
/// A symlinked grok home makes `--sandbox read-only` refuse to start, so a run
/// under one would measure that refusal and nothing else.
fn home_is_real() -> bool {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let grok = home.join(".grok");

    grok.is_dir() && !grok.symlink_metadata().is_ok_and(|meta| meta.is_symlink())
}

/// A spawn against `cwd`, as the shipped composition needs one.
fn spec(cwd: &std::path::Path) -> SpawnSpec {
    SpawnSpec {
        name: MemberName::parse("w1").expect("a member name"),
        team: TeamName::default_team(),
        lead: MemberName::lead(),
        root: TeamsRoot::new(cwd.join("teams")),
        backend: MemberBackend::Grok,
        agent_type: "general".to_owned(),
        model: "whatever-the-person-configured".to_owned(),
        color: "blue".to_owned(),
        prompt: String::new(),
        cwd: cwd.to_path_buf(),
        plan_mode_required: false,
        bypass: false,
        parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
    }
}

/// What one probe turn produced.
struct Ran {
    /// The argv it was launched with, so the recording names what was measured.
    argv: Vec<String>,
    stdout: String,
    stderr: String,
    code: Option<i32>,
    took: Duration,
}

impl Ran {
    /// Every tool the stream named, in the order it named them.
    fn tools(&self) -> Vec<String> {
        self.blocks("tool_use", "name")
    }

    /// Every tool that reached a **terminal** status.
    ///
    /// On the Messages wire a call reaches terminal status by its result being
    /// written back as a `tool_result` block, which is what that vendor's own
    /// reducer flushes on `Completed | Failed`. A `tool_use` with no matching
    /// `tool_result` is a call that was open when the turn died.
    fn settled(&self) -> Vec<String> {
        self.blocks("tool_result", "tool_use_id")
    }

    /// The `field` of every content block of kind `kind`, anywhere in the
    /// stream.
    fn blocks(&self, kind: &str, field: &str) -> Vec<String> {
        let mut found = Vec::new();
        for line in self.stdout.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            collect(&value, kind, field, &mut found);
        }

        found
    }

    /// What the terminal `result` record said, if one arrived at all — which is
    /// itself one of the two observations this probe was asked to record.
    fn result(&self) -> Option<serde_json::Value> {
        self.stdout.lines().rev().find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            (value.get("type").and_then(serde_json::Value::as_str) == Some("result"))
                .then_some(value)
        })
    }

    /// Why the terminal `result` said the turn stopped.
    fn stop_reason(&self) -> Option<String> {
        self.result()?
            .get("stop_reason")?
            .as_str()
            .map(str::to_owned)
    }

    /// Everything worth having in the recording, printed rather than asserted.
    fn record(&self, label: &str) {
        eprintln!("--- probe {label} ---");
        eprintln!("argv:   {}", self.argv.join(" "));
        eprintln!(
            "exit:   {:?} after {:.1}s",
            self.code,
            self.took.as_secs_f64()
        );
        eprintln!("tools:  {:?} (settled: {:?})", self.tools(), self.settled());
        eprintln!(
            "result: {}",
            self.result().map_or_else(
                || "(no result record arrived)".to_owned(),
                |value| value.to_string()
            )
        );
        if !self.stderr.trim().is_empty() {
            eprintln!("stderr: {}", self.stderr.trim());
        }
    }
}

/// Every `field` of every block whose `type` is `kind`, at any depth.
fn collect(value: &serde_json::Value, kind: &str, field: &str, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("type").and_then(serde_json::Value::as_str) == Some(kind)
                && let Some(named) = map.get(field).and_then(serde_json::Value::as_str)
            {
                found.push(named.to_owned());
            }
            for nested in map.values() {
                collect(nested, kind, field, found);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                collect(nested, kind, field, found);
            }
        }
        _ => {}
    }
}

/// One turn, composed by the shipped driver and run through the shipped launch.
async fn run(cwd: &std::path::Path, text: &str, session: Option<&str>) -> Ran {
    let spec = spec(cwd);
    let launch =
        shim::prepare(&Grok::new(), &spec, None).expect("this machine has a grok on its PATH");
    let prompt = Prompt::write(&launch.tmp, text).expect("a 0600 prompt file");
    let argv = Grok.argv(&Turn {
        spec: &spec,
        text,
        prompt: Some(prompt.path()),
        session,
    });
    let started = std::time::Instant::now();
    let output = tokio::time::timeout(TURN, launch.command(&argv).output())
        .await
        .expect("the turn finished inside this test's own patience")
        .expect("the child ran");

    Ran {
        argv: argv
            .iter()
            .map(|token| token.to_string_lossy().into_owned())
            .collect(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
        took: started.elapsed(),
    }
}

/// The gating ladder: viability, the cost of an unapproved ask, the resume
/// composition, and the two drift questions.
#[tokio::test]
#[ignore = "spends somebody's grok quota; needs GANJA_LIVE_TEST=1"]
async fn what_a_read_only_grok_teammate_can_do_and_what_an_unapproved_ask_costs() {
    if !enabled() {
        eprintln!("GANJA_LIVE_TEST is not set; this test is inert");

        return;
    }
    assert!(
        home_is_real(),
        "$HOME/.grok is a symlink (or absent), and `--sandbox read-only` refuses to start under \
         one — run this with a HOME whose .grok is a real directory, or the ladder measures that \
         refusal rather than anything about a turn"
    );

    let work = ganja_testkit::temp_dir();
    std::fs::write(
        work.path().join("NOTES.txt"),
        "The probe workspace holds exactly three facts.\nOne: it is a workspace.\nTwo: it is \
         temporary.\nThree: it has three facts.\n",
    )
    .expect("a file to have been read");

    // ---- (a) Viability, pure read. The ship/no-ship instrument. -------------
    //
    // The only stimulus that answers "does the ordinary read path raise an
    // ask", which is the question the grok row actually ships behind. Its
    // decisive outcome is a *completion*, so it carries the same
    // did-the-mechanism-run guard the abort arms carry: a turn that completes
    // without calling a read tool at all would otherwise ship the optimistic
    // sentence on an observation produced without the mechanism.
    let read = run(
        work.path(),
        "Summarize NOTES.txt in your current working directory. Read it first.",
        None,
    )
    .await;
    read.record("(a) pure read");
    let session = read
        .stdout
        .lines()
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            (value.get("subtype").and_then(serde_json::Value::as_str) == Some("init"))
                .then(|| value.get("session_id")?.as_str().map(str::to_owned))?
        })
        .expect("the child named the session it was running");
    assert!(
        !read.tools().is_empty(),
        "(a) is a recorded answer only if the mechanism ran: no tool was called at all, so this \
         is a probe failure to re-run rather than evidence that reads raise no ask"
    );
    assert!(
        !read.settled().is_empty(),
        "and only if a call reached terminal status: {:?} were opened and none settled",
        read.tools()
    );

    // ---- (b) The write turn, on the resume line. ---------------------------
    //
    // **This is assertion 2's instrument as well**: `--resume <minted uuid>`
    // beside `--prompt-file` is a composition neither door's own documentation
    // covers, and a turn that answers on it is what says the two compose.
    //
    // The cwd is inside the profile's writable temp set — that profile denies
    // writes *except* under `~/.grok` and temp — so the kernel is deliberately
    // **not** the thing that would stop this write. Whatever stops it is the
    // permission layer, which is what makes this a measurement of the ask
    // rather than of the sandbox.
    let target = work.path().join("PROBE_WROTE.txt");
    let write = run(
        work.path(),
        "Create a file named PROBE_WROTE.txt in your current working directory containing the \
         single word WROTE. Then reply with exactly WROTE if you created it, or exactly REFUSED \
         if you could not.",
        Some(&session),
    )
    .await;
    write.record("(b) write on the resume line");
    assert!(
        write.code == Some(0) || !write.stdout.trim().is_empty(),
        "assertion 2: `--resume <uuid> --prompt-file <file>` composes as a turn — this one \
         produced nothing at all: {}",
        write.stderr
    );
    assert!(
        !write.tools().is_empty(),
        "a turn that stopped with no tool identified is not a recorded answer: {}",
        write.stdout
    );
    // **Assertion 3b, and it asserts rather than records.** This is the drift
    // check on a known answer: at the probed version an unapproved tool ask is
    // answered `Cancelled`, which *ends* the turn. The failure this catches is
    // a later version wiring `dontAsk` into the permission engine, where the
    // documented intent is to silently **deny** and let the turn continue — a
    // turn that then completes, writes nothing, and looks like success. Without
    // this equality that version passes here in silence.
    //
    // A failure is a **finding to investigate, not a flake to re-run**: the
    // shipped posture sentence, the cancel mail and D508(a)'s own paragraph all
    // say "an unapproved ask ends the turn", and each of them is wrong the day
    // this line fails.
    assert_eq!(
        write.stop_reason().as_deref(),
        Some("cancelled"),
        "the write turn did not end as a cancel — if this version denies-and-continues instead, \
         the posture row, the cancel mail and D508(a) all need re-deciding: {}",
        write.stdout
    );

    // ---- (c) The bash-shaped turn. -----------------------------------------
    //
    // Separates "the sandbox refused at the kernel" from "the permission layer
    // raised an ask": a shell command is sandbox-permitted but the auto-allow
    // for bash is off by default, so an ask is what should stop it.
    let bash = run(
        work.path(),
        "Run the shell command `echo probe-ran` and reply with exactly what it printed.",
        Some(&session),
    )
    .await;
    bash.record("(c) bash-shaped on the resume line");
    // The same drift assertion on the arm that tells a kernel refusal from a
    // permission ask: a bash tool is sandbox-permitted, so a cancel here is the
    // permission layer and nothing else.
    assert_eq!(
        bash.stop_reason().as_deref(),
        Some("cancelled"),
        "the shell turn did not end as a cancel, so the cancel measured on the write turn was \
         not the permission layer after all: {}",
        bash.stdout
    );

    // ---- Assertion 3a, drift: is the sandbox still read on a resume? -------
    //
    // Turn-free, and it is the only honest instrument for this: a refusal that
    // never fires and a flag that is ignored look identical from outside, so
    // the thing to measure is the refusal. That vendor records the profile a
    // conversation started under and refuses a resume asking for a *different*
    // one — which can only happen if the resume line's `--sandbox` is read.
    let spec = spec(work.path());
    let launch = shim::prepare(&Grok::new(), &spec, None).expect("a grok on PATH");
    let conflicting = launch
        .command(&[
            "--resume".into(),
            session.clone().into(),
            "--prompt-file".into(),
            "/nonexistent/never-read.txt".into(),
            "--sandbox".into(),
            "strict".into(),
        ])
        .output()
        .await
        .expect("the child ran");
    let refusal = String::from_utf8_lossy(&conflicting.stderr).into_owned();
    eprintln!("--- probe (3a) conflicting profile on resume ---\n{refusal}");
    // Asserted, for the reason the paragraph above gives: a refusal that never
    // fires and a flag that is ignored are indistinguishable, so *recording*
    // this one proves nothing a silent pass would not also prove. If a later
    // version stops refusing, the claim "the resume line's `--sandbox` is read"
    // — which is the whole basis for pinning the posture on every turn rather
    // than only the first — has to be re-measured another way.
    assert!(
        refusal.contains("cannot resume this session under sandbox profile"),
        "the vendor's own conflict refusal is what guarantees the posture on a resume, and it \
         did not fire: {refusal}"
    );

    // ---- What the deadline is derived from. --------------------------------
    let longest = [read.took, write.took, bash.took]
        .into_iter()
        .max()
        .expect("three turns");
    eprintln!(
        "grok probe wall-clock: (a) {:.1}s, (b) {:.1}s, (c) {:.1}s; twice the longest is {:.1}s, \
         so the shipped deadline is max(15m, that)",
        read.took.as_secs_f64(),
        write.took.as_secs_f64(),
        bash.took.as_secs_f64(),
        2.0 * longest.as_secs_f64(),
    );

    // ---- The decided consequence. ------------------------------------------
    //
    // Asserted last because it is a statement about all three turns together:
    // the write must not have happened, whichever of the pre-decided shapes
    // the ladder turned out to be.
    assert!(
        !target.exists(),
        "the write turn created a file, so neither the sandbox nor the permission layer stopped \
         it — the posture row cannot ship as written"
    );
}

/// The same posture through the **whole chain**: a real grok teammate takes a
/// message off its mailbox and answers into the lead's.
///
/// Non-gating, and cheap on purpose — one trivial turn. What it witnesses is
/// the seam the ladder above deliberately steps around: that the shipped
/// runner, the shipped parser and a real vendor agree well enough for a
/// sentence to travel from a lead's message to a lead's inbox.
#[tokio::test]
#[ignore = "spends somebody's grok quota; needs GANJA_LIVE_TEST=1"]
async fn a_real_grok_teammate_answers_its_lead_through_the_mailbox() {
    if !enabled() {
        eprintln!("GANJA_LIVE_TEST is not set; this test is inert");

        return;
    }
    assert!(home_is_real(), "see the ladder's own precondition");

    let home = ganja_testkit::temp_dir();
    let work = ganja_testkit::temp_dir();
    let (registry, door) = shim_support::lead(
        home.path(),
        work.path(),
        Arc::new(Grok::new()),
        // Production's own answer, spelled explicitly because the fixture's
        // constructor takes one: the real `grok`, wherever `PATH` finds it.
        std::env::var_os("PATH").expect("a PATH"),
    );
    let (root, team) = shim_support::team_of(&registry);

    door.start(
        ganja_testkit::spawn_with_prompt(
            "w1",
            Some("grok"),
            "Reply with exactly: HELLO. Do not do anything else.",
        ),
        &ganja_testkit::caller(work.path()),
        &AllowSpawn,
    )
    .await
    .expect("a real grok spawns");

    let inbox = root.inbox_path(&team, &MemberName::lead());
    let mail = || {
        mailbox::read(&inbox)
            .map(|contents| {
                contents
                    .valid
                    .into_iter()
                    .map(|message| message.text)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    assert!(
        until(TURN, || !mail().is_empty()).await,
        "the turn answered"
    );
    let answered = mail().join("\n");
    eprintln!("--- probe (e2e) lead mail ---\n{answered}");
    assert!(
        answered.contains("HELLO"),
        "the Messages-wire shapes this build parses are the shapes that arrive: {answered}"
    );

    // And a second message is a second turn on the same conversation, which is
    // the resume composition witnessed through the chain rather than raw.
    mailbox::write(
        &root.inbox_path(&team, &MemberName::parse("w1").expect("a member name")),
        MailboxMessage::new(
            "team-lead",
            "Reply with exactly: AGAIN.".to_owned(),
            record::now_iso8601(),
        ),
    )
    .expect("the message is written");
    assert!(
        until(TURN, || mail().len() > 1).await,
        "the resumed turn answered: {:?}",
        mail()
    );
    eprintln!("--- probe (e2e) after resume ---\n{}", mail().join("\n"));

    registry.shutdown().await;
}
