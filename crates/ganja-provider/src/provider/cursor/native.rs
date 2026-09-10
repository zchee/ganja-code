//! The native redirect table: cursor's own exec kinds, run as ganja's tools.
//!
//! Spec: the behaviour of the `opencode-cursor` plugin's proxy at
//! `a37a6ba9a6d6d8d176bb68248f59240271f46767` (MIT, see
//! `THIRD_PARTY_NOTICES.md`) — `proxy.ts:1443-1473` and `:1579-1660`, where a
//! native exec is answered by running the client's own equivalent and
//! composing the kind's result frame from what it produced. **Behaviour only:
//! no code is copied**, the tools are ganja's, and the composition below is
//! written against `cursor.proto`'s messages rather than translated from
//! TypeScript.
//!
//! Declaring a tool roster does not stop the server asking for native kinds —
//! it has handlers of its own in mind and asks for them by name — so a build
//! that only bridged `mcp_args` would still answer `read_args` with a refusal
//! and leave the model working around a client that cannot read files. This
//! module is the other half: seven exec kinds mapped onto the ganja tool that
//! does the same job, and each of those tools' outcomes rendered back into the
//! kind's own result frame.
//!
//! **Nothing here runs anything.** [`redirect`] turns an exec into a *call* —
//! a tool name and an argument object — which the wire surfaces to ganja's
//! engine exactly as a model-issued tool call, and [`answer`] turns the result
//! that comes back into frames. The execution happens in the engine's own four
//! phases, which is what puts a bridged shell command under the same
//! permission dialog, the same rules and the same transcript as one the model
//! asked for directly. A redirect that ran the tool here would have none of
//! those, and no gate in this workspace can see the difference — which is why
//! it is a test that does (**D552**, AC-17).
//!
//! **A redirect happens only when the mapped tool is on this request's
//! roster.** A turn that is not offering `bash` does not run a shell because
//! the server asked; that exec keeps D550's typed refusal, exactly as before.

// Whether an `Error` part's text is a refusal rather than a failure was this
// module's own two-constant predicate until **D556 amended D552 here**, and
// the amendment is visible on the wire: `is_refusal` knows the `PreToolUse`
// hook's sentence beside the two permission ones, so from that landing a call
// a user's hook refused travels to cursor's server as the kind's typed
// `rejected` arm, where D552 sent it as `success{is_error: true}`. Root
// `AGENTS.md` says a hook block "routes the same `fail_call` a denied rule
// does", and the two have to read alike on every wire.
use ganja_tool::permission_text::is_refusal;

use super::{decode, proto, request};
use crate::protocol::ToolState;

/// The sentence `write` refuses an unread file with.
///
/// A **suffix of** `ganja_tool`'s own text (`ganja-tool/src/lib.rs`,
/// `FileTimes::check_fresh_stat`), matched rather than shared because that
/// sentence is not a permission refusal and does not live beside the two that
/// are (`ganja_tool::permission_text`, **D552**'s Dv-8). Duplicating it is a
/// real fragility, so the pairing is proved rather than assumed: a test builds
/// the refusal through the real `FileTimes` and asserts this recognises it.
///
/// It earns the `rejected` arm rather than `error` because it is a refusal the
/// model can act on — ask for a `read_args` first, then write — which is
/// exactly what that arm means to the loop on the other end (AC-16).
const READ_FIRST: &str = "has not been read this session; read it first";

/// What the engine's tool part said, reduced to the three answers a result
/// frame distinguishes.
#[derive(Debug, PartialEq)]
pub(super) enum Outcome {
    /// It ran and produced output.
    Ran { output: String, metadata: serde_json::Value },
    /// It ran and failed; the text is what the model would have read.
    Failed(String),
    /// It never ran: a rule refused it, or the person at the dialog did.
    Refused(String),
}

