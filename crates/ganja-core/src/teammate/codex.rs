//! A teammate that is a headless `codex exec` child (**D508**, **D509**).
//!
//! Spec: none. Upstream opencode has no teammates and Claude Code does not run
//! another vendor's agent as one, so every sentence here is ganja's own, and
//! the vendor surface it is written against is the binary itself —
//! `codex-cli 0.149.0-alpha.1`, probed on this machine rather than read out of
//! a checkout that may not match what a person has installed.
//!
//! This module is the **words**, and only the words: which flags go on a
//! command line and what a finished child's stdout meant. When a turn starts,
//! how long it may run, what its failure becomes and where its answer goes are
//! all [`crate::teammate::shim`]'s, shared with the other two CLIs.
//!
//! # Why one child per message
//!
//! `codex exec` takes one prompt and ends, so [`Shape::PerMessage`](crate::teammate::shim::Shape::PerMessage) is not a
//! choice between two doors — it is the only door that vendor has
//! non-interactively. Continuity therefore rides `codex exec resume <id>`
//! rather than a live process, and the id comes from the stream itself: the
//! first line of a first turn is `thread.started`, and its `thread_id` is what
//! every later turn resumes.
//!
//! # The posture, and why it is spelled twice
//!
//! D508(a) pins `read-only`, and the first turn states it two ways on purpose:
//! `-s read-only` is the documented flag, and `-c sandbox_mode="read-only"`
//! beside it makes turn 1 and turn *n* textually identical in posture, so a
//! reader comparing the two argvs sees one rule rather than two. The resume
//! turn carries only the `-c` form, because **`codex exec resume` has no `-s`
//! at all** — the vendor's own `--help` lists the flag on `exec` and not on
//! `exec resume`, and that asymmetry is the most fragile seam in this file.
//!
//! It is measured rather than assumed. A probe pair on 2026-08-20 created a
//! thread under the first-turn argv and resumed it under the resume argv on a
//! machine whose own `config.toml` sets `sandbox_mode` to `danger-full-access`;
//! the resumed turn declined to write and created no file, and the vendor's own
//! persisted rollout recorded **two** `turn_context` entries with distinct
//! `turn_id`s, each carrying `"sandbox_policy":{"type":"read-only"}` and
//! `"approval_policy":"never"`. The person's permissive config did not reach
//! either turn, which is the failure this seam exists to rule out.
//!
//! `approval_policy` is pinned beside the sandbox for the same reason and it is
//! not decoration: the approval posture is otherwise whatever the person's own
//! `config.toml` says, which on one machine is `never` and on another is not.
//! Two keys, and the never-composed rule below narrows to **exactly** these
//! two.
//!
//! # What the posture actually bounds
//!
//! Measured turn-free through `codex sandbox`, which runs an arbitrary command
//! under exactly the composed override, and corroborated by the rollout's own
//! `permission_profile` — `file_system: restricted` with a single `root`/`read`
//! entry, `network: restricted`:
//!
//! - **writes** — denied, including inside the child's own cwd.
//! - **reads** — the whole filesystem. `~/.ssh/known_hosts` and codex's own
//!   `auth.json` both read cleanly under it.
//! - **network** — denied, at the socket and not only at DNS: a request to a
//!   literal address fails to connect rather than failing to resolve.
//!
//! The read scope is why [`crate::teammate::posture_line`]'s codex arm says
//! what the read *enables* and not only what the write bound is: whole-disk
//! read is the ability to read a credential. That it is paired with a denied
//! network is the one clause that separates this floor from grok's, and it is
//! the reason the sentence ends the way it does rather than in grok's words.
//!
//! # What is never composed
//!
//! [`NEVER_COMPOSED`](crate::teammate::codex::NEVER_COMPOSED) is the single source, iterated by the test rather than
//! copied into it, and it is asserted absent from **both** argvs. Two entries
//! are values rather than flags — `workspace-write` and `danger-full-access`,
//! the two `-s` values that are not the floor — because a posture is escaped
//! as easily by a value as by a flag.
//!
//! `-c/--config` is the one flag here that is composed *and* dangerous: it can
//! set any key the config file can. So the rule over it is narrower than
//! presence — the only overrides on either argv are [`PINNED_KEYS`](crate::teammate::codex::PINNED_KEYS), each as
//! **one argv token including its quotes**. The quotes are load-bearing rather
//! than cosmetic: `-c`'s own help says the value portion "is parsed as TOML; if
//! it fails to parse as TOML, the raw string is used as a literal", so
//! `sandbox_mode="read-only"` is a TOML string where `sandbox_mode=read-only`
//! is a bare word that happens to work today.
//!
//! `-p/--profile` is on the list for a reason worth stating, because a probe
//! narrowed it: at this version a person's own config **cannot** select a
//! profile — `profile = "x"` in `config.toml` is refused at load with *"legacy
//! `profile` config is no longer supported; use `--profile`"* — so the only way
//! a profile layer exists at all is that flag, and this file never composes it.
//! There is no configured-profile state left for a spawn to detect or refuse.
//!
//! # What travels in the environment
//!
//! One addition to [`crate::teammate::shim::CARRIED`], and it is required
//! rather than convenient: `CODEX_HOME` is where that CLI keeps both its config
//! and its credentials, so a child without it authenticates as nobody. What
//! rides in with it is the person's own config — their model, their MCP
//! servers, their approval defaults — which is their own recorded consent in
//! their own tool and not ganja's to overwrite; the two pinned `-c` keys
//! override it on the two axes that are the grant, and the ring's
//! "effective posture bounded by codex's own config" rider says the rest out
//! loud.
//!
//! Deliberately **not** carried, and the omission is the point:
//! `CODEX_PERMISSION_PROFILE`, the environment door onto the very permission
//! profile the rollout above records. Enumeration is what closes it, the same
//! way it closes grok's three.

