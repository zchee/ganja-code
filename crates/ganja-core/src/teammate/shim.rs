//! The bridge between ganja's mailbox and a foreign CLI's non-interactive
//! surface (**D508**, **D509**).
//!
//! Upstream opencode has no counterpart and neither does Claude Code: neither
//! harness runs another vendor's agent as a teammate, so the whole of this
//! module is ganja's own. What it is *not* is a fourth provider — a shim
//! teammate runs the foreign CLI's own agent implementation, its tools and its
//! sandbox, and this side only carries words in and words out.
//!
//! # Two shapes, one deadline
//!
//! A CLI is driven in whichever shape its own non-interactive door has:
//!
//! - [`Shape::Resident`](crate::teammate::shim::Shape::Resident) — one child for the member's whole life, one NDJSON
//!   line per turn on its stdin, read until the line that says the turn is
//!   over.
//! - [`Shape::PerMessage`](crate::teammate::shim::Shape::PerMessage) — one child per inbox message, argv composed fresh
//!   for a first turn or a resume, stdout read to the end.
//!
//! Both are bounded by **one** per-turn deadline mechanism, and that asymmetry
//! against every other teammate shape is deliberate (**D509**). Every other
//! `Duration` on this path is an unwind budget — [`SETTLE`], `CANCELLED`,
//! `RECORD_WAIT` — and no native teammate's *turn* has a wall-clock bound at
//! all. A foreign child earns one because it is the only shape whose progress
//! ganja cannot observe: an in-process teammate streams events into this
//! process and a `ganja` pane is a ganja whose own status bar a person can look
//! at, while a shim child that has stopped writing to a pipe is
//! indistinguishable from one that is thinking. The deadline is what turns
//! that ambiguity into Principle 4's mail. Two of the three vendors ship no
//! timeout flag of their own, so the mechanism has to live here rather than in
//! a per-CLI module.
//!
//! Since P28 (**D512**) this is the *headless* door's rule alone: every spawn
//! door reaches [`crate::teammate::shim_tui`] instead, a CLI's own TUI in a
//! pane, which runs no per-turn deadline because a pane is a thing a person
//! can look at. The machinery here stays built and unit-tested, reachable
//! through `ganja_testkit::backends`, and `teammates.shim_turn_timeout` moves
//! its number and nothing a spawn reaches.
//!
//! The *value* is per CLI ([`default_turn_timeout`](crate::teammate::shim::default_turn_timeout)) and one curated config
//! key moves all three ([`TIMEOUT_KEY`](crate::teammate::shim::TIMEOUT_KEY)). The key is resolved once, at
//! [`crate::teammate::TeammateRegistry`] construction, and read off the
//! registry here — which is why nothing in this file names a `Config` type at
//! all, and why a test can drive the whole shim core without building one.
//!
//! # What is never composed
//!
//! The posture each CLI launches under is D508(a)'s, pinned on **every** turn
//! rather than only the first, and the escalation door is not built. Until
//! 2026-08-22 a `refuse_bypass` stood here refusing a [`SpawnSpec`] carrying
//! `bypass` by name for every shim backend, because a silent downgrade to the
//! conservative posture would have been a worse lie than a refusal; **D513**
//! retired the bypass axis itself, so there is no such spec left to refuse and
//! the pinned posture is the only one a spawn can ask for.
//!
//! The child's environment is **enumerated** rather than inherited
//! ([`environment`](crate::teammate::shim::environment)), and one clause of that enumeration is a class rule
//! rather than a list: **no `GROK_*` variable is ever in the additions list**.
//! That vendor has at least three environment doors onto its own posture, and
//! inheriting a person's `GROK_SANDBOX=off` would silently undo the pinned
//! profile. Enumeration already excludes all three; the class rule is what
//! keeps the fourth one excluded the day the vendor adds it. No ganja
//! credential variable is ever in the set either.
//!
//! Prompt text never appears in a child's argv ([`Prompt`](crate::teammate::shim::Prompt)): argv is for flags
//! only, because argv is world-readable through `ps` and a teammate's task is
//! documented as a place a credential lands in cleartext.
//!
//! # The frame table is total by construction
//!
//! [`ShimRunner`](crate::teammate::shim::ShimRunner) mirrors [`crate::teammate::runner`]'s loop shape but adds one
//! guard that loop does not have, and the reason is what the two loops deliver
//! into: an in-process teammate reading odd JSON is a model reading odd JSON,
//! while a shim pasting the same text into a foreign agent's prompt is ganja's
//! internals leaving the building. So the rule is structural rather than
//! enumerated — [`Frame`](ganja_protocol::team::Frame) is `#[serde(tag = "type")]`, so every frame this
//! build has and every frame any future build mints carries a `type` key:
//!
//! > **Any inbox message whose text parses as a JSON object bearing a `type`
//! > key is not prompt material.**
//!
//! It is implemented on [`Frame::classify`](ganja_protocol::team::Frame::classify) and emphatically **not** on
//! `Frame::reserved_kind`, which answers [`None`] both for "no `type` key" and
//! for "a `type` key this build does not know" and so cannot tell the two
//! apart. A message dropped by that guard is mailed back to whoever sent it,
//! because all three shims promise `Delivery::Acknowledged` and a dropped
//! message prunes exactly as a consumed one does — without the mail a peer
//! watches its queue entry retire and learns nothing.

pub mod records;

use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ganja_protocol::team::{
    DISPLAY_FIELD_CAP, Frame, MemberBackend, ShutdownApproved, ShutdownRequest, Tagged,
};
use ganja_team::{MailboxMessage, MemberName, ShimCli, Surface, mailbox, record};
pub use records::ShimRecords;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    sync::watch,
};
use tokio_util::sync::CancellationToken;

use crate::teammate::{
    Handle, RECENT_CALLS, SETTLE, SpawnSpec, Unsupported, backend_name, posture_line, runner,
};

/// The config key that moves every CLI's deadline at once.
///
/// Named in the timeout mail rather than only in the schema: whoever reads
/// "this turn was ended after 900s" needs the same line to say what to write
/// if fifteen minutes was the wrong number.
pub const TIMEOUT_KEY: &str = "teammates.shim_turn_timeout";

/// `agy`'s per-turn deadline — **derived** from a vendor constraint, and now
/// measured against real turns as well.
///
/// The derivation is what fixes the number, and it is an ordering rather than a
/// budget: agy's own `--print-timeout` defaults to `5m0s`, and two timeouts
/// bounding one turn wedge it unless this side's fires first — a shim deadline
/// equal to that default could let both fire together. 4m is the largest round
/// value that keeps the shim strictly first. The composed flag is then derived
/// back from *this* number at `deadline + 1m` ([`crate::teammate::agy`]), which
/// is what preserves the ordering when [`TIMEOUT_KEY`] moves the deadline.
///
/// Unlike codex's and grok's, it is therefore **not** the larger of fifteen
/// minutes and twice the longest probe turn — that rule has no ordering to
/// respect. Dv-7's ship probe recorded turns of **54.0s** and **20.8s** on the
/// shipped launch line, so twice the longest is 108s and this deadline sits
/// comfortably above a real turn while staying below the vendor's own.
pub const AGY_TURN_TIMEOUT: Duration = Duration::from_secs(4 * 60);

/// `codex`'s per-turn deadline, **derived** from W3's own gating probe.
///
/// That vendor bounds no turn — `--max-turns` counts turns, not wall-clock —
/// so there is no ordering constraint to derive from as `agy`'s
/// `--print-timeout` gives, only a measurement. The plan's rule is the larger
/// of fifteen minutes and twice the longest turn the probe recorded. Driven
/// through this very backend on 2026-08-20 against `codex-cli 0.149.0-alpha.1`,
/// a first `codex exec` took **37.1s** and its `codex exec resume` **39.1s**,
/// so twice the longest is 78.3s and the fifteen-minute clause is what ships.
///
/// The measurement is what makes the number honest rather than what makes it
/// large: both probe turns were deliberately trivial — reply with one word,
/// then try to write one file — so 78.3s is a floor on a real turn and not an
/// estimate of one, and the 15m clause is doing the work. That is the right way
/// round for this failure: being generous costs a slow teammate, being tight
/// costs a working one, and [`TIMEOUT_KEY`] is the single line that moves all
/// three CLIs when somebody's turns are genuinely longer.
pub const CODEX_TURN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// `grok`'s per-turn deadline, **derived** from W5's own gating probe.
///
/// That vendor bounds no turn either — its `--max-turns` counts turns, not
/// wall-clock — so this is [`CODEX_TURN_TIMEOUT`]'s situation exactly, and the
/// rule is the same: the larger of fifteen minutes and twice the longest turn
/// the probe recorded. Composed by the shipped driver and run through the
/// shipped launch on 2026-08-20 against `grok 1.0.6`, the three ladder turns
/// took **14.7s** (a pure read that completed), **4.1s** and **6.8s** (a write
/// and a shell turn, each cancelled on an unapproved tool ask), so twice the
/// longest is 29.4s and the fifteen-minute clause is what ships.
///
/// The measurement is what makes the number honest rather than what makes it
/// large: all three turns were deliberately trivial, so 29.4s is a floor on a
/// real turn and not an estimate of one, and the 15m clause is doing the work.
/// A fourth run — an unauthenticated turn that failed on the network after 30s
/// — is recorded in the probe file and changes nothing, since twice it is still
/// a minute. [`TIMEOUT_KEY`] is the single line that moves all three CLIs when
/// somebody's turns are genuinely longer.
pub const GROK_TURN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long a failing resident turn waits for the child's own stderr to have
/// been read.
///
/// Short and hard-bounded: it buys the difference between a vendor's sentence
/// and a pipe error, and a child that said nothing must not cost a turn.
const COMPLAINT: Duration = Duration::from_millis(500);

/// How often a shim member reads its own inbox.
///
/// [`crate::teammate::runner::POLL`]'s cadence, and deliberately the same one:
/// two teammate shapes noticing a `shutdown_request` at different speeds would
/// be a difference nobody asked for.
pub const POLL: Duration = runner::POLL;

/// The variables every shim child gets, whatever CLI it is.
///
/// Three, and each earns its place: `HOME` because every one of these CLIs
/// keeps its credentials and its config under it, `PATH` because they shell out
/// to their own tools, and `TMPDIR` because that is where this side puts the
/// prompt file. Everything else is per CLI, and the list is data so growing it
/// is a one-line edit.
pub const CARRIED: [&str; 3] = ["HOME", "PATH", "TMPDIR"];

/// What a headless shim teammate is told before its task (**D514**): the
/// headless channel of [`crate::teammate::preamble::frame`].
///
/// A headless child has no tool to answer with and needs none: what it prints
/// is carried to the lead as mail by the core's own loop. **How much** of it
/// is per CLI, and the paragraph says exactly what each driver does rather
/// than one sentence for all three — codex's driver mails every
/// `agent_message` item in arrival order, where grok's mails the last whole
/// answer of the turn and agy's only the terminal `result` — so an agent that
/// narrates across several messages is told, ahead of time, whether the lead
/// will read all of them or only the last. Bare `who`/`prompt`, as the pane
/// channel takes them, so a test can compute the exact first prompt a child
/// reads.
#[must_use]
pub fn preamble(
    who: crate::teammate::preamble::Names<'_>,
    backend: MemberBackend,
    prompt: &str,
) -> String {
    let answers = match backend {
        MemberBackend::Codex => {
            "every message you print in answer is delivered to the lead as mail, in order"
        }
        // grok and agy forward one answer per turn; a surface this function is
        // never asked about gets the conservative sentence, since "only the
        // last" is the one that cannot lose a teammate's work by being wrong.
        _ => {
            "only your final answer for the turn reaches the lead, as one mail — so put the whole \
             of it in your last message"
        }
    };

    crate::teammate::preamble::frame(
        who,
        &format!(
            "You are running as a headless {cli} process your lead started. Each message from the \
             lead is one turn of yours, opening with who sent it — this one did — and {answers}; \
             there is nobody else you can address, and nothing else you need to do to report.",
            cli = backend_name(backend),
        ),
        prompt,
    )
}

