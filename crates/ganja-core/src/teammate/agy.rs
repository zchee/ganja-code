//! The teammate that **ships write-capable, and says so** (**Dv-7**, amending
//! **D508(a)**).
//!
//! Spec: none. Upstream opencode has no teammates and Claude Code does not run
//! another vendor's agent as one, so every sentence here is ganja's own, and
//! the vendor surface it is written against is the binary itself — `agy 1.1.15`
//! (the Antigravity CLI), probed on this machine rather than read out of a
//! checkout that may not match what a person has installed.
//!
//! # What changed, and what did not
//!
//! W4 measured this CLI's floor and refused to ship it: `--sandbox` bounds
//! agy's *terminal* and not its filesystem, so an agy teammate is a foreign
//! agent holding its own tools with no enforced filesystem bound, one consent
//! at spawn and no permission channel. **That measurement is not disputed and
//! is not repealed** — the W4 recording still stands, and this build reproduced
//! its consequence directly: under the shipped launch line a turn asked for a
//! file inside its working directory and got one.
//!
//! What changed is the *decision*, on a user directive recorded as Dv-7: v1's
//! "grant read, not write" cut is extended for this one CLI, deliberately and
//! on the record, and the consent surface says exactly what that means rather
//! than implying a bound agy does not have. So the sentence in
//! [`posture_line`] is not a description of a
//! sandbox — it is a description of **its absence**, which is the honest thing
//! to put in front of somebody being asked to approve one.
//!
//! `--sandbox` is composed anyway. Not as a filesystem bound, which it is not,
//! but because it is a real bound on the child's *terminal* and defence in
//! depth costs nothing here. `--mode plan` is **not** composed: it would neuter
//! the writes that shipping this backend exists to enable.
//!
//! # The wire
//!
//! The only [`Shape::Resident`](crate::teammate::shim::Shape::Resident) driver this build ships, and the vendor's own
//! flag is what makes it one: *"stream-json reads one NDJSON message per line
//! from stdin and runs a turn for each; it requires `--output-format
//! stream-json`"*. One child for the member's whole life, one line per turn.
//!
//! Both directions are keyed on **`event`** rather than on `type`, which is
//! worth stating because every other NDJSON wire in this tree is keyed on the
//! other word. Inbound is `{"event":"user","message":{"content":"…"}}` —
//! measured against the vendor's own decoder, which names the field it wants in
//! each of its three refusals (a missing `event`, a missing `message`, a
//! `content` that is *"a string or a list of content blocks"*).
//!
//! Outbound is three kinds: one `init` for the whole child, carrying the
//! `conversation_id`; a stream of `step_update`s; and one `result` per turn.
//! **A turn ends at `result` and nowhere else**, which is what makes this shape
//! drivable at all.
//!
//! ## Why one mail per turn, off `result.response`
//!
//! Because it is the only text there is. A `step_update` carries `step_index`,
//! `state`, `step_type`, timings and token usage — and no words at all, not
//! even on the `agent_response` kind whose name promises them. The whole of
//! what the agent said arrives once, in `result.response`. So this driver has
//! no forwarding choice to make, unlike codex's, which mails every
//! `AgentMessage` because that vendor writes no final field.
//!
//! The consequence is worth naming rather than discovering: a lead hears from
//! an agy teammate **once per turn**, at the end. Its intermediate narration is
//! not withheld, it is not on the wire.
//!
//! ## Anything that is not a `result` is ignored
//!
//! Including a line this build cannot parse. The forward-compatibility posture
//! is codex's and grok's — a vendor printing one more kind, or one more field,
//! must not cost a turn that otherwise succeeded — and here it is load-bearing
//! twice over, since the vendor also prints ordinary warnings on this stream.
//!
//! ## One vendor quirk this driver reports rather than hides
//!
//! `result.status` and `result.error` are **sticky across turns of one
//! conversation**: the ship probe's second turn answered correctly and its own
//! result still carried `ERROR` and the first turn's sentence, about a tool it
//! never asked for. So from the first errored turn onwards, every turn on that
//! conversation reaches the lead as words **plus** a failure.
//!
//! Reported faithfully anyway, and the choice is deliberate: suppressing a
//! status the vendor set is a policy decision rather than a parsing one, and
//! the direction of that error matters — a build that learned to ignore an
//! `ERROR` because it *might* be stale is a build that will one day ignore a
//! real one. Recorded in `tests/fixtures/agy-posture-probe.txt` and carried as
//! a follow-up.
//!
//! # The two traps this file was written around
//!
//! Both are W4's findings (**Dv-6**), and both would have cost a debugging
//! session:
//!
//! - **`-p`/`--print` takes the prompt as its flag value.** agy parses with
//!   Go's `flag` package, so `agy -p --input-format stream-json …` parses
//!   `--input-format` *as the prompt*. [`Driver::argv`](crate::teammate::shim::Driver::argv) therefore puts `-p` last
//!   and gives it an explicit empty value. A test pins the position, because
//!   the failure it prevents is a turn that answers a question nobody asked.
//! - **agy runs shell commands with cwd = its own scratch directory**, not the
//!   directory it was launched from. The child is still launched in
//!   `SpawnSpec.cwd` and its *file* tools honour it — this build's own ship
//!   probe wrote a file there and read one back — but a `run_command` step is
//!   somewhere else, and anybody reading a shell step's output should know it.
//!
//! # `--print-timeout` is ordered against the shim's own deadline
//!
//! Two timeouts bound one turn, and the wrong order wedges it: if agy's own
//! `--print-timeout` fires first, this side is left reading a pipe that will
//! never carry a `result`. So the flag is **derived** from the effective
//! deadline rather than fixed — [`print_timeout`](crate::teammate::agy::print_timeout) renders `deadline + 1m` as
//! the Go duration that flag parses — which is what makes
//! [`TIMEOUT_KEY`](crate::teammate::shim::TIMEOUT_KEY) move both numbers
//! together instead of only one of them.
//!
//! Rendered with a unit on purpose: `time.ParseDuration` refuses a bare
//! integer, so `300` is a child that will not start where `300s` is one that
//! will.
//!
//! # No auth pre-check
//!
//! There is none to write, and no `ready` beyond the trait's default: agy
//! offers no `login status` equivalent, so a first-turn failure is the auth
//! surface. Recorded rather than left as an omission, because the shape of
//! that failure is known — a child that cannot authenticate still prints a
//! `result` whose `status` is not `SUCCESS`, which this driver reports as the
//! vendor's own sentence rather than as silence.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use ganja_protocol::team::MemberBackend;
use ganja_team::ShimCli;
use serde::Deserialize;

