//! What the copy commands put on the clipboard.
//!
//! Spec: upstream `packages/tui/src/util/transcript.ts` (`formatTranscript`,
//! `formatMessage`, `formatPart`) and the two command bodies at
//! `routes/session/index.tsx:871-941`. The markdown shape is upstream's: an
//! `# title` heading, the session's id and times, then every message under a
//! `## User` / `## Assistant` heading with a `---` rule after each.
//!
//! Upstream's three render toggles are read from its kv store, all three
//! defaulting **on** (`index.tsx:254-257`); this port keeps `toolDetails` on
//! and diverges on the other two, in both cases because ganja has nothing to
//! print:
//!
//! - `thinking` is **off** here, and now by choice rather than by absence:
//!   [`PartBody::ReasoningText`] carries thinking a person can read, and the
//!   chat pane draws it. This surface is the clipboard, where what somebody
//!   means by "copy this conversation" is the conversation — so the model's
//!   way to an answer stays out of it, upstream's own default
//!   notwithstanding (deviation: `transcript-thinking-omitted`).
//! - `assistantMetadata` would print `## Assistant (Agent · model · duration)`
//!   (deviation: transcript-assistant-metadata-omitted). The transcript this
//!   renders from holds a role and its parts, and the agent a message ran as
//!   is not on [`Message`](ganja_protocol::Message) at all — filling the line from
//!   the session's *current* agent and model would misattribute every earlier
//!   message, which is worse than the heading upstream prints when the toggle
//!   is off.
//!
//! Times render as UTC rather than `toLocaleString`'s machine-local spelling,
//! following **D24**, which made the same call for the prompt's date block.

use ganja_core::SessionInfo;
use ganja_protocol::{Part, PartBody, Role, ToolState, team};
use jiff::Timestamp;

/// What a session with no title of its own is headed with. Upstream's title
/// is always a string; ganja's is absent until a title call has named the
/// conversation, and a fake-provider session never gets one at all.
const UNTITLED: &str = "Untitled session";

/// One message, as both formatters read it.
///
/// A pair rather than a [`Message`](ganja_protocol::Message) because the
/// transcript on screen is what gets copied, and the chat holds each entry as
/// its role and the parts that arrived — which is exactly, and only, what
/// these two functions need.
pub type Entry<'a> = (Role, &'a [Part]);

/// Why there was no message to copy.
///
/// The variants are upstream's three error toasts, which is why the empty-text
/// case is distinct from the no-text-parts one: a reply that was all tool
/// calls and a reply whose text came out blank are different things to be told
/// (`index.tsx:876-905`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Missing {
    /// Nothing in the transcript came from the model.
    #[error("No assistant messages found")]
    Assistant,
    /// The last reply carried no text parts — tool calls only.
    #[error("No text parts found in last assistant message")]
    TextParts,
    /// It carried text parts, and they were blank.
    #[error("No text content found in last assistant message")]
    Text,
}

