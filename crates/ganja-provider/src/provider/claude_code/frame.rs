//! The frames that cross the CLI's stdio, decoded and encoded.
//!
//! Spec: the recording, `tests/fixtures/claude-code-sdk-mcp-probe.txt`. Every
//! shape below is a frame that file holds, and where this module and that
//! file disagree the file wins.
//!
//! # Two-stage decode, and required fields only
//!
//! Inbound decoding reads `type` — and `subtype`, for `system` and
//! `control_request` — off a `serde_json::Value` and only then decodes the
//! known shape. A `#[serde(tag)]` enum would have to enumerate every subtype
//! the CLI has, and a release that adds one would fail the decode of a frame
//! this wire could safely have skipped. So the unknown arm is
//! [`Inbound::Unknown`], logged at `debug!` and skipped, and each known shape
//! decodes **required fields only**: the CLI adds fields between releases,
//! and a decoder that insisted on the whole set would break on a field this
//! wire does not read. (The fixture's own `assistant` frames carry a reduced
//! key set — the first W2 lane omitted `timestamp`, `request_id` and
//! `tool_use_meta` — so a replay derived from it under-tests exactly those
//! three keys, which is why an extra-field row is pinned by test.)
//!
//! # Three readings the recording forced
//!
//! **The exit code is the last `result`'s `is_error`, never its `subtype`.**
//! Every `result` in the recording says `subtype: "success"`; the ten refused
//! ones say `is_error: true`, `stop_reason: "refusal"`. A wire reading
//! `subtype` is wrong on eight of sixteen runs, so [`Result_`] carries
//! `is_error` and this module never consults `subtype` for an outcome.
//!
//! **`system/model_refusal_no_fallback` is a known frame**, not an unknown
//! one: ten of the recording's twenty-one paid turns produced one. The
//! `assistant` frame that follows it is the CLI's own `API Error:` banner and
//! is **not model speech**. Suppressing it is load-bearing rather than
//! cosmetic: emitted, it would enter ganja's transcript as assistant text,
//! which a later preamble would render as an `[Assistant]` line — the exact
//! shape the safeguard refused 3 of 3 times. The suppression is what keeps a
//! refusal from seeding the next refusal, and nobody should simplify it back.
//!
//! **Every `control_response` this wire sends comes back verbatim.** The CLI
//! echoes each one on stdout (run 1, five of five). A `control_response`
//! whose `request_id` this wire did not mint is dropped with a `debug!` and
//! never matched to a pending request.

use serde::Serialize;

/// A frame read off the CLI's stdout.
///
/// Every variant but [`Self::Unknown`] carries the fields this wire reads and
/// no others.
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    /// `system/init`, re-emitted at the head of **every** turn (M10): the
    /// recording's second one differs from its first in `uuid` alone, so a
    /// wire must not treat it as once-per-process.
    Init(Init),
    /// `system/model_refusal_no_fallback` — the vendor safeguard refused the
    /// request before the model produced anything.
    Refusal(Refusal),
    /// `system/model_fallback` — the vendor served a different model than the
    /// one asked for.
    ModelFallback {
        /// What it fell back to, as the vendor spells it.
        model: String,
    },
    /// A `system` subtype this wire knows about and does not act on:
    /// `thinking_tokens` and `status`. Named rather than [`Self::Unknown`] so
    /// the unknown-subtype log line stays a signal.
    KnownSystem {
        /// The subtype, for the `debug!` line.
        subtype: String,
    },
    /// An `assistant` frame's content blocks.
    Assistant(Vec<Block>),
    /// A `user` frame the CLI wrote: a `--replay-user-messages` echo, or the
    /// tool result it recorded. This wire reads neither for content — it
    /// knows what it wrote — and the variant exists so the watchdog sees a
    /// frame and the `debug!` line can name it.
    User {
        /// Whether this is the CLI's echo of a message this wire sent.
        is_replay: bool,
    },
    /// The turn ended.
    Result(Result_),
    /// The CLI asking this side for something.
    ControlRequest {
        /// The id every answer echoes.
        request_id: String,
        /// What is being asked.
        request: Request,
    },
    /// The CLI answering something this side asked — or echoing back an
    /// answer this side sent, which is why the `request_id` is what decides.
    ControlResponse {
        /// The id this side minted, or the id of an echoed answer.
        request_id: String,
        /// The answer's payload, or [`None`] when the CLI reported an error.
        response: Option<serde_json::Value>,
        /// What the CLI said went wrong, when it did.
        error: Option<String>,
    },
    /// The vendor's account-window report.
    RateLimit(serde_json::Value),
    /// Whether an interactive login is in flight. It carries no login state —
    /// `{isAuthenticating: false, output: []}` on every run of the recording,
    /// a fully logged-in CLI included.
    AuthStatus {
        /// Whether a login is in progress right now.
        is_authenticating: bool,
        /// The lines the login printed, if any.
        output: Vec<String>,
    },
    /// A frame this build does not know. Skipped with a `debug!`.
    Unknown {
        /// The frame's `type`.
        kind: String,
        /// Its `subtype`, where it had one.
        subtype: Option<String>,
    },
}

