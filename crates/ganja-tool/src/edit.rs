//! The `edit` tool: replace a string in a file, and be forgiving about what
//! "the same string" means.
//!
//! Spec: upstream `packages/opencode/src/tool/edit.ts` and `edit.txt`, whose
//! replacer strategies come in turn from cline and gemini-cli. A model that
//! quotes a file back at you gets the indentation, the escaping or the blank
//! lines wrong often enough that an exact-match-only edit tool feels broken,
//! so nine strategies run in a fixed order and the first candidate that
//! resolves to exactly one place in the file wins. Order is the contract: a
//! later strategy may only see what every earlier one declined.
//!
//! Nothing is written until the whole replacement has succeeded, so a refused
//! or failed edit leaves the file byte for byte as it was.
//!
//! The read and the write that bracket that replacement both go through one
//! directory this call holds open (`tool/anchor.rs`) rather than through the
//! path twice, so what is written back is the file that was read — and a link
//! at the file's own name is refused rather than followed.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use serde::Deserialize;
use similar::{ChangeTag, TextDiff};

use crate::anchor::{self, Anchor};
use crate::{Tool, ToolCtx, ToolError, ToolOutput, display, resolve};

/// What the model is told about the tool: upstream's prompt file, verbatim.
const DESCRIPTION: &str = include_str!("edit.txt");

/// Refused because the two strings say the same thing.
const IDENTICAL: &str = "No changes to apply: oldString and newString are identical.";

/// Refused because an empty `oldString` against an existing file means "throw
/// the file away", which is `write`'s job and not this one's.
const EMPTY_OLD_STRING: &str = "oldString cannot be empty when editing an existing file. Provide the exact text to replace, or use write for an intentional full-file replacement.";

/// Refused because the loosest strategies matched far more of the file than
/// the model asked for, which is how a fuzzy edit tool deletes a function.
const DISPROPORTIONATE: &str = "Refusing replacement because the matched span is much larger than oldString. Re-read the file and provide the full exact oldString for the intended replacement.";

/// No strategy found anything. The wording tells the model what to fix.
const NOT_FOUND: &str = "Could not find oldString in the file. It must match exactly, including whitespace, indentation, and line endings.";

/// Something matched, but in more than one place, and the call did not ask
/// for every occurrence.
const MULTIPLE_MATCHES: &str = "Found multiple matches for oldString. Provide more surrounding context to make the match unique.";

/// What the model sees after the file is written.
const APPLIED: &str = "Edit applied successfully.";

/// The byte order mark, which is part of the file but not part of any line
/// the model quotes back.
const BOM: char = '\u{feff}';

/// Lines of context a hunk carries, matching the `diff` package upstream
/// generates its patches with.
const CONTEXT_RADIUS: usize = 4;

/// The rule the `diff` package draws under a patch's file headers.
const SEPARATOR: &str = "===================================================================";

/// How alike a block anchor's middle lines must be before the block counts
/// as the one the model meant.
const SINGLE_CANDIDATE_SIMILARITY_THRESHOLD: f64 = 0.65;

/// The same bar, applied to the best of several anchored blocks.
const MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD: f64 = 0.65;

/// One `tokio` mutex per file this process has edited, so two calls in one
/// turn cannot both read the old text and write over each other. Keyed by
/// resolved path, and, as upstream, kept for the life of the process: the map
/// grows with the working set, like the session's read log beside it.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The lock guarding `path`, creating it the first time the file is edited.
fn lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    Arc::clone(
        LOCKS.lock().expect("the lock table is never poisoned").entry(path.to_owned()).or_default(),
    )
}

/// Arguments the model passes, named as upstream's schema names them because
/// the names are what the model was trained against.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Args {
    /// The absolute path to the file to modify
    file_path: String,
    /// The text to replace
    old_string: String,
    /// The text to replace it with (must be different from oldString)
    new_string: String,
    /// Replace all occurrences of oldString (default false)
    #[serde(default)]
    replace_all: Option<bool>,
}