impl Outcome {
    /// The outcome a finished tool part carries, or [`None`] while it is still
    /// pending or running.
    ///
    /// **The refusal is told from the failure by its text**, which is the only
    /// thing that reaches a wire: `ganja_tool::permission_text::is_refusal`
    /// knows the sentences the permission engine and D458's hook block render,
    /// and a wire that could not name them would have to answer every declined
    /// call as a tool that ran and broke. Anything else — an unknown tool, bad
    /// arguments, a command that exited non-zero — is a failure, which is
    /// cursor's own `is_error` shape rather than its `rejected` one.
    pub(super) fn of(state: &ToolState) -> Option<Self> {
        match state {
            ToolState::Completed { output, metadata, .. } => {
                Some(Self::Ran { output: output.clone(), metadata: metadata.clone() })
            }
            ToolState::Error { error, .. } => Some(if is_refusal(error) {
                Self::Refused(error.clone())
            } else {
                Self::Failed(error.clone())
            }),
            ToolState::Pending { .. } | ToolState::Running { .. } => None,
        }
    }

    /// The text the model would have read, whichever arm this is.
    fn text(&self) -> &str {
        match self {
            Self::Ran { output, .. } => output,
            Self::Failed(text) | Self::Refused(text) => text,
        }
    }
}

/// One exec turned into a ganja tool call, and the shape its answer takes.
#[derive(Debug)]
pub(super) struct Bridged {
    /// The registry name the engine will run.
    pub(super) tool: String,
    /// The arguments it runs with, as the tool's own schema spells them.
    pub(super) input: serde_json::Value,
    /// How the outcome is rendered back onto the wire.
    pub(super) answer: Answer,
}

/// Which result frame a bridged exec is answered with, and what that frame
/// echoes back from the args.
#[derive(Debug)]
pub(super) enum Answer {
    /// `mcp_result = 11`: a tool the model called by name.
    Mcp,
    /// `read_result = 7` or `redacted_read_result = 29` — one message type,
    /// two fields, which is why the field travels beside the echo.
    Read { redacted: bool, path: String, ranged: bool },
    /// `shell_stream = 14`, whose answer is events rather than one result.
    Shell,
    /// `grep_result = 5`.
    Grep { pattern: String, path: String },
    /// `ls_result = 8`.
    Ls { path: String },
    /// `write_result = 3`; the counts are computed from the text that was
    /// written, because that is exactly what landed.
    Write { path: String, lines: i32, size: i32 },
    /// `fetch_result = 20`.
    Fetch { url: String },
}

