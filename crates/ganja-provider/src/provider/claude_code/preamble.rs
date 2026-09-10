//! Ganja's transcript rendered as the text a fresh record opens with.
//!
//! Spec: the recording. Run 9d carried a rendering of run 1 with **no**
//! `[Assistant]` line and was served; runs 9a, 9b and 9c carried the same
//! conversation *with* its assistant text and were refused three of three,
//! the refusal's own explanation naming "duplicating model outputs". So a
//! rendered transcript is served when it carries the user's asks and the tool
//! trail and not the model's own words, and that is what this module renders.
//!
//! The cost is stated where it falls due rather than hidden: across every
//! fresh record the model keeps what it was asked and what its tools
//! returned and **loses the words of its own earlier replies**. Two
//! consequences follow, and neither is a defect to be fixed here. A
//! compaction summary is `Message::assistant`, so it renders as *nothing* —
//! `/compact` on this wire opens a fresh record with the prompt alone. And an
//! ask the model answered in text alone reads as unanswered: run 9d's
//! preamble ended on run 1's third ask, and with no reply beneath it the
//! model obeyed the transcript and called the tool again.
//!
//! # The render is a write
//!
//! [`render`] returns the text **and the ids of the user messages it
//! rendered**. That is not bookkeeping — it is the rule that keeps a fresh
//! record's opening frame from carrying a message twice and the next request
//! honest. The caller empties `sent`, appends what the render returns, and
//! only then computes what is owed; so the owed set is by construction the
//! messages the preamble did not cover, and after the frame `sent` is the
//! request's whole ordered user-id list.
//!
//! Without it the arm has two producers and no rule for their overlap: on the
//! `locked-elsewhere` arm, where no binding is read and `sent` starts empty,
//! the whole conversation would go to the CLI twice in one paid frame.

use crate::protocol::{Message, PartBody, Role, ToolState};
use crate::provider::cursor::history::{CALL_INPUT_LIMIT, clamp};

/// The line a fresh record's preamble opens with.
///
/// It says what the block is, so the model reads a rendering as a rendering
/// rather than as somebody talking strangely.
pub const HEADER: &str = "[Conversation so far]";

/// The line a recovered turn's rendering closes with.
///
/// The turn's tool calls were answered by the engine on a process that is
/// gone, so the model is told that — otherwise it reads its own unanswered
/// `[Tool Call]` lines as work still to do and does it again.
pub const MID_TURN_RESUME: &str = "[the tool calls above have been answered; continue the turn]";

/// The line a message carried inside a **deny** answer is introduced by.
///
/// Only the deny path carries one: a `deny.message` is text the model reads
/// as the tool's own refusal, where the same words inside an allowed call's
/// result are a user's voice arriving through a tool, which the model names
/// as injection and declines (M19 (b)).
pub const MID_TURN_HEADER: &str = "[User, while the tool ran]";

/// What one rendering produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    /// The text, or empty when nothing was renderable.
    pub text: String,
    /// The ids of the user messages the text carries, in order. The caller
    /// appends these to a freshly emptied `sent` **before** computing what is
    /// owed.
    pub user_ids: Vec<String>,
    /// How many assistant messages the render skipped.
    ///
    /// The cost of a fresh record, counted so the line that pays it can say
    /// so: an idle eviction logs `assistant_turns_dropped` and a person can
    /// see how much of the conversation's own voice the next turn opens
    /// without.
    pub assistant_turns_dropped: usize,
}

/// Renders the conversation before this turn.
///
/// `[User]`, `[Tool Call] <tool> <input>` and `[Tool Result]` /
/// `[Tool Result (error)]` lines, under [`HEADER`]. **Never an `[Assistant]`
/// line** — see the module doc for what that costs and why it is not
/// negotiable.
///
/// A `Peer` part is another agent's words and is treated as the assistant's
/// for this rule: nothing rendered.
#[must_use]
pub fn render(history: &[Message]) -> Rendered {
    let mut rendered = lines(history);
    if rendered.text.is_empty() {
        return rendered;
    }

    rendered.text = format!("{HEADER}\n\n{}", rendered.text);

    rendered
}

