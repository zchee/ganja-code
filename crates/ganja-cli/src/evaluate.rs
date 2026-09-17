//! `ganja evaluate`: the TypeSafe client from outside a turn.
//!
//! **D564.** The second of the two surfaces over the one client in
//! [`ganja_tool::typesafe`]. The first is the model-callable `evaluate` tool;
//! this one is what a script or a `PreToolUse` hook calls, and it needs no
//! engine, no session, no protocol event and no dialog.
//!
//! # Why there is no permission gate
//!
//! Because there is nobody to ask and nobody to ask on behalf of. A tool call
//! is the model's choice and is gated as one; this command is typed by a
//! person, or written into a config file by a person, and that is the consent
//! — the same posture as `ganja auth login` and `ganja mcp login`.
//!
//! # This binary's first exit-code taxonomy
//!
//! Every other subcommand exits 0 or, through `anyhow`, 1. This one answers
//! with a code a hook can branch on, which is why [`evaluate`] returns an
//! [`ExitCode`] rather than a `Result`: every failure it can have is a row of
//! the table below, so there is no path left for an unexplained 1.
//!
//! | code | meaning |
//! |---|---|
//! | 0 | answered |
//! | 2 | clap's own parse failure, and **only** that |
//! | 3 | not configured: no key, or a refused base URL |
//! | 4 | the vendor refused: 401, 403, 422, any other 4xx |
//! | 5 | unavailable: 429, 529, 5xx, 3xx, timeout, transport, too large, malformed |
//! | 64 | this command's own argument error (`EX_USAGE`) |
//!
//! Three more things answer **5**, written down here rather than left to be
//! inferred from the `match` that decides them:
//!
//! - an arm of `#[non_exhaustive]` [`typesafe::Error`] this build does not
//!   know. A later build may add one, and "nothing was answered, a later
//!   attempt might be" is the reading that costs a caller a judgement rather
//!   than telling it something false about its own request.
//! - [`typesafe::Error::Cancelled`], which nothing here can currently
//!   produce — a command has no turn to be abandoned from — but which is an
//!   arm all the same, and an arm with no row is how a taxonomy stops being
//!   total.
//! - a write to standard output that failed, which is what a hook that
//!   stopped reading looks like from this side. It is an answer nobody
//!   received, so it is reported as one rather than as a success.
//!
//! **2 is the one that matters**, and nothing here ever returns it: under a
//! `PreToolUse` hook an exit 2 *blocks the tool call*
//! (`ganja_core::hook`), so a code of ganja's own choosing in that
//! position would turn a judgement this command could not obtain into a
//! refusal of something unrelated. Every other non-zero code is a
//! non-blocking notice, which is what makes `docs/recipes/` able to degrade
//! to the normal permission rules by printing nothing.

use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::process::ExitCode;

use clap::{Args, ValueEnum};
use ganja_tool::typesafe::{self, Question, Request, Settings, State};
use tokio_util::sync::CancellationToken;

/// Answered; the answers are on standard output.
const ANSWERED: u8 = 0;
/// No key, or a base URL this build will not put a key on.
const NOT_CONFIGURED: u8 = 3;
/// The vendor refused the request. A second attempt would be refused too.
const REFUSED: u8 = 4;
/// Nothing was answered, but a later attempt might be.
const UNAVAILABLE: u8 = 5;
/// `EX_USAGE` from `sysexits.h`, which is what every BSD-descended tool means
/// by a caller's mistake — and deliberately not 2, which clap owns and a hook
/// reads as a block.
const USAGE: u8 = 64;

/// `ganja evaluate`.
#[derive(Debug, Args)]
pub struct EvaluateArgs {
    /// The questions to ask, as a JSON object or `@PATH` to a file holding
    /// one.
    ///
    /// Each question sits under an id of your choosing and names its `type`
    /// — `noul`, `choice` or `score` — and its `instructions`. The id is not
    /// sent to the model, so each question's instructions must stand alone.
    #[arg(long, value_name = "JSON|@PATH")]
    questions: String,
    /// The content to judge: a JSON string, object or array, `@PATH` to a
    /// file, or `-` for standard input.
    ///
    /// Standard input is the default, which is what lets a hook pipe its
    /// whole envelope in. Input that is not valid JSON is sent as a text
    /// state; input that is valid JSON but a number, a boolean or null is
    /// refused, because those are not states.
    #[arg(long, value_name = "JSON|@PATH|-", default_value = "-")]
    state: String,
    /// Which model judges. Defaults to TYPESAFE_DEFAULT_MODEL, or
    /// `jev-latest`.
    ///
    /// `jev-latest` is the flagship alias and `jev-preview` the other one; a
    /// versioned id such as `jev-1.13.0` is equally valid. Nothing here
    /// decides which ids the vendor serves.
    #[arg(long, value_name = "ID")]
    model: Option<String>,
    /// How to print the answers.
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
}