/// Replaces a string in a file.
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn id(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    /// One line for the permission dialog. [`Tool::describe`] is handed the
    /// arguments and nothing else, so the path is shortened against the
    /// process's working directory rather than [`ToolCtx::cwd`]; a path
    /// outside it is shown whole.
    fn describe(&self, args: &serde_json::Value) -> String {
        match args.get("filePath").and_then(serde_json::Value::as_str) {
            Some(file_path) => {
                let path = Path::new(file_path);
                let shown = std::env::current_dir()
                    .ok()
                    .and_then(|cwd| path.strip_prefix(cwd).ok())
                    .unwrap_or(path);
                format!("edit {}", shown.display())
            }
            None => self.id().to_owned(),
        }
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        if args.file_path.is_empty() {
            return Err(ToolError::Failed("filePath is required".to_owned()));
        }
        if args.old_string == args.new_string {
            return Err(ToolError::Failed(IDENTICAL.to_owned()));
        }
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let path = resolve(&ctx.cwd, &args.file_path);
        // Before the lock, and long before anything is written: a refused path
        // has no business holding up another call to the same file.
        anchor::refuse_link_escape(&ctx.cwd, &path)?;
        let guard = lock(&path);
        let _held = guard.lock().await;

        // From here the file is addressed through a directory this call holds
        // open (`tool/anchor.rs`), so the read below and the write further down
        // reach the same file whatever happens to the name in between. Parents
        // are not made yet: an edit that fails has no business leaving
        // directories behind, and it is `prepare` that decides whether there is
        // a file to create at all.
        let anchor = open_anchor(&ctx.cwd, &path, false).await?;
        let (content_old, content_new, bom) = prepare(anchor.clone(), &path, &args, ctx).await?;

        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // The one case that anchors a second time: a file being created whose
        // parents are not there yet, made under the same rules.
        let anchor = match anchor {
            Some(anchor) => anchor,
            None => open_anchor(&ctx.cwd, &path, true).await?.ok_or_else(|| {
                ToolError::Failed(format!("{} could not be created", path.display()))
            })?,
        };
        let stamp = write_through(&anchor, join_bom(&content_new, bom)).await?;
        // Recorded from the descriptor that was just written, and recorded
        // here rather than anywhere later: this edit is about to arrive back
        // as a filesystem event, and `crate::watch` decides staleness by
        // comparing the file's stamp against the recorded one. Because the
        // record happens inside the call that caused the event, the agent's
        // own edit compares clean; a session that recorded afterwards would
        // condemn its own work.
        ctx.files.record_stat(&path, stamp);

        // The patch describes the change, not the file's line endings, so both
        // sides go in normalized and a CRLF file does not read as one long
        // replacement.
        let before = normalize_line_endings(&content_old);
        let after = normalize_line_endings(&content_new);
        let name = path.display().to_string();
        let diff = patch(&name, &before, &after);
        let (additions, deletions) = line_counts(&before, &after);

        Ok(ToolOutput {
            title: display(&ctx.cwd, &path),
            output: APPLIED.to_owned(),
            metadata: serde_json::json!({
                "diff": diff,
                "filediff": {
                    "file": name,
                    "patch": diff,
                    "additions": additions,
                    "deletions": deletions,
                },
            }),
        })
    }
}

/// What the anchored file held when it was opened.
struct Opened {
    /// Whether the name is a directory, which is not a thing to edit.
    is_dir: bool,
    /// The modification stamp, from an `fstat` on the descriptor that was
    /// read — never a second look at the path.
    stamp: Option<SystemTime>,
    /// The bytes, empty for a directory.
    bytes: Vec<u8>,
}

/// Opens the anchor for `path`, and refuses it if the directory it really
/// landed on is one a link led out of the project to.
///
/// [`None`] means the parent does not exist and this call was not asked to
/// make it — which, for an edit, is one of the ways a file turns out not to be
/// there.
async fn open_anchor(
    cwd: &Path,
    path: &Path,
    create_parents: bool,
) -> Result<Option<Arc<Anchor>>, ToolError> {
    let owned = path.to_owned();
    let opened = blocking(move || match Anchor::open(&owned, create_parents) {
        Ok(anchor) => Ok(Some(anchor)),
        Err(error) if error.is_missing() => Ok(None),
        Err(error) => Err(error.into()),
    })
    .await?;

    let Some(anchor) = opened else {
        return Ok(None);
    };
    anchor::refuse_anchor_escape(cwd, path, &anchor)?;

    Ok(Some(Arc::new(anchor)))
}