/// Why a spawn refuses when the CLI is not on this session's `PATH`.
///
/// The binary's own name is appended by [`prepare`], because "codex is not on
/// this session's PATH" is the whole of what a person needs in order to fix it.
pub const REFUSED_NO_BINARY: &str = "this session's PATH holds no executable named";

/// What the composed `--permission-mode dontAsk` does, in the words D508(a)
/// settled on after two corrections in opposite directions.
///
/// Never "inert" — an explicit mode suppresses a config-level always-approve
/// and a config-level auto for that launch, which is what forces the vendor's
/// headless client onto its unconditional-cancel arm on a machine whose own
/// grok config says otherwise. And never an approval axis either: the flag
/// reaches an agent definition rather than that vendor's permission engine at
/// the probed version.
pub const GROK_MODE_LINE: &str = "permission-mode dontAsk composed; selects neither yolo nor auto and suppresses a \
     config-level always-approve for this launch; not an approval-policy axis at the probed \
     version";

/// What one shim child is: how it is driven, and therefore what a turn costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// One child for the member's whole life; one turn is one line on its
    /// stdin.
    ///
    /// [`crate::teammate::agy::Agy`] is its consumer, and the vendor's own
    /// flag is what makes this shape the right one rather than a preference:
    /// `--input-format stream-json` *"reads one NDJSON message per line from
    /// stdin and runs a turn for each"*. The child therefore holds the
    /// conversation, and this side holds a pipe into it.
    ///
    /// What that buys, and what it costs, are the same fact: context survives
    /// between turns without a resume flag, and a wedged child takes the
    /// member's whole conversation with it — which is why this shape is the
    /// only one with a respawn, and the only one
    /// whose driver has to compose a vendor timeout of its own.
    Resident,
    /// One child per inbox message.
    PerMessage,
}

/// How a turn's prompt reaches its child.
///
/// Never argv, in either arm — see the module doc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Door {
    /// Written to the child's stdin, which is then closed.
    Stdin,
    /// Written to a `0600` file whose *path* the argv names.
    File,
}

/// What a per-CLI module has to answer, and the whole of what it has to
/// answer.
///
/// The split is the point: everything about *when* a turn starts, how long it
/// may run, what its failure becomes and where its answer goes is this
/// module's, and everything about *which words* go on a command line is the
/// vendor's. A driver that needed something else from here is a driver that
/// needs another field on [`Turn`].
#[async_trait]
pub trait Driver: std::fmt::Debug + Send + Sync + 'static {
    /// Which CLI this is, as a record names it.
    fn cli(&self) -> ShimCli;

    /// Which backend name a person spawns it under.
    fn backend(&self) -> MemberBackend;

    /// The executable to look for on `PATH`.
    fn binary(&self) -> &str;

    /// How it is driven.
    fn shape(&self) -> Shape;

    /// Which environment variables it needs beyond [`CARRIED`].
    ///
    /// **Never a `GROK_*` name** — see the module doc for why that is a class
    /// rule rather than a list of three.
    fn additions(&self) -> &[&str] {
        &[]
    }

    /// How a turn's prompt reaches it.
    fn door(&self) -> Door;

    /// Whether this CLI is in a state where a turn could succeed at all,
    /// asked once at spawn.
    ///
    /// Defaulted to yes, because most of what would make a turn fail cannot be
    /// asked cheaply and a spawn that guessed would be worse than a first turn
    /// that reports. A driver overrides it only where its vendor offers a
    /// *cheap* answer — codex's `login status` is the one such door among the
    /// three — and the alternative it buys is worth naming: without it a member
    /// spawns, accepts a message, and reports an authentication failure a whole
    /// turn later, having already told a person it existed.
    ///
    /// Runs after [`prepare`], so `launch` carries the resolved binary and the
    /// enumerated environment: whatever it asks, it asks as the child would.
    ///
    /// # Errors
    ///
    /// One sentence naming what said no, which becomes the spawn's own
    /// refusal.
    async fn ready(&self, launch: &Launch) -> Result<(), String> {
        let _ = launch;

        Ok(())
    }

    /// The argv after the binary: for [`Shape::PerMessage`] one turn's whole
    /// invocation, for [`Shape::Resident`] the one launch line.
    ///
    /// Pure, so a composed line is a thing a test can hold in its hand — and
    /// so the posture assertions are over a value rather than over a running
    /// process.
    ///
    /// **One driver departs from that, bounded and deliberately**:
    /// [`crate::teammate::grok::Grok`] mints a fresh session UUID on a *first*
    /// turn, because that vendor's door is choose-then-resume rather than
    /// observe-then-resume. It is called exactly once per turn, so one call is
    /// one conversation, and the minted id is written back through
    /// [`Reply::session`] like any observed one — the runner still owns the
    /// per-member state. That module's header carries the whole argument.
    fn argv(&self, turn: &Turn<'_>) -> Vec<OsString>;

    /// One turn's NDJSON line for a [`Shape::Resident`] child.
    ///
    /// # Errors
    ///
    /// One sentence, when the turn cannot be encoded at all. The default
    /// refuses, because a resident driver that forgot to override this would
    /// otherwise wedge silently.
    fn line(&self, turn: &Turn<'_>) -> Result<String, String> {
        let _ = turn;

        Err("this CLI is not driven as a resident child".to_owned())
    }

    /// What a finished [`Shape::PerMessage`] child's stdout said.
    ///
    /// # Errors
    ///
    /// One sentence, when the output is not this vendor's shape — which
    /// becomes a structured failure mail rather than a dead member.
    fn reply(&self, stdout: &str) -> Result<Reply, String>;

    /// What one line of a [`Shape::Resident`] child's stdout means.
    ///
    /// The default ignores everything, which is right for a driver that is not
    /// resident and wrong for one that is — [`Driver::line`]'s refusal is what
    /// catches the second case first.
    fn read(&self, line: &str) -> Read {
        let _ = line;

        Read::Ignored
    }
}

/// One turn, as a [`Driver`] composes it.
///
/// Carries the message text so a driver can put it on a pipe or in a file —
/// never on a command line — and the CLI's own conversation id once one has
/// been observed, which is what tells a first turn from a resume.
#[derive(Clone, Copy, Debug)]
pub struct Turn<'a> {
    /// The spawn this member came from.
    pub spec: &'a SpawnSpec,
    /// What the lead, or a peer, actually said — already enveloped.
    pub text: &'a str,
    /// Where the prompt was written, for a [`Door::File`] driver.
    pub prompt: Option<&'a Path>,
    /// The CLI's own conversation id, when a previous turn revealed one.
    pub session: Option<&'a str>,
    /// How long this turn may run before its process group is ended.
    ///
    /// Carried because one driver has to **compose** it rather than only obey
    /// it: agy takes a `--print-timeout` of its own, and two timeouts bounding
    /// one turn wedge it unless this side's fires first. So the vendor's flag
    /// is derived from this number, which is what makes
    /// [`TIMEOUT_KEY`] move both together.
    ///
    /// A field here rather than a second resolver in the driver, because the
    /// trait's own rule is that a driver needing something else from this
    /// module needs another field on [`Turn`] — and because two answers to
    /// "how long may this run" is exactly the disagreement that ordering
    /// constraint exists to prevent.
    pub deadline: Duration,
}

/// What one CLI turn produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    /// What the lead reads, in arrival order — one mail each.
    pub messages: Vec<String>,
    /// The CLI's own conversation id, when this turn revealed one. Recorded so
    /// the next turn resumes rather than starting a second conversation.
    pub session: Option<String>,
    /// Why the turn ended without an answer, in the **CLI's** own account,
    /// when it ended that way.
    ///
    /// The difference from an `Err` out of [`Driver::reply`] is the difference
    /// between *this build could not read what the child wrote* and *this build
    /// read it perfectly and the child says it cancelled*, and both halves of
    /// that difference are load-bearing:
    ///
    /// - the lead is told the second thing rather than the first, because a
    ///   refusal that reads as garbage output is a refusal nobody acts on;
    /// - the **session is still recorded**, because a cancelled turn is a live
    ///   conversation the CLI created and the next message should resume it
    ///   rather than starting a second one.
    ///
    /// Set beside [`Reply::messages`] rather than instead of them: a turn may
    /// say something and *then* stop, and those words are still owed to the
    /// lead.
    pub refused: Option<String>,
}

/// What one line of a resident child's stdout was worth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    /// Nothing this turn's result depends on.
    Ignored,
    /// The turn is over, and this is what it produced.
    Done(Reply),
    /// The CLI said something this build cannot read, which ends the turn as a
    /// structured failure rather than as silence.
    ///
    /// **Not the same fact as [`Failure::Refused`]**, and the shared word is
    /// worth one clause because the two are opposites: this is *this build*
    /// failing to read the child, and it becomes [`Failure::Unreadable`]; that
    /// one is *the child* saying it ended the turn without an answer, read
    /// perfectly. Naming them apart is a doc's job here rather than a rename's,
    /// since one of the two names is on a resident path this build ships no
    /// driver for.
    Refused(String),
}

/// The per-CLI deadline nothing overrode.
#[must_use]
pub const fn default_turn_timeout(cli: ShimCli) -> Duration {
    match cli {
        ShimCli::Codex => CODEX_TURN_TIMEOUT,
        ShimCli::Agy => AGY_TURN_TIMEOUT,
        ShimCli::Grok => GROK_TURN_TIMEOUT,
    }
}

/// `binary` as `PATH` resolves it for this process, or [`None`].
///
/// Hoisted here from [`crate::teammate::claude`] in P27 so there is **one**
/// copy: four backends now resolve a foreign binary before they spawn, and a
/// second walk would be the day two of them disagree about what is runnable.
///
/// `which` asks the operating system whether this process may execute each
/// candidate, which is the question the later spawn needs answered. A mode-bit
/// walk instead asks whether *somebody* could, so a binary executable only by
/// another owner reads as runnable and fails later with `EACCES` instead of a
/// refusal naming it.
#[must_use]
pub fn on_path(binary: &str) -> Option<PathBuf> {
    resolve(&std::env::var_os("PATH")?, binary)
}

/// [`on_path`]'s decision over an explicit path list.
///
/// The split lets a test hold a `PATH` of its own without mutating the process
/// it runs in — which is what would otherwise cost every fake-CLI test its own
/// test binary. Empty and relative components are removed before `which` sees
/// the list because its Unix behavior follows `which(1)` and can resolve them
/// against the working directory, while no backend here discovers a teammate
/// binary from a turn's incidental directory. A literal `~/bin` entry is
/// dropped by the same filter — the crate would tilde-expand it, but an entry
/// only a shell would have expanded is not a directory this trusts.
#[must_use]
pub fn resolve(path: &OsStr, binary: &str) -> Option<PathBuf> {
    let mut directories = std::env::split_paths(path)
        .filter(|directory| !directory.as_os_str().is_empty() && directory.is_absolute())
        .peekable();
    directories.peek()?;
    let search_path = std::env::join_paths(directories).ok()?;

    which::which_in_global(binary, Some(search_path))
        .ok()?
        .next()
}

