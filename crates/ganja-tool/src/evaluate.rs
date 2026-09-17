//! The `evaluate` tool: bounded judgements from TypeSafe's Jev, over the
//! client in [`crate::typesafe`].
//!
//! **D564.** No upstream counterpart, so every sentence the model reads here
//! is ganja's own.
//!
//! # Present only when configured
//!
//! [`Registry::with_builtins`](crate::Registry::with_builtins) does **not**
//! carry this tool. Each frontend overlays it with one line
//! (`EvaluateTool::configured()`), so a session with no `TYPESAFE_API_KEY`
//! has no `evaluate` at all rather than one that refuses at call time —
//! which is the opposite of what `websearch` does, deliberately.
//!
//! The reason is not tidiness. A builtin's description rides on every request
//! of every session and builtins are never deferred
//! (`deferral.rs` skips every name without `MCP_PREFIX`), so an unconfigured
//! `evaluate` would be about 1 KB of prompt bought by people who never asked
//! for it. `websearch` does not transfer because `websearch` has no consent
//! disclosure to produce, while this tool's [`Tool::describe`] must name the
//! host the content would travel to — and only a tool built from its settings
//! can.
//!
//! The consequence to know: **a frontend that forgets its overlay line fails
//! silently.** The three sites are `crates/ganja-cli/src/assemble.rs` (which
//! serves both `run` and `serve`), `crates/ganja-tui/src/lib.rs` at startup
//! and `crates/ganja-tui/src/app.rs` in `reload_plugins`. tmux-pane teammates
//! inherit tmux's environment rather than the lead's, so a team can have
//! mixed rosters.
//!
//! # The dialog is the disclosure
//!
//! Sending project content to a third party is a consent event. The
//! permission dialog's argument preview is eight lines and cannot carry the
//! payload, so what the person is told rides in the title row, which is
//! outside that clamp: the host, the byte count of the **whole** request
//! body, how many questions, which model, and the shape of the state.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::typesafe::{self, Answer, Question, Request, Response, Settings, State};
use crate::{Tool, ToolCtx, ToolError, ToolOutput, shell, truncate};

/// The name the model calls and the permission engine gates.
pub const ID: &str = "evaluate";

/// What the model is told about the tool.
///
/// Ganja's own words — there is no upstream text to port — and pinned by
/// `DESCRIPTION.len()` plus a test naming each sentence that has to survive
/// an edit. Every request of a configured session carries this, so it is
/// budgeted rather than written freely.
pub const DESCRIPTION: &str = "\
Ask TypeSafe's Jev model for calibrated probabilities about content you \
supply. Three primitives: `noul` answers a yes/no question with the \
probability of yes; `choice` picks one of the options you name and returns \
the whole distribution; `score` places the content on ordered levels you \
define.

Each question is one narrow judgement. Ask independent questions together in \
one call rather than one call each.

A question's id is not sent to the model and means nothing to it, so each \
question's `instructions` must stand alone and say everything it needs.

Put the evidence in `state` as named fields and refer to them from \
`instructions` by backticked path, such as `diff` or `tool_input.command`.

A `noul` near 0.5 is undecided, not a no.

Every answer is a probability, not a fact. Rank, route or flag with one; \
never make it the sole ground for an irreversible action.";

/// What one call may ask.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    /// The content to judge: a string, an object of named fields, or an
    /// array. Numbers, booleans and null are not states.
    state: State,
    /// The questions to ask about it, each under an id you choose. The
    /// answers come back under the same ids.
    questions: BTreeMap<String, Question>,
    /// Which model judges. Omit it to use the session's configured default.
    /// `jev-latest` is the flagship alias and `jev-preview` the other one; a
    /// versioned id such as `jev-1.13.0` is equally valid.
    #[serde(default)]
    model: Option<String>,
}

/// Asks Jev.
pub struct EvaluateTool {
    /// The one door to the vendor, built from settings read once.
    client: typesafe::Client,
}

impl EvaluateTool {
    /// The tool this process's environment configures, or nothing.
    ///
    /// The overlay line at each of the three assembly sites is exactly
    /// `if let Some(tool) = EvaluateTool::configured() { tools = tools.with(tool) }`.
    ///
    /// A refused [`typesafe::BASE_ENV`] is nothing plus one warning naming
    /// the variable — never its value, which is configuration and may carry a
    /// credential in its userinfo. Each call builds a fresh client, so a
    /// `/plugin` reload re-reads the (unchanged) environment and may warn
    /// again; that is accepted.
    #[must_use]
    pub fn configured() -> Option<Arc<dyn Tool>> {
        let settings = match Settings::from_env() {
            Ok(Some(settings)) => settings,
            Ok(None) => return None,
            Err(_refused) => {
                tracing::warn!(
                    variable = typesafe::BASE_ENV,
                    "the TypeSafe base URL is not https or loopback; `evaluate` is not offered"
                );

                return None;
            }
        };

        match typesafe::Client::new(settings) {
            Ok(client) => Some(Arc::new(Self { client })),
            Err(error) => {
                tracing::warn!(%error, "no TypeSafe client; `evaluate` is not offered");

                None
            }
        }
    }

    /// The tool over `settings`, for tests: [`EvaluateTool::configured`] is
    /// the only shipped constructor, and it reads the environment.
    #[cfg(test)]
    pub(crate) fn against(settings: Settings) -> Self {
        Self { client: typesafe::Client::new(settings).expect("an HTTP client builds") }
    }

    /// The request `args` spells, already validated.
    fn requested(&self, args: Args) -> Result<Request, typesafe::Error> {
        let model = args.model.unwrap_or_else(|| self.client.settings().model().to_owned());

        Request::checked(args.state, args.questions, model)
    }
}