/// Reads the anchored file, or reports that there is nothing at the name.
async fn read_through(anchor: &Arc<Anchor>) -> Result<Option<Opened>, ToolError> {
    let anchor = Arc::clone(anchor);

    blocking(move || {
        let mut file = match anchor.read() {
            Ok(file) => file,
            Err(error) if error.is_missing() => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let path = anchor.path();
        let failed = |error: std::io::Error| {
            ToolError::Failed(format!("{} could not be read: {error}", path.display()))
        };

        let meta = file.metadata().map_err(failed)?;
        if meta.is_dir() {
            return Ok(Some(Opened { is_dir: true, stamp: None, bytes: Vec::new() }));
        }

        let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or_default());
        file.read_to_end(&mut bytes).map_err(failed)?;

        Ok(Some(Opened { is_dir: false, stamp: meta.modified().ok(), bytes }))
    })
    .await
}

/// Writes `content` through the anchor and hands back the stamp the file
/// carries afterwards, read from the same descriptor that wrote it.
async fn write_through(
    anchor: &Arc<Anchor>,
    content: String,
) -> Result<Option<SystemTime>, ToolError> {
    let anchor = Arc::clone(anchor);

    blocking(move || {
        let path = anchor.path();
        let failed = |error: std::io::Error| {
            ToolError::Failed(format!("{} could not be written: {error}", path.display()))
        };

        let (mut file, _existed) = anchor.write()?;
        file.set_len(0).map_err(failed)?;
        file.write_all(content.as_bytes()).map_err(failed)?;

        Ok(anchor::stamp(&file))
    })
    .await
}

/// Runs one blocking filesystem step on the pool tokio keeps for exactly that,
/// so the reactor is never held while a file is opened, read or written.
///
/// This is what `tokio::fs` does internally; the anchored calls simply cannot
/// be spelled through it, since what they operate on is a descriptor rather
/// than a path.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ToolError::Failed(format!("the edit could not be run: {error}")))?
}

/// Works out what the file should contain, without changing it.
///
/// Returns the old text, the new text and whether the file carries a byte
/// order mark, all with the mark itself stripped so no strategy has to reason
/// about an invisible first character.
async fn prepare(
    anchor: Option<Arc<Anchor>>,
    path: &Path,
    args: &Args,
    ctx: &ToolCtx,
) -> Result<(String, String, bool), ToolError> {
    let existing = match &anchor {
        Some(anchor) => read_through(anchor).await?,
        None => None,
    };

    if args.old_string.is_empty() {
        if existing.is_some() {
            return Err(ToolError::Failed(EMPTY_OLD_STRING.to_owned()));
        }
        let (bom, text) = split_bom(&args.new_string);
        return Ok((String::new(), text.to_owned(), bom));
    }

    let Some(existing) = existing else {
        return Err(ToolError::Failed(format!("File {} not found", path.display())));
    };
    if existing.is_dir {
        return Err(ToolError::Failed(format!(
            "Path is a directory, not a file: {}",
            path.display()
        )));
    }
    ctx.files.check_fresh_stat(path, existing.stamp)?;

    // Decoding is strict: a lossy decode would replace whatever it could not
    // read and then write the replacement characters back over the file.
    let text = String::from_utf8(existing.bytes).map_err(|_| {
        ToolError::Failed(format!(
            "{} is not valid UTF-8; edit cannot change it without corrupting it",
            path.display()
        ))
    })?;
    let (bom, content_old) = split_bom(&text);

    // The model quotes the file as it was shown, which may not be how the file
    // is stored; both strings are moved onto the file's own line ending first.
    let ending = detect_line_ending(content_old);
    let old = to_line_ending(&args.old_string, ending);
    let new = to_line_ending(&args.new_string, ending);

    let replaced = replace(content_old, &old, &new, args.replace_all.unwrap_or(false))?;
    tracing::debug!(
        strategy = replaced.strategy,
        file = %path.display(),
        "edit matched"
    );
    let (next_bom, content_new) = split_bom(&replaced.text);

    Ok((content_old.to_owned(), content_new.to_owned(), bom || next_bom))
}

// ---------------------------------------------------------------------------
// Replacement
// ---------------------------------------------------------------------------

/// A replacement that succeeded, and the strategy that resolved it.
#[derive(Debug)]
struct Replacement {
    /// The whole file, after the replacement.
    text: String,
    /// Which of [`REPLACERS`] matched, for the log.
    strategy: &'static str,
}