/// The ganja tool an exec redirects to, or [`None`] when there is none — a
/// kind outside the table, or one whose tool this request is not offering.
///
/// The roster check is the load-bearing half: a seat that declared no `bash`
/// on this request is a seat not offering to run shell commands, and the
/// server asking for one does not change that. Such an exec falls through to
/// D550's typed refusal, which is what it got before this table existed.
pub(super) fn redirect(args: &decode::ExecArgs, roster: &[String]) -> Option<Bridged> {
    let offered = |name: &str| roster.iter().any(|tool| tool == name);
    let bridged = |tool: &str, input: serde_json::Value, answer: Answer| {
        offered(tool).then(|| Bridged { tool: tool.to_owned(), input, answer })
    };

    match args {
        // Both members pass through unchanged; cursor's own indexing is not
        // measured by the recording, so nothing here re-bases them. A
        // *negative* offset is dropped rather than passed on: `read`'s own
        // `offset` is an unsigned line number, so `-3` reaches its
        // deserializer as "invalid type: integer `-3`" and the model reads a
        // broken tool where it asked for a window that does not exist. Absent
        // is what this build has to say about that window.
        decode::ExecArgs::Read { redacted, path, offset, limit } => {
            let offset = offset.filter(|line| *line >= 0);
            let mut input = serde_json::json!({ "filePath": path });
            if let Some(offset) = offset {
                input["offset"] = serde_json::json!(offset);
            }
            if let Some(limit) = limit {
                input["limit"] = serde_json::json!(limit);
            }

            bridged(
                "read",
                input,
                Answer::Read {
                    redacted: *redacted,
                    path: path.clone(),
                    ranged: offset.is_some() || limit.is_some(),
                },
            )
        }
        decode::ExecArgs::ShellStream { command, working_directory } => {
            let mut input = serde_json::json!({ "command": command });
            if !working_directory.is_empty() {
                input["workdir"] = serde_json::json!(working_directory);
            }

            bridged("bash", input, Answer::Shell)
        }
        // Case-insensitivity is folded into the pattern because ganja's `grep`
        // has no flag for it and its engine reads inline flags; a `(?i)` in
        // front means what the member asked for, where decoding the member and
        // ignoring it would answer a different search than the one requested.
        decode::ExecArgs::Grep { pattern, path, glob, case_insensitive } => {
            let searched =
                if *case_insensitive { format!("(?i){pattern}") } else { pattern.clone() };
            let mut input = serde_json::json!({ "pattern": searched });
            if !path.is_empty() {
                input["path"] = serde_json::json!(path);
            }
            if !glob.is_empty() {
                input["include"] = serde_json::json!(glob);
            }

            bridged("grep", input, Answer::Grep { pattern: pattern.clone(), path: path.clone() })
        }
        // Only an absolute path, and [`listable`] says why: `glob` answers
        // with absolute paths and [`tree`] builds the listing by stripping
        // the directory off each of them, so a relative or absent path
        // matches nothing and would answer an empty directory the model
        // could not tell from a real one. Such an exec keeps D550's typed
        // rejection, under [`ABSOLUTE_LISTING`] rather than the kind-level
        // sentence — ganja does list directories, and saying it does not
        // would be the falsehood this table exists to avoid.
        decode::ExecArgs::Ls { path } if listable(path) => bridged(
            "glob",
            serde_json::json!({ "pattern": LISTING, "path": path }),
            Answer::Ls { path: path.clone() },
        ),
        decode::ExecArgs::Ls { .. } => None,
        decode::ExecArgs::Write { path, file_text } => bridged(
            "write",
            serde_json::json!({ "filePath": path, "content": file_text }),
            Answer::Write {
                path: path.clone(),
                // Both counts describe the text that was sent, which is the
                // text that was written: `write` truncates and writes exactly
                // this, so counting here and counting the file agree.
                lines: clamped(file_text.lines().count()),
                size: clamped(file_text.len()),
            },
        ),
        // `webfetch`'s own default format (markdown) is what the model gets on
        // any other wire, so the redirect names none.
        decode::ExecArgs::Fetch { url } => bridged(
            "webfetch",
            serde_json::json!({ "url": url }),
            Answer::Fetch { url: url.clone() },
        ),
        decode::ExecArgs::Mcp(_)
        | decode::ExecArgs::Shell { .. }
        | decode::ExecArgs::Delete { .. }
        | decode::ExecArgs::Unmodelled => None,
    }
}

/// The glob one `ls_args` becomes.
///
/// `glob` matches **files**, never directories — so this pattern is what a
/// one-level listing can be built out of: `*` is every file directly in the
/// directory, and `*/*` is every file one level down, whose first path
/// component names a child directory. What the answer can therefore show is
/// the files in the directory and the child directories that contain at least
/// one file of their own. **Dotfiles are included**, which is measured rather
/// than assumed (`an_ls_exec_over_a_real_directory_lists_what_glob_really_finds`
/// runs the real tool over a real directory): the walker's `hidden(true)`
/// applies only to entries the glob override did not match, and `{*,*/*}` is
/// gitignore-glob syntax, where a leading `*` matches a leading dot. What it
/// cannot show is an empty child directory, or anything deeper than one level
/// — which is why the tree it fills says `children_were_processed = false`
/// rather than claiming a walk it did not do.
const LISTING: &str = "{*,*/*}";

/// Whether an `ls_args` path is one this build can turn into a listing.
///
/// Absolute or nothing: [`tree`] matches `glob`'s absolute output against this
/// path, so anything else describes no directory the two ends can agree on.
fn listable(path: &str) -> bool {
    path.starts_with('/')
}