/// The environment one shim child gets: [`CARRIED`], plus this CLI's own
/// additions, and nothing else.
///
/// D502's posture, adapted from the pane backend — enumerate, never inherit.
/// `path` overrides what `PATH` is set to, which is how a test points a child
/// at a fake CLI without touching the process it runs in.
///
/// A name that is not set in this process is simply absent from the answer
/// rather than set empty: a CLI reading an empty `HOME` behaves worse than one
/// reading none.
#[must_use]
pub fn environment(additions: &[&str], path: Option<&OsStr>) -> Vec<(OsString, OsString)> {
    let mut carried = Vec::with_capacity(CARRIED.len() + additions.len());
    for name in CARRIED.iter().copied().chain(additions.iter().copied()) {
        // The class rule, **enforced** rather than only documented. Until this
        // filter existed the exclusion held only because no additions list
        // happened to name one, which is a property of today's drivers rather
        // than of the mechanism — and the day a grok driver names any `GROK_*`
        // variable, the posture a person consented to at spawn moves silently.
        // A driver that names one is caught loudly at [`prepare`]; here the
        // answer is simply that it does not travel.
        if !admits(name) {
            continue;
        }
        if name == "PATH"
            && let Some(path) = path
        {
            carried.push((OsString::from(name), path.to_owned()));
            continue;
        }
        if let Some(value) = std::env::var_os(name) {
            carried.push((OsString::from(name), value));
        }
    }

    carried
}

/// Whether a variable may reach a shim child at all.
///
/// One clause, and it is a **class** rather than a list: no `GROK_*` name,
/// ever. That vendor has at least three environment doors onto the very
/// posture D508(a) pins — the sandbox profile itself, the auto-allow-bash
/// switch, and the workspace server's own profile variable — and the list is
/// expected to grow. Naming the three would be excluding the three; naming the
/// prefix excludes the fourth one the day the vendor adds it.
///
/// Deliberately not a judgement about *which* of them are dangerous: a rule
/// that has to be re-derived per variable is a rule somebody gets wrong once.
#[must_use]
pub fn admits(name: &str) -> bool {
    !name.starts_with("GROK_")
}

/// One turn's prompt, in a `0600` file inside a private directory of this
/// turn's, removed when the turn settles.
///
/// A file rather than an argument because argv is world-readable through `ps`,
/// and a teammate's task is documented as a place a credential lands in
/// cleartext. A directory of its own rather than a bare tempfile because the
/// mode on the file is only half the answer: a world-readable directory tells
/// everybody the file exists and how big it is.
#[derive(Debug)]
pub struct Prompt {
    /// Held rather than read: dropping it is what removes the directory and
    /// the prompt inside it when the turn settles. The underscore is the
    /// tree's own spelling for a value whose whole job is its `Drop`.
    _directory: tempfile::TempDir,
    path: PathBuf,
}

impl Prompt {
    /// Writes `text` where only this user can read it.
    ///
    /// # Errors
    ///
    /// One sentence, when the directory or the file could not be made — which
    /// the caller turns into a structured failure mail rather than a dead
    /// member.
    pub fn write(under: &Path, text: &str) -> Result<Self, String> {
        use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};

        let directory = tempfile::Builder::new()
            .prefix("ganja-shim-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(under)
            .map_err(|error| format!("a private directory for the prompt: {error}"))?;
        let path = directory.path().join("prompt.txt");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("the prompt file: {error}"))?;
        file.write_all(text.as_bytes())
            .map_err(|error| format!("the prompt file: {error}"))?;

        Ok(Self {
            _directory: directory,
            path,
        })
    }

    /// Where the child reads it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether a child of this member's is running right now.
///
/// **A state, not an edge**, and that is the whole reason it is a [`watch`]
/// rather than a `Notify`: a notification fires only when somebody is already
/// listening, so an individual retire of a per-message member sitting *between*
/// messages — no turn task, nothing to fire — would wait out the full
/// [`SETTLE`] for an event that is never coming. A kill reads the current value
/// first and returns at once when there is nothing to wait for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Running {
    /// No child of this member's exists at this moment.
    Idle,
    /// One does.
    Child,
    /// This member is finished; there will not be another.
    Ended,
}

/// What [`crate::teammate::Handle::Child`] holds.
///
/// Deliberately holds no `tokio::process::Child`: `TeammateBackend::kill` takes
/// a **shared** reference and a member is reached only through an `Arc`, while
/// a child's `kill`/`wait` all want `&mut`. So the handle holds what a signal
/// needs — the group — and the child itself is owned by the task that drives
/// it, under `kill_on_drop(true)`.
///
/// Every shape carries which CLI it is, because `Handle::surface` has to answer
/// `Surface::Shim` with that CLI: "and nothing else" was never available.
pub struct Child {
    /// The per-CLI rules, held here rather than on the backend value so that
    /// the registry's track seam needs nothing but this handle to build the
    /// member's loop — a backend is shared across spawns and could not hold
    /// one spawn's launch anyway.
    driver: Arc<dyn Driver>,
    launch: Launch,
    /// The group a signal goes to right now, when a child of this member's is
    /// running.
    ///
    /// A cell rather than a value because a per-message member has no stable
    /// group: each turn is a new child in a new group, so a pgid recorded at
    /// spawn would be wrong from the second turn on. A resident member's is set
    /// once at spawn and cleared once at its end.
    group: Mutex<Option<i32>>,
    running: watch::Sender<Running>,
    cancel: CancellationToken,
    /// The machinery the runner task takes exactly once — a resident child's
    /// pipes, or nothing at all for a per-message one.
    started: Mutex<Option<Started>>,
}

impl std::fmt::Debug for Child {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Child")
            .field("cli", &self.cli())
            .field("shape", &self.shape())
            .field("group", &self.group())
            .field("running", &self.running())
            .finish_non_exhaustive()
    }
}

/// A resident child's pipes, handed from a backend's `spawn` to the runner task
/// exactly once.
#[derive(Debug)]
pub struct Started {
    /// The child itself, owned by whoever takes this.
    pub child: tokio::process::Child,
    /// Its stdin, which one turn is one line on.
    pub stdin: tokio::process::ChildStdin,
    /// Its stdout, read a line at a time.
    pub stdout: tokio::process::ChildStdout,
    /// Its stderr, drained by a task of its own — a pipe nobody reads is a
    /// pipe that fills, and a resident child blocked on its own error output
    /// is a member that answers nothing for a reason no log line names.
    pub stderr: tokio::process::ChildStderr,
}

impl Child {
    /// A handle over a resident child that is already running.
    #[must_use]
    pub fn resident(driver: Arc<dyn Driver>, launch: Launch, group: i32, started: Started) -> Self {
        let (running, _) = watch::channel(Running::Child);

        Self {
            driver,
            launch,
            group: Mutex::new(Some(group)),
            running,
            cancel: CancellationToken::new(),
            started: Mutex::new(Some(started)),
        }
    }

    /// A handle over a member whose children are one per message, and which
    /// therefore has none yet.
    #[must_use]
    pub fn per_message(driver: Arc<dyn Driver>, launch: Launch) -> Self {
        let (running, _) = watch::channel(Running::Idle);

        Self {
            driver,
            launch,
            group: Mutex::new(None),
            running,
            cancel: CancellationToken::new(),
            started: Mutex::new(None),
        }
    }

    /// The per-CLI rules this member runs under.
    #[must_use]
    pub fn driver(&self) -> &Arc<dyn Driver> {
        &self.driver
    }

    /// The binary, environment and directory its turns run in.
    #[must_use]
    pub const fn launch(&self) -> &Launch {
        &self.launch
    }

    /// Which CLI drives this member — what `Handle::surface` answers with.
    #[must_use]
    pub fn cli(&self) -> ShimCli {
        self.driver().cli()
    }

    /// How it is driven.
    #[must_use]
    pub fn shape(&self) -> Shape {
        self.driver().shape()
    }

    /// The group a signal would go to right now.
    #[must_use]
    pub fn group(&self) -> Option<i32> {
        *self.group.lock().expect("the shim group is never poisoned")
    }

    /// The token that ends this member's loop and whatever turn it is in.
    #[must_use]
    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Whether a child of this member's exists at this moment.
    #[must_use]
    pub fn running(&self) -> Running {
        *self.running.borrow()
    }

    /// Says a child has started, in `group`.
    pub fn entered(&self, group: i32) {
        *self.group.lock().expect("the shim group is never poisoned") = Some(group);
        let _ = self.running.send(Running::Child);
    }

    /// Says the child is reaped and no other is running.
    pub fn left(&self) {
        *self.group.lock().expect("the shim group is never poisoned") = None;
        // A member already ended stays ended: a late reap must not resurrect it
        // into an idle member the roster would list.
        if self.running() != Running::Ended {
            let _ = self.running.send(Running::Idle);
        }
    }

    /// Says this member is finished for good.
    pub fn ended(&self) {
        *self.group.lock().expect("the shim group is never poisoned") = None;
        let _ = self.running.send(Running::Ended);
    }

    /// Takes the resident pipes, which exactly one caller ever may.
    #[must_use]
    pub fn take_started(&self) -> Option<Started> {
        self.started
            .lock()
            .expect("the shim pipes are never poisoned")
            .take()
    }