/// Given the file and the model's `oldString`, the spans of the file this
/// strategy is willing to treat as that string.
///
/// Upstream's replacers are generators, consumed until one candidate is
/// accepted. Collecting eagerly changes no outcome, because whether a
/// candidate is accepted never depends on a candidate the driver has not
/// reached yet.
type Replacer = for<'a> fn(&'a str, &'a str) -> Vec<Cow<'a, str>>;

/// Every strategy, in the order upstream tries them. Order is behavior: each
/// one only ever sees what the ones above it declined.
const REPLACERS: [(&str, Replacer); 9] = [
    ("simple", simple),
    ("line-trimmed", line_trimmed),
    ("block-anchor", block_anchor),
    ("whitespace-normalized", whitespace_normalized),
    ("indentation-flexible", indentation_flexible),
    ("escape-normalized", escape_normalized),
    ("trimmed-boundary", trimmed_boundary),
    ("context-aware", context_aware),
    ("multi-occurrence", multi_occurrence),
];

/// Replaces `old_string` with `new_string` in `content`.
///
/// Each strategy runs in turn, and the first candidate it offers that appears
/// in the file wins — once, unless `replace_all` asks for every occurrence, in
/// which case an ambiguous candidate is exactly what was wanted.
///
/// # Errors
///
/// Returns [`ToolError::Failed`] carrying the sentence the model reads next:
/// nothing matched, several places matched, the match was suspiciously large,
/// or the two strings were never different to begin with.
fn replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<Replacement, ToolError> {
    if old_string == new_string {
        return Err(ToolError::Failed(IDENTICAL.to_owned()));
    }
    if old_string.is_empty() {
        return Err(ToolError::Failed(EMPTY_OLD_STRING.to_owned()));
    }

    let mut found_any = false;
    for (strategy, replacer) in REPLACERS {
        for search in replacer(content, old_string) {
            // An empty candidate is "found" everywhere and nowhere: upstream
            // lets it through, where `replaceAll` would then splice the new
            // text between every character of the file. It is dropped here.
            if search.is_empty() {
                continue;
            }
            let Some(index) = content.find(search.as_ref()) else {
                continue;
            };
            found_any = true;
            if is_disproportionate_match(&search, old_string) {
                return Err(ToolError::Failed(DISPROPORTIONATE.to_owned()));
            }
            if replace_all {
                return Ok(Replacement {
                    text: content.replace(search.as_ref(), new_string),
                    strategy,
                });
            }
            if content.rfind(search.as_ref()) != Some(index) {
                continue;
            }
            let mut text = String::with_capacity(content.len() + new_string.len());
            text.push_str(&content[..index]);
            text.push_str(new_string);
            text.push_str(&content[index + search.len()..]);
            return Ok(Replacement { text, strategy });
        }
    }

    if found_any {
        Err(ToolError::Failed(MULTIPLE_MATCHES.to_owned()))
    } else {
        Err(ToolError::Failed(NOT_FOUND.to_owned()))
    }
}