/// The `system/init` fields this wire reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Init {
    /// The record's id, echoed from `--session-id`.
    pub session_id: String,
    /// The model the CLI resolved, in the vendor's own spelling —
    /// `claude-opus-5[1m]` where the request said `default`.
    pub model: String,
    /// What this build of the CLI says it can do, for the log line.
    pub capabilities: Vec<String>,
}

/// The `system/model_refusal_no_fallback` fields this wire reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    /// The safeguard's own category — `reasoning_extraction` in every one of
    /// the recording's ten.
    pub category: String,
    /// The safeguard's own explanation, carried **verbatim** into the failure
    /// a person reads: a paraphrase of a vendor's compliance sentence is a
    /// thing nobody should be reading second-hand.
    pub explanation: String,
    /// The model the request asked for, in the vendor's spelling.
    pub original_model: String,
}

/// The `result` fields this wire reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Result_ {
    /// **The outcome.** Never `subtype`, which reads `"success"` on every
    /// frame in the recording, refused ones included.
    pub is_error: bool,
    /// What the turn produced, or the error text when it did not.
    pub text: String,
    /// What the turn cost.
    pub usage: Usage,
}

/// The four counters a `result` carries, under this side's own names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Fresh input tokens.
    pub input: u64,
    /// Tokens the model produced.
    pub output: u64,
    /// Tokens read from the prompt cache.
    pub cache_read: u64,
    /// Tokens written to the prompt cache.
    pub cache_write: u64,
}

/// One block of an `assistant` frame's content.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// Model speech.
    Text(String),
    /// Model thinking. Empty under no `--thinking-display`, in which case the
    /// signature is still a full blob — a wire that replayed thinking would
    /// have to carry a signature with no text, which this one does not.
    Thinking(String),
    /// The model called a tool.
    ToolUse {
        /// The call's own id, which `can_use_tool` and `tools/call` both echo.
        id: String,
        /// The model-facing name, prefixed `mcp__ganja__`.
        name: String,
        /// The arguments, as the model produced them.
        input: serde_json::Value,
    },
}

/// What a `control_request` asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// May this tool run?
    CanUseTool {
        /// The model-facing name, prefixed.
        tool_name: String,
        /// The arguments.
        input: serde_json::Value,
        /// The id this ask is matched back by — the same `toolu_…` the
        /// `tools/call` carries in `_meta["claudecode/toolUseId"]`.
        tool_use_id: String,
    },
    /// A JSON-RPC message for a server this side declared.
    Mcp {
        /// Which declared server it is addressed to.
        server_name: String,
        /// The JSON-RPC envelope.
        message: serde_json::Value,
    },
    /// A subtype this build does not know. Answered with an `error` naming
    /// it — never silence, because a permission prompt the CLI is waiting on
    /// never times out.
    Unknown {
        /// The subtype, for the refusal's own sentence.
        subtype: String,
    },
}