    /// Ends whatever is running and waits for it to really be gone.
    ///
    /// The trait contract is *"Ends what `spawn` produced"*, so this awaits
    /// rather than only asking. Two mechanisms cover the two callers and both
    /// are needed: the shim's task is registered in the registry's task list,
    /// so a registry `shutdown()` drains it after `join_all`ing every kill; and
    /// this call itself awaits, for the individual-kill path — a `/team` retire
    /// of one member while the lead lives — which never touches that list.
    ///
    /// TERM at once, KILL after [`SETTLE`], to the **group** rather than to the
    /// pid, so a CLI's own tool subprocesses die with it. Idempotent: a member
    /// with nothing running is nothing to end, and reading the state first is
    /// what keeps that case from spending [`SETTLE`] on an event that is never
    /// coming.
    pub async fn end(&self) {
        self.cancel.cancel();
        let mut running = self.running.subscribe();
        if matches!(*running.borrow_and_update(), Running::Idle | Running::Ended) {
            return;
        }
        let Some(group) = self.group() else {
            return;
        };
        signal_group(group, libc::SIGTERM);

        // The turn task reaps and says so; this is the wait that makes a kill
        // honest about what it ended.
        let settled = tokio::time::timeout(SETTLE, async {
            while !matches!(*running.borrow_and_update(), Running::Idle | Running::Ended) {
                if running.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        if settled.is_err() {
            tracing::warn!(
                cli = self.cli().backend_type(),
                group,
                "a shim child did not end on SIGTERM, so its process group was killed"
            );
            signal_group(group, libc::SIGKILL);
        }
    }
}

/// Signals a whole process group, and says nothing when there is nothing left
/// to signal.
///
/// `ESRCH` is the ordinary answer for a group that has already gone, which is
/// the common case on the KILL leg of a TERM that worked.
pub fn signal_group(pgid: i32, signal: i32) {
    // SAFETY: `kill` with a negative pid signals the process group; both
    // arguments are plain integers and the call cannot touch this process's
    // memory.
    let sent = unsafe { libc::kill(-pgid, signal) };
    if sent != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(pgid, signal, %error, "a shim process group could not be signalled");
        }
    }
}

/// The ring lines a **headless** shim spawn writes (**AC-17**).
///
/// Written by the shim rather than folded from an engine event stream, because
/// `fold_calls` folds from exactly that and a shim member has no engine.
///
/// [`posture_lines`]' two, then a third that is grok's alone and says what the
/// composed permission mode actually does on the headless door, so the ring
/// neither implies an axis the vendor's code does not have nor understates the
/// one it does. That third line is this door's and not the pane's: a grok TUI
/// (**D512**) runs under the same flag and, measured, does the opposite with
/// it — the TUI raises its own approval prompt to the person where this door
/// cancels the turn — so [`crate::teammate::shim_tui::spawn_lines`] ends on
/// its own pane sentence instead of borrowing this one (the lead's ruling 5
/// for P28, and the 1.0.7 recording in `grok-tui-probe.txt` that settled it).
#[must_use]
pub fn spawn_lines(backend: MemberBackend) -> Vec<String> {
    let mut lines = posture_lines(backend);
    if !lines.is_empty() && backend == MemberBackend::Grok {
        lines.push(GROK_MODE_LINE.to_owned());
    }

    lines
}

/// The two ring lines every shim spawn opens with, on either door.
///
/// The first is [`posture_line`]'s own sentence, so the ring and the spawn
/// dialog cannot come to say different things about one grant — that is the
/// whole reason the table is a function rather than two string literals. The
/// second is the honest rider: a managed requirement, a person's own
/// always-approve, their own allow rules all outrank what ganja composed, so
/// ganja cannot promise more than its own flags bound — and on the pane door
/// the rider is if anything truer, since a pane inherits the tmux server's
/// environment on top of the CLI's own config. Empty for a backend with no
/// posture to disclose, so a caller composing a ring can chain its own door's
/// rider onto this without first asking which backend it holds.
#[must_use]
pub fn posture_lines(backend: MemberBackend) -> Vec<String> {
    let Some(posture) = posture_line(backend) else {
        return Vec::new();
    };
    let cli = backend_name(backend);

    vec![
        format!("spawned {cli} · {posture}"),
        format!("effective posture bounded by {cli}'s own config"),
    ]
}

/// Pushes one line onto a member's ring, the way the engine-folded one does.
///
/// Shared with `fold_calls` so the two writers cannot come to disagree about
/// the cap or about what counts as a repeat: a line identical to the one
/// already at the back says nothing the first said, and the ring is a live view
/// rather than a log.
pub(crate) fn push_recent(ring: &Mutex<VecDeque<String>>, line: String) {
    let mut ring = ring.lock().expect("the call ring is never poisoned");
    if ring.back() == Some(&line) {
        return;
    }
    if ring.len() == RECENT_CALLS {
        ring.pop_front();
    }
    ring.push_back(line);
}

/// Everything one shim member's turns need that is not the [`Driver`].
pub struct Launch {
    /// The resolved binary, so a `PATH` that changed mid-session cannot move
    /// which executable a member's later turns run.
    pub binary: PathBuf,
    /// The enumerated child environment.
    pub environment: Vec<(OsString, OsString)>,
    /// Where the child works: `SpawnSpec::cwd`, verbatim — the same directory
    /// the spawn's own outside-project clause already gated.
    pub cwd: PathBuf,
    /// Where a [`Door::File`] prompt is written.
    pub tmp: PathBuf,
}

impl std::fmt::Debug for Launch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Launch")
            .field("binary", &self.binary)
            // The names, never the values: the habit of printing an environment
            // is how one gets printed.
            .field("environment", &self.names())
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

impl Launch {
    /// The names in the child environment, in order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.environment
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect()
    }

    /// The command one turn runs, with the enumerated environment and nothing
    /// else.
    ///
    /// Its own process group, so a kill reaches the CLI's own tool subprocesses
    /// and never this process's siblings; `kill_on_drop`, so a turn task that
    /// goes away takes its child with it.
    #[must_use]
    pub fn command(&self, argv: &[OsString]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .args(argv)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(self.environment.iter().cloned())
            .process_group(0)
            .kill_on_drop(true);

        command
    }
}

/// What every shim backend does before it spawns anything, in one place.
///
/// The two checks are the same two for every CLI, and answering them in three
/// modules would be three places for one of them to go missing: the binary is
/// resolved, and the environment is enumerated. (A third, refusing the
/// escalation door, stood here until **D513** retired the bypass axis that
/// could have asked for one.)
///
/// `path` overrides where the binary is looked for and what the child's `PATH`
/// is set to — one value for both, because a build that resolved a fake CLI and
/// then handed the child the real `PATH` would be testing neither.
///
/// # Errors
///
/// [`Unsupported`] naming which of the three refused.
pub fn prepare(
    driver: &dyn Driver,
    spec: &SpawnSpec,
    path: Option<&OsStr>,
) -> Result<Launch, Unsupported> {
    // Loud where a driver's own list is first consulted, silent where the
    // environment is built: [`environment`] drops such a name whatever
    // happens, so a release build is safe, and this is what makes a developer
    // who adds one find out in the same run rather than in a probe six weeks
    // later. Split across the two on purpose — an assertion inside
    // `environment` would make the very unit test that pins the safe fallback
    // unwritable.
    debug_assert!(
        driver.additions().iter().all(|name| admits(name)),
        "no GROK_* variable may ever be in a shim driver's additions list: it is a door onto \
         the posture a person consented to at spawn, and enumeration is what closes it"
    );
    let binary = match path {
        Some(path) => resolve(path, driver.binary()),
        None => on_path(driver.binary()),
    }
    .ok_or_else(|| Unsupported {
        backend: driver.backend(),
        reason: format!("{REFUSED_NO_BINARY} {}", driver.binary()),
    })?;
    let environment = environment(driver.additions(), path);
    let tmp = environment
        .iter()
        .find(|(name, _)| name == "TMPDIR")
        .map_or_else(std::env::temp_dir, |(_, value)| PathBuf::from(value));

    Ok(Launch {
        binary,
        environment,
        cwd: spec.cwd.clone(),
        tmp,
    })
}

/// Starts a [`Shape::Resident`] child and wraps it as a handle.
///
/// Here rather than in each per-CLI module because the pipe shape, the process
/// group and the pid-to-group identity are the *mechanism*, and a second
/// spelling of them would be a second chance to forget `process_group(0)` —
/// which is the difference between killing a CLI's tool subprocesses and
/// orphaning them.
///
/// # Errors
///
/// [`Unsupported`] when the child could not be started, or when it was reaped
/// before its pid could be read.
pub fn start_resident(
    driver: Arc<dyn Driver>,
    launch: Launch,
    argv: &[OsString],
) -> Result<Handle, Unsupported> {
    let cannot = |reason: String| Unsupported {
        backend: driver.backend(),
        reason,
    };
    let (started, pid) = open_resident(&launch, argv, driver.binary()).map_err(cannot)?;

    Ok(Handle::Child(Arc::new(Child::resident(
        driver, launch,
        // Spawned with `process_group(0)`, so the child leads its own group and
        // the group id is its pid.
        pid, started,
    ))))
}

/// One resident child, started, with its three pipes taken.
///
/// Split out of [`start_resident`] because a **respawn** needs exactly this and
/// not the handle around it: a wedged member is replaced in place, keeping the
/// `Child` it is already reachable through. Two spellings of this would be two
/// chances to forget `process_group(0)`, which is the difference between
/// killing a CLI's tool subprocesses and orphaning them.
///
/// # Errors
///
/// One sentence naming the binary, for a child that could not be started or
/// that was reaped before its pid could be read.
fn open_resident(
    launch: &Launch,
    argv: &[OsString],
    binary: &str,
) -> Result<(Started, i32), String> {
    let mut child = launch
        .command(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{binary} could not be started: {error}"))?;
    let taken = child
        .stdin
        .take()
        .zip(child.stdout.take())
        .zip(child.stderr.take());
    let (Some(((stdin, stdout), stderr)), Some(pid)) = (taken, child.id()) else {
        return Err(format!(
            "{binary} was reaped before this side could speak to it"
        ));
    };

    Ok((
        Started {
            child,
            stdin,
            stdout,
            stderr,
        },
        i32::try_from(pid).unwrap_or_default(),
    ))
}

/// A [`Shape::PerMessage`] member's handle: nothing is running yet, and the
/// first inbox message is what starts anything.
#[must_use]
pub fn start_per_message(driver: Arc<dyn Driver>, launch: Launch) -> Handle {
    Handle::Child(Arc::new(Child::per_message(driver, launch)))
}

/// One [`Driver`] as a [`crate::teammate::TeammateBackend`].
///
/// One value for all three CLIs rather than three implementations of the same
/// trait: what differs between codex, agy and grok is entirely inside the
/// driver, and three copies of "prepare, start, end" would be three chances
/// for one of them to forget `process_group(0)`.
///
/// It also makes the shim core drivable from a test that owns a fake CLI —
/// hand it a driver and a `PATH`, and the whole loop runs against a script.
pub struct ShimBackend {
    driver: Arc<dyn Driver>,
    /// Where the binary is looked for, and what the child's own `PATH` is set
    /// to. [`None`] is this process's — the production answer; a value is how
    /// a test points a child at a fake CLI without mutating the process it
    /// runs in.
    path: Option<OsString>,
    /// The deadline a turn on this backend runs under, for the one thing a
    /// spawn needs it for: composing a [`Shape::Resident`] launch line.
    ///
    /// [`None`] means [`default_turn_timeout`], which is right for every
    /// caller that has no config to consult. The production caller passes the
    /// registry's **own** resolved answer rather than re-reading the config,
    /// so the number in the launch line and the number the runner enforces
    /// cannot come from two places and disagree.
    deadline: Option<Duration>,
}

impl std::fmt::Debug for ShimBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShimBackend")
            .field("driver", &self.driver)
            .finish_non_exhaustive()
    }
}

impl ShimBackend {
    /// The backend for `driver`, resolving its binary on this process's own
    /// `PATH`.
    #[must_use]
    pub fn new(driver: Arc<dyn Driver>) -> Self {
        Self {
            driver,
            path: None,
            deadline: None,
        }
    }

    /// The same backend against an explicit search path.
    #[must_use]
    pub fn searching(mut self, path: OsString) -> Self {
        self.path = Some(path);

        self
    }

    /// The same backend composing its launch line for `deadline`.
    ///
    /// Only a [`Shape::Resident`] driver reads it — a per-message one composes
    /// a fresh argv per turn, where the runner's own deadline is already in
    /// hand — so the two backends that never call this lose nothing by it.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);

        self
    }

    /// The deadline this backend composes a launch line for.
    fn deadline(&self) -> Duration {
        self.deadline
            .unwrap_or_else(|| default_turn_timeout(self.driver.cli()))
    }
}

#[async_trait]
impl crate::teammate::TeammateBackend for ShimBackend {
    fn backend(&self) -> MemberBackend {
        self.driver.backend()
    }

    /// The headless channel, in this CLI's name: answers are mail.
    fn preamble(&self, spec: &SpawnSpec) -> String {
        preamble(
            crate::teammate::preamble::Names::of(spec),
            self.driver.backend(),
            &spec.prompt,
        )
    }

