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
//! returned and **loses the words of its own earlier replies**. An
//! ask the model answered in text alone therefore reads as unanswered: run
//! 9d's preamble ended on run 1's third ask, and with no reply beneath it the
//! model obeyed the transcript and called the tool again. That is not a
//! defect to be fixed here.
//!
//! # A compaction summary is carried, in the user's voice
//!
//! The one assistant message this render does not drop is a **compaction
//! summary**, and it is recognised by position: ganja's window opens on an
//! assistant message exactly when a compaction has replaced the history with
//! its summary (`session.rs`'s `compact_if_needed` installs `[summary]` as
//! the whole window). Dropping it, as this module did first, made `/compact`
//! on this wire open a fresh record with the prompt alone — a compaction that
//! kept nothing. So it is rendered as **context the user carries in**: a
//! `[User]` paragraph opening [`CARRIED_CONTEXT`], its text through
//! `neutralize` like every other rendered byte, and never an `[Assistant]`
//! line — the assistant voice is the one the recording measured refused.
//!
//! **Unmeasured live.** No recorded run carried a summary at all, so whether
//! the vendor's safeguard serves model-written text in the user's voice is
//! not known. If it refuses, the bound the wire already has catches it: a
//! refused record's replacement opens with no preamble, and a second refusal
//! in a row spends nothing.
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

/// What a carried compaction summary opens with, after its `[User]` marker.
///
/// It says whose words follow and how old they are, so the model reads the
/// summary as the conversation's own memory handed back rather than as a new
/// ask to act on.
///
/// Not one of `MARKERS`: it never opens a line — `[User]` does, and that
/// marker is already escaped wherever rendered content spells it.
pub const CARRIED_CONTEXT: &str = "Context carried from before this record:";

/// Every line marker this module's grammar gives a meaning to.
///
/// Declared beside the three constants because the set and its escaping are
/// one rule: this render flattens a structured transcript into text, so the
/// model's only cue for *who said this* is the marker at the head of a line —
/// and a line of **content** that begins with one would be read as a turn
/// somebody took. Content reaching a `[Tool Result]` is routinely not the
/// operator's: a fetched page, a file from a cloned repository, an MCP
/// server's answer. So a marker is not merely rendered here, it is also the
/// one thing rendered text may not spell, and [`neutralize`] is what enforces
/// that. A marker added above belongs in this list in the same commit.
const MARKERS: &[&str] = &[
    HEADER,
    MID_TURN_RESUME,
    MID_TURN_HEADER,
    "[User]",
    "[Assistant]",
    "[Tool Call]",
    "[Tool Result]",
    "[Tool Result (error)]",
];

/// What an escaped marker line opens with.
///
/// A backslash: the conventional "this bracket is literal", visible to the
/// reader rather than hidden, and a byte no marker carries — so an escaped
/// line cannot itself be escaped into a marker.
const ESCAPE: char = '\\';

/// Rendered content that cannot spell one of [`MARKERS`].
///
/// Only a line that *begins* with a marker — at its first byte, or after its
/// leading whitespace — is touched, and it is touched by one character. Every
/// other byte of a rendering is what the recording measured being served, so
/// a blanket re-indent would change all of it to fix a line in a thousand —
/// and this wire's renderings are the one thing about it a live safeguard has
/// already ruled on.
fn neutralize(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    // `split_inclusive` rather than `lines`, which would eat a `\r` and hand
    // the model back content it was not given.
    for line in text.split_inclusive('\n') {
        // Looked for past the indentation, because how a model reads
        // `  [User]` is not something this side can assert (RR-3); and the
        // escape goes immediately before the marker, so the indentation stays
        // byte for byte and the bracket reads as literal the way it does at
        // the start of a line.
        let marked = line.trim_start();
        if MARKERS.iter().any(|marker| marked.starts_with(marker)) {
            let indent = line.len() - marked.len();
            escaped.push_str(&line[..indent]);
            escaped.push(ESCAPE);
            escaped.push_str(marked);
        } else {
            escaped.push_str(line);
        }
    }

    escaped
}

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
/// negotiable. A history that opens on an assistant message opens on a
/// compaction summary, and that one is carried as a `[User]` paragraph under
/// [`CARRIED_CONTEXT`] instead of being dropped.
///
/// A `Peer` part is another agent's words and is treated as the assistant's
/// for this rule: nothing rendered.
#[must_use]
pub fn render(history: &[Message]) -> Rendered {
    let mut rendered = lines(history, Summary::Carried);
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
    // A turn's slice opens on its own prompt, so nothing in it is a summary.
    let mut rendered = lines(turn, Summary::Dropped);
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
    format!("{MID_TURN_HEADER} {}", neutralize(&message_text(message)))
}

/// Whether a history's leading assistant message is carried as a summary.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Summary {
    /// The whole history before a turn: a leading assistant message is the
    /// compaction summary, carried under [`CARRIED_CONTEXT`].
    Carried,
    /// A turn's own messages: every assistant message is dropped.
    Dropped,
}

/// The three line kinds, with no header and no closing line.
fn lines(messages: &[Message], summary: Summary) -> Rendered {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut user_ids = Vec::new();
    let mut assistant_turns_dropped = 0;

    for (index, message) in messages.iter().enumerate() {
        // An assistant message contributes no text and no id — but its tool
        // parts still do, because a `[Tool Call]` and its `[Tool Result]` are
        // the trail that makes an answered ask read as answered. An ask
        // answered in words alone has no trail, and the model reads it as
        // unanswered: run 9d obeyed exactly that and called the tool again.
        //
        // The exception is the summary a compacted window opens on, which is
        // carried in the user's voice. Its id joins no `user_ids`: it is not
        // a user message, and `sent` is the request's user-id list.
        if message.role == Role::Assistant {
            let text = message_text(message);
            if summary == Summary::Carried && index == 0 && !text.trim().is_empty() {
                paragraphs.push(format!("[User] {CARRIED_CONTEXT}\n\n{}", neutralize(&text)));
            } else {
                assistant_turns_dropped += 1;
            }
        } else {
            let text = message_text(message);
            if !text.trim().is_empty() {
                paragraphs.push(format!("[User] {}", neutralize(&text)));
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
        // A result and an error are the two blocks whose content the operator
        // did not write, so both go through `neutralize` before the marker
        // above them means anything.
        ToolState::Completed { input, output, .. } => {
            vec![call(input), format!("[Tool Result]\n{}", neutralize(output))]
        }
        ToolState::Error { input, error, .. } => {
            vec![call(input), format!("[Tool Result (error)]\n{}", neutralize(error))]
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