/// The whole conversation, as markdown.
#[must_use]
pub fn format(session: &SessionInfo, messages: &[Entry<'_>]) -> String {
    let mut transcript = format!("# {}\n\n", session.title.as_deref().unwrap_or(UNTITLED));
    transcript.push_str(&format!("**Session ID:** {}\n", session.id.as_str()));
    transcript.push_str(&format!("**Created:** {}\n", stamp(session.created)));
    transcript.push_str(&format!("**Updated:** {}\n\n", stamp(session.updated)));
    transcript.push_str("---\n\n");

    for (role, parts) in messages {
        transcript.push_str(match role {
            Role::User => "## User\n\n",
            Role::Assistant => "## Assistant\n\n",
        });
        for part in *parts {
            transcript.push_str(&formatted(part));
        }
        transcript.push_str("---\n\n");
    }

    transcript
}

/// The text of the newest assistant message in `messages`.
///
/// Upstream's `messages.copy`: the parts are joined with a newline and
/// trimmed, and each of the three ways that can come to nothing is its own
/// answer.
///
/// # Errors
///
/// Returns the [`Missing`] variant naming which of the three it was.
pub fn last_reply(messages: &[Entry<'_>]) -> Result<String, Missing> {
    let parts = messages
        .iter()
        .rev()
        .find(|(role, _)| *role == Role::Assistant)
        .map(|(_, parts)| *parts)
        .ok_or(Missing::Assistant)?;

    let texts: Vec<&str> = parts.iter().filter_map(Part::as_text).collect();
    if texts.is_empty() {
        return Err(Missing::TextParts);
    }

    let text = texts.join("\n").trim().to_owned();
    if text.is_empty() {
        return Err(Missing::Text);
    }

    Ok(text)
}

/// One part, as upstream's `formatPart` writes it.
///
/// A part upstream has no arm for renders as nothing, which here is every
/// [`PartBody::File`] (the mention's own `@path` is already in the user text
/// beside it) and both step markers.
fn formatted(part: &Part) -> String {
    match &part.body {
        PartBody::Text { text } => format!("{text}\n\n"),
        PartBody::Tool { tool, state, .. } => {
            let mut rendered = format!("**Tool: {tool}**\n");

            match state {
                // A waiting call whose arguments have settled prints them the
                // way a running one does (2026-08-15); one still streaming
                // has nothing parseable to print.
                ToolState::Pending { input: Some(input) } => {
                    rendered.push_str(&fenced("Input", "json", &pretty(input)));
                }
                ToolState::Pending { input: None } => {}
                ToolState::Running { input, metadata, .. } => {
                    rendered.push_str(&fenced("Input", "json", &pretty(input)));
                    rendered.push_str(&calls_fence(metadata));
                }
                ToolState::Completed { input, output, metadata, .. } => {
                    rendered.push_str(&fenced("Input", "json", &pretty(input)));
                    rendered.push_str(&calls_fence(metadata));
                    rendered.push_str(&fenced("Output", "", output));
                }
                ToolState::Error { input, error, .. } => {
                    rendered.push_str(&fenced("Input", "json", &pretty(input)));
                    rendered.push_str(&fenced("Error", "", error));
                }
            }
            rendered.push('\n');

            rendered
        }
        // Sealed reasoning renders as nothing on purpose: it is bytes only the
        // provider can read, so the honest rendering of it is the one upstream
        // gives a part it has no arm for. Readable thinking renders as nothing
        // for a different reason, and a deliberate one: this is the clipboard,
        // and what a person means by "copy this conversation" is the
        // conversation, not the model's scratch paper. The pane draws it
        // behind a `∴`; the two surfaces are allowed to disagree, and the
        // output of this function is unchanged by its arriving here.
        // A tool the *provider* ran (**D489**) renders in the tool shape
        // above, because that is what it is: the conversation includes it,
        // and a copy that silently dropped the search a reply was built on
        // would be a copy of a reply with no visible source. `Tool:` rather
        // than a label of its own keeps upstream's vocabulary — the name it
        // carries already says whose tool it was.
        PartBody::ServerTool { tool, input, output } => {
            let mut rendered = format!("**Tool: {tool}**\n");
            if !input.is_null() {
                rendered.push_str(&fenced("Input", "json", &pretty(input)));
            }
            if !output.is_empty() {
                rendered.push_str(&fenced("Output", "", output));
            }
            rendered.push('\n');

            rendered
        }
        // A teammate's message renders for the server tool's reason and one
        // more (**D495**): the model was told this, so a copy that dropped it
        // would be a copy of a reply answering something the reader cannot
        // see. The name leads it because this is the one part in a transcript
        // that neither of the two headings above wrote, and a copy that let a
        // peer's sentence pass for the session's own would misattribute it.
        PartBody::Peer { from, summary, body, .. } => {
            // `display_summary` owns the blank-dropped, capped projection
            // this formatter shares with the two renderers. The summary then
            // rides this heading as text and only text ([`inline_text`]) —
            // the heading is ganja's own sentence about who wrote what
            // follows, and a field inside it that could still set its own
            // bold is a peer writing in this side's voice.
            let mut rendered = match team::display_summary(summary.as_deref()) {
                Some(line) => {
                    format!("**Teammate: {from}** — {}\n\n", inline_text(line))
                }
                None => format!("**Teammate: {from}**\n\n"),
            };
            // **The decision this arm exists for.** The heading above is a
            // claim about who wrote what follows, and what follows is the one
            // thing in a transcript that neither the person nor the model
            // wrote. Left raw, a peer could put `**Teammate: someone-else**`
            // — or a `## Assistant`, or a `---` — in its own message and forge
            // a heading in the copied markdown, which is exactly the
            // misattribution this arm was added to prevent.
            //
            // So the body is quoted rather than pasted: a fenced block, with a
            // fence longer than any run of backticks inside it, which is
            // CommonMark's own escape and the shape this module already uses
            // for content the conversation *received* (a tool's input, output
            // and error, above). The cost is that a teammate's markdown reads
            // as source instead of rendering, and that is the honest trade: a
            // quote that re-renders as this document's own markup is the
            // forgery. `/copy` still copies what was said, whole and
            // unedited — the fence adds no character to the body and removes
            // none.
            //
            // The name needs none of this. A member name is §1.1's
            // `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`, so it holds no newline and
            // no markdown metacharacter to forge with.
            //
            // This is the clipboard's defence, not the model's: what keeps a
            // peer's words from carrying authority into a *request* is the
            // `<teammate-message>` envelope the engine builds at request
            // assembly (D495). Two surfaces, two readers, two mechanisms.
            let fence = fence_for(body);
            rendered.push_str(&format!("{fence}\n{body}\n{fence}\n\n"));

            rendered
        }
        PartBody::File { .. }
        | PartBody::StepStart
        | PartBody::StepFinish { .. }
        | PartBody::Patch { .. }
        | PartBody::ReasoningText { .. }
        | PartBody::Reasoning { .. } => String::new(),
    }
}

/// One labelled fenced block, in upstream's layout.
fn fenced(label: &str, language: &str, body: &str) -> String {
    format!("\n**{label}:**\n```{language}\n{body}\n```\n")
}

/// The fence a body cannot close: three backticks, or one more than the
/// longest run of them inside it.
///
/// CommonMark's own rule — a fenced block ends at a run of the fence character
/// at least as long as the one that opened it — so a fence one longer than
/// anything in the body is a fence the body cannot end.
fn fence_for(body: &str) -> String {
    let longest = body.split(|character| character != '`').map(str::len).max().unwrap_or(0);

    "`".repeat(longest.max(2) + 1)
}

/// A peer-authored one-line field, as text that can only be text.
///
/// Two rules, both of them about the line it is going to sit on:
///
/// - it is folded onto one line. Both line breaks, because a lone `\r` ends a
///   line in enough readers to count; a `\r\n` therefore becomes two spaces,
///   which is the whole of what that costs.
/// - markdown's three inline markers are backslash-escaped, so a summary
///   cannot set bold, italics or code inside a heading this side wrote. The
///   backslash goes first, and has to: escaping it afterwards would walk back
///   over the escapes the other three just wrote.
///
/// Nothing else needs escaping here. `#`, `>` and `-` are block markers, which
/// only mean anything at the start of a line — and after the fold this text
/// never starts one.
fn inline_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('`', "\\`")
        .replace(['\n', '\r'], " ")
}

/// The child-call log a `task` part's metadata carries (2026-08-15), rendered
/// whole — the expansion the chat row's clamp hint points at, with the rows
/// the engine's own cap dropped admitted off the true `toolcalls` count.
/// Empty for a part that carries no log, so every other tool is unchanged.
fn calls_fence(metadata: &serde_json::Value) -> String {
    let calls: Vec<&str> = metadata
        .get("calls")
        .and_then(serde_json::Value::as_array)
        .map(|calls| calls.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    if calls.is_empty() {
        return String::new();
    }

    let total = metadata
        .get("toolcalls")
        .and_then(serde_json::Value::as_u64)
        .map_or(calls.len(), |total| usize::try_from(total).unwrap_or(usize::MAX));
    let mut body = String::new();
    let dropped = total.saturating_sub(calls.len());
    if dropped > 0 {
        body.push_str(&format!("\u{2026} +{dropped} earlier\n"));
    }
    body.push_str(&calls.join("\n"));

    fenced("Calls", "", &body)
}

/// A tool's arguments, indented the way upstream's `JSON.stringify(_, null, 2)`
/// indents them.
fn pretty(input: &serde_json::Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// `millis` since the Unix epoch, as `YYYY-MM-DD HH:MM:SS UTC`.
///
/// A shared formatter's home would be a crate, not a module, and the only
/// common leaf is `ganja-protocol`, whose external dependency allowlist is
/// intentionally fixed. The thin jiff call stays per-crate so the protocol
/// vocabulary does not become a utility layer.
fn stamp(millis: u64) -> String {
    let millis = i64::try_from(millis).unwrap_or(i64::MAX);
    let timestamp = Timestamp::from_millisecond(millis).unwrap_or(Timestamp::MAX);

    timestamp.strftime("%Y-%m-%d %H:%M:%S UTC").to_string()
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