/// The longest single line this wire reads off a child's pipe.
///
/// Sixteen mebibytes: far above anything the recording measured and far below
/// what an unbounded read costs. It has to be generous rather than tight
/// because the largest legitimate frame is a `user` echo — `--replay-user-messages`
/// is on the base argv — carrying a whole rendered preamble, and nothing
/// bounds a preamble (D556's stated cost). It has to exist at all because the
/// bytes are a peer's: a frame that never ends would otherwise be read into
/// memory until the machine gave out (CC-12).
///
/// Exceeding it skips **that line** and nothing else, which is what the
/// decode-failure path already does for a frame that will not parse.
pub const MAX_LINE: usize = 16 * 1024 * 1024;

/// How much of an unreadable line an error quotes.
const HEAD: usize = 200;

/// Reads one line of the CLI's stdout.
///
/// # Errors
///
/// Returns what would not parse as JSON, described rather than embedded: its
/// length and its first two hundred characters. A frame this side cannot read may
/// be megabytes of somebody else's output, and an error is a thing that gets
/// logged and carried around (CC-12).
pub fn decode(line: &str) -> Result<Inbound, String> {
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|error| format!("{error}: {} bytes beginning `{}`", line.len(), head(line)))?;

    Ok(read(&value))
}

/// The first `HEAD` characters of `line`, with the cut admitted.
///
/// By characters rather than bytes, so the quote is never a panic on a
/// multi-byte boundary.
fn head(line: &str) -> String {
    match line.char_indices().nth(HEAD) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_owned(),
    }
}

/// A child's pipe, read one line at a time and **bounded**.
///
/// [`tokio::io::AsyncBufReadExt::lines`] allocates whatever one line contains,
/// which is the wrong posture for a pipe somebody else writes. This reads a
/// buffer at a time, keeps at most [`MAX_LINE`] bytes, and reports a longer
/// line as its length once the rest of it has been skipped — so an oversized
/// frame costs the bound and not the line.
///
/// **Cancel-safe**, which is what makes it usable in the task's `select!`:
/// every partial byte lives in `self`, so a dropped future loses nothing. That
/// is the one guarantee `tokio`'s own `Lines` gives that had to be preserved
/// rather than reimplemented differently.
pub struct Lines<R> {
    reader: R,
    /// The bytes of the line being read, up to [`MAX_LINE`].
    partial: Vec<u8>,
    /// How many bytes of the current line were dropped for being past it.
    dropped: usize,
}

/// One line read off a child's pipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    /// The line, whole.
    Read(String),
    /// A line past [`MAX_LINE`], skipped. Carries how long it turned out to
    /// be, which is the only thing left worth saying about it.
    TooLong(usize),
}

impl<R: tokio::io::AsyncBufRead + Unpin> Lines<R> {
    /// Reads `reader` a line at a time.
    pub fn new(reader: R) -> Self {
        Self { reader, partial: Vec::new(), dropped: 0 }
    }

    /// The next line, or [`None`] at EOF.
    ///
    /// A last line with no newline is returned as a line: a child that dies
    /// mid-frame has still said what it said.
    ///
    /// # Errors
    ///
    /// Returns whatever the read failed with.
    pub async fn next(&mut self) -> std::io::Result<Option<Line>> {
        use tokio::io::AsyncBufReadExt as _;

        loop {
            let (ended, used) = {
                // Field by field rather than through a helper: `fill_buf`
                // borrows `self.reader` for as long as its slice lives, and
                // only a disjoint borrow of `self.partial` may run beside it.
                let available = self.reader.fill_buf().await?;
                if available.is_empty() {
                    // EOF. Whatever is held is the last line, if anything is.
                    let ended = !self.partial.is_empty() || self.dropped > 0;

                    return Ok(ended.then(|| Self::end(&mut self.partial, &mut self.dropped)));
                }

                let (bytes, ended, used) = match available.iter().position(|byte| *byte == b'\n') {
                    Some(at) => (&available[..at], true, at + 1),
                    None => (available, false, available.len()),
                };

                // What still fits is kept; what does not is counted.
                let room = MAX_LINE.saturating_sub(self.partial.len());
                let (kept, over) = bytes.split_at(room.min(bytes.len()));
                self.partial.extend_from_slice(kept);
                self.dropped += over.len();

                (ended, used)
            };

            self.reader.consume(used);
            if ended {
                return Ok(Some(Self::end(&mut self.partial, &mut self.dropped)));
            }
        }
    }