use crate::teammate::{
    readback,
    shim::{Door, Driver, Read, Reply, Shape, Turn},
};

/// The executable a spawn looks for on `PATH`.
pub const BINARY: &str = "agy";

/// The one value both format flags take, and they must agree.
///
/// A single constant rather than two, because the vendor refuses the pair
/// outright — *"`--input-format stream-json` requires `--output-format
/// stream-json`"* — so a build that spelled them apart would be a build that
/// never starts.
pub const STREAM_JSON: &str = "stream-json";

/// What this side adds to the effective deadline to get `--print-timeout`.
///
/// One minute, which is W4's derivation kept intact: the requirement is only
/// that agy's own timeout fires **strictly after** this build's, and a minute
/// is enough headroom to be sure of it without leaving a wedged child running
/// noticeably long past the mail that reported it.
pub const PRINT_TIMEOUT_HEADROOM: Duration = Duration::from_secs(60);

/// Everything this CLI's argv may **never** carry, in every spelling that
/// binary has.
///
/// The single source for the test that asserts it rather than a list the test
/// repeats. Read off `agy --help` at 1.1.15 rather than guessed, which is what
/// caught the third and fourth entries: `--continue` resumes *the machine's
/// most recent conversation* — which may be another teammate's, or the
/// person's own — and its short alias is `-c`, which a `--continue` grep walks
/// straight past. [`CONVERSATION`] is the resume door that names what it
/// resumes, and it is the only one this file composes.
///
/// **`-c` is why the test that asserts this list compares whole argv tokens
/// rather than substrings**: `"--conversation".contains("-c")` is true, so a
/// substring check would report the flag this driver *must* compose as the one
/// it must never.
///
/// The last entry is a **value** rather than a flag, for the reason grok's list
/// carries six: a posture is escaped as easily by a value as by a flag.
/// `--mode accept-edits` stays banned because this build's probe settled that
/// nothing needs it — a headless turn ran a write tool with no approval prompt
/// and no escalation flag — so composing it would widen a grant in exchange for
/// nothing. `--mode plan`'s absence is not a ban and is not here: it is a mode
/// this driver declines to compose, recorded in the module header.
pub const NEVER_COMPOSED: [&str; 5] = [
    "--dangerously-skip-permissions",
    "--add-dir",
    "--continue",
    "-c",
    "accept-edits",
];

/// The resume door, which names the conversation it resumes.
pub const CONVERSATION: &str = "--conversation";