/// Whether a candidate covers so much more than the model asked for that
/// replacing it would be a deletion in disguise.
fn is_disproportionate_match(search: &str, old_string: &str) -> bool {
    let old_lines = old_string.split('\n').count();
    let search_lines = search.split('\n').count();
    if search_lines >= (old_lines + 3).max(old_lines * 2) {
        return true;
    }
    if old_lines == 1 {
        return false;
    }
    let old_length = js_trim(old_string).chars().count();
    js_trim(search).chars().count() > (old_length + 500).max(old_length * 4)
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// The string as given, which is the whole strategy when the model quoted the
/// file correctly.
fn simple<'a>(_content: &'a str, find: &'a str) -> Vec<Cow<'a, str>> {
    vec![Cow::Borrowed(find)]
}

/// The same lines, ignoring what surrounds them on each — the model dropped or
/// added indentation while copying.
fn line_trimmed<'a>(content: &'a str, find: &'a str) -> Vec<Cow<'a, str>> {
    let original: Vec<&str> = content.split('\n').collect();
    let mut search: Vec<&str> = find.split('\n').collect();
    // A trailing newline in `find` means the last line, not an empty one.
    if search.last() == Some(&"") {
        search.pop();
    }
    if search.is_empty() || search.len() > original.len() {
        return Vec::new();
    }

    let spans = line_spans(content);
    (0..=original.len() - search.len())
        .filter(|start| {
            search
                .iter()
                .zip(&original[*start..])
                .all(|(want, line)| js_trim(line) == js_trim(want))
        })
        .map(|start| Cow::Borrowed(block(content, &spans, start, search.len())))
        .collect()
}

/// The block between the same first and last line, when what is in between is
/// close enough — the model paraphrased the body but framed it correctly.
fn block_anchor<'a>(content: &'a str, find: &'a str) -> Vec<Cow<'a, str>> {
    let original: Vec<&str> = content.split('\n').collect();
    let mut search: Vec<&str> = find.split('\n').collect();
    // Two anchors and something between them, counted before the trailing
    // newline is dropped, exactly as upstream counts it.
    if search.len() < 3 {
        return Vec::new();
    }
    if search.last() == Some(&"") {
        search.pop();
    }

    let first_line = js_trim(search[0]);
    let last_line = js_trim(search[search.len() - 1]);
    let block_size = search.len();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "line counts stay tiny"
    )]
    let max_delta = ((block_size as f64 * 0.25).floor() as usize).max(1);

    let mut candidates = Vec::new();
    for (start, line) in original.iter().enumerate() {
        if js_trim(line) != first_line {
            continue;
        }
        // The closing anchor cannot be the line after the opening one — a
        // block needs a middle for the similarity test to mean anything — and
        // only the first one after this opening anchor is considered.
        let closing = original
            .iter()
            .enumerate()
            .skip(start + 2)
            .find(|(_, line)| js_trim(line) == last_line);
        if let Some((end, _)) = closing
            && (end - start + 1).abs_diff(block_size) <= max_delta
        {
            candidates.push((start, end));
        }
    }

    let spans = line_spans(content);
    let Some(&(first_start, first_end)) = candidates.first() else {
        return Vec::new();
    };

    if candidates.len() == 1 {
        // One candidate is scored as it goes and accepted the moment it clears
        // the bar, so a long block stops comparing early.
        let mut similarity = 0.0;
        let to_check = block_size.min(first_end - first_start + 1) - 2;
        if to_check == 0 {
            similarity = 1.0;
        } else {
            for offset in 1..block_size.min(first_end - first_start + 1) - 1 {
                let Some(step) = line_similarity(original[first_start + offset], search[offset])
                else {
                    continue;
                };
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "line and char counts stay far below 2^53"
                )]
                let share = step / to_check as f64;
                similarity += share;
                if similarity >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD {
                    break;
                }
            }
        }
        if similarity >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD {
            return vec![Cow::Borrowed(block(
                content,
                &spans,
                first_start,
                first_end - first_start + 1,
            ))];
        }
        return Vec::new();
    }

    // Several candidates are scored in full and the best one competes against
    // the bar, so an edit never lands on the second-most-similar block.
    let mut best = None;
    let mut best_similarity = -1.0;
    for (start, end) in candidates {
        let to_check = block_size.min(end - start + 1) - 2;
        let similarity = if to_check == 0 {
            1.0
        } else {
            let total: f64 = (1..block_size.min(end - start + 1) - 1)
                .filter_map(|offset| line_similarity(original[start + offset], search[offset]))
                .sum();
            #[expect(
                clippy::cast_precision_loss,
                reason = "line and char counts stay far below 2^53"
            )]
            let averaged = total / to_check as f64;
            averaged
        };
        if similarity > best_similarity {
            best_similarity = similarity;
            best = Some((start, end));
        }
    }

    match best {
        Some((start, end)) if best_similarity >= MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD => {
            vec![Cow::Borrowed(block(content, &spans, start, end - start + 1))]
        }
        _ => Vec::new(),
    }
}

/// How alike two lines are once trimmed, from 0 to 1, or [`None`] when both
/// are empty and comparing them would say nothing.
fn line_similarity(original: &str, search: &str) -> Option<f64> {
    let original = js_trim(original);
    let search = js_trim(search);
    let longest = original.chars().count().max(search.chars().count());
    if longest == 0 {
        return None;
    }
    #[expect(clippy::cast_precision_loss, reason = "line and char counts stay far below 2^53")]
    Some(1.0 - levenshtein(original, search) as f64 / longest as f64)
}