use std::{ffi::OsString, process::Stdio, time::Duration};

use async_trait::async_trait;
use ganja_protocol::team::MemberBackend;
use ganja_team::ShimCli;

use crate::teammate::shim::{Door, Driver, Launch, Reply, Shape, Turn};

/// The executable a spawn looks for on `PATH`.
pub const BINARY: &str = "codex";

/// The `-s` value D508(a) pins, and one of that flag's three documented values.
pub const SANDBOX_VALUE: &str = "read-only";

/// The sandbox override, as **one argv token** — quotes included.
pub const SANDBOX_OVERRIDE: &str = "sandbox_mode=\"read-only\"";

/// The approval override, as one argv token.
pub const APPROVAL_OVERRIDE: &str = "approval_policy=\"never\"";

/// The only two `-c` keys either argv may carry.
///
/// A test asserts every `-c` value on both argvs parses to one of these, which
/// is a narrower promise than "these two are present": the danger of `-c` is
/// the key nobody listed.
pub const PINNED_KEYS: [&str; 2] = ["sandbox_mode", "approval_policy"];

/// The feature the worker-to-lead mail rides.
///
/// Composed explicitly rather than trusted: `codex features list` reports it
/// `under development`, and a stage that is not `stable` is a default that can
/// move between releases.
pub const FEATURE: &str = "send_async_message";

/// How long the spawn pre-check may take before it is the thing that failed.
///
/// It is one process asking a local file whether a token is still good, so ten
/// seconds is generous — but that call can reach the network to refresh, and an
/// unbounded pre-check is a spawn that wedges a lead with no deadline and no
/// mail. [`crate::teammate::shim`]'s per-turn deadline does not cover this: it
/// bounds turns, and this runs before there is one.
pub const READY_DEADLINE: Duration = Duration::from_secs(10);

/// The command a spawn runs before it promises anything.
///
/// Named in the refusal rather than only here, because "codex is not logged in"
/// is only useful next to the command that says so.
pub const AUTH_CHECK: &str = "codex login status";

/// Why a spawn refuses when that CLI has no usable login.
///
/// codex is the only one of the three that offers a cheap answer to the
/// question, so it is the only one that asks it: the alternative is a member
/// that spawns, takes a message, and reports an authentication failure one
/// whole turn later.
pub const REFUSED_NO_LOGIN: &str = "codex is on this session's PATH but has no usable login, so a teammate on it could not take \
     a turn; `codex login status` is what said so, and what to run to see why";