/// What a listing this build will not redirect is refused with.
///
/// Names the requirement rather than the kind, because the kind-level sentence
/// ("ganja does not run ls_args for a provider") would be false here: ganja
/// lists directories, and this one only under a path both ends can name the
/// same way.
pub(super) const ABSOLUTE_LISTING: &str = "ganja lists an absolute path: its listing answers with absolute paths, and a relative one \
     names no directory those entries could be matched against.";

/// What a listing whose entries lie somewhere else is answered with.
///
/// Not an empty directory: a `glob` that found files and put none of them
/// under the path that was asked about has answered about a *different*
/// directory, and reporting that as "nothing here" is a fact this build did
/// not measure. The model can act on the distinction; it cannot act on a
/// silence.
const LISTING_ELSEWHERE: &str = "ganja's listing answered with entries under a different directory than the one asked \
     about, so nothing it found describes this path.";

/// Whether a refused redirect has a sentence of its own — the arguments being
/// the reason rather than the kind.
///
/// Consulted only where [`redirect`] returned [`None`], and spelled beside the
/// arm that refuses so the two cannot drift: the predicate is [`listable`] on
/// both sides.
pub(super) fn argument_refusal(args: &decode::ExecArgs) -> Option<&'static str> {
    match args {
        decode::ExecArgs::Ls { path } if !listable(path) => Some(ABSOLUTE_LISTING),
        _ => None,
    }
}

/// A count as the wire's `int32`, saturating rather than wrapping.
fn clamped(count: usize) -> i32 {
    i32::try_from(count).unwrap_or(i32::MAX)
}

/// The messages answering one bridged exec — the result frames, and then the
/// stream close that ends every exec.
pub(super) fn answer(
    shape: &Answer,
    id: Option<u32>,
    exec_id: Option<&str>,
    outcome: &Outcome,
) -> Vec<proto::ClientMessage> {
    let empty =
        || proto::ExecResponse { id, exec_id: exec_id.map(str::to_owned), ..Default::default() };
    let sent = |response: proto::ExecResponse| proto::ClientMessage {
        exec_response: buffa::MessageField::some(response),
        ..Default::default()
    };
    // One result message, its arm set by `fill`.
    let one = |fill: &dyn Fn(&mut proto::ExecResponse)| {
        let mut response = empty();
        fill(&mut response);
        vec![sent(response)]
    };

    let mut messages = match shape {
        Answer::Mcp => one(&|response| {
            response.mcp_result = buffa::MessageField::some(mcp_result(outcome));
        }),
        Answer::Read { redacted, path, ranged } => one(&|response| {
            let result = buffa::MessageField::some(read_result(path, *ranged, outcome));
            if *redacted {
                response.redacted_read_result = result;
            } else {
                response.read_result = result;
            }
        }),
        // The one kind whose answer is several messages: a streamed shell
        // writes its output and then its exit, each its own ExecResponse.
        Answer::Shell => shell_events(outcome)
            .into_iter()
            .map(|event| {
                let mut response = empty();
                response.shell_stream = buffa::MessageField::some(event);
                sent(response)
            })
            .collect(),
        Answer::Grep { pattern, path } => one(&|response| {
            response.grep_result = buffa::MessageField::some(grep_result(pattern, path, outcome));
        }),
        Answer::Ls { path } => one(&|response| {
            response.ls_result = buffa::MessageField::some(ls_result(path, outcome));
        }),
        Answer::Write { path, lines, size } => one(&|response| {
            response.write_result =
                buffa::MessageField::some(write_result(path, *lines, *size, outcome));
        }),
        Answer::Fetch { url } => one(&|response| {
            response.fetch_result = buffa::MessageField::some(fetch_result(url, outcome));
        }),
    };
    messages.push(request::stream_close(id));

    messages
}

