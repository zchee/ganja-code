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
use ganja_protocol::{Part, PartBody, Role, ToolState};

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
                ToolState::Pending => {}
                ToolState::Running { input, .. } => {
                    rendered.push_str(&fenced("Input", "json", &pretty(input)))
                }
                ToolState::Completed { input, output, .. } => {
                    rendered.push_str(&fenced("Input", "json", &pretty(input)));
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
        // behind a `✻`; the two surfaces are allowed to disagree, and the
        // output of this function is unchanged by its arriving here.
        // A tool the *provider* ran (**D489**) renders in the tool shape
        // above, because that is what it is: the conversation includes it,
        // and a copy that silently dropped the search a reply was built on
        // would be a copy of a reply with no visible source. `Tool:` rather
        // than a label of its own keeps upstream's vocabulary — the name it
        // carries already says whose tool it was.
        PartBody::ServerTool {
            tool,
            input,
            output,
        } => {
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

/// A tool's arguments, indented the way upstream's `JSON.stringify(_, null, 2)`
/// indents them.
fn pretty(input: &serde_json::Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// `millis` since the Unix epoch, as `YYYY-MM-DD HH:MM:SS UTC`.
///
/// The calendar arithmetic is spelled out for the same reason
/// `ganja_core::instruction` spells its own copy out — two timestamps are not
/// worth a date crate — and duplicated rather than shared because that copy is
/// private to another crate. Worth folding into one exported helper the day
/// either side needs a third caller.
fn stamp(millis: u64) -> String {
    let seconds = millis / 1_000;
    let days = i64::try_from(seconds / 86_400).unwrap_or_default();
    let (hour, minute, second) = (seconds / 3_600 % 24, seconds / 60 % 60, seconds % 60);
    let (year, month, day) = civil_date(days);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Days in each month of a non-leap year.
const MONTH_LENGTHS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// The civil date `days` after 1970-01-01, as `(year, month, day)` with the
/// month 1-based.
fn civil_date(days: i64) -> (i64, u32, u32) {
    /// Days in the 400-year Gregorian cycle: 400 × 365 plus its leap days.
    const CYCLE: i64 = 146_097;

    let mut year = 1970;
    let mut remaining = days;

    // Whole cycles first, so a date centuries away costs a handful of
    // iterations rather than a loop over every year in between.
    let cycles = remaining.div_euclid(CYCLE);
    year += cycles * 400;
    remaining -= cycles * CYCLE;

    loop {
        let length = if leap(year) { 366 } else { 365 };
        if remaining < length {
            break;
        }
        remaining -= length;
        year += 1;
    }

    let mut month = 1;
    for (index, length) in MONTH_LENGTHS.iter().enumerate() {
        let length = i64::from(*length) + i64::from(index == 1 && leap(year));
        if remaining < length {
            break;
        }
        remaining -= length;
        month += 1;
    }

    (
        year,
        month,
        u32::try_from(remaining).unwrap_or_default() + 1,
    )
}

/// Whether `year` is a leap year in the proleptic Gregorian calendar.
fn leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use ganja_core::{SessionId, SessionInfo};
    use ganja_protocol::{Part, PartBody, Role, ToolState, Usage};

    use super::{Missing, civil_date, format, last_reply, stamp};

    /// A session whose times are fixed, so what this renders is the same
    /// string on every machine that runs it.
    fn session(title: Option<&str>) -> SessionInfo {
        SessionInfo {
            effort: None,
            id: SessionId::from("ses_fixture".to_owned()),
            version: ganja_core::storage::VERSION,
            title: title.map(str::to_owned),
            // 2026-08-04 12:00:00 UTC and one minute later.
            created: 1_785_844_800_000,
            updated: 1_785_844_860_000,
            usage: Usage::default(),
            context_tokens: 0,
            summary: None,
            agent: None,
            model: None,
            parent: None,
            revert: None,
        }
    }

    fn completed(tool: &str, input: serde_json::Value, output: &str) -> Part {
        Part {
            id: ganja_protocol::PartId::ascending(),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: tool.to_owned(),
                state: ToolState::Completed {
                    input,
                    output: output.to_owned(),
                    title: tool.to_owned(),
                    metadata: serde_json::Value::Null,
                    started: 0,
                    completed: 1,
                },
            },
        }
    }

    /// The whole shape in one assertion, because the shape *is* the port:
    /// heading, the three fields, and a rule after every message.
    #[test]
    fn a_transcript_is_upstreams_markdown_shape() {
        let asked = [Part::text("what does this do?")];
        let answered = [Part::text("it copies things.")];
        let rendered = format(
            &session(Some("clipboard work")),
            &[(Role::User, &asked[..]), (Role::Assistant, &answered[..])],
        );

        assert_eq!(
            rendered,
            "# clipboard work\n\n\
             **Session ID:** ses_fixture\n\
             **Created:** 2026-08-04 12:00:00 UTC\n\
             **Updated:** 2026-08-04 12:01:00 UTC\n\n\
             ---\n\n\
             ## User\n\n\
             what does this do?\n\n\
             ---\n\n\
             ## Assistant\n\n\
             it copies things.\n\n\
             ---\n\n"
        );
    }

    #[test]
    fn a_session_nothing_has_named_is_still_headed() {
        let rendered = format(&session(None), &[]);

        assert!(
            rendered.starts_with("# Untitled session\n\n"),
            "got: {rendered}"
        );
    }

    /// Tool details are the one upstream toggle this port keeps on, so a call
    /// carries its arguments and its result into the copy.
    #[test]
    fn a_tool_call_carries_its_input_and_output() {
        let parts = [completed(
            "read",
            serde_json::json!({ "file_path": "src/lib.rs" }),
            "one line",
        )];

        let rendered = format(&session(None), &[(Role::Assistant, &parts[..])]);

        assert!(rendered.contains("**Tool: read**\n"), "got: {rendered}");
        assert!(
            rendered
                .contains("\n**Input:**\n```json\n{\n  \"file_path\": \"src/lib.rs\"\n}\n```\n"),
            "the arguments render pretty-printed: {rendered}"
        );
        assert!(
            rendered.contains("\n**Output:**\n```\none line\n```\n"),
            "got: {rendered}"
        );
    }

    /// A tool the *provider* ran belongs in a copy of the conversation too
    /// (**D489**): a reply built on a search, copied without the search, is a
    /// reply whose source vanished.
    #[test]
    fn a_provider_run_tool_is_copied_in_the_tool_shape() {
        let parts = [Part {
            id: ganja_protocol::PartId::ascending(),
            body: PartBody::ServerTool {
                tool: "openrouter:web_search".to_owned(),
                input: serde_json::json!({ "query": "rust 2024" }),
                output: "3 results".to_owned(),
            },
        }];

        let rendered = format(&session(None), &[(Role::Assistant, &parts[..])]);

        assert!(
            rendered.contains("**Tool: openrouter:web_search**\n"),
            "the name it came under, namespace and all: {rendered}"
        );
        assert!(
            rendered.contains("\n**Input:**\n```json\n{\n  \"query\": \"rust 2024\"\n}\n```\n"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("\n**Output:**\n```\n3 results\n```\n"),
            "got: {rendered}"
        );

        // A row the gateway reported nothing for draws no empty fences.
        let bare = [Part {
            id: ganja_protocol::PartId::ascending(),
            body: PartBody::ServerTool {
                tool: "openrouter:datetime".to_owned(),
                input: serde_json::Value::Null,
                output: String::new(),
            },
        }];
        let rendered = format(&session(None), &[(Role::Assistant, &bare[..])]);
        assert!(
            rendered.contains("**Tool: openrouter:datetime**"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("**Input:**") && !rendered.contains("**Output:**"),
            "an absent field is absent rather than an empty block: {rendered}"
        );
    }

    #[test]
    fn a_failed_call_carries_what_went_wrong() {
        let parts = [Part {
            id: ganja_protocol::PartId::ascending(),
            body: PartBody::Tool {
                call_id: "call_1".to_owned(),
                tool: "edit".to_owned(),
                state: ToolState::Error {
                    input: serde_json::json!({}),
                    error: "the file has not been read".to_owned(),
                    started: 0,
                    completed: 1,
                },
            },
        }];

        let rendered = format(&session(None), &[(Role::Assistant, &parts[..])]);

        assert!(
            rendered.contains("\n**Error:**\n```\nthe file has not been read\n```\n"),
            "got: {rendered}"
        );
    }

    /// A mention is already in the user's own text beside it, so the part
    /// that carries the reference renders as nothing — upstream's `formatPart`
    /// has no arm for it either.
    #[test]
    fn the_parts_upstream_has_no_arm_for_render_as_nothing() {
        let parts = [
            Part::file("src/lib.rs", "text/plain"),
            Part {
                id: ganja_protocol::PartId::ascending(),
                body: PartBody::StepStart,
            },
        ];

        let rendered = format(&session(None), &[(Role::User, &parts[..])]);

        assert!(rendered.ends_with("## User\n\n---\n\n"), "got: {rendered}");
    }

    /// **AC5.** The pane draws thinking behind a `✻`; the clipboard does not
    /// carry it. What a person means by "copy this conversation" is the
    /// conversation, and the model's way to an answer is not the answer —
    /// which is also why `last_reply` cannot be answered by one.
    #[test]
    fn thinking_is_on_the_screen_and_never_on_the_clipboard() {
        let parts = [
            Part::reasoning_text("weighing a greeting"),
            Part::text("Hello, world!"),
        ];

        let rendered = format(&session(None), &[(Role::Assistant, &parts[..])]);

        assert!(!rendered.contains("weighing a greeting"), "got: {rendered}");
        assert!(rendered.contains("Hello, world!"), "got: {rendered}");

        let thinking = [Part::reasoning_text("weighing a greeting")];
        assert!(
            last_reply(&[(Role::Assistant, &thinking[..])]).is_err(),
            "a turn that only thought has no reply to hand over"
        );
    }

    #[test]
    fn the_last_reply_is_its_text_parts_joined_and_trimmed() {
        let first = [Part::text("an older answer")];
        let last = [Part::text("  the newest answer  "), Part::text("and more")];

        assert_eq!(
            last_reply(&[
                (Role::Assistant, &first[..]),
                (Role::User, &[][..]),
                (Role::Assistant, &last[..]),
            ]),
            Ok("the newest answer  \nand more".to_owned()),
            "the join is between parts and the trim is around the whole"
        );
    }

    /// The three ways there is nothing to copy, which upstream tells apart.
    #[test]
    fn each_way_a_reply_comes_to_nothing_says_which_it_was() {
        let user = [Part::text("asked")];
        let tools = [completed("read", serde_json::json!({}), "output")];
        let blank = [Part::text("   \n ")];

        let cases = [
            (vec![(Role::User, &user[..])], Missing::Assistant),
            (vec![(Role::Assistant, &tools[..])], Missing::TextParts),
            (vec![(Role::Assistant, &blank[..])], Missing::Text),
        ];

        for (messages, expected) in cases {
            assert_eq!(last_reply(&messages), Err(expected));
        }
        assert_eq!(last_reply(&[]), Err(Missing::Assistant));
    }

    /// Upstream's toast texts, which the status line prints verbatim.
    #[test]
    fn the_refusals_are_spelled_the_way_upstream_spells_them() {
        let cases = [
            (Missing::Assistant, "No assistant messages found"),
            (
                Missing::TextParts,
                "No text parts found in last assistant message",
            ),
            (
                Missing::Text,
                "No text content found in last assistant message",
            ),
        ];

        for (missing, expected) in cases {
            assert_eq!(missing.to_string(), expected);
        }
    }

    /// The calendar, at the edges that catch an off-by-one: an epoch, a leap
    /// day, the century that is not a leap year and the one that is.
    #[test]
    fn the_civil_date_holds_at_the_gregorian_edges() {
        let cases = [
            (0_i64, (1970_i64, 1_u32, 1_u32)),
            (59, (1970, 3, 1)),
            (365, (1971, 1, 1)),
            // 2000 was a leap year (÷400) where 1900 was not (÷100).
            (11_016, (2000, 2, 29)),
            (11_017, (2000, 3, 1)),
            (20_513, (2026, 3, 1)),
        ];

        for (days, expected) in cases {
            assert_eq!(civil_date(days), expected, "{days} days after the epoch");
        }
    }

    #[test]
    fn a_stamp_is_the_utc_wall_clock_of_its_milliseconds() {
        let cases = [
            (0_u64, "1970-01-01 00:00:00 UTC"),
            (1_785_844_800_000, "2026-08-04 12:00:00 UTC"),
            // Milliseconds below the second are dropped, not rounded.
            (1_785_844_800_999, "2026-08-04 12:00:00 UTC"),
            (1_785_931_199_000, "2026-08-05 11:59:59 UTC"),
        ];

        for (millis, expected) in cases {
            assert_eq!(stamp(millis), expected);
        }
    }
}