    async fn spawn(&self, spec: &SpawnSpec) -> Result<Handle, Unsupported> {
        let launch = prepare(&*self.driver, spec, self.path.as_deref())?;
        // After `prepare` rather than inside it: the check is a *subprocess*,
        // and `prepare` is the sync half every backend shares.
        self.driver
            .ready(&launch)
            .await
            .map_err(|reason| Unsupported {
                backend: self.driver.backend(),
                reason,
            })?;
        match self.driver.shape() {
            Shape::Resident => {
                // The launch turn carries no text and no session: a resident
                // child is started before anybody has said anything to it, and
                // its turns arrive on stdin through `Driver::line`.
                let argv = self.driver.argv(&Turn {
                    spec,
                    text: "",
                    prompt: None,
                    session: None,
                    deadline: self.deadline(),
                });
                start_resident(Arc::clone(&self.driver), launch, &argv)
            }
            Shape::PerMessage => Ok(start_per_message(Arc::clone(&self.driver), launch)),
        }
    }

    async fn kill(&self, handle: &Handle) {
        let Some(child) = handle.child() else {
            // Not reachable through the registry, which hands back the handle
            // this backend's own `spawn` returned — but a handle of another
            // shape arriving here would mean a registry had crossed two
            // backends, and that is worth saying rather than ignoring.
            tracing::warn!(
                ?handle,
                backend = backend_name(self.driver.backend()),
                "a shim backend was asked to end something it did not start"
            );

            return;
        };
        child.end().await;
    }

    fn delivery(&self) -> crate::teammate::Delivery {
        // The shim reads its own inbox and takes the message onto a turn in
        // this process, so the acknowledgement is that read — unlike a real
        // `claude` pane, where a foreign process reads at its own pace and
        // there is nothing to watch.
        crate::teammate::Delivery::Acknowledged
    }
}

/// What one pass of [`ShimRunner`] did.
///
/// Returned rather than only logged so a test can drive a single pass and
/// assert the frame table, which is the part of this loop that is the contract.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// The request id of a shutdown this pass answered, if it answered one.
    pub shutdown: Option<String>,
    /// How many messages became CLI turns.
    pub turns: usize,
    /// The frames this pass dropped as information, by kind — `None` where the
    /// kind was one this build has never heard of and the sender gave no name
    /// for it.
    pub dropped: Vec<Option<String>>,
    /// How many turns ended without an answer and were reported as mail.
    pub failed: usize,
}

/// What the registry lends one shim member's loop.
///
/// Grouped rather than passed one by one because every field is the
/// *registry's* — the ring it will show in `/team`, the flag its roster reads,
/// the records file it owns, the token that ends every member at once — and a
/// loop that had been handed them separately would be a loop somebody could
/// build with one of them missing.
pub struct Lent {
    /// Where this member answers.
    pub lead_inbox: PathBuf,
    /// **D503**'s ring, which a shim writes itself because `fold_calls` folds
    /// from an engine event stream a shim member has none of.
    pub recent: Arc<Mutex<VecDeque<String>>>,
    /// Cleared when the loop ends, so a member that shut itself down stops
    /// being listed without the registry having to be told.
    pub alive: Arc<AtomicBool>,
    /// The one writer of this session's orphan records.
    pub shims: Arc<Mutex<ShimRecords>>,
    /// The registry's own cancellation, beside the handle's own.
    pub cancel: CancellationToken,
}

/// One shim member's mailbox loop.
///
/// Mirrors [`crate::teammate::runner::Runner`]'s **shape** and shares none of
/// its implementation, which is deliberate rather than duplication:
/// `Member.runner` is typed to the in-process runner and a shim member is
/// `runner: None`, because everything that loop does past reading the inbox —
/// deliver to an engine, apply a mode, answer a plan approval — is exactly what
/// a shim has no engine for.
pub struct ShimRunner {
    handle: Arc<Child>,
    spec: SpawnSpec,
    lead_inbox: PathBuf,
    recent: Arc<Mutex<VecDeque<String>>>,
    alive: Arc<AtomicBool>,
    shims: Arc<Mutex<ShimRecords>>,
    deadline: Duration,
    /// The registry's own token, beside the handle's. Two of them, because
    /// they answer different questions: the handle's ends *this member* (a
    /// `/team` retire), and this one ends *every* member at once (a session
    /// shutting down). A loop that watched only the first would outlive its
    /// registry.
    registry: CancellationToken,
    poll: Duration,
    /// The CLI's own conversation id, once a turn has revealed one.
    session: Mutex<Option<String>>,
    /// The resident child's pipes, once the loop has taken them.
    resident: Mutex<Option<Resident>>,
    /// The first thing a resident child said on stderr, drained by a task of
    /// its own so the pipe cannot fill.
    complaint: Arc<Mutex<Option<String>>>,
}

/// A resident child's live pipes, as the loop holds them between turns.
struct Resident {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl std::fmt::Debug for ShimRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShimRunner")
            .field("driver", self.driver())
            .field("member", &self.spec.name)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl ShimRunner {
    /// Builds the loop for one shim member.
    #[must_use]
    pub fn new(handle: Arc<Child>, spec: SpawnSpec, lent: Lent, deadline: Duration) -> Self {
        Self {
            handle,
            spec,
            lead_inbox: lent.lead_inbox,
            recent: lent.recent,
            alive: lent.alive,
            shims: lent.shims,
            deadline,
            registry: lent.cancel,
            poll: POLL,
            session: Mutex::new(None),
            resident: Mutex::new(None),
            complaint: Arc::new(Mutex::new(None)),
        }
    }

    /// Runs until the registry cancels it, a `shutdown_request` is answered, or
    /// this member's child is gone for good.
    pub async fn run(self) {
        if let Some(started) = self.handle.take_started() {
            let pid = started
                .child
                .id()
                .map(|pid| i32::try_from(pid).unwrap_or_default());
            self.drain_stderr(started.stderr);
            *self
                .resident
                .lock()
                .expect("the resident pipes are never poisoned") = Some(Resident {
                child: started.child,
                stdin: started.stdin,
                stdout: BufReader::new(started.stdout).lines(),
            });
            // A resident child is running from the moment it is spawned, so it
            // is recorded now rather than at a turn boundary — the whole point
            // of the record is the window in which nothing is happening.
            if let Some(pid) = pid {
                self.record(pid);
            }
        }
        let cancel = self.handle.cancel().clone();
        let registry = self.registry.clone();
        let mut poll = tokio::time::interval(self.poll);
        // A pass that ran late is taken late rather than immediately again: a
        // member whose inbox read blocked on a peer's lock must not then spin
        // through the passes it missed.
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = registry.cancelled() => break,
                _ = poll.tick() => {
                    if self.tick().await.shutdown.is_some() {
                        break;
                    }
                }
            }
        }