/// Everything this CLI's argv may **never** carry, in every spelling it has.
///
/// The single source for the test that asserts it, rather than a list the test
/// repeats — a flag added here is a flag the assertion picks up. Each entry
/// either hands the child a wider posture than the grant, or widens what it
/// reads a posture *from*:
///
/// - the two `-s` values that are not the floor, as values rather than flags;
/// - the three documented escape hatches (`--approve-for-me` routes approvals
///   through the workspace-write sandbox; the two `--dangerously-` flags say
///   what they are);
/// - `--ignore-rules` and `--ignore-user-config`, which discard the person's
///   own execpolicy and config — the second is refused rather than used even
///   though it would *simplify* the posture story, because it would also
///   silently change the model and the MCP servers a person's own `codex`
///   runs with;
/// - `--add-dir`, which widens the writable set;
/// - `-C/--cd`, which moves the agent's working root away from the cwd the
///   spawn dialog gated — `--add-dir`'s exact sibling, and the reason this
///   table exists is to make a future addition a failing test rather than a
///   review finding;
/// - `--skip-git-repo-check`, because outside a git repository the vendor's own
///   refusal is the honest answer and a structured first-turn failure naming
///   its flag is a better one than a silent launch;
/// - `-p/--profile`, a posture door in exactly the `-c` class;
/// - `--last` and `--all`, which resume or list *somebody else's* session —
///   this file resumes an observed id or starts fresh, never "the most recent";
/// - `--ephemeral`, `--oss`, `--local-provider` and `-m/--model`, which move
///   where the turn is recorded or which model answers it. The last of those is
///   the strong spelling of the class the other two reach obliquely, so it
///   belongs beside them rather than being left to the person's own config.
pub const NEVER_COMPOSED: [&str; 20] = [
    "workspace-write",
    "danger-full-access",
    "--approve-for-me",
    "--dangerously-bypass-approvals-and-sandbox",
    "--dangerously-bypass-hook-trust",
    "--ignore-rules",
    "--ignore-user-config",
    "--add-dir",
    "-C",
    "--cd",
    "--skip-git-repo-check",
    "-p",
    "--profile",
    "--last",
    "--all",
    "--ephemeral",
    "--oss",
    "--local-provider",
    "-m",
    "--model",
];

/// What this CLI needs beyond [`crate::teammate::shim::CARRIED`].
const ADDITIONS: [&str; 1] = ["CODEX_HOME"];

/// The JSONL event that names the conversation.
const THREAD_STARTED: &str = "thread.started";

/// The JSONL event that carries one finished item.
const ITEM_COMPLETED: &str = "item.completed";

/// The JSONL event that ends a turn the vendor could not take.
const TURN_FAILED: &str = "turn.failed";

/// The one item kind whose text is a teammate talking.
///
/// Every one of them becomes a mail, in arrival order. If several
/// `agent_message` items ever have to be disambiguated from the turn's final
/// answer, `-o <file>` is the vendor's own authoritative source for the last
/// message; this build does not compose it because arrival order already
/// answers the question.
const AGENT_MESSAGE: &str = "agent_message";

/// A teammate driven through `codex exec`.
///
/// Stateless: the conversation id lives in the shim runner, which is what lets
/// one driver serve every member on this CLI.
#[derive(Clone, Copy, Debug, Default)]
pub struct Codex;

impl Codex {
    /// The driver.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for Codex {
    fn cli(&self) -> ShimCli {
        ShimCli::Codex
    }

    fn backend(&self) -> MemberBackend {
        MemberBackend::Codex
    }

    fn binary(&self) -> &str {
        BINARY
    }

    fn shape(&self) -> Shape {
        Shape::PerMessage
    }

    fn additions(&self) -> &[&str] {
        &ADDITIONS
    }

    fn door(&self) -> Door {
        // `-` is the vendor's own spelling for "the prompt is on stdin", which
        // is what keeps it out of argv and therefore out of `ps`.
        Door::Stdin
    }