/// The same text with every run of whitespace treated as one space — the model
/// reflowed the line.
fn whitespace_normalized<'a>(content: &'a str, find: &'a str) -> Vec<Cow<'a, str>> {
    let normalized_find = normalize_whitespace(find);
    let words: Vec<&str> = find.split(is_js_whitespace).filter(|word| !word.is_empty()).collect();
    let lines: Vec<&str> = content.split('\n').collect();
    let mut found = Vec::new();

    for line in &lines {
        let normalized_line = normalize_whitespace(line);
        if normalized_line == normalized_find {
            found.push(Cow::Borrowed(*line));
            continue;
        }
        // The whole line does not match, so look for the words inside it, in
        // order, separated by any whitespace — the file's spacing, not the
        // model's.
        if normalized_line.contains(&normalized_find)
            && let Some(matched) = match_words(line, &words)
        {
            found.push(Cow::Borrowed(matched));
        }
    }

    let find_lines = find.split('\n').count();
    if find_lines > 1 && find_lines <= lines.len() {
        let spans = line_spans(content);
        found.extend(
            (0..=lines.len() - find_lines)
                .map(|start| block(content, &spans, start, find_lines))
                .filter(|candidate| normalize_whitespace(candidate) == normalized_find)
                .map(Cow::Borrowed),
        );
    }

    found
}