/// What standard output carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Format {
    /// The whole response as one JSON object: `model`, `answers`, `usage`.
    Json,
    /// One line per question, in id order — the rendering the `evaluate`
    /// tool puts in front of the model, so a script and a session read the
    /// same words.
    Text,
}

/// Asks Jev what `args` says, prints it, and answers with the code for what
/// happened.
///
/// See the module docs for the table. Nothing here can panic on input: every
/// refusal is a code and a sentence on stderr, so a hook that captured only
/// stdout sees an empty answer rather than a stack trace.
pub async fn evaluate(args: EvaluateArgs) -> ExitCode {
    // Before anything is read, so that a machine with no key spends no time
    // on a file it will not send.
    let settings = match Settings::from_env() {
        Ok(Some(settings)) => settings,
        Ok(None) => {
            return failed(
                NOT_CONFIGURED,
                &format!(
                    "{} is not set, so there is nothing to ask. Export it where `ganja` runs.",
                    typesafe::KEY_ENV
                ),
            );
        }
        // The one error `from_env` has, and its message deliberately does not
        // echo the URL: a base URL can carry a credential in its userinfo.
        Err(error) => return failed(NOT_CONFIGURED, &error.to_string()),
    };

    let questions = match read_argument(&args.questions, Source::Argument) {
        Ok(text) => match serde_json::from_str::<BTreeMap<String, Question>>(&text) {
            Ok(questions) => questions,
            Err(error) => {
                return failed(
                    USAGE,
                    &format!("--questions is not a JSON object of questions: {error}"),
                );
            }
        },
        Err(why) => return failed(USAGE, &format!("--questions {why}")),
    };

    let state = match read_argument(&args.state, Source::Stdin) {
        Ok(text) => match state_from(text) {
            Ok(state) => state,
            Err(why) => return failed(USAGE, &format!("--state {why}")),
        },
        Err(why) => return failed(USAGE, &format!("--state {why}")),
    };

    let model = args.model.unwrap_or_else(|| settings.model().to_owned());
    // The same validator the tool uses. Every limit is decided here, so a
    // request that is going to be refused is refused without the vendor
    // hearing any of it.
    let request = match Request::checked(state, questions, model) {
        Ok(request) => request,
        Err(error) => return failed(USAGE, &error.to_string()),
    };

    let client = match typesafe::Client::new(settings) {
        Ok(client) => client,
        Err(error) => return failed(code_for(&error), &error.to_string()),
    };
    // One attempt, under the client's own ten-second deadline. The token is
    // fresh and nothing cancels it: a command has no turn to be abandoned
    // from, and a signal ends the process rather than the request.
    let answered = match client.evaluate(&request, &CancellationToken::new()).await {
        Ok(answered) => answered,
        Err(error) => return failed(code_for(&error), &error.to_string()),
    };

    let printed = match args.format {
        // Nothing to filter: `serde_json` escapes a control character rather
        // than emitting it, so the document is safe to print as it stands and
        // a caller piping it into a parser gets the vendor's bytes back.
        Format::Json => serde_json::to_string_pretty(&answered)
            .expect("a response this build parsed serializes back"),
        // Not clamped, unlike the tool's: a session's output budget is a
        // context-window decision, and truncating a script's input would be
        // a bug wearing its clothes. Filtered, though — see [`legible`].
        Format::Text => legible(&ganja_tool::evaluate::lines(&answered.answers)),
    };

    write(&printed)
}