    fn argv(&self, turn: &Turn<'_>) -> Vec<OsString> {
        let mut argv = vec![OsString::from("exec")];
        // A resume names the thread before any option, because the id is a
        // positional argument of the `resume` subcommand.
        if let Some(session) = turn.session {
            argv.push(OsString::from("resume"));
            argv.push(OsString::from(session));
        }
        argv.push(OsString::from("--json"));
        argv.push(OsString::from("--enable"));
        argv.push(OsString::from(FEATURE));
        if turn.session.is_none() {
            // The documented flag, first turn only: `codex exec resume` has no
            // `-s`, which is why the `-c` below carries the posture on both.
            argv.push(OsString::from("-s"));
            argv.push(OsString::from(SANDBOX_VALUE));
        }
        argv.push(OsString::from("-c"));
        argv.push(OsString::from(SANDBOX_OVERRIDE));
        argv.push(OsString::from("-c"));
        argv.push(OsString::from(APPROVAL_OVERRIDE));
        if turn.session.is_none() {
            // Only the first turn has it to give: `exec resume` takes no
            // `--color`, so composing it on both would be a parse error rather
            // than a consistency.
            argv.push(OsString::from("--color"));
            argv.push(OsString::from("never"));
        }
        argv.push(OsString::from("-"));

        argv
    }

    async fn ready(&self, launch: &Launch) -> Result<(), String> {
        let mut child = launch.command(&[OsString::from("login"), OsString::from("status")]);
        let status = tokio::time::timeout(
            READY_DEADLINE,
            child
                .stdin(Stdio::null())
                // Neither pipe is read, and both are closed rather than
                // inherited: a pre-check that printed onto the lead's own
                // terminal would be a spawn writing over a person's transcript.
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await;

        match status {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(_)) => Err(REFUSED_NO_LOGIN.to_owned()),
            // A pre-check that could not run is not a login failure, and saying
            // so is what keeps a person from re-running `codex login` at a
            // machine that has no `codex` to run it with.
            Ok(Err(error)) => Err(format!("`{AUTH_CHECK}` could not be run: {error}")),
            // `kill_on_drop` is set by `Launch::command`, so the child goes with
            // the dropped future rather than outliving the spawn that gave up.
            Err(_) => Err(format!(
                "`{AUTH_CHECK}` did not answer within {}s",
                READY_DEADLINE.as_secs()
            )),
        }
    }