    /// Ends the line being read and starts the next.
    fn end(partial: &mut Vec<u8>, dropped: &mut usize) -> Line {
        let bytes = std::mem::take(partial);
        let dropped = std::mem::replace(dropped, 0);
        if dropped > 0 {
            return Line::TooLong(bytes.len() + dropped);
        }

        // A frame is JSON, so it is UTF-8 or it is not a frame; lossy rather
        // than an error, because what a caller does with either is the same —
        // log it and read on.
        Line::Read(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Classifies an already-parsed frame.
#[must_use]
pub fn read(value: &serde_json::Value) -> Inbound {
    let kind = value["type"].as_str().unwrap_or_default();
    let subtype = value["subtype"].as_str();

    match (kind, subtype) {
        ("system", Some("init")) => Inbound::Init(Init {
            session_id: text(&value["session_id"]),
            model: text(&value["model"]),
            capabilities: value["capabilities"]
                .as_array()
                .map(|names| names.iter().map(text).collect())
                .unwrap_or_default(),
        }),
        ("system", Some("model_refusal_no_fallback")) => Inbound::Refusal(Refusal {
            category: text(&value["api_refusal_category"]),
            explanation: text(&value["api_refusal_explanation"]),
            original_model: text(&value["original_model"]),
        }),
        ("system", Some("model_fallback")) => {
            Inbound::ModelFallback { model: text(&value["model"]) }
        }
        ("system", Some(known @ ("thinking_tokens" | "status"))) => {
            Inbound::KnownSystem { subtype: known.to_owned() }
        }
        ("assistant", _) => Inbound::Assistant(blocks(&value["message"]["content"])),
        ("user", _) => Inbound::User { is_replay: value["isReplay"].as_bool().unwrap_or(false) },
        ("result", _) => Inbound::Result(Result_ {
            // The field, and the reason it is this one rather than `subtype`
            // is the module doc's.
            is_error: value["is_error"].as_bool().unwrap_or(false),
            text: text(&value["result"]),
            usage: usage(&value["usage"]),
        }),
        ("control_request", _) => Inbound::ControlRequest {
            request_id: text(&value["request_id"]),
            request: request(&value["request"]),
        },
        ("control_response", _) => {
            let response = &value["response"];
            let failed = response["subtype"].as_str() == Some("error");

            Inbound::ControlResponse {
                request_id: text(&response["request_id"]),
                response: (!failed).then(|| response["response"].clone()),
                error: failed.then(|| text(&response["error"])),
            }
        }
        ("rate_limit_event", _) => Inbound::RateLimit(value["rate_limit_info"].clone()),
        ("auth_status", _) => Inbound::AuthStatus {
            is_authenticating: value["isAuthenticating"].as_bool().unwrap_or(false),
            output: value["output"]
                .as_array()
                .map(|lines| lines.iter().map(text).collect())
                .unwrap_or_default(),
        },
        _ => Inbound::Unknown { kind: kind.to_owned(), subtype: subtype.map(str::to_owned) },
    }
}

/// A JSON string as a `String`, and anything else as empty.
fn text(value: &serde_json::Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// An `assistant` frame's content blocks, skipping any kind this build does
/// not know rather than failing the frame.
fn blocks(content: &serde_json::Value) -> Vec<Block> {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| match block["type"].as_str() {
                    Some("text") => Some(Block::Text(text(&block["text"]))),
                    Some("thinking") => Some(Block::Thinking(text(&block["thinking"]))),
                    Some("tool_use") => Some(Block::ToolUse {
                        id: text(&block["id"]),
                        name: text(&block["name"]),
                        input: block["input"].clone(),
                    }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The four counters, under the vendor's own field names.
fn usage(value: &serde_json::Value) -> Usage {
    let counter = |name: &str| value[name].as_u64().unwrap_or_default();

    Usage {
        input: counter("input_tokens"),
        output: counter("output_tokens"),
        cache_read: counter("cache_read_input_tokens"),
        cache_write: counter("cache_creation_input_tokens"),
    }
}

/// What one `control_request` asks for.
fn request(value: &serde_json::Value) -> Request {
    match value["subtype"].as_str() {
        Some("can_use_tool") => Request::CanUseTool {
            tool_name: text(&value["tool_name"]),
            input: value["input"].clone(),
            tool_use_id: text(&value["tool_use_id"]),
        },
        Some("mcp_message") => Request::Mcp {
            server_name: text(&value["server_name"]),
            message: value["message"].clone(),
        },
        subtype => Request::Unknown { subtype: subtype.unwrap_or_default().to_owned() },
    }
}

// ------------------------------------------------------------- outbound

/// The first line this wire writes: what the CLI is being driven as.
#[derive(Debug, Serialize)]
pub struct Initialize {
    /// The prompt this record runs under, or [`None`] for the CLI's own
    /// preset. A `Vec` because that is the shape the CLI parses.
    #[serde(rename = "systemPrompt", skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<Vec<String>>,
    /// The servers this side will answer `mcp_message` for.
    #[serde(rename = "sdkMcpServers")]
    pub sdk_mcp_servers: Vec<String>,
    /// Per-server settings, which for this wire is one hour of patience per
    /// `tools/call` — measured honoured, unclamped, at a 25 s hold (M12).
    #[serde(rename = "sdkMcpServerConfigs")]
    pub sdk_mcp_server_configs: serde_json::Value,
}

/// A user message written to the CLI's stdin.
///
/// One frame is one turn: the CLI enqueues each as a new prompt, which is why
/// several owed messages are joined into one frame's `content` rather than
/// written as several frames.
#[derive(Debug, Serialize)]
pub struct UserFrame {
    /// The text.
    pub content: String,
    /// Always [`None`] on this wire: nothing it writes is a subagent's.
    #[serde(rename = "parent_tool_use_id")]
    pub parent_tool_use_id: Option<String>,
}

/// A request this side makes of the CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlRequest {
    /// End the running turn. The process keeps reading stdin afterwards (run
    /// 8), so this ends a turn and never a process.
    Interrupt,
    /// What the account has spent.
    GetUsage,
}

impl ControlRequest {
    /// The subtype the CLI reads.
    #[must_use]
    pub fn subtype(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::GetUsage => "get_usage",
        }
    }
}

/// One line this wire writes to the CLI's stdin.
#[must_use]
pub fn initialize_line(request_id: &str, initialize: &Initialize) -> String {
    // Serialized rather than hand-built, so that `skip_serializing_if` is
    // what decides: a record on the CLI's own preset sends **no**
    // `systemPrompt` key, where a `null` would be this side asserting
    // something about a field the recording never carries.
    let mut request = serde_json::to_value(initialize)
        .expect("an Initialize holds only strings and a JSON value");
    request["subtype"] = serde_json::Value::from("initialize");

    line(&serde_json::json!({
        "type": "control_request",
        "request_id": request_id,
        "request": request,
    }))
}

/// A user message frame.
#[must_use]
pub fn user_line(frame: &UserFrame) -> String {
    line(&serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": frame.content},
        "parent_tool_use_id": frame.parent_tool_use_id,
    }))
}

/// A request of the CLI.
#[must_use]
pub fn control_request_line(request_id: &str, request: ControlRequest) -> String {
    line(&serde_json::json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": request.subtype()},
    }))
}

/// A successful answer to something the CLI asked.
#[must_use]
pub fn control_response_line(request_id: &str, response: &serde_json::Value) -> String {
    line(&serde_json::json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": response},
    }))
}

/// A refusal of something the CLI asked — an unknown `control_request`
/// subtype, say. **Never silence**: a permission prompt the CLI is waiting on
/// does not time out, so an unanswered ask hangs the turn.
#[must_use]
pub fn control_error_line(request_id: &str, error: &str) -> String {
    line(&serde_json::json!({
        "type": "control_response",
        "response": {"subtype": "error", "request_id": request_id, "error": error},
    }))
}

/// One frame, compact, newline-terminated — the CLI reads a frame per line.
fn line(value: &serde_json::Value) -> String {
    let mut rendered = value.to_string();
    rendered.push('\n');

    rendered
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