        // The member is over: whatever it owned is ended and the roster stops
        // listing it. Both are the shim's own to do — `Member.alive` is cleared
        // by the in-process runner when *its* task ends, and there is no such
        // task here.
        self.retire_child().await;
        self.handle.ended();
        self.alive.store(false, Ordering::Relaxed);
    }

    /// One pass: read, classify, run whatever was really prompt material.
    pub async fn tick(&self) -> Tick {
        let mut tick = Tick::default();
        let Some(contents) = runner::read_inbox(self.inbox(), self.spec.name.as_str()).await else {
            return tick;
        };
        if contents.valid.is_empty() {
            return tick;
        }

        // Step 1, and it is a step of its own because it goes first: a member
        // wedged behind a hundred queued messages stays reclaimable. **From any
        // sender**, matching the in-process runner, which matches
        // `shutdown_request` with no `from` check at all — two retirement rules
        // on one mailbox would be a difference nobody asked for.
        let shutdown = contents
            .valid
            .iter()
            .enumerate()
            .find_map(|(position, message)| match message.frame() {
                Some(Frame::ShutdownRequest(request)) => Some((position, message, request)),
                _ => None,
            });
        if let Some((position, message, request)) = shutdown {
            tracing::info!(
                teammate = self.spec.name.as_str(),
                request = request.request_id,
                jumped = position,
                "a shutdown request goes ahead of everything else in the inbox"
            );
            self.tear_down(&request).await;
            self.prune(vec![mailbox::identity(message)]).await;
            tick.shutdown = Some(request.request_id);

            return tick;
        }

        let mut handled = Vec::new();
        for message in &contents.valid {
            handled.push(mailbox::identity(message));
            match Frame::classify(&message.text) {
                // Prose, or somebody's data carrying no `type` at all: the only
                // two shapes that are prompt material.
                Tagged::NotAnObject | Tagged::Untagged => {
                    tick.turns += 1;
                    if let Err(failure) = self.take_turn(&message.from, &message.text).await {
                        tick.failed += 1;
                        self.report(&failure).await;
                        // After the report, and only for the shape that can
                        // lose a child mid-conversation.
                        self.replace_lost_child().await;
                    }
                }
                Tagged::Reserved(kind) => {
                    tick.dropped.push(Some(kind.to_owned()));
                    self.drop_reserved(kind, &message.from).await;
                }
                Tagged::Unknown { name } => {
                    tick.dropped.push(name.clone());
                    self.drop_unknown(name.as_deref(), &message.from).await;
                }
            }
        }
        if !handled.is_empty() {
            self.prune(handled).await;
        }

        tick
    }

    /// This member's own inbox.
    fn inbox(&self) -> PathBuf {
        self.spec.inbox()
    }

    /// The per-CLI rules, off the handle that carries them.
    fn driver(&self) -> &Arc<dyn Driver> {
        self.handle.driver()
    }

    /// The binary, environment and directory a turn runs in.
    fn launch(&self) -> &Launch {
        self.handle.launch()
    }

    /// Keeps a resident child's stderr moving, and its first line.
    ///
    /// A pipe nobody reads is a pipe that fills, and a resident child blocked
    /// on its own error output is a member that answers nothing for a reason no
    /// log line names. The first non-empty line is kept because that is what a
    /// vendor's startup refusal says, and it is what a failure mail carries.
    ///
    /// # This task is deliberately not in the registry's task list
    ///
    /// It ends when the pipe reaches EOF, and ending the child's process group
    /// is **expected** to bring that about rather than guaranteed to: the write
    /// end is inherited, so a CLI that `setsid`s a helper of its own leaves
    /// that helper holding the fd and the pipe open after the group is gone.
    ///
    /// Which is exactly why it is not registered where `shutdown()` drains:
    /// registering it would make a session's exit wait on the one case that
    /// cannot end. What is lost by not waiting is a few bytes of somebody
    /// else's log line, and the task holds nothing a shutdown has to see
    /// through — the failure mail it feeds was written before the kill, or
    /// there was no failure to mail.
    fn drain_stderr(&self, stderr: tokio::process::ChildStderr) {
        let complaint = Arc::clone(&self.complaint);
        let who = self.spec.name.as_str().to_owned();
        let cli = backend_name(self.driver().backend()).to_owned();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                tracing::debug!(teammate = who, cli, "{}", first_line(&line));
                let mut held = complaint.lock().expect("the complaint is never poisoned");
                if held.is_none() {
                    *held = Some(first_line(&line));
                }
            }
        });
    }

    /// What a resident child said on stderr, if it said anything.
    fn complaint(&self) -> String {
        self.complaint
            .lock()
            .expect("the complaint is never poisoned")
            .clone()
            .unwrap_or_default()
    }

    /// The same, giving the drain task a moment to have read it.
    ///
    /// A vendor's startup refusal is a race this side loses by default: the
    /// child writes its sentence and exits, and what this side notices first is
    /// the broken pipe on the *next* write. Waiting briefly for the sentence is
    /// the difference between telling the lead "agy refused: not logged in" and
    /// telling it "broken pipe", which is AC-8's whole point about this arm.
    /// Bounded hard, because a child that said nothing must not cost a turn.
    async fn settled_complaint(&self) -> String {
        let deadline = tokio::time::Instant::now() + COMPLAINT;
        while tokio::time::Instant::now() < deadline {
            let held = self.complaint();
            if !held.is_empty() {
                return held;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        self.complaint()
    }

    /// A resident failure, with whatever the child said about itself.
    async fn wedged(&self, what: &str, error: &std::io::Error) -> Failure {
        let complaint = self.settled_complaint().await;
        if complaint.is_empty() {
            return Failure::Local(format!("{what}: {error}"));
        }

        // The vendor's own sentence leads, because it is the one that says
        // *why*; the pipe error is the symptom this side happened to see
        // first.
        Failure::Unreadable {
            reason: format!("{complaint} ({what}: {error})"),
        }
    }

    /// A recognized frame kind, dropped as information.
    ///
    /// A shim has no engine, so the two kinds the in-process runner *applies* —
    /// a plan approval and a mode set — have nothing to be applied to. Both
    /// still leave a ring entry, and `mode_set_request` also mails the lead: a
    /// lead that set a mode and heard nothing would reasonably believe the mode
    /// was set.
    async fn drop_reserved(&self, kind: &'static str, from: &str) {
        self.remember(format!(
            "dropped frame {kind} · a shim member has no engine to apply it to"
        ));
        tracing::info!(
            teammate = self.spec.name.as_str(),
            from,
            kind,
            "a reserved frame reached a shim member, which has no engine to apply it to"
        );
        if kind == "mode_set_request" {
            self.mail(
                self.lead_inbox.clone(),
                format!(
                    "{name} runs on the {cli} CLI, which has no ganja permission mode to set: \
                     the mode_set_request was read and dropped. Its posture is the one pinned at \
                     spawn, and that one holds for every turn.",
                    name = self.spec.name.as_str(),
                    cli = backend_name(self.driver().backend()),
                ),
            )
            .await;
        }
    }

    /// A JSON object carrying a `type` this build has never heard of.
    ///
    /// Dropped rather than delivered as prose — the deliberate divergence from
    /// the in-process runner — and the sender is told, because
    /// `Delivery::Acknowledged` prunes a dropped message exactly as it prunes a
    /// consumed one: without the mail a peer watches its queue entry retire and
    /// learns nothing. False positives are not exotic either — a JSON Schema,
    /// an OpenAPI fragment or any API payload somebody is asking a teammate
    /// about carries a top-level `"type"`.
    async fn drop_unknown(&self, name: Option<&str>, from: &str) {
        let named = name.unwrap_or("(unnamed)");
        self.remember(format!(
            "dropped frame-shaped message of unknown type {named}"
        ));
        tracing::warn!(
            teammate = self.spec.name.as_str(),
            from,
            kind = named,
            "a message shaped like a frame this build cannot read was dropped rather than \
             composed into a foreign CLI's prompt"
        );
        let Ok(sender) = MemberName::parse(from) else {
            tracing::warn!(
                teammate = self.spec.name.as_str(),
                from,
                "and its sender's name cannot be addressed, so nobody could be told"
            );

            return;
        };
        let inbox = self.spec.root.inbox_path(&self.spec.team, &sender);
        self.mail(
            inbox,
            format!(
                "That message was not delivered. It is a JSON object carrying a \"type\" of \
                 {named:?}, which this build does not recognize as a frame, and {name} runs on \
                 the {cli} CLI — a message shaped like a frame is never composed into that CLI's \
                 prompt. Send prose, or a JSON document with no top-level \"type\" key.",
                name = self.spec.name.as_str(),
                cli = backend_name(self.driver().backend()),
            ),
        )
        .await;
    }

    /// One inbox message, one CLI turn.
    async fn take_turn(&self, from: &str, text: &str) -> Result<(), Failure> {
        let prompt = runner::envelope(from, text);
        let cli = backend_name(self.driver().backend());
        self.remember(format!("turn on {cli} · {} bytes in", prompt.len()));

        let reply = match self.driver().shape() {
            Shape::Resident => self.resident_turn(&prompt).await,
            Shape::PerMessage => self.per_message_turn(&prompt).await,
        }?;

        if let Some(session) = reply.session {
            *self
                .session
                .lock()
                .expect("the shim session is never poisoned") = Some(session);
        }
        let count = reply.messages.len();
        for message in reply.messages {
            self.mail(self.lead_inbox.clone(), message).await;
        }
        self.remember(format!("turn ended on {cli} · {count} message(s) out"));
        // After the session and after the words: a turn the CLI stopped is
        // still a conversation to resume, and whatever it managed to say before
        // stopping is still owed to whoever asked.
        if let Some(reason) = reply.refused {
            return Err(Failure::Refused {
                reason,
                spoke: count > 0,
            });
        }

        Ok(())
    }

    /// The observed conversation id, for a driver composing a resume.
    fn session(&self) -> Option<String> {
        self.session
            .lock()
            .expect("the shim session is never poisoned")
            .clone()
    }

    /// One turn as a child of its own.
    async fn per_message_turn(&self, prompt: &str) -> Result<Reply, Failure> {
        let held = if self.driver().door() == Door::File {
            Some(Prompt::write(&self.launch().tmp, prompt).map_err(Failure::Local)?)
        } else {
            None
        };
        let session = self.session();
        let turn = Turn {
            spec: &self.spec,
            text: prompt,
            prompt: held.as_ref().map(Prompt::path),
            session: session.as_deref(),
            deadline: self.deadline,
        };
        let argv = self.driver().argv(&turn);
        let mut child = self
            .launch()
            .command(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                Failure::Local(format!(
                    "{} could not be started: {error}",
                    self.driver().binary()
                ))
            })?;
        let Some(pid) = child.id().map(|pid| i32::try_from(pid).unwrap_or_default()) else {
            return Err(Failure::Local(
                "the child was reaped before its pid could be read".to_owned(),
            ));
        };
        self.entered(pid);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut stdin = child.stdin.take();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let door = self.driver().door();
        let halt = {
            let body = async {
                // All four run together, and **the prompt write is one of
                // them**: a pipe holds some sixteen kilobytes, so a prompt
                // larger than that against a CLI that is not reading would
                // block this side forever — outside the deadline, which is
                // exactly the wedge the deadline exists for. Reading the two
                // output pipes concurrently is the same argument in the other
                // direction: a child whose output filled a pipe while this side
                // waited on its exit would never exit.
                let (_, _, _, status) = tokio::join!(
                    async {
                        if let Some(mut stdin) = stdin.take() {
                            if door == Door::Stdin {
                                let _ = stdin.write_all(prompt.as_bytes()).await;
                            }
                            // Closed either way: a CLI waiting on an EOF it
                            // never gets is the same wedge again.
                            let _ = stdin.shutdown().await;
                        }
                    },
                    async {
                        if let Some(stdout) = stdout.as_mut() {
                            stdout.read_to_end(&mut out).await
                        } else {
                            Ok(0)
                        }
                    },
                    async {
                        if let Some(stderr) = stderr.as_mut() {
                            stderr.read_to_end(&mut err).await
                        } else {
                            Ok(0)
                        }
                    },
                    child.wait(),
                );

                status
            };
            tokio::select! {
                () = self.handle.cancel().cancelled() => Halt::Cancelled,
                () = self.registry.cancelled() => Halt::Cancelled,
                outcome = tokio::time::timeout(self.deadline, body) => match outcome {
                    Ok(status) => Halt::Finished(status),
                    Err(_) => Halt::Deadline,
                },
            }
        };
        if matches!(halt, Halt::Cancelled | Halt::Deadline) {
            self.end_group(pid, &mut child).await;
        }
        self.left(pid);
        drop(held);

        match halt {
            Halt::Finished(Ok(status)) if status.success() => {
                let stdout = String::from_utf8_lossy(&out).into_owned();
                self.driver()
                    .reply(&stdout)
                    .map_err(|reason| Failure::Unreadable { reason })
            }
            Halt::Finished(Ok(status)) => Err(Failure::Exit {
                status: status_of(&status),
                stderr: first_line(&String::from_utf8_lossy(&err)),
            }),
            Halt::Finished(Err(error)) => Err(Failure::Local(format!(
                "the child could not be waited on: {error}"
            ))),
            Halt::Deadline => Err(Failure::Deadline {
                after: self.deadline,
            }),
            Halt::Cancelled => Err(Failure::Cancelled),
        }
    }

    /// TERM the group, wait out [`SETTLE`], KILL it — the same unwind a kill
    /// does, because a deadline and a kill are the same fact about a child that
    /// will not stop.
    async fn end_group(&self, pgid: i32, child: &mut tokio::process::Child) {
        signal_group(pgid, libc::SIGTERM);
        if tokio::time::timeout(SETTLE, child.wait()).await.is_err() {
            signal_group(pgid, libc::SIGKILL);
            let _ = child.start_kill();
        }
    }

    /// One turn on the resident child's stdin.
    async fn resident_turn(&self, prompt: &str) -> Result<Reply, Failure> {
        let session = self.session();
        let turn = Turn {
            spec: &self.spec,
            text: prompt,
            prompt: None,
            session: session.as_deref(),
            deadline: self.deadline,
        };
        let line = self.driver().line(&turn).map_err(Failure::Local)?;
        let mut held = self
            .resident
            .lock()
            .expect("the resident pipes are never poisoned")
            .take()
            .ok_or_else(|| Failure::Local("this member's resident child is gone".to_owned()))?;

        let outcome = self.drive_resident(&mut held, &line).await;
        // A wedged or broken child is ended rather than left holding the
        // member, and then **replaced** (**AC-7**): a dead pipe never silently
        // becomes a member that answers nothing, and a member retired over one
        // bad turn would make every transient failure permanent — which is the
        // rule `report` already states for the per-message shape and which a
        // resident one has to spend a process to keep.
        if matches!(
            outcome,
            Err(Failure::Deadline { .. } | Failure::Local(_) | Failure::Unreadable { .. })
        ) {
            // Read **before** the child is ended: `end_group` awaits the reap,
            // and a reaped child reports no pid, so asking afterwards leaves
            // the record naming a process that no longer exists.
            let pid = held
                .child
                .id()
                .map(|pid| i32::try_from(pid).unwrap_or_default());
            if let Some(group) = self.handle.group() {
                self.end_group(group, &mut held.child).await;
            }
            if let Some(pid) = pid {
                self.forget(pid);
            }
            drop(held);
            // Idle rather than ended: this member is between children, and the
            // replacement is started after the failure has been reported so
            // that the two mails arrive in the order a person reads them —
            // what went wrong, then what was done about it.
            self.handle.left();
        } else {
            *self
                .resident
                .lock()
                .expect("the resident pipes are never poisoned") = Some(held);
        }

        outcome
    }

    /// Starts a replacement child where this member has lost one (**AC-7**).
    ///
    /// Derived from state rather than from a flag somebody has to remember to
    /// set: a resident member with no pipes and no verdict is a member between
    /// children, and there is exactly one thing to do about it. A per-message
    /// member is never in that state — it holds no child between turns — and a
    /// member that has ended stays ended.
    async fn replace_lost_child(&self) {
        let lost = self.driver().shape() == Shape::Resident
            && self.handle.running() != Running::Ended
            && self
                .resident
                .lock()
                .expect("the resident pipes are never poisoned")
                .is_none();
        if lost {
            self.respawn().await;
        }
    }

    /// Replaces a resident child that will not answer again (**AC-7**).
    ///
    /// Called only after the wedged one's group has been ended and its record
    /// taken back, so this side is never running two children for one member.
    ///
    /// # What the new child resumes, and what it does not
    ///
    /// The argv is composed through [`Driver::argv`] with **whatever
    /// conversation id this member has observed**, which is the whole of the
    /// resume rule: a CLI that told this side its conversation id gets asked
    /// for that conversation again, and one that never did gets a fresh
    /// process with no resume flag at all. Composing a resume off anything
    /// else — a "most recent conversation" door, say — would hand this member
    /// somebody else's transcript, which is why the per-CLI modules ban those
    /// flags by name rather than merely not composing them.
    ///
    /// Both outcomes are **mail**, never silence: a lead that has just been
    /// told a turn failed needs the next sentence to say whether the teammate
    /// it was talking to still exists, and — where the context is gone — that
    /// the next message starts a conversation rather than continuing one. This
    /// is the one place a *shim* member reports the thing D-3's post-restart
    /// case cannot, and the difference is that here the member is live and its
    /// identity is not in question.
    async fn respawn(&self) {
        let session = self.session();
        let argv = self.driver().argv(&Turn {
            spec: &self.spec,
            text: "",
            prompt: None,
            session: session.as_deref(),
            deadline: self.deadline,
        });
        let cli = backend_name(self.driver().backend());
        let started = open_resident(self.launch(), &argv, self.driver().binary());
        let (started, pid) = match started {
            Ok(started) => started,
            Err(reason) => {
                // Nothing left to run this member on. It ends here rather than
                // lingering as a row that answers every message with the same
                // failure.
                self.remember(format!("{cli} could not be restarted · {reason}"));
                self.handle.ended();
                self.mail(
                    self.lead_inbox.clone(),
                    format!(
                        "{name} could not be restarted after that failure: {reason}. The \
                         teammate is finished; spawn another to carry on its work.",
                        name = self.spec.name.as_str()
                    ),
                )
                .await;

                return;
            }
        };
        // The old child's first complaint belongs to the old child. Cleared
        // before the new one's stderr is drained, or a later failure would be
        // reported with a sentence a dead process said.
        *self
            .complaint
            .lock()
            .expect("the complaint is never poisoned") = None;
        self.drain_stderr(started.stderr);
        *self
            .resident
            .lock()
            .expect("the resident pipes are never poisoned") = Some(Resident {
            child: started.child,
            stdin: started.stdin,
            stdout: BufReader::new(started.stdout).lines(),
        });
        self.entered(pid);

        let (line, told) = match session {
            Some(id) => (
                format!("{cli} restarted · resumed {id}"),
                format!(
                    "{name} has been restarted on a fresh {cli} process, resuming \
                     conversation {id}. The next message it is sent starts a turn \
                     there.",
                    name = self.spec.name.as_str()
                ),
            ),
            None => (
                format!("{cli} restarted · context lost, fresh session"),
                format!(
                    "{name} has been restarted on a fresh {cli} process. That CLI \
                     had not yet named a conversation, so there was nothing to \
                     resume: context lost, fresh session — the next message it is \
                     sent begins a new conversation, and anything said before it \
                     is gone.",
                    name = self.spec.name.as_str()
                ),
            ),
        };
        self.remember(line);
        self.mail(self.lead_inbox.clone(), told).await;
    }

    /// Writes one line and reads until the driver says the turn is over.
    async fn drive_resident(&self, held: &mut Resident, line: &str) -> Result<Reply, Failure> {
        // The write is **inside** the deadline for the per-message shape's
        // reason: a resident CLI that has stopped reading its stdin blocks this
        // side on a full pipe, and a turn that can hang before its deadline
        // starts has no deadline.
        let Resident { stdin, stdout, .. } = held;
        let turn = async {
            if let Err(error) = stdin.write_all(format!("{line}\n").as_bytes()).await {
                return Err(self.wedged("the turn could not be written", &error).await);
            }
            if let Err(error) = stdin.flush().await {
                return Err(self.wedged("the turn could not be written", &error).await);
            }

            loop {
                match stdout.next_line().await {
                    Ok(Some(line)) => match self.driver().read(&line) {
                        Read::Ignored => {}
                        Read::Done(reply) => return Ok(reply),
                        Read::Refused(reason) => return Err(Failure::Unreadable { reason }),
                    },
                    Ok(None) => {
                        let complaint = self.settled_complaint().await;
                        let reason = if complaint.is_empty() {
                            "the child closed its output before the turn was over".to_owned()
                        } else {
                            format!(
                                "the child closed its output before the turn was over: \
                                 {complaint}"
                            )
                        };

                        return Err(Failure::Unreadable { reason });
                    }
                    Err(error) => {
                        return Err(Failure::Local(format!(
                            "the child's output could not be read: {error}"
                        )));
                    }
                }
            }
        };

        tokio::select! {
            () = self.handle.cancel().cancelled() => Err(Failure::Cancelled),
            () = self.registry.cancelled() => Err(Failure::Cancelled),
            outcome = tokio::time::timeout(self.deadline, turn) => match outcome {
                Ok(outcome) => outcome,
                Err(_) => Err(Failure::Deadline { after: self.deadline }),
            },
        }
    }

    /// Records a child that has started, in the handle and in the orphan
    /// records both.
    fn entered(&self, pid: i32) {
        self.handle.entered(pid);
        self.record(pid);
    }

    /// Writes one child into the orphan records.
    fn record(&self, pid: i32) {
        let cli = self.driver().cli();
        if let records::Started::At(started) = records::started_at(pid) {
            self.shims
                .lock()
                .expect("the shim records are never poisoned")
                .add(records::Recorded {
                    cli,
                    process: records::Identity { pid, started },
                    // Every shim child is its own group leader, so the group is
                    // the pid — recorded beside it because the leader can die
                    // while the group lives, and that case is decided on the
                    // two values separately.
                    pgid: pid,
                });
        }
    }

    /// Forgets a child that has been reaped.
    fn left(&self, pid: i32) {
        self.handle.left();
        self.forget(pid);
    }

    /// Takes one child back out of the orphan records.
    fn forget(&self, pid: i32) {
        self.shims
            .lock()
            .expect("the shim records are never poisoned")
            .remove(pid);
    }

    /// Ends whatever this member owns, without answering anybody.
    async fn retire_child(&self) {
        let held = self
            .resident
            .lock()
            .expect("the resident pipes are never poisoned")
            .take();
        if let Some(mut held) = held {
            let pid = held
                .child
                .id()
                .map(|pid| i32::try_from(pid).unwrap_or_default());
            // Layer 2 first: a well-behaved CLI exits on stdin EOF. Stated as
            // an expectation about a foreign binary, which is why the group
            // signal below is not conditional on it.
            drop(held.stdin);
            if let Some(group) = self.handle.group() {
                self.end_group(group, &mut held.child).await;
            }
            if let Some(pid) = pid {
                self.forget(pid);
            }
        } else if let Some(group) = self.handle.group() {
            signal_group(group, libc::SIGTERM);
        }
    }

    /// Ends this member and tells the lead it is done.
    ///
    /// The `from` on that answer is **this member's own name**, taken from the
    /// value the loop was built with and never from the request being answered:
    /// a message that carried its own sender would let whoever wrote it choose
    /// whose name the lead reads.
    async fn tear_down(&self, request: &ShutdownRequest) {
        self.retire_child().await;
        self.handle.ended();
        let surface = Surface::Shim {
            cli: self.driver().cli(),
            // A headless child owns no pane; the pane-mode shim is
            // `crate::teammate::shim_tui`'s, and writes its own surface.
            pane: None,
        };
        let approved = Frame::ShutdownApproved(ShutdownApproved {
            request_id: request.request_id.clone(),
            from: self.spec.name.as_str().to_owned(),
            timestamp: record::now_iso8601(),
            pane_id: Some(surface.tmux_pane_id().to_owned()),
            backend_type: Some(surface.backend_type().to_owned()),
        });
        runner::write_frame(
            self.lead_inbox.clone(),
            self.spec.name.as_str(),
            &approved,
            "a shutdown answer",
        )
        .await;
    }

    /// Tells the lead a turn failed, and leaves the member spawnable-to.
    ///
    /// The member stays alive on purpose: a CLI that refused one turn is a CLI
    /// that will take the next one, and retiring a teammate over a non-zero
    /// exit would make every transient failure permanent.
    async fn report(&self, failure: &Failure) {
        if matches!(failure, Failure::Cancelled) {
            // Nobody is left to read it: a cancel is the lead ending this
            // member, and the mail would arrive after its reader.
            return;
        }
        let cli = backend_name(self.driver().backend());
        let sentence = failure.sentence(cli);
        self.remember(format!("turn failed on {cli} · {}", failure.summary()));
        tracing::warn!(teammate = self.spec.name.as_str(), cli, "{sentence}");
        // Branched on what the lead has already read rather than fixed, because
        // the fixed opening is a lie in exactly one case: a turn that mailed
        // half an answer and then stopped, where "did not produce an answer"
        // contradicts the message directly above it in the same inbox.
        let opening = match failure {
            Failure::Refused { spoke: true, .. } => "ended without completing",
            _ => "did not produce an answer",
        };
        self.mail(
            self.lead_inbox.clone(),
            format!(
                "{name}'s turn {opening}. {sentence} The teammate is still running, and the next \
                 message it is sent starts a fresh turn.",
                name = self.spec.name.as_str()
            ),
        )
        .await;
    }

    /// One line onto this member's ring.
    fn remember(&self, line: String) {
        push_recent(&self.recent, line);
    }

    /// Writes one plain message into an inbox, as this member.
    async fn mail(&self, inbox: PathBuf, text: String) {
        let message = MailboxMessage::new(self.spec.name.as_str(), text, record::now_iso8601());
        if let Err(error) =
            crate::teammate::blocking_io(move || mailbox::write(&inbox, message)).await
        {
            tracing::error!(
                who = self.spec.name.as_str(),
                %error,
                "a shim teammate's message could not be written, so nobody is being told"
            );
        }
    }

    /// Takes everything this pass finished out of the inbox, in one write.
    async fn prune(&self, handled: Vec<mailbox::Identity>) {
        runner::prune_inbox(self.inbox(), handled, self.spec.name.as_str()).await;
    }
}