    fn reply(&self, stdout: &str) -> Result<Reply, String> {
        let mut messages = Vec::new();
        let mut session = None;
        let mut read_anything = false;

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // A line this side cannot parse is the vendor's, not this build's,
            // to explain: `--json` is documented to print events, and a future
            // version printing one more kind must not cost a turn that
            // otherwise succeeded.
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(kind) = event.get("type").and_then(serde_json::Value::as_str) else {
                continue;
            };
            read_anything = true;
            match kind {
                THREAD_STARTED => {
                    session = event
                        .get("thread_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                }
                ITEM_COMPLETED => {
                    let item = event.get("item");
                    let is_message = item
                        .and_then(|item| item.get("type"))
                        .and_then(serde_json::Value::as_str)
                        == Some(AGENT_MESSAGE);
                    if is_message
                        && let Some(text) = item
                            .and_then(|item| item.get("text"))
                            .and_then(serde_json::Value::as_str)
                        && !text.is_empty()
                    {
                        // Every one of them, in arrival order: with
                        // `send_async_message` enabled a turn may say several
                        // things before it ends, and folding them into one mail
                        // would lose the order a reader needs.
                        messages.push(text.to_owned());
                    }
                }
                TURN_FAILED => {
                    let reason = event
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("the vendor named no reason");

                    return Err(format!("codex ended the turn as failed: {reason}"));
                }
                _ => {}
            }
        }

        if !read_anything {
            return Err(
                "codex printed no event this build reads; `--json` prints one JSON object per line"
                    .to_owned(),
            );
        }

        Ok(Reply { messages, session })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ganja_team::{MemberName, TeamName, TeamsRoot};

    use super::*;
    use crate::teammate::SpawnSpec;

    /// A spawn to compose against. Nothing in an argv reads any of it — which
    /// is itself the point of **AC-21**, and is why this can be one value.
    fn spec() -> SpawnSpec {
        SpawnSpec {
            name: MemberName::parse("w1").expect("a member name"),
            team: TeamName::default_team(),
            lead: MemberName::lead(),
            root: TeamsRoot::new(PathBuf::from("/nonexistent/teams")),
            backend: MemberBackend::Codex,
            agent_type: "general".to_owned(),
            model: "whatever-the-person-configured".to_owned(),
            color: "blue".to_owned(),
            prompt: "the spawn prompt, which travels through the mailbox".to_owned(),
            cwd: PathBuf::from("/nonexistent/work"),
            plan_mode_required: false,
            bypass: false,
            parent_session_id: "01998ad0-0000-7000-8000-000000000000".to_owned(),
        }
    }

    /// The argv for a turn that has, or has not, seen a conversation id.
    fn argv(session: Option<&str>) -> Vec<String> {
        let spec = spec();
        Codex
            .argv(&Turn {
                spec: &spec,
                text: "a teammate's words, which never reach a command line",
                prompt: None,
                session,
            })
            .iter()
            .map(|token| token.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_first_turn_states_the_posture_twice_and_takes_its_prompt_on_stdin() {
        // Byte for byte, and the two `-c` tokens include their quotes: `-c`'s
        // own help parses the value as TOML and falls back to a literal, so
        // `sandbox_mode="read-only"` is a TOML string where the unquoted
        // spelling is a bare word that happens to work.
        assert_eq!(
            argv(None),
            vec![
                "exec",
                "--json",
                "--enable",
                "send_async_message",
                "-s",
                "read-only",
                "-c",
                "sandbox_mode=\"read-only\"",
                "-c",
                "approval_policy=\"never\"",
                "--color",
                "never",
                "-",
            ]
        );
    }

    #[test]
    fn a_resume_turn_carries_the_posture_without_the_flag_resume_does_not_have() {
        // `codex exec resume` has no `-s` — the vendor's own `--help` lists it
        // on `exec` and not here — which is the whole reason the `-c` form is
        // composed on both turns rather than only on the one that lacks a flag.
        assert_eq!(
            argv(Some("01a01b4f-174e-7fe2-8abd-ba8e51156c43")),
            vec![
                "exec",
                "resume",
                "01a01b4f-174e-7fe2-8abd-ba8e51156c43",
                "--json",
                "--enable",
                "send_async_message",
                "-c",
                "sandbox_mode=\"read-only\"",
                "-c",
                "approval_policy=\"never\"",
                "-",
            ]
        );
        assert!(
            !argv(Some("x")).iter().any(|token| token == "-s"),
            "a resume turn that carried `-s` would be a parse error, not a tighter posture"
        );
    }

    #[test]
    fn the_posture_is_pinned_on_every_turn_and_not_only_the_first() {
        for session in [None, Some("01a01b4f-174e-7fe2-8abd-ba8e51156c43")] {
            let argv = argv(session);
            for pinned in [SANDBOX_OVERRIDE, APPROVAL_OVERRIDE] {
                assert!(
                    argv.iter().any(|token| token == pinned),
                    "{pinned} is missing from {argv:?}"
                );
            }
        }
    }

    #[test]
    fn no_never_composed_spelling_reaches_either_argv() {
        // Iterated rather than re-listed: [`NEVER_COMPOSED`] is the single
        // source, so a flag added to it is a flag this assertion picks up.
        for session in [None, Some("01a01b4f-174e-7fe2-8abd-ba8e51156c43")] {
            let argv = argv(session);
            for refused in NEVER_COMPOSED {
                assert!(
                    !argv.iter().any(|token| token == refused),
                    "{refused} must never be composed, and is in {argv:?}"
                );
            }
        }
    }

    #[test]
    fn the_only_config_overrides_are_the_two_pinned_posture_keys() {
        // Narrower than "the two are present", and deliberately: `-c` can set
        // any key the config file can, so the danger is the third one nobody
        // listed rather than the two that are.
        for session in [None, Some("01a01b4f-174e-7fe2-8abd-ba8e51156c43")] {
            let argv = argv(session);
            let overrides: Vec<&String> = argv
                .iter()
                .zip(argv.iter().skip(1))
                .filter_map(|(flag, value)| (flag == "-c" || flag == "--config").then_some(value))
                .collect();
            assert_eq!(overrides.len(), PINNED_KEYS.len(), "in {argv:?}");
            for value in overrides {
                let key = value.split('=').next().expect("a key before the equals");
                assert!(
                    PINNED_KEYS.contains(&key),
                    "{key} is not one of the pinned posture keys"
                );
            }
        }
    }

    #[test]
    fn no_prompt_text_is_ever_on_a_command_line() {
        let spec = spec();
        let secret = "the words a peer said, which argv is world-readable through ps";
        let argv = Codex.argv(&Turn {
            spec: &spec,
            text: secret,
            prompt: None,
            session: None,
        });
        assert!(
            !argv
                .iter()
                .any(|token| token.to_string_lossy().contains(secret)),
            "argv is for flags; `-` is what says the prompt is on stdin"
        );
        assert_eq!(Codex.door(), Door::Stdin);
    }

    #[test]
    fn a_thread_started_line_is_where_a_later_turn_gets_its_id() {
        let reply = Codex
            .reply(concat!(
                r#"{"type":"thread.started","thread_id":"01a01b4f-174e-7fe2-8abd-ba8e51156c43"}"#,
                "\n",
                r#"{"type":"turn.started"}"#,
                "\n",
                r#"{"type":"turn.completed","usage":{"input_tokens":31723,"output_tokens":6}}"#,
                "\n",
            ))
            .expect("the shapes a probed 0.149.0-alpha.1 actually printed");
        assert_eq!(
            reply.session.as_deref(),
            Some("01a01b4f-174e-7fe2-8abd-ba8e51156c43")
        );
    }

    #[test]
    fn every_agent_message_becomes_one_mail_in_arrival_order() {
        // **AC-5**. `send_async_message` is what lets a turn say something
        // before it ends, so a mid-turn item and a final one both arrive and
        // folding them into one mail would lose the order a reader needs.
        let reply = Codex
            .reply(concat!(
                r#"{"type":"thread.started","thread_id":"t-1"}"#,
                "\n",
                r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"starting on it"}}"#,
                "\n",
                r#"{"type":"item.completed","item":{"id":"item_1","type":"reasoning","text":"not a teammate talking"}}"#,
                "\n",
                r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"done"}}"#,
                "\n",
                r#"{"type":"turn.completed","usage":{}}"#,
                "\n",
            ))
            .expect("a two-message turn");
        assert_eq!(reply.messages, vec!["starting on it", "done"]);
    }