#[async_trait]
impl Tool for EvaluateTool {
    fn id(&self) -> &str {
        ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    /// The consent disclosure, because this line is the dialog's title row
    /// and title rows are outside the eight-line argument preview.
    ///
    /// The host and the byte count come **first**, and within the first 60
    /// columns: the modal's body wraps by verbatim chunking, and a host
    /// broken across two rows does not read as a host. A forwarded teammate
    /// dialog prefixes `"<teammate> · "`, which shifts the whole line right
    /// without moving the disclosure off the head of the sentence.
    ///
    /// Arguments that do not validate produce a line that says so and shows
    /// **nothing from them**. It cannot panic, and there is nothing it needs
    /// to describe: the same [`Request::checked`] that refused here will
    /// refuse in [`Tool::run`], so no such call reaches the vendor.
    fn describe(&self, args: &serde_json::Value) -> String {
        let host = self.client.settings().host();
        let Ok(args) = serde_json::from_value::<Args>(args.clone()) else {
            return format!("{ID} → {host} · the arguments are not a valid request");
        };
        let Ok(request) = self.requested(args) else {
            return format!("{ID} → {host} · the arguments are not a valid request");
        };

        format!(
            "{ID} → {host} · {} B · {} question(s) · {} · state {}",
            request.body_len(),
            request.questions(),
            request.model(),
            shell::shorten(&request.state().to_string(), shell::DESCRIBE_LIMIT)
        )
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        // Every limit is decided here, before a socket is opened: a request
        // that is going to be refused should be refused without a third party
        // hearing any of it.
        let request = self.requested(args)?;
        let started = Instant::now();
        let answered = self.client.evaluate(&request, &ctx.cancel).await?;
        // Measured around the call rather than taken from the client, which
        // logs its own: what belongs in the metadata is what this tool call
        // cost, and the two spans are the same one.
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        Ok(output(&answered, latency_ms))
    }
}

/// Every answer as one line each, in id order.
///
/// The rendering `ganja evaluate --format text` prints, shared so the two
/// surfaces cannot drift into two spellings of the same line.
///
/// The **join** is what is shared and the clamp deliberately is not. A tool
/// call's output is spent out of a context window, so [`output`] clamps it;
/// a subcommand's stdout is a script's input, and truncating that is a bug
/// rather than a budget.
#[must_use]
pub fn lines(answers: &BTreeMap<String, Answer>) -> String {
    answers.iter().map(|(id, answer)| line(id, answer)).collect::<Vec<_>>().join("\n")
}

/// What the model and the transcript are handed.
fn output(answered: &Response, latency_ms: u64) -> ToolOutput {
    let clamped = truncate::clamp(&lines(&answered.answers));

    ToolOutput {
        title: format!(
            "{} answer(s) · {} · {} input tokens",
            answered.answers.len(),
            // The **served** model, which for an alias is the version it
            // resolved to — `jev-preview` answers as `jev-1.13.0`. The title
            // says what actually judged, not what was asked for.
            answered.model,
            answered.usage.input_tokens
        ),
        output: clamped.text,
        metadata: serde_json::json!({
            "model": answered.model,
            "answers": answered.answers,
            "usage": answered.usage,
            "latency_ms": latency_ms,
        }),
    }
}

/// One answer as one line.
///
/// One line per question, in id order, because a model reading twelve
/// judgements needs them to line up — and because the output budget is the
/// whole call's, not each answer's.
fn line(id: &str, answer: &Answer) -> String {
    match answer {
        Answer::Noul { noul } => format!("{id}: noul={noul:.2}"),
        Answer::Choice { choice, probabilities, confidence } => {
            let others = probabilities
                .iter()
                .filter(|(option, _)| *option != choice)
                .map(|(option, probability)| format!("{option} {probability:.2}"))
                .collect::<Vec<_>>()
                .join(", ");
            // `?`, not `0.00`, when the chosen option is absent from its own
            // distribution: "the vendor did not say" and "the vendor said
            // zero" are different facts, and a model reading the second acts
            // on a certainty nobody expressed.
            let chosen = probabilities
                .get(choice)
                .map_or_else(|| "?".to_owned(), |probability| format!("{probability:.2}"));
            let line = format!("{id}: choice={choice} p={chosen} confidence={confidence:.2}");

            if others.is_empty() { line } else { format!("{line} ({others})") }
        }
        Answer::Score { score, legend, confidence, .. } => {
            format!("{id}: score={score:.2} of {} confidence={confidence:.2}", levels(legend))
        }
        // A `type` this build does not know. Named rather than hidden, so the
        // model can see that it asked something this build cannot read back
        // instead of finding the id simply missing.
        Answer::Other(_) => format!("{id}: {} (unrecognised answer type)", answer.kind()),
    }
}

/// The range a score was placed on, as `0..2`.
///
/// The legend's keys are level indices written as strings, so they are
/// compared as numbers: sorted as text, `"10"` would come before `"2"` and a
/// ten-level rubric would report a range it does not have. A key that is not
/// a number at all falls back to the map's own order, which is the only other
/// thing there is to say.
fn levels(legend: &BTreeMap<String, String>) -> String {
    let numbered =
        legend.keys().map(|level| level.parse::<i64>()).collect::<Result<Vec<_>, _>>().ok();

    match numbered.as_deref() {
        Some([]) | None => {
            let first = legend.keys().next().map_or("?", String::as_str);
            let last = legend.keys().next_back().map_or("?", String::as_str);

            format!("{first}..{last}")
        }
        Some(levels) => {
            let low = levels.iter().min().unwrap_or(&0);
            let high = levels.iter().max().unwrap_or(&0);

            format!("{low}..{high}")
        }
    }
}

#[cfg(test)]
#[path = "evaluate_tests.rs"]
mod tests;