/// The one floor for a pane running this CLI's own TUI (**D512**): the bool
/// flag, with no value.
///
/// Nothing else of the headless launch line carries, and that was measured
/// rather than assumed: `--input-format`, `--output-format`, `--print-timeout`
/// and `--disable-slash-commands` are print-mode flags, and `-p` is print mode
/// itself — an interactive agy takes its prompt from its composer, and the
/// words travel a tmux paste buffer rather than argv. No identity flag, because
/// that CLI has none to give, and no [`CONVERSATION`], because a TUI holds its
/// own.
///
/// `--sandbox` is composed for the reason it is composed headless — a real
/// bound on the child's *terminal*, and defence in depth — and it is still not
/// a filesystem bound: the TUI opens in **accept-edits mode** ("file edits
/// auto-approved"), which is Dv-7's write-capable posture now drawn on a
/// screen a person can read. Measured on 2026-08-20 against `agy 1.1.16`; the
/// recording, `tests/fixtures/agy-tui-probe.txt`, is what the test compares
/// this against rather than a second literal.
pub const TUI_ARGV: [&str; 1] = ["--sandbox"];

/// What a readiness poll looks for in a captured pane: the footer hint drawn
/// beside the composer once it is up.
///
/// The footer and not the banner or the mode notice, because it is the line
/// the recording shows on screen with the composer — which is what says a
/// paste has somewhere to land. Not a promise that the TUI *stays* ready: a
/// first spawn in an untrusted directory shows a trust dialog first, which is
/// why a poll that never sees this string is a ring note and a proceed rather
/// than a failure. Pinned against the recording, which names the binary it was
/// read off: a release that rewords its footer moves the fixture, not a
/// literal in a test.
pub const READY_MARKER: &str = "? for shortcuts";

/// The outbound record that opens a child and names its conversation.
const INIT: &str = "init";

/// The outbound record that ends a turn.
const RESULT: &str = "result";

/// The one `status` that means the turn answered.
const SUCCESS: &str = "SUCCESS";

/// One outbound NDJSON record, in the minimum shape this side reads.
///
/// Every field is optional and unknown fields are ignored — the same
/// forward-compatibility posture codex's and grok's parsers take.
#[derive(Debug, Default, Deserialize)]
struct Record {
    /// `init`, `step_update` or `result`.
    #[serde(default)]
    event: String,
    /// Carried on `init`, beside the payload rather than inside it.
    #[serde(default)]
    conversation_id: Option<String>,
    /// Carried on `result`.
    #[serde(default)]
    result: Option<Ended>,
}

/// What one finished turn said.
#[derive(Debug, Default, Deserialize)]
struct Ended {
    /// The conversation this turn ran in — the same id `init` announced, and
    /// present on a failed turn too, which is why the session is read from
    /// here rather than only from `init`.
    #[serde(default)]
    conversation_id: Option<String>,
    /// `SUCCESS`, or one of the vendor's several ways of not succeeding.
    #[serde(default)]
    status: String,
    /// The whole of what the agent said this turn.
    #[serde(default)]
    response: String,
    /// Why it did not succeed, in the vendor's own words.
    #[serde(default)]
    error: String,
}

/// `--print-timeout`'s value for a member running under `deadline`.
///
/// Seconds with the unit spelled, because Go's `time.ParseDuration` — which is
/// what reads this flag — refuses a bare integer.
#[must_use]
pub fn print_timeout(deadline: Duration) -> String {
    format!(
        "{}s",
        deadline.saturating_add(PRINT_TIMEOUT_HEADROOM).as_secs()
    )
}

/// A teammate driven through a resident headless `agy`.
///
/// Stateless: the conversation id lives in the shim runner, which is what lets
/// one driver serve every member on this CLI — two agy teammates hold two
/// conversations because the runner holds two ids, not because there are two
/// of these.
#[derive(Clone, Copy, Debug, Default)]
pub struct Agy;

impl Agy {
    /// The backend.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// [`TUI_ARGV`] as the owned words a pane's launch line is composed from.
    ///
    /// Takes no [`Turn`] on purpose: there is no prompt, no conversation and
    /// no deadline to read — and therefore no `--print-timeout` to derive —
    /// so nothing a peer said can reach this argv by construction rather than
    /// by a test.
    #[must_use]
    pub fn tui_argv(&self) -> Vec<OsString> {
        TUI_ARGV.iter().map(OsString::from).collect()
    }
}

#[async_trait]
impl Driver for Agy {
    fn cli(&self) -> ShimCli {
        ShimCli::Agy
    }

    fn backend(&self) -> MemberBackend {
        MemberBackend::Agy
    }

    fn binary(&self) -> &str {
        BINARY
    }