/// Why a per-message child stopped being waited on.
enum Halt {
    /// It ran to completion, for whatever the operating system then said.
    Finished(std::io::Result<std::process::ExitStatus>),
    /// The per-turn deadline fired.
    Deadline,
    /// The lead ended this member mid-turn.
    Cancelled,
}

/// Why a turn ended without an answer.
///
/// Five shapes the lead reads differently — a non-zero exit, output this build
/// cannot parse, a turn the CLI itself ended without an answer, a deadline, and
/// something that went wrong on this side before the CLI was reached — plus the
/// one that is never mailed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The CLI exited non-zero.
    Exit {
        /// How it exited, rendered.
        status: String,
        /// The first line of its stderr, capped.
        stderr: String,
    },
    /// It exited cleanly and said something this build cannot read.
    Unreadable {
        /// What the driver said was wrong with it.
        reason: String,
    },
    /// It ran the turn, ended it without completing, and said why.
    ///
    /// Distinct from [`Failure::Unreadable`] because the two are opposite
    /// facts: that one is this build failing to read a child, and this one is
    /// this build reading it exactly and reporting what it found. The reason is
    /// the driver's own complete sentence, so it stands alone rather than being
    /// wrapped in one of this side's.
    Refused {
        /// The CLI's own account, as its driver phrased it.
        reason: String,
        /// Whether the turn had already said something to the lead before it
        /// stopped.
        ///
        /// Carried because the mail opens on it: a turn that mailed half an
        /// answer and *then* stopped has not "produced no answer", and telling
        /// a lead it did — in the message right after the words themselves —
        /// is a contradiction a reader has to resolve against the evidence in
        /// their own inbox.
        spoke: bool,
    },
    /// The per-turn deadline fired.
    Deadline {
        /// The deadline that fired, so the mail can name it beside the key that
        /// moves it.
        after: Duration,
    },
    /// This side could not run the turn at all.
    Local(String),
    /// The lead ended this member mid-turn. Never mailed — the reader is gone.
    Cancelled,
}