/// The leftmost span of `line` holding `words` in order, separated by runs of
/// whitespace.
///
/// Upstream builds this as a regular expression of the escaped words joined
/// with `\s+`. Here it is a literal scan, which is the same match: the words
/// carry no whitespace of their own, so a greedy run of it can never need to
/// give any back.
fn match_words<'a>(line: &'a str, words: &[&str]) -> Option<&'a str> {
    // No words at all means the model's string was pure whitespace, and
    // upstream's empty pattern matches the empty span at the start.
    let Some((first, rest)) = words.split_first() else {
        return Some(&line[..0]);
    };

    let mut from = 0;
    while let Some(offset) = line[from..].find(first) {
        let start = from + offset;
        let mut end = start + first.len();
        let matched = rest.iter().all(|word| {
            let gap = line[end..].len() - line[end..].trim_start_matches(is_js_whitespace).len();
            if gap == 0 || !line[end + gap..].starts_with(word) {
                return false;
            }
            end += gap + word.len();
            true
        });
        if matched {
            return Some(&line[start..end]);
        }
        from = start + first.chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// The same lines under a different common indent — the model re-indented the
/// block it copied, or the file did.
///
/// [`line_trimmed`] ignores indentation outright and runs first, so this
/// rarely decides an edit; what it adds is that relative indentation inside
/// the block still has to line up.
fn indentation_flexible<'a>(content: &'a str, find: &'a str) -> Vec<Cow<'a, str>> {
    let normalized_find = remove_indentation(find);
    let lines: Vec<&str> = content.split('\n').collect();
    let find_lines = find.split('\n').count();
    if find_lines > lines.len() {
        return Vec::new();
    }

    let spans = line_spans(content);
    (0..=lines.len() - find_lines)
        .map(|start| block(content, &spans, start, find_lines))
        .filter(|candidate| remove_indentation(candidate) == normalized_find)
        .map(Cow::Borrowed)
        .collect()
}

/// The same text once escape sequences are resolved — the model handed back a
/// string as it appeared inside source code rather than as it sits in the file.
fn escape_normalized<'a>(content: &'a str, find: &'a str) -> Vec<Cow<'a, str>> {
    let unescaped_find = unescape(find);
    let mut found = Vec::new();
    if content.contains(&unescaped_find) {
        found.push(Cow::Owned(unescaped_find.clone()));
    }

    let lines: Vec<&str> = content.split('\n').collect();
    let find_lines = unescaped_find.split('\n').count();
    if find_lines <= lines.len() {
        let spans = line_spans(content);
        found.extend(
            (0..=lines.len() - find_lines)
                .map(|start| block(content, &spans, start, find_lines))
                .filter(|candidate| unescape(candidate) == unescaped_find)
                .map(Cow::Borrowed),
        );
    }

    found
}

/// The same text without the blank space around it — the model included a
/// leading or trailing newline that the file does not have there.
fn trimmed_boundary<'a>(content: &'a str, find: &'a str) -> Vec<Cow<'a, str>> {
    let trimmed = js_trim(find);
    if trimmed == find {
        return Vec::new();
    }

    let mut found = Vec::new();
    if content.contains(trimmed) {
        found.push(Cow::Borrowed(trimmed));
    }

    let lines: Vec<&str> = content.split('\n').collect();
    let find_lines = find.split('\n').count();
    if find_lines <= lines.len() {
        let spans = line_spans(content);
        found.extend(
            (0..=lines.len() - find_lines)
                .map(|start| block(content, &spans, start, find_lines))
                .filter(|candidate| js_trim(candidate) == trimmed)
                .map(Cow::Borrowed),
        );
    }

    found
}

/// The block framed by the same first and last line and the same length, when
/// half its middle lines still match — the looser sibling of the block anchor,
/// and the last resort before giving up on inexact text.
fn context_aware<'a>(content: &'a str, find: &'a str) -> Vec<Cow<'a, str>> {
    let mut find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() < 3 {
        return Vec::new();
    }
    if find_lines.last() == Some(&"") {
        find_lines.pop();
    }

    let content_lines: Vec<&str> = content.split('\n').collect();
    let first_line = js_trim(find_lines[0]);
    let last_line = js_trim(find_lines[find_lines.len() - 1]);
    let spans = line_spans(content);
    let mut found = Vec::new();

    for (start, line) in content_lines.iter().enumerate() {
        if js_trim(line) != first_line {
            continue;
        }
        // Whether or not the block it frames is taken, the search for this
        // opening anchor ends at the first closing one.
        let closing = content_lines
            .iter()
            .enumerate()
            .skip(start + 2)
            .find(|(_, line)| js_trim(line) == last_line);
        let Some((end, _)) = closing else {
            continue;
        };

        let length = end - start + 1;
        if length != find_lines.len() {
            continue;
        }

        let mut matching = 0_usize;
        let mut compared = 0_usize;
        for offset in 1..length - 1 {
            let block_line = js_trim(content_lines[start + offset]);
            let find_line = js_trim(find_lines[offset]);
            if block_line.is_empty() && find_line.is_empty() {
                continue;
            }
            compared += 1;
            if block_line == find_line {
                matching += 1;
            }
        }

        #[expect(clippy::cast_precision_loss, reason = "line and char counts stay far below 2^53")]
        let alike = compared == 0 || matching as f64 / compared as f64 >= 0.5;
        if alike {
            found.push(Cow::Borrowed(block(content, &spans, start, length)));
        }
    }

    found
}

/// Every exact occurrence of the string.
///
/// This can never be the strategy that resolves an edit, upstream or here:
/// it offers the same string [`simple`] already offered, so the driver has
/// always reached its verdict by the time this runs. It is ported because the
/// order is the contract, and dropping a link from the chain would be a claim
/// about upstream that a later version could falsify.
fn multi_occurrence<'a>(content: &'a str, find: &'a str) -> Vec<Cow<'a, str>> {
    content.match_indices(find).map(|_| Cow::Borrowed(find)).collect()
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// Whether `c` is whitespace to JavaScript, whose `\s` and `trim` also cover
/// the byte order mark. Matching that keeps a strategy from deciding a line
/// differs only because a mark sits in front of it.
fn is_js_whitespace(c: char) -> bool {
    c.is_whitespace() || c == BOM
}

/// `text` without the whitespace at either end, by the same rule.
fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// `text` with every run of whitespace collapsed to one space and the ends cut.
fn normalize_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split(is_js_whitespace).filter(|word| !word.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// `text` with the indentation every non-blank line shares taken off all of
/// them, so two blocks at different depths compare equal.
fn remove_indentation(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let Some(min_indent) = lines
        .iter()
        .filter(|line| !js_trim(line).is_empty())
        .map(|line| line.chars().take_while(|c| is_js_whitespace(*c)).count())
        .min()
    else {
        return text.to_owned();
    };

    let mut out = String::with_capacity(text.len());
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if js_trim(line).is_empty() {
            out.push_str(line);
        } else {
            out.push_str(chars_from(line, min_indent));
        }
    }
    out
}

/// `text` from its `count`-th character on, or nothing when it is shorter.
fn chars_from(text: &str, count: usize) -> &str {
    text.char_indices().nth(count).map_or("", |(offset, _)| &text[offset..])
}

/// `text` with the escape sequences a model writes inside source code resolved
/// to the characters they stand for. Any other backslash is left alone.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let resolved = match chars.peek() {
            Some('n' | '\n') => Some('\n'),
            Some('t') => Some('\t'),
            Some('r') => Some('\r'),
            Some('\'') => Some('\''),
            Some('"') => Some('"'),
            Some('`') => Some('`'),
            Some('\\') => Some('\\'),
            Some('$') => Some('$'),
            _ => None,
        };
        match resolved {
            Some(c) => {
                out.push(c);
                chars.next();
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Distance between two strings in single-character edits, counted over
/// characters so that a multi-byte one weighs the same as any other.
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    if b.is_empty() {
        return a.chars().count();
    }

    // Only the previous row is ever read, so the full matrix upstream builds
    // collapses to two.
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0_usize; b.len() + 1];
    let mut rows = 0;

    for (i, left) in a.chars().enumerate() {
        rows = i + 1;
        current[0] = rows;
        for (j, right) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(left != *right);
            current[j + 1] = (previous[j + 1] + 1).min(current[j] + 1).min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }

    if rows == 0 { b.len() } else { previous[b.len()] }
}

/// The line ending the file is written with, which is CRLF as soon as it uses
/// one anywhere.
fn detect_line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// `text` with every CRLF turned into a bare newline.
fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// `text` rewritten to end its lines the way `ending` does, whatever it
/// arrived with.
fn to_line_ending(text: &str, ending: &str) -> String {
    let normalized = normalize_line_endings(text);
    if ending == "\n" { normalized } else { normalized.replace('\n', ending) }
}

/// Splits the byte order mark off `text`, reporting whether there was one.
fn split_bom(text: &str) -> (bool, &str) {
    match text.strip_prefix(BOM) {
        Some(rest) => (true, rest),
        None => (false, text),
    }
}

/// `text` written back with a byte order mark if the file had one.
fn join_bom(text: &str, bom: bool) -> String {
    let (_, stripped) = split_bom(text);
    if bom {
        let mut out = String::with_capacity(stripped.len() + BOM.len_utf8());
        out.push(BOM);
        out.push_str(stripped);
        out
    } else {
        stripped.to_owned()
    }
}

/// Byte range of every `\n`-separated line of `text`, in order.
///
/// The lines `first..=last` are `text[spans[first].start..spans[last].end]`,
/// which is what upstream reconstructs by summing line lengths, and what lets
/// every strategy hand back a borrowed slice of the file instead of a
/// rebuilt copy.
fn line_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (offset, _) in text.match_indices('\n') {
        spans.push(start..offset);
        start = offset + 1;
    }
    spans.push(start..text.len());
    spans
}