    fn shape(&self) -> Shape {
        Shape::Resident
    }

    fn door(&self) -> Door {
        // One NDJSON line per turn, on the stdin of a child that outlives the
        // turn — so this is `Stdin` in a sense the per-message drivers' is not:
        // the pipe is not closed after it.
        Door::Stdin
    }

    fn additions(&self) -> &[&str] {
        // Nothing. agy keeps its things under `~/.gemini`, and `HOME` is
        // already carried, so there is no home variable to add — the
        // `CODEX_HOME` case has no counterpart here.
        //
        // `GEMINI_API_KEY` is deliberately **not** here. It is a credential,
        // and the enumeration exists precisely so that a foreign child is
        // handed what it needs to be itself and nothing else; a key that
        // reaches it through the vendor's own stored login is a key this build
        // never touched.
        &[]
    }

    fn argv(&self, turn: &Turn<'_>) -> Vec<OsString> {
        let mut argv = Vec::with_capacity(12);
        argv.push(OsString::from("--input-format"));
        argv.push(OsString::from(STREAM_JSON));
        argv.push(OsString::from("--output-format"));
        argv.push(OsString::from(STREAM_JSON));
        // Composed as a bound on the child's terminal, which it is, and not as
        // a filesystem bound, which it is not. See the module header.
        argv.push(OsString::from("--sandbox"));
        // A teammate's prompt is a peer's words. A leading `/` in them is not a
        // command anybody typed, and this vendor would otherwise expand one.
        argv.push(OsString::from("--disable-slash-commands"));
        argv.push(OsString::from("--print-timeout"));
        argv.push(OsString::from(print_timeout(turn.deadline)));
        // Only when a previous child of this member's revealed one. A first
        // launch names no conversation, and `--continue` is never the door
        // taken instead: it would resume whatever this machine touched last,
        // which may be another teammate's conversation or the person's own.
        if let Some(session) = turn.session {
            argv.push(OsString::from(CONVERSATION));
            argv.push(OsString::from(session));
        }
        // **Last, and with an explicit empty value.** `-p` takes the next word
        // as the prompt, so anything after it is eaten; the empty value is what
        // says "print mode, and the prompts arrive on stdin".
        argv.push(OsString::from("-p"));
        argv.push(OsString::new());

        argv
    }

    fn line(&self, turn: &Turn<'_>) -> Result<String, String> {
        serde_json::to_string(&serde_json::json!({
            "event": "user",
            "message": { "content": turn.text },
        }))
        .map_err(|error| format!("this turn could not be encoded as agy stream-json: {error}"))
    }

    fn reply(&self, _stdout: &str) -> Result<Reply, String> {
        // Unreachable through the runner, which asks a resident driver for
        // `read` and never for this. Named rather than left to a default,
        // because the mirror of it — `line`'s default refusing a driver that
        // forgot to override it — is what catches the opposite mistake.
        Err(
            "agy is driven as a resident child, one line at a time, and never read in one piece"
                .to_owned(),
        )
    }

    fn read(&self, line: &str) -> Read {
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            // Not this turn's answer, and not this build's business: the
            // vendor prints warnings on this stream too.
            return Read::Ignored;
        };
        if record.event != RESULT {
            // `init` is where the conversation id is first announced, and it
            // is deliberately *not* captured here: `Read` carries a session
            // only on `Done`, and every `result` carries the same id anyway —
            // including a failed one. One place to read it is one place for it
            // to be wrong.
            if record.event == INIT && record.conversation_id.is_none() {
                // Logged rather than asserted, because this module's own rule
                // is that a vendor printing one more kind — or one fewer
                // field — must not cost a turn that otherwise succeeds. A
                // `debug_assert` here would panic the runner task on a dev
                // build the day agy emits an `init` without an id, and a
                // panicked runner is a teammate that goes silent: the one
                // outcome the whole failure channel exists to rule out.
                tracing::debug!("an agy init record named no conversation");
            }

            return Read::Ignored;
        }
        let Some(ended) = record.result else {
            return Read::Refused(
                "agy ended a turn with a result record carrying no result.".to_owned(),
            );
        };