/// Prints `printed` and answers 0, or answers [`UNAVAILABLE`] if it could not
/// be printed.
///
/// An **empty** rendering is printed as nothing at all rather than as a blank
/// line. `evaluate::lines` answers `""` for a response carrying no answers,
/// and the recipe's `awk` reads standard output a line at a time: a blank
/// line there is one more record to not match, and this way there is no
/// record at all.
///
/// `writeln!` rather than `println!` because `println!` **panics** when the
/// write fails, and the ordinary way for it to fail here is a hook that
/// stopped reading — a closed pipe, which is not a bug and must not be a
/// stack trace on somebody's terminal.
fn write(printed: &str) -> ExitCode {
    if printed.is_empty() {
        return ExitCode::from(ANSWERED);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // Flushed here rather than left to the handle's drop, where a failure
    // would be discarded and this command would answer 0 for an answer that
    // never arrived.
    match writeln!(out, "{printed}").and_then(|()| out.flush()) {
        Ok(()) => ExitCode::from(ANSWERED),
        Err(error) => failed(UNAVAILABLE, &format!("the answers could not be written: {error}")),
    }
}

/// `text` with every control character replaced, one line at a time.
///
/// Part of what `--format text` prints is chosen by the vendor — a `choice`
/// value, the option names beside it, the `type` of an answer this build does
/// not recognise — and it lands unread on the terminal of whoever wrote the
/// hook. The tool path is protected twice over (ratatui drops control
/// characters, and the headless reporter filters); this path had neither, so
/// it borrows the reporter's own filter rather than growing a second one.
///
/// Line by line because the rendering is newline-joined and a newline is a
/// control character: filtering the whole string at once would replace every
/// join with a replacement character and put all the answers on one line.
fn legible(text: &str) -> String {
    text.lines().map(crate::report::printable).collect::<Vec<_>>().join("\n")
}

/// Where a `-` argument reads from, if anywhere.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// `-` is standard input.
    Stdin,
    /// `-` is just a value, and not a JSON one.
    Argument,
}

/// The text behind one argument: a file after `@`, standard input for `-`, or
/// the argument itself.
///
/// # Errors
///
/// A sentence completing "--questions " or "--state ", for a file that cannot
/// be read and for the terminal case below.
fn read_argument(value: &str, dash: Source) -> Result<String, String> {
    if let Some(path) = value.strip_prefix('@') {
        return std::fs::read_to_string(path)
            .map_err(|error| format!("could not read the file `{path}`: {error}"));
    }
    if value == "-" && dash == Source::Stdin {
        // A terminal here is somebody who typed the command without piping
        // anything into it. Reading would hang with no prompt and no hint
        // that it was waiting, so it is a usage error instead — which is
        // also what a hook misconfigured to run without its envelope gets.
        if std::io::stdin().is_terminal() {
            return Err(
                "is `-` and standard input is a terminal; pipe the state in, or pass it as JSON \
                 or @PATH."
                    .to_owned(),
            );
        }

        let mut text = String::new();
        return std::io::stdin()
            .read_to_string(&mut text)
            .map(|_| text)
            .map_err(|error| format!("could not be read from standard input: {error}"));
    }

    Ok(value.to_owned())
}

/// `text` as a state.
///
/// JSON that is a string, an object or an array is that state. JSON that is a
/// number, a boolean or null is refused by name — those are values, not
/// content to judge, and silently wrapping one in quotes would send something
/// the caller did not write. Anything that is not JSON at all is text, which
/// is the case that lets a log or a diff be piped in as it stands.
///
/// # Errors
///
/// A sentence completing "--state ".
fn state_from(text: String) -> Result<State, String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(State::Text(text));
    };

    match value {
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) | serde_json::Value::Null => {
            Err(format!(
                "is the JSON value `{}`; a state is a string, an object or an array.",
                text.trim()
            ))
        }
        value => serde_json::from_value(value)
            .map_err(|error| format!("is not a state this build can send: {error}")),
    }
}

/// Which row of the table an error is.
///
/// [`typesafe::Error`] is `#[non_exhaustive]`, and the catch-all answers
/// [`UNAVAILABLE`] on purpose: an arm added by a later build is one this one
/// has never classified, and "nothing was answered, try later" is the reading
/// that costs a caller a judgement rather than telling it something false
/// about its own request.
fn code_for(error: &typesafe::Error) -> u8 {
    match error {
        typesafe::Error::RefusedBase => NOT_CONFIGURED,
        typesafe::Error::InvalidRequest(_) => USAGE,
        typesafe::Error::Rejected { .. } | typesafe::Error::Invalid { .. } => REFUSED,
        typesafe::Error::Unavailable { .. }
        | typesafe::Error::Timeout
        | typesafe::Error::TooLarge
        | typesafe::Error::Transport(_)
        | typesafe::Error::Malformed(_)
        | typesafe::Error::Cancelled => UNAVAILABLE,
        _ => UNAVAILABLE,
    }
}

/// Prints `why` on stderr and answers with `code`.
///
/// Stderr, never stdout: a hook's stdout is read as its answer, and a
/// diagnostic printed there would be parsed as one. A hook's stderr is read
/// only on exit 2, which this command never returns, so these sentences are
/// for whoever runs it by hand.
fn failed(code: u8, why: &str) -> ExitCode {
    // Filtered like standard output, and for the same reason: the vendor's
    // own 422 `detail` reaches this sentence, so a refusal can carry text a
    // third party wrote straight onto a terminal.
    eprintln!("ganja evaluate: {}", legible(why));

    ExitCode::from(code)
}
