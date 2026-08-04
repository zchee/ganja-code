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

use std::{
    borrow::Cow,
    collections::HashMap,
    io::{Read as _, Write as _},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::SystemTime,
};

use async_trait::async_trait;
use serde::Deserialize;
use similar::{ChangeTag, TextDiff};

use crate::tool::{
    Tool, ToolCtx, ToolError, ToolOutput,
    anchor::{self, Anchor},
};

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
        LOCKS
            .lock()
            .expect("the lock table is never poisoned")
            .entry(path.to_owned())
            .or_default(),
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
            title: relative(&ctx.cwd, &path),
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
            return Ok(Some(Opened {
                is_dir: true,
                stamp: None,
                bytes: Vec::new(),
            }));
        }

        let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or_default());
        file.read_to_end(&mut bytes).map_err(failed)?;

        Ok(Some(Opened {
            is_dir: false,
            stamp: meta.modified().ok(),
            bytes,
        }))
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
        return Err(ToolError::Failed(format!(
            "File {} not found",
            path.display()
        )));
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

    Ok((
        content_old.to_owned(),
        content_new.to_owned(),
        bom || next_bom,
    ))
}

/// `file_path` as an absolute path, resolved against the session's directory
/// when the model passed a relative one.
fn resolve(cwd: &Path, file_path: &str) -> PathBuf {
    let path = Path::new(file_path);
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

/// `path` as the transcript should show it: relative to the session's
/// directory, or whole when it lies outside.
fn relative(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
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
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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
                #[allow(clippy::cast_precision_loss)]
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
            #[allow(clippy::cast_precision_loss)]
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
            vec![Cow::Borrowed(block(
                content,
                &spans,
                start,
                end - start + 1,
            ))]
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
    #[allow(clippy::cast_precision_loss)]
    Some(1.0 - levenshtein(original, search) as f64 / longest as f64)
}

/// The same text with every run of whitespace treated as one space — the model
/// reflowed the line.
fn whitespace_normalized<'a>(content: &'a str, find: &'a str) -> Vec<Cow<'a, str>> {
    let normalized_find = normalize_whitespace(find);
    let words: Vec<&str> = find
        .split(is_js_whitespace)
        .filter(|word| !word.is_empty())
        .collect();
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

        #[allow(clippy::cast_precision_loss)]
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
    content
        .match_indices(find)
        .map(|_| Cow::Borrowed(find))
        .collect()
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
    text.char_indices()
        .nth(count)
        .map_or("", |(offset, _)| &text[offset..])
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

    if rows == 0 {
        b.len()
    } else {
        previous[b.len()]
    }
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
    if ending == "\n" {
        normalized
    } else {
        normalized.replace('\n', ending)
    }
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
    let hunks = diff
        .unified_diff()
        .context_radius(CONTEXT_RADIUS)
        .header(name, name)
        .to_string();
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
mod tests {
    use std::{path::Path, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::{
        Args, BOM, DESCRIPTION, EditTool, IDENTICAL, MULTIPLE_MATCHES, NOT_FOUND, REPLACERS,
        SEPARATOR, block, chars_from, is_disproportionate_match, levenshtein, line_spans,
        normalize_whitespace, remove_indentation, replace, trim_diff, unescape,
    };
    use crate::tool::{FileTimes, Tool, ToolCtx, ToolError, ToolOutput};

    /// A context over `cwd` whose file log starts empty.
    fn ctx(cwd: &Path) -> ToolCtx {
        ToolCtx {
            cwd: cwd.to_owned(),
            cancel: CancellationToken::new(),
            call_id: "call_edit".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: None,
            spawn: None,
        }
    }

    /// Writes `content` to `name` under `cwd` and marks it read, which is what
    /// a `read` call ahead of the edit would have done.
    fn seed(cwd: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = cwd.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the fixture makes its directories");
        }
        std::fs::write(&path, content).expect("the fixture writes");
        path
    }

    /// Runs an edit and gives back what the model would see.
    async fn run(ctx: &ToolCtx, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        EditTool.run(args, ctx).await
    }

    /// A project whose root is pinned by a `.git`, and somewhere outside it.
    ///
    /// The marker is what makes the boundary deterministic: `Project::resolve`
    /// walks up for one, so without it the root would be whatever ancestor of
    /// the temporary directory happened to be a checkout — and on a machine
    /// whose `TMPDIR` sits inside one, that ancestor would contain "outside"
    /// too and these tests would quietly stop testing anything.
    #[cfg(unix)]
    fn project_and_elsewhere() -> (tempfile::TempDir, tempfile::TempDir) {
        let project = tempfile::tempdir().expect("a scratch directory");
        std::fs::create_dir(project.path().join(".git")).expect("the marker is creatable");
        let elsewhere = tempfile::tempdir().expect("a second scratch directory");

        (project, elsewhere)
    }

    /// An edit follows a link exactly as a write does, and a link planted at a
    /// path the project allows is how the file outside it gets rewritten.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_edit_through_a_link_that_leaves_the_project_is_refused() {
        let (project, elsewhere) = project_and_elsewhere();
        let secret = elsewhere.path().join("secret.txt");
        std::fs::write(&secret, "alpha\n").expect("the fixture writes");
        let planted = project.path().join("notes.txt");
        std::os::unix::fs::symlink(&secret, &planted).expect("the link is creatable");

        let context = ctx(project.path());
        context.files.record(&planted);
        let refused = failure(
            &context,
            serde_json::json!({
                "filePath": "notes.txt",
                "oldString": "alpha",
                "newString": "omega",
            }),
        )
        .await;

        assert!(
            refused.contains("symbolic link"),
            "an edit through a link out of the project must say so: {refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&secret).expect("the file outside still exists"),
            "alpha\n",
            "the edit followed the link and rewrote a file outside the project"
        );
    }

    /// The same escape one level up, where it is the directory that leads out.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_edit_inside_a_linked_directory_that_leaves_the_project_is_refused() {
        let (project, elsewhere) = project_and_elsewhere();
        let secret = elsewhere.path().join("secret.txt");
        std::fs::write(&secret, "alpha\n").expect("the fixture writes");
        std::os::unix::fs::symlink(elsewhere.path(), project.path().join("escape"))
            .expect("the link is creatable");

        let context = ctx(project.path());
        // Recorded under the name the call spells, so read-before-write is
        // satisfied and the refusal below can only be the escape guard's.
        context
            .files
            .record(&project.path().join("escape").join("secret.txt"));
        let refused = failure(
            &context,
            serde_json::json!({
                "filePath": "escape/secret.txt",
                "oldString": "alpha",
                "newString": "omega",
            }),
        )
        .await;

        assert!(
            refused.contains("symbolic link"),
            "a linked parent leads out of the project just as well: {refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&secret).expect("the file outside still exists"),
            "alpha\n"
        );
    }

    /// `..` is not resolved by `std::path::absolute`, so a path can carry one
    /// all the way here — `grep` hands the model absolute paths that may hold
    /// one. A `..` *after* a link lands where the link led: the text collapses
    /// to a path inside the project, so a prefix test on it would pass, while
    /// the kernel resolves it somewhere else entirely. That is why both sides
    /// of the comparison are canonical and never raw text.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dot_dot_path_that_climbs_out_through_a_link_is_refused() {
        let (project, elsewhere) = project_and_elsewhere();
        // Two levels, so `link/..` lands somewhere this test owns rather than
        // in the shared temporary root.
        let inner = elsewhere.path().join("inner");
        std::fs::create_dir(&inner).expect("the fixture makes a directory");
        let landing = elsewhere.path().join("secret.txt");
        std::fs::write(&landing, "alpha\n").expect("the fixture writes");
        std::os::unix::fs::symlink(&inner, project.path().join("link"))
            .expect("the link is creatable");

        let context = ctx(project.path());
        context
            .files
            .record(&project.path().join("link").join("..").join("secret.txt"));
        let refused = failure(
            &context,
            serde_json::json!({
                "filePath": "link/../secret.txt",
                "oldString": "alpha",
                "newString": "omega",
            }),
        )
        .await;

        assert!(
            refused.contains("symbolic link"),
            "`link/..` is the link's parent, not the project: {refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&landing).expect("the file outside still exists"),
            "alpha\n",
            "the edit escaped the project through `..` after a link"
        );
    }

    /// The other direction, and the one `grep` actually produces: a `..` that
    /// comes back inside the project is an ordinary path, not an escape.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dot_dot_path_that_lands_back_inside_the_project_is_edited() {
        let (project, _elsewhere) = project_and_elsewhere();
        std::fs::create_dir(project.path().join("nested")).expect("the fixture makes a directory");
        let file = project.path().join("a.rs");
        std::fs::write(&file, "alpha\n").expect("the fixture writes");

        let context = ctx(project.path());
        context
            .files
            .record(&project.path().join("nested").join("..").join("a.rs"));
        run(
            &context,
            serde_json::json!({
                "filePath": "nested/../a.rs",
                "oldString": "alpha",
                "newString": "omega",
            }),
        )
        .await
        .expect("a `..` that comes back inside the project is not an escape");

        assert_eq!(
            std::fs::read_to_string(&file).expect("the file is readable"),
            "omega\n"
        );
    }

    /// The case the guard must not break: a link that stays inside the project
    /// is an ordinary way to arrange a checkout.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_edit_through_a_link_that_stays_inside_the_project_still_applies() {
        let (project, _elsewhere) = project_and_elsewhere();
        let real = project.path().join("real");
        std::fs::create_dir(&real).expect("the fixture makes a directory");
        let file = real.join("notes.txt");
        std::fs::write(&file, "alpha\n").expect("the fixture writes");
        std::os::unix::fs::symlink(&real, project.path().join("link"))
            .expect("the link is creatable");

        let context = ctx(project.path());
        // Recorded under the name the edit uses: read-before-write keys on the
        // path as the call spells it, which is a link away from `file`.
        context
            .files
            .record(&project.path().join("link").join("notes.txt"));
        run(
            &context,
            serde_json::json!({
                "filePath": "link/notes.txt",
                "oldString": "alpha",
                "newString": "omega",
            }),
        )
        .await
        .expect("a link that goes nowhere new is not an escape");

        assert_eq!(
            std::fs::read_to_string(&file).expect("the file is readable"),
            "omega\n"
        );
    }

    /// The window the guard could only narrow, now closed — the edit half of
    /// the same story `write` tells.
    ///
    /// The link stays *inside* the project, so the lexical guard passes it,
    /// which is asserted rather than assumed: without that, the refusal below
    /// would prove nothing about where it came from. The old code read this
    /// file through the link and wrote back through it. `openat` with
    /// `O_NOFOLLOW` refuses the name outright, at the read, before any
    /// replacement is even attempted.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_planted_at_the_name_is_refused_by_the_open_not_by_the_guard() {
        let (project, _elsewhere) = project_and_elsewhere();
        let target = project.path().join("real.txt");
        std::fs::write(&target, "alpha\n").expect("the fixture writes");
        let planted = project.path().join("notes.txt");
        std::os::unix::fs::symlink(&target, &planted).expect("the link is creatable");

        crate::tool::anchor::refuse_link_escape(project.path(), &planted).expect(
            "a link that stays inside the project is no escape — if this starts \
             failing, the refusal below stops proving anything about the open",
        );

        let context = ctx(project.path());
        context.files.record(&planted);
        let refused = failure(
            &context,
            serde_json::json!({
                "filePath": "notes.txt",
                "oldString": "alpha",
                "newString": "omega",
            }),
        )
        .await;

        assert!(
            refused.contains("symbolic link"),
            "a link at the final component is refused by the open: {refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("the target still exists"),
            "alpha\n",
            "the edit followed a link planted at the name"
        );
        assert!(
            std::fs::symlink_metadata(&planted)
                .expect("the link is still there")
                .file_type()
                .is_symlink(),
            "the link is refused, not replaced"
        );
    }

    /// The message of a failed edit.
    async fn failure(ctx: &ToolCtx, args: serde_json::Value) -> String {
        match run(ctx, args).await {
            Ok(output) => panic!("the edit was expected to fail, and applied: {output:?}"),
            Err(error) => error.to_string(),
        }
    }

    /// `path` as it stands on disk.
    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("the file is readable")
    }

    // -----------------------------------------------------------------------
    // Strategies, ported from upstream's replacer suite
    // -----------------------------------------------------------------------

    /// One replacement, and which strategy upstream resolves it with.
    struct Case {
        content: &'static str,
        old: &'static str,
        new: &'static str,
        replace_all: bool,
        expected: &'static str,
        strategy: &'static str,
    }

    #[test]
    fn an_edit_is_resolved_by_the_strategy_upstream_resolves_it_with() {
        let cases: std::collections::BTreeMap<&str, Case> = [
            (
                "simple: the model quoted the file exactly",
                Case {
                    content: "old content here",
                    old: "old content",
                    new: "new content",
                    replace_all: false,
                    expected: "new content here",
                    strategy: "simple",
                },
            ),
            (
                "simple: a multi-line block quoted exactly",
                Case {
                    content: "line1\nline2\nline3",
                    old: "line2",
                    new: "new line 2\nextra line",
                    replace_all: false,
                    expected: "line1\nnew line 2\nextra line\nline3",
                    strategy: "simple",
                },
            ),
            (
                "line-trimmed: the model lost the indentation",
                Case {
                    content: "function a() {\n    const value = 1\n    return value\n}",
                    old: "const value = 1\nreturn value",
                    new: "const value = 2\nreturn value",
                    replace_all: false,
                    // The indented span is what matched, and it is replaced
                    // by exactly what the model wrote, indentation included
                    // or not. Upstream does not re-indent either.
                    expected: "function a() {\nconst value = 2\nreturn value\n}",
                    strategy: "line-trimmed",
                },
            ),
            (
                "line-trimmed: the model added trailing whitespace",
                Case {
                    content: "alpha\nbeta\ngamma",
                    old: "beta   ",
                    new: "beta-updated",
                    replace_all: false,
                    expected: "alpha\nbeta-updated\ngamma",
                    strategy: "line-trimmed",
                },
            ),
            (
                "block-anchor: the middle drifted but the frame held",
                Case {
                    content: "function configure() {\n  const enabled = true\n  return enabled\n}",
                    old: "function configure() {\n  const enable = true\n  return enable\n}",
                    new: "function configure() {\n  return false\n}",
                    replace_all: false,
                    expected: "function configure() {\n  return false\n}",
                    strategy: "block-anchor",
                },
            ),
            (
                "whitespace-normalized: the model reflowed the line",
                Case {
                    content: "const   value   =   compute( a,   b )",
                    old: "const value = compute( a, b )",
                    new: "const value = compute(a, b)",
                    replace_all: false,
                    expected: "const value = compute(a, b)",
                    strategy: "whitespace-normalized",
                },
            ),
            (
                "whitespace-normalized: the words sit inside a longer line",
                Case {
                    content: "prefix const   value = 1 suffix",
                    old: "const value = 1",
                    new: "const value = 2",
                    replace_all: false,
                    expected: "prefix const value = 2 suffix",
                    strategy: "whitespace-normalized",
                },
            ),
            (
                // The block was copied at another depth. Line-trimmed reaches
                // it first, and replaces the span it matched rather than the
                // one the model wrote, so the file's own indentation is not
                // preserved — upstream behaves the same way.
                "line-trimmed: the block was copied at another depth",
                Case {
                    content: "class A {\n        method() {\n            return 1\n        }\n}",
                    old: "  method() {\n      return 1\n  }",
                    new: "  method() {\n      return 2\n  }",
                    replace_all: false,
                    expected: "class A {\n  method() {\n      return 2\n  }\n}",
                    strategy: "line-trimmed",
                },
            ),
            (
                "escape-normalized: the model escaped what the file spells out",
                Case {
                    content: "const message = \"hello\nworld\"",
                    old: "const message = \\\"hello\\nworld\\\"",
                    new: "const message = \"goodbye\"",
                    replace_all: false,
                    expected: "const message = \"goodbye\"",
                    strategy: "escape-normalized",
                },
            ),
            (
                // The model wrapped the text in blank space. Whitespace
                // normalization takes it before the boundary trimmer does.
                "whitespace-normalized: the model wrapped the text in blank space",
                Case {
                    content: "alpha\nbeta\ngamma",
                    old: "\n  beta  \n",
                    new: "beta-updated",
                    replace_all: false,
                    expected: "alpha\nbeta-updated\ngamma",
                    strategy: "whitespace-normalized",
                },
            ),
            (
                "block-anchor: one middle line renamed inside the frame",
                Case {
                    content: "function go() {\n  first()\n  second()\n  third()\n}",
                    old: "function go() {\n  first()\n  second()\n  renamed()\n}",
                    new: "function go() {\n  only()\n}",
                    replace_all: false,
                    expected: "function go() {\n  only()\n}",
                    strategy: "block-anchor",
                },
            ),
            (
                // Two middle lines, one quoted exactly and one nothing like
                // the file: the anchor's average similarity falls under its
                // bar while the share of exactly matching lines clears the
                // looser one, which is the only gap this strategy fills.
                "context-aware: half the middle matches exactly, the rest not at all",
                Case {
                    content: "fn go() {\n  first()\n  second()\n}",
                    old: "fn go() {\n  first()\n  zzzzzzzzzzzzzzzz()\n}",
                    new: "fn go() {\n  only()\n}",
                    replace_all: false,
                    expected: "fn go() {\n  only()\n}",
                    strategy: "context-aware",
                },
            ),
            (
                "context-aware: the same, one indent deeper",
                Case {
                    content: "class A:\n    keep()\n    drop()\n    end()",
                    old: "class A:\n    keep()\n    qqqqqqqqqqqqqqqqqqq()\n    end()",
                    new: "class A:\n    replaced()\n    end()",
                    replace_all: false,
                    expected: "class A:\n    replaced()\n    end()",
                    strategy: "context-aware",
                },
            ),
            (
                "multi-occurrence: every exact occurrence, on request",
                Case {
                    content: "foo bar foo baz foo",
                    old: "foo",
                    new: "qux",
                    replace_all: true,
                    expected: "qux bar qux baz qux",
                    strategy: "simple",
                },
            ),
        ]
        .into_iter()
        .collect();

        for (name, case) in cases {
            let replaced = replace(case.content, case.old, case.new, case.replace_all)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(replaced.text, case.expected, "{name}");
            assert_eq!(replaced.strategy, case.strategy, "{name}: wrong strategy");
        }
    }

    /// One strategy, asked directly what it would offer.
    struct Offer {
        replacer: &'static str,
        content: &'static str,
        find: &'static str,
        candidates: &'static [&'static str],
    }

    #[test]
    fn every_strategy_offers_the_candidates_upstream_offers() {
        // Three of the nine rarely or never win in the driver, because the
        // ones above them accept the same spans first, so they are pinned
        // here at the only level where their behavior is observable. Each
        // expectation is what upstream's generator yields for the same input.
        let cases: std::collections::BTreeMap<&str, Offer> = [
            (
                "simple offers the string whether or not it is there",
                Offer {
                    replacer: "simple",
                    content: "anything",
                    find: "zz",
                    candidates: &["zz"],
                },
            ),
            (
                "line-trimmed offers every line whose trimmed form matches",
                Offer {
                    replacer: "line-trimmed",
                    content: "value = 1\nvalue = 1\n   value = 1   ",
                    find: "value = 1",
                    candidates: &["value = 1", "value = 1", "   value = 1   "],
                },
            ),
            (
                "block-anchor declines a frame whose middle shares nothing",
                Offer {
                    replacer: "block-anchor",
                    content: "a\nb\nc\na\nd\nc",
                    find: "a\nX\nc",
                    candidates: &[],
                },
            ),
            (
                "whitespace-normalized offers the file's own spacing",
                Offer {
                    replacer: "whitespace-normalized",
                    content: "prefix const   value = 1 suffix",
                    find: "const value = 1",
                    candidates: &["const   value = 1"],
                },
            ),
            (
                "indentation-flexible offers the block at the depth it sits at",
                Offer {
                    replacer: "indentation-flexible",
                    content: "class A {\n        method() {\n            return 1\n        }\n}",
                    find: "  method() {\n      return 1\n  }",
                    candidates: &["        method() {\n            return 1\n        }"],
                },
            ),
            (
                "indentation-flexible keeps relative indentation",
                Offer {
                    replacer: "indentation-flexible",
                    content: "  a\n    b\n",
                    find: "a\n  b",
                    candidates: &["  a\n    b"],
                },
            ),
            (
                "escape-normalized offers the resolved text and the line holding it",
                Offer {
                    replacer: "escape-normalized",
                    content: "a\tb",
                    find: "a\\tb",
                    candidates: &["a\tb", "a\tb"],
                },
            ),
            (
                "trimmed-boundary offers the text without its blank space",
                Offer {
                    replacer: "trimmed-boundary",
                    content: "alpha\nbeta\ngamma",
                    find: "\n  beta  \n",
                    candidates: &["beta"],
                },
            ),
            (
                "trimmed-boundary declines a string with nothing to trim",
                Offer {
                    replacer: "trimmed-boundary",
                    content: "x\ny\nz",
                    find: "y",
                    candidates: &[],
                },
            ),
            (
                "context-aware offers the framed block of the same length",
                Offer {
                    replacer: "context-aware",
                    content: "fn go() {\n  first()\n  second()\n}",
                    find: "fn go() {\n  first()\n  zzzzzzzzzzzzzzzz()\n}",
                    candidates: &["fn go() {\n  first()\n  second()\n}"],
                },
            ),
            (
                "multi-occurrence offers the string once per occurrence",
                Offer {
                    replacer: "multi-occurrence",
                    content: "foo bar foo",
                    find: "foo",
                    candidates: &["foo", "foo"],
                },
            ),
            (
                "multi-occurrence counts occurrences without overlapping them",
                Offer {
                    replacer: "multi-occurrence",
                    content: "aaaa",
                    find: "aa",
                    candidates: &["aa", "aa"],
                },
            ),
        ]
        .into_iter()
        .collect();

        for (name, case) in cases {
            let (_, replacer) = REPLACERS
                .iter()
                .find(|(named, _)| *named == case.replacer)
                .unwrap_or_else(|| panic!("{name}: no strategy called {}", case.replacer));
            let offered: Vec<String> = replacer(case.content, case.find)
                .into_iter()
                .map(std::borrow::Cow::into_owned)
                .collect();
            assert_eq!(offered, case.candidates, "{name}");
        }
    }

    #[test]
    fn a_strategy_only_ever_sees_what_the_ones_above_it_declined() {
        // The exact text is present twice and a spaced-out copy once. Simple
        // offers the ambiguous candidate, which is skipped rather than
        // accepted, and the search goes on down the list until a strategy
        // offers one that resolves to a single place.
        let content = "value = 1\nvalue = 1\n   value = 1   ";

        let replaced = replace(content, "value = 1", "value = 2", false)
            .expect("a later strategy resolves it");
        assert_eq!(replaced.strategy, "line-trimmed");
        assert_eq!(replaced.text, "value = 1\nvalue = 1\nvalue = 2");
    }

    #[test]
    fn a_match_far_larger_than_the_model_asked_for_is_refused() {
        // The frame matches and one of the two middle lines matches exactly,
        // which is enough for the loosest strategy to offer the block — and
        // the block is six hundred characters the model never asked about.
        let long = "z".repeat(600);
        let content = format!("head\n  keep()\n  {long}\ntail");

        let refused = replace(&content, "head\n  keep()\n  x()\ntail", "head\ntail", false)
            .expect_err("the span is disproportionate");
        assert!(
            refused.to_string().contains("much larger than oldString"),
            "got {refused}"
        );
    }

    #[test]
    fn a_match_is_disproportionate_by_lines_or_by_length() {
        // Three lines more, or twice as many, whichever bound is larger.
        assert!(is_disproportionate_match("a\nb\nc\nd", "a"));
        assert!(!is_disproportionate_match("a\nb\nc", "a\nb"));
        assert!(is_disproportionate_match("a\nb\nc\nd\ne", "a\nb"));
        // A single-line request is never measured by length.
        assert!(!is_disproportionate_match(&"x".repeat(5_000), "x"));
        // A multi-line one is.
        assert!(is_disproportionate_match(
            &format!("a\n{}", "x".repeat(5_000)),
            "a\nx"
        ));
        assert!(!is_disproportionate_match("a\nb", "a\nb"));
    }

    #[test]
    fn a_string_in_two_places_is_refused_unless_every_place_was_asked_for() {
        let refused =
            replace("same same", "same", "other", false).expect_err("two matches are ambiguous");
        assert_eq!(refused.to_string(), MULTIPLE_MATCHES);

        let replaced = replace("same same", "same", "other", true).expect("replaceAll takes both");
        assert_eq!(replaced.text, "other other");
    }

    #[test]
    fn a_string_that_is_nowhere_in_the_file_says_so() {
        let refused = replace("actual content", "not in file", "replacement", false)
            .expect_err("nothing matches");
        assert_eq!(refused.to_string(), NOT_FOUND);
    }

    #[test]
    fn the_two_strings_being_the_same_is_refused_before_anything_is_searched() {
        let refused = replace("content", "same", "same", false).expect_err("nothing would change");
        assert_eq!(refused.to_string(), IDENTICAL);
    }

    #[test]
    fn an_empty_old_string_never_reaches_a_strategy() {
        let refused =
            replace("content", "", "new", false).expect_err("an empty string matches everywhere");
        assert!(
            refused.to_string().contains("oldString cannot be empty"),
            "got {refused}"
        );
    }

    #[test]
    fn a_loose_block_anchor_match_is_declined_rather_than_guessed_at() {
        // Upstream's case: the anchors line up but the body is unrelated and
        // much longer, so no strategy may claim it.
        let content = "function configure() {\n  keepImportantState()\n  removeAllUserData()\n  archiveBackups()\n  auditLog()\n}";
        let old = "function configure() {\n  const enabled = true\n}";

        let refused = replace(
            content,
            old,
            "function configure() {\n  const enabled = false\n}",
            false,
        )
        .expect_err("the block is not the one the model meant");
        assert_eq!(refused.to_string(), NOT_FOUND);
    }

    #[test]
    fn a_block_anchor_match_with_unrelated_middle_content_is_declined() {
        let content = "function configure() {\n  removeAllUserData()\n}";
        let old = "function configure() {\n  const enabled = true\n}";

        let refused = replace(
            content,
            old,
            "function configure() {\n  const enabled = false\n}",
            false,
        )
        .expect_err("the middle line shares nothing with the one asked for");
        assert_eq!(refused.to_string(), NOT_FOUND);
    }

    #[test]
    fn replace_all_takes_every_occurrence_a_strategy_offers() {
        let replaced =
            replace("  keep  \n  keep  ", "keep", "kept", true).expect("both lines are replaced");
        assert_eq!(replaced.text, "  kept  \n  kept  ");
    }

    // -----------------------------------------------------------------------
    // Text helpers
    // -----------------------------------------------------------------------

    #[test]
    fn a_block_of_lines_is_the_slice_joining_them_would_be() {
        let text = "alpha\nbeta\ngamma\n";
        let spans = line_spans(text);
        let lines: Vec<&str> = text.split('\n').collect();

        assert_eq!(spans.len(), lines.len());
        for first in 0..lines.len() {
            for count in 1..=lines.len() - first {
                assert_eq!(
                    block(text, &spans, first, count),
                    lines[first..first + count].join("\n")
                );
            }
        }
    }

    #[test]
    fn line_spans_survive_multi_byte_characters() {
        let text = "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\nsecond\n";
        let spans = line_spans(text);

        assert_eq!(
            block(text, &spans, 0, 1),
            "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}"
        );
        assert_eq!(
            block(text, &spans, 0, 2),
            "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\nsecond"
        );
    }

    #[test]
    fn whitespace_normalizes_to_single_spaces_with_the_ends_cut() {
        assert_eq!(normalize_whitespace("  a \t\n  b  "), "a b");
        assert_eq!(normalize_whitespace("   "), "");
        assert_eq!(normalize_whitespace(""), "");
    }

    #[test]
    fn indentation_comes_off_every_line_by_the_shallowest_one() {
        assert_eq!(remove_indentation("    a\n      b\n"), "a\n  b\n");
        assert_eq!(remove_indentation("\n\n"), "\n\n");
        assert_eq!(remove_indentation("no indent"), "no indent");
    }

    #[test]
    fn escapes_resolve_only_where_javascript_resolves_them() {
        assert_eq!(unescape("a\\nb"), "a\nb");
        assert_eq!(unescape("a\\tb"), "a\tb");
        assert_eq!(unescape("\\\\n"), "\\n");
        assert_eq!(unescape("\\q"), "\\q");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    #[test]
    fn levenshtein_counts_single_character_edits() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("flaw", "lawn"), 2);
        // Characters, not bytes: one substitution, whatever it is encoded in.
        assert_eq!(levenshtein("\u{3042}\u{3044}", "\u{3042}\u{3046}"), 1);
    }

    #[test]
    fn a_character_slice_never_lands_inside_a_character() {
        assert_eq!(
            chars_from("\u{3042}\u{3044}\u{3046}", 1),
            "\u{3044}\u{3046}"
        );
        assert_eq!(chars_from("ab", 5), "");
    }

    #[test]
    fn a_patch_loses_the_indentation_all_of_its_lines_share() {
        let diff = "Index: a\n--- a\n+++ a\n@@ -1,2 +1,2 @@\n     kept\n-    old\n+    new\n";

        assert_eq!(
            trim_diff(diff),
            "Index: a\n--- a\n+++ a\n@@ -1,2 +1,2 @@\n kept\n-old\n+new\n"
        );
        // Nothing shared, nothing taken.
        assert_eq!(
            trim_diff("--- a\n+++ a\n-old\n+new\n"),
            "--- a\n+++ a\n-old\n+new\n"
        );
        assert_eq!(SEPARATOR.len(), 67);
    }

    // -----------------------------------------------------------------------
    // The tool
    // -----------------------------------------------------------------------

    #[test]
    fn the_description_is_upstreams_prompt_file() {
        assert_eq!(EditTool.description(), DESCRIPTION);
        assert!(
            EditTool
                .description()
                .starts_with("Performs exact string replacements in files.")
        );
        assert!(EditTool.description().contains("replaceAll"));
    }

    #[test]
    fn the_schema_is_the_one_the_model_was_trained_against() {
        let schema = serde_json::to_value(EditTool.schema()).expect("a schema is JSON");
        let properties = schema["properties"]
            .as_object()
            .expect("an object of properties");

        let mut names: Vec<&String> = properties.keys().collect();
        names.sort();
        assert_eq!(names, ["filePath", "newString", "oldString", "replaceAll"]);
        assert_eq!(
            schema["required"],
            serde_json::json!(["filePath", "oldString", "newString"])
        );
        assert_eq!(
            properties["filePath"]["description"],
            serde_json::json!("The absolute path to the file to modify")
        );
        assert_eq!(
            properties["oldString"]["description"],
            serde_json::json!("The text to replace")
        );
        assert_eq!(
            properties["newString"]["description"],
            serde_json::json!("The text to replace it with (must be different from oldString)")
        );
        assert_eq!(
            properties["replaceAll"]["description"],
            serde_json::json!("Replace all occurrences of oldString (default false)")
        );
    }

    #[test]
    fn the_arguments_parse_by_the_names_upstream_uses() {
        let args: Args = serde_json::from_value(serde_json::json!({
            "filePath": "/a", "oldString": "x", "newString": "y", "replaceAll": true
        }))
        .expect("all four fields parse");
        assert_eq!(args.file_path, "/a");
        assert_eq!(args.replace_all, Some(true));

        let args: Args = serde_json::from_value(
            serde_json::json!({"filePath": "/a", "oldString": "x", "newString": "y"}),
        )
        .expect("replaceAll is optional");
        assert_eq!(args.replace_all, None);

        serde_json::from_value::<Args>(serde_json::json!({"oldString": "x", "newString": "y"}))
            .expect_err("filePath is required");
    }

    #[test]
    fn describe_names_the_file_the_call_would_change() {
        let described = EditTool.describe(&serde_json::json!({"filePath": "src/main.rs"}));
        assert_eq!(described, "edit src/main.rs");
        assert_eq!(EditTool.describe(&serde_json::json!({})), "edit");
    }

    #[tokio::test]
    async fn an_empty_old_string_creates_the_file_it_names() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = dir.path().join("newfile.txt");

        let output = run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "", "newString": "new content"}),
        )
        .await
        .expect("a new file is created");

        assert_eq!(read(&path), "new content");
        assert!(
            output.metadata["diff"]
                .as_str()
                .expect("a patch")
                .contains("new content"),
            "got {}",
            output.metadata["diff"]
        );
    }

    #[tokio::test]
    async fn creating_a_file_makes_the_directories_it_sits_in() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = dir.path().join("nested").join("dir").join("file.txt");

        run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "", "newString": "nested file"}),
        )
        .await
        .expect("the directories are made");

        assert_eq!(read(&path), "nested file");
    }

    #[tokio::test]
    async fn an_empty_old_string_against_an_existing_file_is_refused_and_changes_nothing() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let original = format!("{BOM}using System;\n");
        let path = seed(dir.path(), "existing.cs", &original);
        ctx.files.record(&path);

        let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "", "newString": "using Up;\n"}),
        )
        .await;

        assert!(
            refused.contains("oldString cannot be empty"),
            "got {refused}"
        );
        assert_eq!(read(&path), original);
    }

    #[tokio::test]
    async fn an_edit_replaces_the_text_it_names() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(dir.path(), "existing.txt", "old content here");
        ctx.files.record(&path);

        let output = run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old content", "newString": "new content"}),
        )
        .await
        .expect("the edit applies");

        assert_eq!(output.output, "Edit applied successfully.");
        assert_eq!(output.title, "existing.txt");
        assert_eq!(read(&path), "new content here");
    }

    #[tokio::test]
    async fn a_file_with_a_byte_order_mark_keeps_it_and_the_patch_does_not_show_it() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(
            dir.path(),
            "existing.cs",
            &format!("{BOM}using System;\nclass Test {{}}\n"),
        );
        ctx.files.record(&path);

        let output = run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "using System;", "newString": "using Up;"}),
        )
        .await
        .expect("the mark does not hide the first line");

        let diff = output.metadata["diff"].as_str().expect("a patch");
        assert!(diff.contains("-using System;"), "got {diff}");
        assert!(diff.contains("+using Up;"), "got {diff}");
        assert!(!diff.contains(BOM), "the patch shows the mark");

        let content = read(&path);
        assert!(content.starts_with(BOM));
        assert_eq!(&content[BOM.len_utf8()..], "using Up;\nclass Test {}\n");
    }

    #[tokio::test]
    async fn editing_a_file_that_is_not_there_says_so() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = dir.path().join("nonexistent.txt");

        let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old", "newString": "new"}),
        )
        .await;

        assert!(refused.contains("not found"), "got {refused}");
    }

    #[tokio::test]
    async fn editing_a_directory_says_so() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = dir.path().join("adir");
        std::fs::create_dir(&path).expect("the fixture makes a directory");

        let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old", "newString": "new"}),
        )
        .await;

        assert!(refused.contains("directory"), "got {refused}");
    }

    #[tokio::test]
    async fn the_two_strings_being_the_same_is_refused_before_the_file_is_opened() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(dir.path(), "file.txt", "content");

        for old in ["same", ""] {
            let refused = failure(
                &ctx,
                serde_json::json!({"filePath": path, "oldString": old, "newString": old}),
            )
            .await;
            assert!(refused.contains("identical"), "got {refused}");
        }
        assert_eq!(read(&path), "content");
    }

    #[tokio::test]
    async fn an_edit_that_finds_nothing_leaves_the_file_byte_for_byte_as_it_was() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let original = "actual content\n";
        let path = seed(dir.path(), "file.txt", original);
        ctx.files.record(&path);
        let before = std::fs::read(&path).expect("the fixture is readable");

        let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "not in file", "newString": "replacement"}),
        )
        .await;

        assert_eq!(refused, NOT_FOUND);
        assert_eq!(
            std::fs::read(&path).expect("the file is still there"),
            before
        );
    }

    #[tokio::test]
    async fn an_ambiguous_edit_leaves_the_file_byte_for_byte_as_it_was() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(dir.path(), "file.txt", "same same");
        ctx.files.record(&path);
        let before = std::fs::read(&path).expect("the fixture is readable");

        let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "same", "newString": "other"}),
        )
        .await;

        assert_eq!(refused, MULTIPLE_MATCHES);
        assert_eq!(
            std::fs::read(&path).expect("the file is still there"),
            before
        );
    }

    #[tokio::test]
    async fn replace_all_changes_every_occurrence() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(dir.path(), "file.txt", "foo bar foo baz foo");
        ctx.files.record(&path);

        run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "foo", "newString": "qux", "replaceAll": true}),
        )
        .await
        .expect("every occurrence is replaced");

        assert_eq!(read(&path), "qux bar qux baz qux");
    }

    #[tokio::test]
    async fn a_file_that_was_never_read_is_not_edited() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let original = "old content here";
        let path = seed(dir.path(), "file.txt", original);

        let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old content", "newString": "new content"}),
        )
        .await;

        assert!(refused.contains("read it first"), "got {refused}");
        assert_eq!(read(&path), original);
    }

    #[tokio::test]
    async fn a_file_that_changed_since_it_was_read_is_not_edited() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let original = "old content here";
        let path = seed(dir.path(), "file.txt", original);
        ctx.files.record(&path);
        // Filesystem stamps can be coarse; force one that differs.
        std::fs::File::open(&path)
            .and_then(|file| file.set_modified(std::time::SystemTime::UNIX_EPOCH))
            .expect("the fixture can move the stamp");

        let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old content", "newString": "new content"}),
        )
        .await;

        assert!(refused.contains("read it again"), "got {refused}");
        assert_eq!(read(&path), original);
    }

    #[tokio::test]
    async fn a_successful_edit_records_the_file_so_the_next_one_may_follow_it() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(dir.path(), "file.txt", "one\ntwo\n");
        ctx.files.record(&path);

        run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "one", "newString": "uno"}),
        )
        .await
        .expect("the first edit applies");
        run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "two", "newString": "dos"}),
        )
        .await
        .expect("the second edit follows without another read");

        assert_eq!(read(&path), "uno\ndos\n");
    }

    #[tokio::test]
    async fn a_cancelled_turn_leaves_the_file_alone() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let mut ctx = ctx(dir.path());
        let original = "old content here";
        let path = seed(dir.path(), "file.txt", original);
        ctx.files.record(&path);
        ctx.cancel = CancellationToken::new();
        ctx.cancel.cancel();

        let refused = run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old content", "newString": "new content"}),
        )
        .await
        .expect_err("a cancelled turn does not write");

        assert!(matches!(refused, ToolError::Cancelled), "got {refused:?}");
        assert_eq!(read(&path), original);
    }

    #[tokio::test]
    async fn a_relative_path_resolves_against_the_session_directory() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(dir.path(), "file.txt", "before");
        ctx.files.record(&path);

        let output = run(
            &ctx,
            serde_json::json!({"filePath": "file.txt", "oldString": "before", "newString": "after"}),
        )
        .await
        .expect("the path resolves");

        assert_eq!(output.title, "file.txt");
        assert_eq!(read(&path), "after");
    }

    #[tokio::test]
    async fn the_metadata_carries_the_patch_and_what_it_counts() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(dir.path(), "file.txt", "line1\nline2\nline3");
        ctx.files.record(&path);

        let output = run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "line2", "newString": "new line a\nnew line b"}),
        )
        .await
        .expect("the edit applies");

        let filediff = &output.metadata["filediff"];
        assert_eq!(
            filediff["file"],
            serde_json::json!(path.display().to_string())
        );
        assert_eq!(filediff["patch"], output.metadata["diff"]);
        assert_eq!(filediff["additions"], serde_json::json!(2));
        assert_eq!(filediff["deletions"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn the_file_keeps_the_line_endings_it_had() {
        struct Case {
            content: &'static str,
            old: &'static str,
            new: &'static str,
            replace_all: bool,
            expected: &'static str,
        }

        // Upstream's line-ending table: what the file uses wins, whatever the
        // model quoted.
        let cases: std::collections::BTreeMap<&str, Case> = [
            (
                "lf file, lf strings",
                Case {
                    content: "alpha\nbeta\ngamma\n",
                    old: "alpha\nbeta\ngamma",
                    new: "alpha\nbeta-updated\ngamma",
                    replace_all: false,
                    expected: "alpha\nbeta-updated\ngamma\n",
                },
            ),
            (
                "crlf file, crlf strings",
                Case {
                    content: "alpha\r\nbeta\r\ngamma\r\n",
                    old: "alpha\r\nbeta\r\ngamma",
                    new: "alpha\r\nbeta-updated\r\ngamma",
                    replace_all: false,
                    expected: "alpha\r\nbeta-updated\r\ngamma\r\n",
                },
            ),
            (
                "lf file, crlf strings",
                Case {
                    content: "alpha\nbeta\ngamma\n",
                    old: "alpha\r\nbeta\r\ngamma",
                    new: "alpha\r\nbeta-updated\r\ngamma",
                    replace_all: false,
                    expected: "alpha\nbeta-updated\ngamma\n",
                },
            ),
            (
                "crlf file, lf strings",
                Case {
                    content: "alpha\r\nbeta\r\ngamma\r\n",
                    old: "alpha\nbeta\ngamma",
                    new: "alpha\nbeta-updated\ngamma",
                    replace_all: false,
                    expected: "alpha\r\nbeta-updated\r\ngamma\r\n",
                },
            ),
            (
                "lf file, crlf replacement only",
                Case {
                    content: "alpha\nbeta\ngamma\n",
                    old: "alpha\nbeta\ngamma",
                    new: "alpha\r\nbeta-updated\r\ngamma",
                    replace_all: false,
                    expected: "alpha\nbeta-updated\ngamma\n",
                },
            ),
            (
                "crlf file, lf replacement only",
                Case {
                    content: "alpha\r\nbeta\r\ngamma\r\n",
                    old: "alpha\r\nbeta\r\ngamma",
                    new: "alpha\nbeta-updated\ngamma",
                    replace_all: false,
                    expected: "alpha\r\nbeta-updated\r\ngamma\r\n",
                },
            ),
            (
                "lf file, mixed strings",
                Case {
                    content: "alpha\nbeta\ngamma\n",
                    old: "alpha\nbeta\r\ngamma",
                    new: "alpha\r\nbeta\nomega",
                    replace_all: false,
                    expected: "alpha\nbeta\nomega\n",
                },
            ),
            (
                "crlf file, mixed strings",
                Case {
                    content: "alpha\r\nbeta\r\ngamma\r\n",
                    old: "alpha\r\nbeta\ngamma",
                    new: "alpha\nbeta\r\nomega",
                    replace_all: false,
                    expected: "alpha\r\nbeta\r\nomega\r\n",
                },
            ),
            (
                "lf file, every block replaced",
                Case {
                    content: "alpha\nbeta\nalpha\nbeta\n",
                    old: "alpha\nbeta",
                    new: "alpha\nbeta-updated",
                    replace_all: true,
                    expected: "alpha\nbeta-updated\nalpha\nbeta-updated\n",
                },
            ),
            (
                "crlf file, every block replaced",
                Case {
                    content: "alpha\r\nbeta\r\nalpha\r\nbeta\r\n",
                    old: "alpha\r\nbeta",
                    new: "alpha\r\nbeta-updated",
                    replace_all: true,
                    expected: "alpha\r\nbeta-updated\r\nalpha\r\nbeta-updated\r\n",
                },
            ),
        ]
        .into_iter()
        .collect();

        for (name, case) in cases {
            let dir = tempfile::tempdir().expect("a scratch directory");
            let ctx = ctx(dir.path());
            let path = seed(dir.path(), "test.txt", case.content);
            ctx.files.record(&path);

            run(
                &ctx,
                serde_json::json!({
                    "filePath": path,
                    "oldString": case.old,
                    "newString": case.new,
                    "replaceAll": case.replace_all,
                }),
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));

            assert_eq!(read(&path), case.expected, "{name}");
        }
    }

    #[tokio::test]
    async fn a_crlf_file_edited_by_a_single_line_stays_crlf() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(dir.path(), "file.txt", "line1\r\nold\r\nline3");
        ctx.files.record(&path);

        run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old", "newString": "new"}),
        )
        .await
        .expect("the edit applies");

        assert_eq!(read(&path), "line1\r\nnew\r\nline3");
    }

    #[tokio::test]
    async fn text_outside_ascii_is_replaced_without_being_cut_apart() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(
            dir.path(),
            "file.txt",
            "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\n\u{1f980} crab\n\u{4e16}\u{754c}\n",
        );
        ctx.files.record(&path);

        run(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "\u{1f980} crab", "newString": "\u{1f980} \u{30ab}\u{30cb}"}),
        )
        .await
        .expect("the edit applies");

        assert_eq!(
            read(&path),
            "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\n\u{1f980} \u{30ab}\u{30cb}\n\u{4e16}\u{754c}\n"
        );
    }

    #[tokio::test]
    async fn text_outside_ascii_is_matched_through_the_looser_strategies_too() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = seed(
            dir.path(),
            "file.txt",
            "    \u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\n    \u{4e16}\u{754c}\n",
        );
        ctx.files.record(&path);

        run(
            &ctx,
            serde_json::json!({
                "filePath": path,
                "oldString": "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\n\u{4e16}\u{754c}",
                "newString": "\u{3055}\u{3088}\u{3046}\u{306a}\u{3089}\n\u{4e16}\u{754c}",
            }),
        )
        .await
        .expect("the indentation is forgiven");

        // The indented span is what matched, so the replacement stands where
        // it stood without its indentation — upstream does the same.
        assert_eq!(
            read(&path),
            "\u{3055}\u{3088}\u{3046}\u{306a}\u{3089}\n\u{4e16}\u{754c}\n"
        );
    }

    #[tokio::test]
    async fn a_file_that_is_not_text_is_refused_rather_than_rewritten() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = ctx(dir.path());
        let path = dir.path().join("binary.bin");
        std::fs::write(&path, [0xff_u8, 0xfe, 0x00, 0x01]).expect("the fixture writes bytes");
        ctx.files.record(&path);
        let before = std::fs::read(&path).expect("the fixture is readable");

        let refused = failure(
            &ctx,
            serde_json::json!({"filePath": path, "oldString": "old", "newString": "new"}),
        )
        .await;

        assert!(refused.contains("not valid UTF-8"), "got {refused}");
        assert_eq!(
            std::fs::read(&path).expect("the file is still there"),
            before
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_edits_to_one_file_both_survive() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let ctx = Arc::new(ctx(dir.path()));
        let path = seed(
            dir.path(),
            "file.txt",
            "top = 0\nmiddle = keep\nbottom = 0\n",
        );
        ctx.files.record(&path);

        let top = {
            let ctx = Arc::clone(&ctx);
            let path = path.clone();
            tokio::spawn(async move {
                run(
                    &ctx,
                    serde_json::json!({"filePath": path, "oldString": "top = 0", "newString": "top = 1"}),
                )
                .await
            })
        };
        let bottom = {
            let ctx = Arc::clone(&ctx);
            let path = path.clone();
            tokio::spawn(async move {
                run(
                    &ctx,
                    serde_json::json!({"filePath": path, "oldString": "bottom = 0", "newString": "bottom = 2"}),
                )
                .await
            })
        };

        top.await
            .expect("the task runs")
            .expect("the first edit applies");
        bottom
            .await
            .expect("the task runs")
            .expect("the second edit applies");

        assert_eq!(read(&path), "top = 1\nmiddle = keep\nbottom = 2\n");
    }
}