    #[test]
    fn an_item_that_is_not_a_teammate_talking_is_not_mail() {
        let reply = Codex
            .reply(concat!(
                r#"{"type":"item.completed","item":{"id":"i","type":"command_execution","command":"ls"}}"#,
                "\n",
                r#"{"type":"turn.completed","usage":{}}"#,
                "\n",
            ))
            .expect("a turn that only ran a command");
        assert!(reply.messages.is_empty(), "{:?}", reply.messages);
    }

    #[test]
    fn a_line_this_build_cannot_read_does_not_cost_a_turn_that_otherwise_succeeded() {
        // A future version printing one more event kind, or one more field, is
        // the vendor's business — and failing a turn over it would make every
        // codex release a ganja outage.
        let reply = Codex
            .reply(concat!(
                "a line that is not JSON at all\n",
                r#"{"type":"an.event.kind.this.build.has.never.heard.of","whatever":true}"#,
                "\n",
                r#"{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"answered"}}"#,
                "\n",
            ))
            .expect("the readable half is still readable");
        assert_eq!(reply.messages, vec!["answered"]);
    }

    #[test]
    fn output_carrying_no_event_at_all_is_refused_rather_than_read_as_silence() {
        // **AC-8**'s garbage arm: a clean exit with unreadable stdout becomes a
        // structured failure mail, where an empty [`Reply`] would become a
        // teammate that answered nothing and said nothing about it.
        let refusal = Codex
            .reply("this is not the shape any driver reads\n")
            .expect_err("garbage is refused");
        assert!(refusal.contains("--json"), "{refusal}");
    }

    #[test]
    fn a_turn_the_vendor_failed_is_refused_with_the_vendors_own_reason() {
        let refusal = Codex
            .reply(concat!(
                r#"{"type":"thread.started","thread_id":"t-1"}"#,
                "\n",
                r#"{"type":"turn.failed","error":{"message":"the model is not available"}}"#,
                "\n",
            ))
            .expect_err("a failed turn is a failure");
        assert!(refusal.contains("the model is not available"), "{refusal}");
    }

    #[test]
    fn the_environment_carries_the_credential_home_and_no_posture_door() {
        assert_eq!(Codex.additions(), &["CODEX_HOME"]);
        // `CODEX_PERMISSION_PROFILE` is the door this omission closes: it names
        // the very permission profile a turn's own rollout records, and
        // enumeration is what keeps it out.
        assert!(
            !Codex
                .additions()
                .iter()
                .any(|name| name.contains("PERMISSION")),
            "no environment door onto the posture may be carried"
        );
    }
}