        // Whatever it managed to say is owed to the lead even when the turn
        // then failed, so the words and the reason travel together rather than
        // one replacing the other.
        let said = ended.response.trim();
        let messages: Vec<String> = if said.is_empty() {
            Vec::new()
        } else {
            vec![said.to_owned()]
        };
        let refused = if ended.status == SUCCESS {
            if messages.is_empty() {
                // The turn succeeded and said nothing. Reported rather than
                // passed off as an empty answer: a teammate that goes quiet is
                // the one outcome Principle 4 exists to rule out.
                Some(
                    "agy ended the turn successfully and said nothing at all, so there is no \
                     answer to pass on."
                        .to_owned(),
                )
            } else {
                None
            }
        } else {
            // Total over the vendor's several ways of not succeeding — this
            // build has seen `ERROR`, and that binary also carries `CANCELLED`
            // and `TIMEOUT` — rather than a match that would read a fourth one
            // as success.
            let why = if ended.error.trim().is_empty() {
                "and named no reason".to_owned()
            } else {
                format!("and said: {}", ended.error.trim())
            };

            Some(format!(
                "agy ended the turn {status} {why}.",
                status = ended.status.trim(),
            ))
        };

        Read::Done(Reply {
            messages,
            // From the result rather than from `init`, and on the failing arm
            // too: a failed turn is still a live conversation this member
            // should resume rather than replace.
            session: ended.conversation_id.filter(|id| !id.is_empty()),
            refused,
        })
    }
}

/// agy's own record of a conversation, as this side reads it (**D515**).
///
/// `~/.gemini/antigravity-cli/brain/<conversation>/.system_generated/logs/
/// transcript.jsonl`, under the fixed home that CLI keeps its things in —
/// this driver admits no home variable, so there is one place to look and no
/// door to get it wrong through.
///
/// Re-read whole and counted, [`readback::Cursor::answers`]: this vendor
/// writes its transcript in chunks and re-emits them, and its records do not
/// arrive in `step_index` order — a byte cursor over a file its writer may
/// rewrite would carry half a record or repeat one.
///
/// One answer per finished turn: a `PLANNER_RESPONSE` carrying `content` is
/// what that CLI recorded as something it said, and the ones carrying only
/// `thinking` or `tool_calls` are the work on the way to it. agy's headless
/// driver mails the terminal `result` for the same reason.
#[derive(Debug)]
pub struct Transcript;

/// The reader [`crate::teammate::readback::of`] hands out for this CLI.
pub static TRANSCRIPT: Transcript = Transcript;

/// Where this CLI keeps a conversation's own records, under its home.
const BRAIN: &str = "brain";

/// The path from one conversation's directory to its transcript.
const TRANSCRIPT_PATH: [&str; 3] = [".system_generated", "logs", "transcript.jsonl"];

impl Transcript {
    /// Where this CLI keeps its conversations. No variable: see the type doc.
    fn brains() -> Option<PathBuf> {
        Some(
            PathBuf::from(std::env::var_os("HOME")?)
                .join(".gemini")
                .join("antigravity-cli")
                .join(BRAIN),
        )
    }
}

impl readback::Transcript for Transcript {
    fn find(&self, mark: &str, _cwd: &Path, since: std::time::SystemTime) -> Option<PathBuf> {
        // The cwd is unused for the same reason it is unused for codex: this
        // vendor names a conversation by an id of its own, and the
        // fingerprint is what says which conversation is this member's.
        let candidates = readback::listing(&Self::brains()?, |path| path.is_dir())
            .into_iter()
            .map(|conversation| {
                TRANSCRIPT_PATH
                    .iter()
                    .fold(conversation, |path, step| path.join(step))
            })
            .filter(|path| path.is_file())
            .collect();

        readback::matching(candidates, mark, self, since)
    }

    fn user_said(&self, record: &serde_json::Value, mark: &str) -> bool {
        record["type"] == "USER_INPUT"
            && record["content"]
                .as_str()
                .is_some_and(|text| text.contains(mark))
    }

    fn answers(&self, path: &Path, cursor: &mut readback::Cursor) -> Vec<String> {
        // **Every** record this CLI wrote as something it said, in order —
        // not "the last one of each turn", which was tried and is the wrong
        // shape here. This vendor writes no end-of-turn marker, so a
        // per-turn rule can only carry a turn's answer once the *next* turn
        // begins: a teammate asked one question would then be heard from
        // never. Carrying each record instead means a build whose narration
        // also carries `content` — transcripts on this machine from an
        // earlier one have thirty-four such steps across three turns — is
        // heard in full rather than misreported, and
        // [`readback::answers_clause`] tells an agy teammate exactly that.
        let found = readback::whole(path)
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| record["type"] == "PLANNER_RESPONSE")
            .filter_map(|record| {
                let text = record["content"].as_str()?.to_owned();
                (!text.trim().is_empty()).then_some(text)
            })
            .collect();

        readback::beyond(found, cursor)
    }
}

#[cfg(test)]
#[path = "agy_tests.rs"]
mod tests;