/// Renders this turn's own messages for the recover arm, closed by
/// [`MID_TURN_RESUME`].
///
/// The slice is `messages[turn_start..]`, so it carries the turn's prompt as
/// well as its tool parts — the prompt is what the model is being asked to
/// continue, and dropping it would reopen a turn with a tool trail and no
/// question.
#[must_use]
pub fn render_turn(turn: &[Message]) -> Rendered {
    let mut rendered = lines(turn);
    if rendered.text.is_empty() {
        rendered.text = MID_TURN_RESUME.to_owned();

        return rendered;
    }

    rendered.text = format!("{}\n\n{MID_TURN_RESUME}", rendered.text);

    rendered
}

/// A message's text under [`MID_TURN_HEADER`], for a `deny.message` to carry.
///
/// Its one caller is the bridge's deny path. The preamble never produces such
/// a block by construction: it renders ganja's own messages, in which a steer
/// is an ordinary user message.
#[must_use]
pub fn carried(message: &Message) -> String {
    format!("{MID_TURN_HEADER} {}", message_text(message))
}

/// The three line kinds, with no header and no closing line.
fn lines(messages: &[Message]) -> Rendered {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut user_ids = Vec::new();
    let mut assistant_turns_dropped = 0;

    for message in messages {
        // An assistant message contributes no text and no id — but its tool
        // parts still do, because a `[Tool Call]` and its `[Tool Result]` are
        // the trail that makes an answered ask read as answered. An ask
        // answered in words alone has no trail, and the model reads it as
        // unanswered: run 9d obeyed exactly that and called the tool again.
        if message.role == Role::Assistant {
            assistant_turns_dropped += 1;
        } else {
            let text = message_text(message);
            if !text.trim().is_empty() {
                paragraphs.push(format!("[User] {text}"));
                user_ids.push(message.id.as_str().to_owned());
            }
        }

        for part in &message.parts {
            if let PartBody::Tool { tool, state, .. } = &part.body {
                paragraphs.extend(tool_lines(tool, state));
            }
        }
    }

    Rendered { text: paragraphs.join("\n\n"), user_ids, assistant_turns_dropped }
}

/// The `[Tool Call]` line a finished call earns, and the `[Tool Result]` line
/// its outcome earns.
///
/// A call still pending or running has neither: it has not been asked yet in
/// any sense the model could read, and rendering a call with no answer is
/// what makes a model redo work.
fn tool_lines(tool: &str, state: &ToolState) -> Vec<String> {
    let call = |input: &serde_json::Value| {
        let (input, _) = clamp(input.to_string());

        format!("[Tool Call] {tool} {input}")
    };

    match state {
        ToolState::Completed { input, output, .. } => {
            vec![call(input), format!("[Tool Result]\n{output}")]
        }
        ToolState::Error { input, error, .. } => {
            vec![call(input), format!("[Tool Result (error)]\n{error}")]
        }
        ToolState::Pending { .. } | ToolState::Running { .. } => Vec::new(),
    }
}

/// A message's text, the way every frame this wire writes renders one.
///
/// Text parts joined with a blank line, and a `File` part degraded to its
/// name: this wire carries no attachment, so a file the person attached is
/// named rather than dropped — the model can at least ask about it, and a
/// recorded limitation is better than a silence.
///
/// Every other part kind contributes nothing, each for its own reason —
/// `Peer` is another agent's words, `Reasoning` is sealed for a wire that is
/// not this one, `ReasoningText` is display-only, `ServerTool` is another
/// provider's report, and `Tool`, `StepStart`, `StepFinish` and `Patch` are
/// structure rather than speech. The match names all ten with no wildcard, so
/// a new variant is a compile error here rather than a silent omission.
#[must_use]
pub fn message_text(message: &Message) -> String {
    message
        .parts
        .iter()
        .filter_map(|part| match &part.body {
            PartBody::Text { text } => Some(text.clone()),
            PartBody::File { path, .. } => Some(format!("[attached: {path}]")),
            PartBody::Tool { .. }
            | PartBody::Peer { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Reasoning { .. }
            | PartBody::ReasoningText { .. }
            | PartBody::ServerTool { .. }
            | PartBody::Patch { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The bound one rendered `[Tool Call]` input is cut at, re-exported so a
/// reader of this module does not have to know it is cursor's.
pub const INPUT_LIMIT: usize = CALL_INPUT_LIMIT;

#[cfg(test)]
#[path = "preamble_tests.rs"]
mod tests;