impl Failure {
    /// The sentence the lead reads.
    #[must_use]
    pub fn sentence(&self, cli: &str) -> String {
        match self {
            Self::Exit { status, stderr } if stderr.is_empty() => {
                format!("{cli} exited {status} and said nothing on stderr.")
            }
            Self::Exit { status, stderr } => format!("{cli} exited {status}: {stderr}"),
            Self::Unreadable { reason } => {
                format!("{cli} finished, and this build could not read what it wrote: {reason}")
            }
            Self::Refused { reason, .. } => reason.clone(),
            Self::Deadline { after } => format!(
                "{cli} was still running after {seconds}s, so its process group was ended. Set \
                 {TIMEOUT_KEY} (in seconds) to give it longer.",
                seconds = after.as_secs()
            ),
            Self::Local(reason) => format!("the {cli} turn could not be run at all: {reason}"),
            Self::Cancelled => format!("the {cli} turn was cancelled."),
        }
    }

    /// The same fact, short enough for a ring row.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Exit { status, .. } => format!("exited {status}"),
            Self::Unreadable { .. } => "unreadable output".to_owned(),
            Self::Refused { spoke: false, .. } => "ended without an answer".to_owned(),
            Self::Refused { spoke: true, .. } => "stopped part-way".to_owned(),
            Self::Deadline { after } => format!("deadline of {}s fired", after.as_secs()),
            Self::Local(_) => "could not be run".to_owned(),
            Self::Cancelled => "cancelled".to_owned(),
        }
    }
}

/// How a process exited, in the words a person reads.
fn status_of(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;

    match (status.code(), status.signal()) {
        (Some(code), _) => format!("with status {code}"),
        (None, Some(signal)) => format!("on signal {signal}"),
        (None, None) => "for a reason the operating system did not name".to_owned(),
    }
}

/// The first line of a CLI's stderr, capped at [`DISPLAY_FIELD_CAP`].
///
/// The first line rather than the whole of it, because a vendor's stderr can be
/// a stack trace and what a lead needs is the sentence at the top of it. Cut on
/// a character boundary, since a CLI's own message is not necessarily ASCII.
#[must_use]
pub fn first_line(stderr: &str) -> String {
    let line = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    match line.char_indices().nth(DISPLAY_FIELD_CAP) {
        Some((end, _)) => line[..end].to_owned(),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, time::Duration};

    use ganja_protocol::team::{DISPLAY_FIELD_CAP, MemberBackend};
    use ganja_team::ShimCli;

    use super::{
        AGY_TURN_TIMEOUT, CARRIED, CODEX_TURN_TIMEOUT, Failure, GROK_MODE_LINE, GROK_TURN_TIMEOUT,
        TIMEOUT_KEY, admits, default_turn_timeout, environment, first_line, preamble, resolve,
        spawn_lines,
    };
    use crate::teammate::{posture_line, preamble::Names};

    /// The headless channel says that answers are mail and that there is no
    /// door to go looking for, in the CLI's own name — and says **how much**
    /// is mailed per CLI, since codex's driver forwards every message where
    /// grok's and agy's forward one answer per turn — and the task is what the
    /// message ends with (**D514**).
    #[test]
    fn the_headless_preamble_says_how_much_of_an_answer_is_mail_and_ends_with_the_task() {
        let who = Names {
            name: "w1",
            team: "session-abcd1234",
            lead: "team-lead",
        };
        for (backend, cli, answers) in [
            (
                MemberBackend::Codex,
                "codex",
                "every message you print in answer is delivered to the lead as mail, in order",
            ),
            (
                MemberBackend::Agy,
                "agy",
                "only your final answer for the turn reaches the lead",
            ),
            (
                MemberBackend::Grok,
                "grok",
                "only your final answer for the turn reaches the lead",
            ),
        ] {
            let text = preamble(who, backend, "hold the fort");
            assert!(
                text.contains(&format!("headless {cli} process")),
                "{cli}: the channel is in the CLI's name: {text}"
            );
            assert!(
                text.contains(answers),
                "{cli}: the answer road is said as the driver walks it: {text}"
            );
            assert!(
                text.ends_with("Your task:\n\nhold the fort"),
                "{cli}: {text}"
            );
        }
    }

    #[test]
    fn every_cli_carries_the_deadline_this_plan_derived_for_it() {
        assert_eq!(default_turn_timeout(ShimCli::Agy), AGY_TURN_TIMEOUT);
        assert_eq!(default_turn_timeout(ShimCli::Codex), CODEX_TURN_TIMEOUT);
        assert_eq!(default_turn_timeout(ShimCli::Grok), GROK_TURN_TIMEOUT);
        // agy's is the derived one — its own `--print-timeout` is composed at
        // deadline + 1m so the shim always fires first — and it is deliberately
        // shorter than the two provisional numbers.
        assert!(AGY_TURN_TIMEOUT < CODEX_TURN_TIMEOUT);
        assert_eq!(AGY_TURN_TIMEOUT, Duration::from_secs(240));
    }

    /// The three that travel, and nothing else — the whole of D502's posture as
    /// this side applies it.
    #[test]
    fn a_child_gets_exactly_the_enumerated_names() {
        let carried = environment(&["CODEX_HOME"], None);
        for (name, _) in &carried {
            let name = name.to_string_lossy().into_owned();
            assert!(
                CARRIED.contains(&name.as_str()) || name == "CODEX_HOME",
                "{name} is not in the enumeration"
            );
        }
        // An explicit path decides what the child's own PATH is, which is how a
        // fake CLI is reached without mutating the process under test.
        let pointed = environment(&[], Some(&OsString::from("/opt/fake/bin")));
        assert_eq!(
            pointed
                .iter()
                .find(|(name, _)| name == "PATH")
                .map(|(_, value)| value.clone()),
            Some(OsString::from("/opt/fake/bin"))
        );
    }

    /// The class rule, **enforced**: a driver that names a `GROK_*` variable
    /// does not get it, whatever its additions list says.
    ///
    /// The loud half of the same rule is a `debug_assert` at `prepare`, where a
    /// driver's own list is first consulted. It is there rather than here for
    /// exactly this test's sake: an assertion inside `environment` would panic
    /// on the call below, and then the safe fallback — the thing that actually
    /// protects a person's consent in a release build — would be untested.
    #[test]
    fn no_grok_variable_is_ever_in_the_enumeration() {
        for name in CARRIED {
            assert!(!name.starts_with("GROK_"), "{name}");
        }

        assert!(!admits("GROK_SANDBOX"));
        assert!(!admits("GROK_SANDBOX_AUTO_ALLOW_BASH"));
        assert!(!admits("GROK_SANDBOX_PROFILE"));
        // The fourth one the vendor has not added yet, which is the whole
        // reason this is a prefix and not three names.
        assert!(!admits("GROK_ANYTHING_AT_ALL"));
        assert!(admits("CODEX_HOME"));
        assert!(admits("HOME"));
        // Not a substring rule: a name that merely *contains* the prefix is
        // somebody else's variable and travels.
        assert!(admits("MY_GROK_NOTES"));

        assert!(
            environment(&["GROK_SANDBOX"], None)
                .iter()
                .all(|(name, _)| name != "GROK_SANDBOX"),
            "a driver naming one does not get it"
        );
    }

    #[test]
    fn an_explicit_path_decides_which_binary_a_shim_would_run() {
        // A relative or empty component is dropped before `which` sees the
        // list, so a turn's incidental directory can never supply a teammate
        // binary.
        assert!(resolve(&OsString::from(""), "sh").is_none());
        assert!(resolve(&OsString::from("relative/bin"), "sh").is_none());
        assert!(resolve(&OsString::from("/usr/bin:/bin"), "sh").is_some());
        assert!(resolve(&OsString::from("/usr/bin:/bin"), "no-such-binary-here").is_none());
    }

    /// The ring's first line and the spawn dialog's sentence are one table read
    /// twice, which is what AC-17 asserts by comparing them rather than by two
    /// string literals.
    #[test]
    fn a_spawn_ring_line_carries_the_same_posture_the_dialog_does() {
        // `Agy` is deliberately absent: W4 measured its floor and it does not
        // hold, so that backend never spawns and has no posture to state. It
        // is asserted the other way — `posture_line` answers `None` and
        // `spawn_lines` is empty — in `teammate_shim_agy.rs`.
        for backend in [MemberBackend::Codex, MemberBackend::Grok] {
            let lines = spawn_lines(backend);
            let posture = posture_line(backend).expect("a shim backend states its posture");
            assert!(lines[0].ends_with(posture), "{lines:?}");
            assert!(
                lines[1].contains("bounded by"),
                "the honest rider travels beside the grant: {lines:?}"
            );
        }
        assert_eq!(
            spawn_lines(MemberBackend::Grok).last().map(String::as_str),
            Some(GROK_MODE_LINE),
            "grok's third line says what the composed mode actually does"
        );
        assert!(
            spawn_lines(MemberBackend::InProcess).is_empty(),
            "a backend with no pinned posture writes no posture line"
        );
    }

    #[test]
    fn a_timeout_mail_names_both_the_deadline_and_the_key_that_moves_it() {
        let sentence = Failure::Deadline {
            after: Duration::from_secs(900),
        }
        .sentence("codex");

        assert!(sentence.contains("900s"), "{sentence}");
        assert!(sentence.contains(TIMEOUT_KEY), "{sentence}");
    }

    #[test]
    fn a_failure_mail_names_the_cli_the_status_and_the_first_stderr_line() {
        let sentence = Failure::Exit {
            status: "with status 3".to_owned(),
            stderr: "error: not logged in".to_owned(),
        }
        .sentence("grok");

        assert!(sentence.contains("grok"), "{sentence}");
        assert!(sentence.contains("with status 3"), "{sentence}");
        assert!(sentence.contains("not logged in"), "{sentence}");
    }

    #[test]
    fn only_the_first_stderr_line_travels_and_it_is_capped() {
        assert_eq!(
            first_line("\n\nerror: no\nstack trace line\nanother"),
            "error: no"
        );
        assert_eq!(first_line(""), "");

        let wide: String = "あ".repeat(DISPLAY_FIELD_CAP * 2);
        assert_eq!(first_line(&wide).chars().count(), DISPLAY_FIELD_CAP);
    }
}