/// The `count` lines of `text` starting at `first`, as they appear in it.
fn block<'a>(text: &'a str, spans: &[Range<usize>], first: usize, count: usize) -> &'a str {
    &text[spans[first].start..spans[first + count - 1].end]
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

/// A unified patch turning `old` into `new`, headed the way the `diff` package
/// upstream uses heads one, and with the indentation every line shares taken
/// off so a deeply nested change does not render mostly as blank space.
fn patch(name: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let hunks = diff.unified_diff().context_radius(CONTEXT_RADIUS).header(name, name).to_string();
    trim_diff(&format!("Index: {name}\n{SEPARATOR}\n{hunks}"))
}

/// How many lines the change adds and removes.
fn line_counts(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    let mut additions = 0;
    let mut deletions = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }
    (additions, deletions)
}

/// Takes the indentation shared by every line of a patch off all of them,
/// leaving the one-character prefix that says what happened to each.
fn trim_diff(diff: &str) -> String {
    // The file headers start with the same characters as an added or removed
    // line and are not content.
    let is_content = |line: &str| {
        (line.starts_with('+') || line.starts_with('-') || line.starts_with(' '))
            && !line.starts_with("---")
            && !line.starts_with("+++")
    };

    let min_indent = diff
        .split('\n')
        .filter(|line| is_content(line))
        .filter_map(|line| {
            let content = &line[1..];
            if js_trim(content).is_empty() {
                None
            } else {
                Some(content.chars().take_while(|c| is_js_whitespace(*c)).count())
            }
        })
        .min();

    let Some(min_indent) = min_indent.filter(|indent| *indent > 0) else {
        return diff.to_owned();
    };

    let mut out = String::with_capacity(diff.len());
    for (index, line) in diff.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if is_content(line) {
            out.push_str(&line[..1]);
            out.push_str(chars_from(&line[1..], min_indent));
        } else {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
#[path = "edit_tests.rs"]
mod tests;
