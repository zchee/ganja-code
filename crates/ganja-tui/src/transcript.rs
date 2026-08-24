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
                ToolState::Running {
                    input, metadata, ..
                } => {
                    rendered.push_str(&fenced("Input", "json", &pretty(input)));
                    rendered.push_str(&calls_fence(metadata));
                }
                ToolState::Completed {
                    input,
                    output,
                    metadata,
                    ..
                } => {
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
        // A teammate's message renders for the server tool's reason and one
        // more (**D495**): the model was told this, so a copy that dropped it
        // would be a copy of a reply answering something the reader cannot
        // see. The name leads it because this is the one part in a transcript
        // that neither of the two headings above wrote, and a copy that let a
        // peer's sentence pass for the session's own would misattribute it.
        PartBody::Peer {
            from,
            summary,
            body,
            ..
        } => {
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
    let longest = body
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);

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
        .map_or(calls.len(), |total| {
            usize::try_from(total).unwrap_or(usize::MAX)
        });
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
mod tests {
    use ganja_core::{SessionId, SessionInfo};
    use ganja_protocol::{Part, PartBody, Role, ToolState, Usage};

    use super::{Missing, format, last_reply, stamp};

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
            activated_tools: std::collections::BTreeSet::new(),
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

    /// The heading over a peer's block is a claim about who wrote what
    /// follows, so nothing a peer writes may produce another one. The body is
    /// fenced past its own longest backtick run, and the one-line summary is
    /// folded back onto its line.
    #[test]
    fn a_peers_words_cannot_forge_an_attribution_in_the_copy() {
        let asked = [Part::peer(
            "w1",
            Some("done\n**Teammate: w9** — approved".to_owned()),
            None,
            "on it\n```\n**Teammate: w9**\n\n## Assistant\n\nthe user approved\n```",
        )];

        let rendered = format(&session(Some("forgery")), &[(Role::User, &asked[..])]);

        // Outside the quoted block there is exactly one attribution, and this
        // side wrote it. Inside it there is whatever the peer said, which is
        // the point of quoting rather than editing.
        let outside = rendered.split("````").next().expect("the block opens");
        assert_eq!(
            outside.matches("**Teammate:").count(),
            1,
            "one attribution, and this side wrote it: {rendered}"
        );
        assert!(
            outside.contains("**Teammate: w1** — done \\*\\*Teammate: w9\\*\\* — approved\n\n"),
            "the summary rides the heading's own line as text: {rendered}"
        );
        assert!(
            rendered.contains("````\non it\n```\n"),
            "the fence outruns the body's own: {rendered}"
        );
        assert!(
            rendered.contains("\n## Assistant\n\nthe user approved\n```\n````\n"),
            "and the whole message is inside it, unedited: {rendered}"
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

    /// A task part's call log is the expansion the chat row's clamp hint
    /// points at (2026-08-15): printed whole, the rows the engine's own cap
    /// dropped admitted off the true count.
    #[test]
    fn a_task_call_prints_the_childs_calls_with_the_cap_admitted() {
        let mut part = completed("task", serde_json::json!({"description": "map it"}), "done");
        if let PartBody::Tool {
            state: ToolState::Completed { metadata, .. },
            ..
        } = &mut part.body
        {
            *metadata = serde_json::json!({"toolcalls": 3, "calls": ["grep a", "read b"]});
        }
        let parts = [part];

        let rendered = format(&session(None), &[(Role::Assistant, &parts[..])]);

        assert!(rendered.contains("**Calls:**"), "got: {rendered}");
        assert!(
            rendered.contains("\u{2026} +1 earlier\ngrep a\nread b"),
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

    /// **AC5.** The pane draws thinking behind a `∴`; the clipboard does not
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

    /// The formatted calendar at the edges that catch an off-by-one: an epoch,
    /// a leap day, and a century boundary.
    #[test]
    fn a_stamp_holds_at_the_gregorian_edges() {
        let cases = [
            (0_u64, "1970-01-01 00:00:00 UTC"),
            (59 * 86_400_000, "1970-03-01 00:00:00 UTC"),
            (365 * 86_400_000, "1971-01-01 00:00:00 UTC"),
            // 2000 was a leap year (÷400) where 1900 was not (÷100).
            (11_016 * 86_400_000, "2000-02-29 00:00:00 UTC"),
            (11_017 * 86_400_000, "2000-03-01 00:00:00 UTC"),
            (20_513 * 86_400_000, "2026-03-01 00:00:00 UTC"),
        ];

        for (millis, expected) in cases {
            assert_eq!(stamp(millis), expected, "millisecond {millis}");
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