/// `mcp_result = 11`: the three answers a called tool can give.
fn mcp_result(outcome: &Outcome) -> proto::McpResult {
    match outcome {
        Outcome::Refused(reason) => proto::McpResult {
            rejected: buffa::MessageField::some(
                proto::McpRejected::default().with_reason(reason.as_str()),
            ),
            ..Default::default()
        },
        // A tool that ran and failed answers on the success arm with
        // `is_error` set, which is cursor's own shape for it: the model reads
        // the message either way, and the flag is what says which it is.
        Outcome::Ran { .. } | Outcome::Failed(_) => proto::McpResult {
            success: buffa::MessageField::some(proto::McpSuccess {
                content: vec![proto::McpContentItem {
                    text: buffa::MessageField::some(
                        proto::McpTextContent::default().with_text(outcome.text()),
                    ),
                    ..Default::default()
                }],
                is_error: Some(matches!(outcome, Outcome::Failed(_))),
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

/// `read_result = 7` / `redacted_read_result = 29`.
///
/// The content is the `read` tool's output verbatim, header, line numbers and
/// footer included: that is what ganja's own model reads, and a bridged answer
/// that reformatted it would tell cursor's model something different about the
/// same file.
fn read_result(path: &str, ranged: bool, outcome: &Outcome) -> proto::ReadResult {
    match outcome {
        Outcome::Ran { output, metadata } => proto::ReadResult {
            success: buffa::MessageField::some(proto::ReadSuccess {
                path: Some(path.to_owned()),
                content: Some(output.clone()),
                total_lines: metadata
                    .pointer("/display/totalLines")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|lines| i32::try_from(lines).ok()),
                truncated: metadata.get("truncated").and_then(serde_json::Value::as_bool),
                range_applied: Some(ranged),
                ..Default::default()
            }),
            ..Default::default()
        },
        Outcome::Failed(error) => proto::ReadResult {
            error: buffa::MessageField::some(
                proto::ReadError::default().with_path(path).with_error(error.as_str()),
            ),
            ..Default::default()
        },
        Outcome::Refused(reason) => proto::ReadResult {
            rejected: buffa::MessageField::some(
                proto::ReadRejected::default().with_path(path).with_reason(reason.as_str()),
            ),
            ..Default::default()
        },
    }
}

/// `shell_stream = 14`: the events a run produces, in the order a running
/// client would have written them.
///
/// One stdout event and then the exit, because `bash` returns when the command
/// is done — there is no output *stream* left to forward, and emitting one
/// event per line would invent a cadence this build never observed. A failure
/// goes out on stderr with exit 1, which is how the loop on the other end
/// reads a command that did not work; a refusal is the single rejected event
/// D550 already sends.
fn shell_events(outcome: &Outcome) -> Vec<proto::ShellStream> {
    match outcome {
        Outcome::Ran { output, metadata } => vec![
            proto::ShellStream {
                stdout: buffa::MessageField::some(
                    proto::ShellStreamStdout::default().with_data(output.as_str()),
                ),
                ..Default::default()
            },
            proto::ShellStream {
                exit: buffa::MessageField::some(
                    proto::ShellStreamExit::default().with_code(
                        metadata
                            .get("exit")
                            .and_then(serde_json::Value::as_i64)
                            .and_then(|code| u32::try_from(code).ok())
                            // A command killed by a signal reports no exit code at
                            // all; `1` is the shell's own convention for "it did
                            // not succeed", and the alternative is claiming a
                            // clean `0`.
                            .unwrap_or(1),
                    ),
                ),
                ..Default::default()
            },
        ],
        Outcome::Failed(error) => vec![
            proto::ShellStream {
                stderr: buffa::MessageField::some(
                    proto::ShellStreamStderr::default().with_data(error.as_str()),
                ),
                ..Default::default()
            },
            proto::ShellStream {
                exit: buffa::MessageField::some(proto::ShellStreamExit::default().with_code(1)),
                ..Default::default()
            },
        ],
        Outcome::Refused(reason) => vec![proto::ShellStream {
            rejected: buffa::MessageField::some(
                proto::ShellRejected::default().with_reason(reason.as_str()),
            ),
            ..Default::default()
        }],
    }
}

/// `grep_result = 5`.
///
/// This kind has **no rejected arm** in the shipped descriptor, so a refusal
/// travels as the error carrying ganja's own refusal sentence — the only place
/// the server's loop can be told anything at all about this call.
fn grep_result(pattern: &str, path: &str, outcome: &Outcome) -> proto::GrepResult {
    match outcome {
        Outcome::Ran { output, .. } => {
            let matches = parse_matches(output);
            let total = clamped(matches.iter().map(|file| file.matches.len()).sum());

            proto::GrepResult {
                success: buffa::MessageField::some(proto::GrepSuccess {
                    pattern: Some(pattern.to_owned()),
                    path: Some(path.to_owned()),
                    output_mode: Some("content".to_owned()),
                    workspace_results: vec![proto::GrepWorkspaceEntry {
                        // The workspace the search ran in is the one the tool
                        // was asked about; an empty key is the honest name for
                        // "wherever the session is", which is what an absent
                        // path means to `grep`.
                        key: Some(path.to_owned()),
                        value: buffa::MessageField::some(proto::GrepUnionResult {
                            content: buffa::MessageField::some(proto::GrepContentResult {
                                matches,
                                total_matched_lines: Some(total),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }
        }
        Outcome::Failed(error) | Outcome::Refused(error) => proto::GrepResult {
            error: buffa::MessageField::some(
                proto::GrepError::default().with_error(error.as_str()),
            ),
            ..Default::default()
        },
    }
}

/// The `grep` tool's own output, read back into matches.
///
/// Its shape is fixed by that tool (`ganja-tool/src/grep.rs`): a count line, a
/// `<path>:` header per file, and `  Line <n>: <text>` under it. Parsing what
/// this build itself wrote is a seam worth naming — the two must agree, and
/// this module's tests drive real `grep` output through it, not only a
/// hand-typed sample.
fn parse_matches(output: &str) -> Vec<proto::GrepFileMatch> {
    let mut files: Vec<proto::GrepFileMatch> = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("  Line ") {
            let Some((number, text)) = rest.split_once(": ") else { continue };
            let Ok(number) = number.parse::<i32>() else { continue };
            let Some(file) = files.last_mut() else { continue };

            file.matches.push(proto::GrepContentMatch {
                line_number: Some(number),
                content: Some(text.to_owned()),
                ..Default::default()
            });
        } else if let Some(path) = line.strip_suffix(':')
            && !path.is_empty()
        {
            files.push(proto::GrepFileMatch { file: Some(path.to_owned()), ..Default::default() });
        }
    }

    files
}

/// `ls_result = 8`, built from the `glob` listing [`LISTING`] asked for.
fn ls_result(path: &str, outcome: &Outcome) -> proto::LsResult {
    match outcome {
        Outcome::Ran { output, .. } => match tree(path, output) {
            Some(root) => proto::LsResult {
                success: buffa::MessageField::some(proto::LsSuccess {
                    directory_tree_root: buffa::MessageField::some(root),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None => proto::LsResult {
                error: buffa::MessageField::some(
                    proto::LsError::default().with_path(path).with_error(LISTING_ELSEWHERE),
                ),
                ..Default::default()
            },
        },
        Outcome::Failed(error) => proto::LsResult {
            error: buffa::MessageField::some(
                proto::LsError::default().with_path(path).with_error(error.as_str()),
            ),
            ..Default::default()
        },
        Outcome::Refused(reason) => proto::LsResult {
            rejected: buffa::MessageField::some(
                proto::LsRejected::default().with_path(path).with_reason(reason.as_str()),
            ),
            ..Default::default()
        },
    }
}

/// One directory level, from the absolute paths `glob` listed — or [`None`]
/// when the listing describes somewhere else.
///
/// A path with one component left under `path` is a file in it; one with two
/// names a child directory. Everything deeper, and the tool's own "No files
/// found" and truncation notes, are passed over — a line that is not a path
/// under the directory asked about has nothing to contribute to a tree of it.
///
/// **An empty directory and a listing of somewhere else are different
/// answers.** `glob` says "No files found" for the first, which is no path at
/// all and leaves the tree honestly empty; the second is absolute paths that
/// simply do not lie under `root`, and answering *that* with an empty
/// directory would report a fact nobody established. So a listing holding
/// absolute paths of which none matched is [`None`], which
/// [`ls_result`] renders as the kind's own error arm.
fn tree(path: &str, output: &str) -> Option<proto::LsDirectoryTreeNode> {
    // Trimmed for the prefix match — `/repo/` and `/repo` name one directory
    // and `glob` writes the second — which leaves the root itself as `""`; it
    // is still `/`, and a child under it is still `/etc` rather than `//etc`.
    let root = path.trim_end_matches('/');
    let mut files: Vec<String> = Vec::new();
    let mut directories: Vec<String> = Vec::new();
    let mut listed = false;

    for line in output.lines() {
        listed |= line.starts_with('/');
        let Some(relative) = line.strip_prefix(root).and_then(|rest| rest.strip_prefix('/')) else {
            continue;
        };
        match relative.split_once('/') {
            None if !relative.is_empty() => files.push(relative.to_owned()),
            Some((directory, _)) if !directory.is_empty() => {
                let child = format!("{root}/{directory}");
                if !directories.contains(&child) {
                    directories.push(child);
                }
            }
            _ => {}
        }
    }

    if listed && files.is_empty() && directories.is_empty() {
        return None;
    }

    Some(proto::LsDirectoryTreeNode {
        abs_path: Some(if root.is_empty() { "/".to_owned() } else { root.to_owned() }),
        children_dirs: directories
            .into_iter()
            .map(|child| proto::LsDirectoryTreeNode {
                abs_path: Some(child),
                children_were_processed: Some(false),
                ..Default::default()
            })
            .collect(),
        num_files: Some(clamped(files.len())),
        children_files: files
            .into_iter()
            .map(|name| proto::LsFile { name: Some(name), ..Default::default() })
            .collect(),
        // The listing is one glob, not a walk: what is inside those child
        // directories was never looked at, and saying so is the difference
        // between an answer and a claim.
        children_were_processed: Some(false),
        ..Default::default()
    })
}

/// `write_result = 3`.
///
/// Ganja's read-before-write rule applies **unchanged** to a bridged write: a
/// file this session never read is refused by the `write` tool itself, and
/// that refusal rides the rejected arm naming the requirement, which the model
/// can act on by asking for a read first (AC-16).
fn write_result(path: &str, lines: i32, size: i32, outcome: &Outcome) -> proto::WriteResult {
    match outcome {
        Outcome::Ran { .. } => proto::WriteResult {
            success: buffa::MessageField::some(proto::WriteSuccess {
                path: Some(path.to_owned()),
                lines_created: Some(lines),
                file_size: Some(size),
                ..Default::default()
            }),
            ..Default::default()
        },
        Outcome::Refused(reason) => proto::WriteResult {
            rejected: buffa::MessageField::some(
                proto::WriteRejected::default().with_path(path).with_reason(reason.as_str()),
            ),
            ..Default::default()
        },
        Outcome::Failed(error) if error.contains(READ_FIRST) => proto::WriteResult {
            rejected: buffa::MessageField::some(
                proto::WriteRejected::default().with_path(path).with_reason(error.as_str()),
            ),
            ..Default::default()
        },
        Outcome::Failed(error) => proto::WriteResult {
            error: buffa::MessageField::some(
                proto::WriteError::default().with_path(path).with_error(error.as_str()),
            ),
            ..Default::default()
        },
    }
}

/// `fetch_result = 20`, which like grep has no rejected arm.
fn fetch_result(url: &str, outcome: &Outcome) -> proto::FetchResult {
    match outcome {
        Outcome::Ran { output, .. } => proto::FetchResult {
            success: buffa::MessageField::some(
                // `status_code` and `content_type` stay absent: `webfetch`
                // answers with the page's text and reports neither, so filling
                // them would be this wire inventing a fact about somebody
                // else's response.
                proto::FetchSuccess::default().with_url(url).with_content(output.as_str()),
            ),
            ..Default::default()
        },
        Outcome::Failed(error) | Outcome::Refused(error) => proto::FetchResult {
            error: buffa::MessageField::some(
                proto::FetchError::default().with_url(url).with_error(error.as_str()),
            ),
            ..Default::default()
        },
    }
}

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;
