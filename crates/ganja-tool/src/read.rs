//! The `read` tool.
//!
//! Spec: upstream `packages/opencode/src/tool/read.ts` and `read.txt`.
//!
//! Upstream also hands back images and PDFs, as attachments the model reads
//! directly. [`ToolOutput`] carries no attachment channel — [`ToolCtx`]'s
//! only postbox is `output: String` — so this port cannot forward those
//! bytes. Rather than silently pretend an unseen image was read, a call
//! against one succeeds with a message saying exactly that, echoing the
//! phrasing `webfetch` already uses for the same limitation.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolCtx, ToolError, ToolOutput, truncate};

/// How many lines a call reads when it names no `limit`. Upstream's
/// `DEFAULT_READ_LIMIT`.
const DEFAULT_READ_LIMIT: u64 = 2_000;

/// Longest a single line may be before it is cut. Upstream's
/// `MAX_LINE_LENGTH`.
const MAX_LINE_LENGTH: usize = 2_000;

/// Appended to a line cut at [`MAX_LINE_LENGTH`]. Upstream's
/// `MAX_LINE_SUFFIX`, which bakes the same `2000` in as text.
const MAX_LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";

/// Most bytes of file content one call returns. Upstream's `MAX_BYTES`.
const MAX_BYTES: usize = 50 * 1024;

/// [`MAX_BYTES`], as the call site names it. Upstream's `MAX_BYTES_LABEL`.
const MAX_BYTES_LABEL: &str = "50 KB";

/// How much of a file is read to guess whether it is binary, an image, or a
/// PDF, before committing to reading it as text. Upstream's `SAMPLE_BYTES`.
const SAMPLE_BYTES: u64 = 4_096;

/// Extensions upstream's `isBinaryFile` refuses outright, without sampling a
/// single byte.
const BINARY_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "exe", "dll", "so", "class", "jar", "war", "7z", "doc", "docx", "xls",
    "xlsx", "ppt", "pptx", "odt", "ods", "odp", "bin", "dat", "obj", "o", "a", "lib", "wasm",
    "pyc", "pyo",
];

/// What the model passes to `read`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The absolute path to the file or directory to read
    #[serde(rename = "filePath")]
    file_path: String,
    /// The line number to start reading from (1-indexed)
    #[serde(default)]
    offset: Option<u64>,
    /// The maximum number of lines to read (defaults to 2000)
    #[serde(default)]
    limit: Option<u64>,
}

/// Reads a file or directory.
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn id(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        include_str!("read.txt")
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let path = args
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        format!("read {path}")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let filepath = resolve(&ctx.cwd, &args.file_path);
        // Checked before the file is stat'ed, so a store that has not been
        // written yet is refused exactly like one that has: what the model
        // learns must not depend on whether this machine is logged in.
        if ctx.is_credential_store(&filepath) {
            return Err(credential_refusal(&filepath));
        }
        let title = display(&ctx.cwd, &filepath);

        let metadata = match std::fs::metadata(&filepath) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(miss(&filepath));
            }
            Err(error) => {
                return Err(ToolError::Failed(format!(
                    "could not stat {}: {error}",
                    filepath.display()
                )));
            }
        };

        if metadata.is_dir() {
            return Ok(read_directory(&filepath, &title, &args));
        }

        read_file(&filepath, &title, &args, ctx)
    }
}

/// Resolves `file_path` against `cwd` — never against the process cwd, so a
/// relative argument means what the call site meant, not what the engine's
/// own working directory happens to be.
fn resolve(cwd: &Path, file_path: &str) -> PathBuf {
    let path = Path::new(file_path);
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

/// `path` relative to `cwd` when it is under it, absolute otherwise — for a
/// title a person can actually read.
fn display(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).map_or_else(
        |_| path.display().to_string(),
        |rel| rel.display().to_string(),
    )
}

/// Upstream's `offset || 1`: an explicit `0` is falsy in JavaScript and so is
/// treated the same as an absent offset, unlike every other non-zero value.
fn effective_offset(offset: Option<u64>) -> u64 {
    match offset {
        Some(0) | None => 1,
        Some(value) => value,
    }
}

/// Upstream's `limit ?? DEFAULT_READ_LIMIT`: only an absent limit falls back
/// to the default — an explicit `0` is honored and reads zero lines.
fn effective_limit(limit: Option<u64>) -> u64 {
    limit.unwrap_or(DEFAULT_READ_LIMIT)
}

/// `File not found`, with up to three similarly-named entries from the same
/// directory when any exist. Upstream's `miss()`.
fn miss(filepath: &Path) -> ToolError {
    let dir = filepath.parent().unwrap_or_else(|| Path::new("."));
    let base = filepath
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    let suggestions: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| {
            let name = name.to_string_lossy().to_ascii_lowercase();
            name.contains(&base) || base.contains(&name)
        })
        .map(|name| dir.join(name))
        .take(3)
        .collect();

    if suggestions.is_empty() {
        return ToolError::Failed(format!("File not found: {}", filepath.display()));
    }

    let list = suggestions
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    ToolError::Failed(format!(
        "File not found: {}\n\nDid you mean one of these?\n{list}",
        filepath.display()
    ))
}

/// What the model is told when it asks for ganja's own credential store.
///
/// The arguments were well formed, so this is a failure rather than an argument
/// complaint, and the text is written to end the attempt rather than just this
/// call: a model that reads "refused" without reading "permanently" spends the
/// next step trying a different spelling of the same path.
fn credential_refusal(filepath: &Path) -> ToolError {
    ToolError::Failed(format!(
        "{} is ganja's credential store: it holds the provider API keys this \
         machine authenticates with, and reading it would put them in the \
         transcript that is sent to a provider. The refusal is fixed, not a \
         permission that can be granted, so retrying will not help. Continue \
         without this file's contents.",
        filepath.display()
    ))
}

/// Entry names in `path`, sorted, with a trailing `/` on directories —
/// including a symlink whose target is one. Upstream's `list()`.
fn list_directory(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| {
            let is_dir = match entry.file_type() {
                Ok(file_type) if file_type.is_symlink() => {
                    std::fs::metadata(entry.path()).is_ok_and(|target| target.is_dir())
                }
                Ok(file_type) => file_type.is_dir(),
                Err(_) => false,
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_dir { format!("{name}/") } else { name }
        })
        .collect();
    names.sort();
    names
}

/// The directory-listing branch of `read`.
fn read_directory(filepath: &Path, title: &str, args: &Args) -> ToolOutput {
    let entries = list_directory(filepath);
    let offset = effective_offset(args.offset) as usize;
    let limit = effective_limit(args.limit) as usize;
    let start = offset.saturating_sub(1);

    let sliced: Vec<&str> = entries
        .iter()
        .skip(start)
        .take(limit)
        .map(String::as_str)
        .collect();
    let truncated = start + sliced.len() < entries.len();

    let status = if truncated {
        format!(
            "\n(Showing {} of {} entries. Use 'offset' parameter to read beyond entry {})",
            sliced.len(),
            entries.len(),
            offset + sliced.len()
        )
    } else {
        format!("\n({} entries)", entries.len())
    };

    let output = [
        format!("<path>{}</path>", filepath.display()),
        "<type>directory</type>".to_owned(),
        "<entries>".to_owned(),
        sliced.join("\n"),
        status,
        "</entries>".to_owned(),
    ]
    .join("\n");
    let clamped = truncate::clamp(&output);

    ToolOutput {
        title: title.to_owned(),
        output: clamped.text,
        metadata: serde_json::json!({
            "preview": sliced.iter().take(20).collect::<Vec<_>>(),
            "truncated": truncated || clamped.truncated,
            "display": {
                "type": "directory",
                "path": filepath,
                "entries": sliced,
                "offset": offset,
                "totalEntries": entries.len(),
                "truncated": truncated,
            },
        }),
    }
}

/// The file branch of `read`: a binary/image/PDF refusal-or-notice, or the
/// line-numbered text content.
fn read_file(
    filepath: &Path,
    title: &str,
    args: &Args,
    ctx: &ToolCtx,
) -> Result<ToolOutput, ToolError> {
    let sample = read_sample(filepath).map_err(|error| {
        ToolError::Failed(format!("could not read {}: {error}", filepath.display()))
    })?;

    if let Some(mime) = sniff_mime(&sample) {
        ctx.files.record(filepath);
        if mime == "application/pdf" {
            return Ok(ToolOutput {
                title: title.to_owned(),
                output: "PDF read successfully. This tool cannot hand file bytes to the model yet."
                    .to_owned(),
                metadata: serde_json::json!({ "preview": "PDF read successfully", "truncated": false, "mime": mime }),
            });
        }
        if matches!(
            mime,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            return Ok(ToolOutput {
                title: title.to_owned(),
                output: format!(
                    "Image read successfully ({mime}). This tool cannot hand image bytes to the model yet."
                ),
                metadata: serde_json::json!({ "preview": "Image read successfully", "truncated": false, "mime": mime }),
            });
        }
    }

    if is_binary_file(filepath, &sample) {
        return Err(ToolError::Failed(format!(
            "Cannot read binary file: {}",
            filepath.display()
        )));
    }

    let file = std::fs::File::open(filepath).map_err(|error| {
        ToolError::Failed(format!("could not open {}: {error}", filepath.display()))
    })?;

    let offset = effective_offset(args.offset) as usize;
    let limit = effective_limit(args.limit) as usize;
    let start = offset.saturating_sub(1);

    let mut raw: Vec<String> = Vec::new();
    let mut count: usize = 0;
    let mut bytes: usize = 0;
    let mut cut = false;
    let mut more = false;

    // Deliberately does not stop at `limit`: once the byte budget has not
    // been hit, upstream keeps consuming the rest of the file so `count`
    // ends up the file's true line total, which the "Showing X-Y of N"
    // message below reports. Only the byte cap short-circuits early.
    for line in lossy_lines(file) {
        count += 1;
        if count <= start {
            continue;
        }
        if raw.len() >= limit {
            more = true;
            continue;
        }

        let line = clamp_line_length(line);
        let size = line.len() + usize::from(!raw.is_empty());
        if bytes + size <= MAX_BYTES {
            bytes += size;
            raw.push(line);
        } else {
            cut = true;
            more = true;
            break;
        }
    }

    if count < offset && !(count == 0 && offset == 1) {
        return Err(ToolError::Failed(format!(
            "Offset {offset} is out of range for this file ({count} lines)"
        )));
    }

    let last = offset + raw.len() - 1;
    let next = last + 1;
    let truncated = more || cut;

    let mut output = format!(
        "<path>{}</path>\n<type>file</type>\n<content>\n",
        filepath.display()
    );
    output.push_str(
        &raw.iter()
            .enumerate()
            .map(|(index, line)| format!("{}: {line}", index + offset))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    if cut {
        output.push_str(&format!(
            "\n\n(Output capped at {MAX_BYTES_LABEL}. Showing lines {offset}-{last}. Use offset={next} to continue.)"
        ));
    } else if more {
        output.push_str(&format!(
            "\n\n(Showing lines {offset}-{last} of {count}. Use offset={next} to continue.)"
        ));
    } else {
        output.push_str(&format!("\n\n(End of file - total {count} lines)"));
    }
    output.push_str("\n</content>");
    let clamped = truncate::clamp(&output);

    ctx.files.record(filepath);

    Ok(ToolOutput {
        title: title.to_owned(),
        output: clamped.text,
        metadata: serde_json::json!({
            "preview": raw.iter().take(20).collect::<Vec<_>>(),
            "truncated": truncated || clamped.truncated,
            "display": {
                "type": "file",
                "path": filepath,
                "lineStart": offset,
                "lineEnd": last,
                "totalLines": count,
                "truncated": truncated,
            },
        }),
    })
}

/// The first [`SAMPLE_BYTES`] of `path`, or fewer if the file is smaller.
fn read_sample(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;

    let file = std::fs::File::open(path)?;
    let mut sample = Vec::new();
    file.take(SAMPLE_BYTES).read_to_end(&mut sample)?;
    Ok(sample)
}

/// Sniffs `bytes` for a magic number identifying an image or PDF, mirroring
/// upstream's `sniffAttachmentMime` (minus its extension-based fallback,
/// which only ever matters for a file whose signature is already corrupt —
/// not a case worth a dependency on a MIME-by-extension crate to cover).
fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    let starts_with =
        |prefix: &[u8]| bytes.len() >= prefix.len() && &bytes[..prefix.len()] == prefix;

    if starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if starts_with(&[0x47, 0x49, 0x46, 0x38]) {
        return Some("image/gif");
    }
    if starts_with(&[0x42, 0x4D]) {
        return Some("image/bmp");
    }
    if starts_with(&[0x25, 0x50, 0x44, 0x46, 0x2D]) {
        return Some("application/pdf");
    }
    if bytes.len() >= 12 && starts_with(&[0x52, 0x49, 0x46, 0x46]) && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Upstream's `isBinaryFile`: a known-binary extension refuses outright,
/// otherwise a NUL byte or a high ratio of non-printable bytes in the sample
/// does.
fn is_binary_file(path: &Path, sample: &[u8]) -> bool {
    if let Some(extension) = path.extension().and_then(std::ffi::OsStr::to_str) {
        let extension = extension.to_ascii_lowercase();
        if BINARY_EXTENSIONS.contains(&extension.as_str()) {
            return true;
        }
    }

    if sample.is_empty() {
        return false;
    }

    let mut nonprintable = 0_usize;
    for &byte in sample {
        if byte == 0 {
            return true;
        }
        if byte < 9 || (byte > 13 && byte < 32) {
            nonprintable += 1;
        }
    }

    (nonprintable as f64) / (sample.len() as f64) > 0.3
}

/// `line`, cut to [`MAX_LINE_LENGTH`] characters with upstream's suffix
/// appended when it was too long.
fn clamp_line_length(line: String) -> String {
    if line.chars().count() <= MAX_LINE_LENGTH {
        return line;
    }

    let mut kept: String = line.chars().take(MAX_LINE_LENGTH).collect();
    kept.push_str(MAX_LINE_SUFFIX);
    kept
}

/// Lines of `file`, decoded lossily so a byte sequence that is not valid
/// UTF-8 substitutes the replacement character instead of failing the read —
/// upstream's `TextDecoder` does the same rather than throw mid-stream.
fn lossy_lines(file: std::fs::File) -> impl Iterator<Item = String> {
    use std::io::BufRead as _;

    let mut reader = std::io::BufReader::new(file);
    std::iter::from_fn(move || {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => None,
            Ok(_) => {
                if line.last() == Some(&b'\n') {
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                }
                Some(String::from_utf8_lossy(&line).into_owned())
            }
            Err(_) => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::ReadTool;
    use crate::{FileTimes, Tool, ToolCtx, ToolError};

    /// A context rooted at `cwd`, with a fresh, empty read log and no
    /// credential store to refuse.
    fn ctx(cwd: PathBuf) -> ToolCtx {
        ToolCtx {
            cwd,
            cancel: CancellationToken::new(),
            call_id: "call-1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: None,
            spawn: None,
        }
    }

    /// The same, told where the credentials are — which is what the engine
    /// hands every call, and the only thing the guard tests below need.
    fn guarding(cwd: PathBuf, store: &std::path::Path) -> ToolCtx {
        ToolCtx {
            credentials: Some(store.to_owned()),
            ..ctx(cwd)
        }
    }

    /// Where a fixture's credential store sits. Deliberately never written:
    /// what the model learns must not depend on whether this machine is logged
    /// in, so the guard answers for a store that is not there yet.
    fn store_in(dir: &std::path::Path) -> PathBuf {
        dir.join("ganja").join("auth.json")
    }

    #[tokio::test]
    async fn a_short_file_is_read_whole_and_numbered_from_one() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\ntwo\nthree").expect("the fixture writes");

        let out = ReadTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a short file reads cleanly");

        assert!(
            out.output.contains("1: one\n2: two\n3: three"),
            "got {:?}",
            out.output
        );
        assert!(
            out.output.contains("(End of file - total 3 lines)"),
            "got {:?}",
            out.output
        );
        assert_eq!(out.title, "a.txt");
    }

    #[tokio::test]
    async fn an_offset_and_limit_select_a_window_and_say_so() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "1\n2\n3\n4\n5\n").expect("the fixture writes");

        let out = ReadTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap(), "offset": 2, "limit": 2 }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a windowed read succeeds");

        assert!(out.output.contains("2: 2\n3: 3"), "got {:?}", out.output);
        assert!(!out.output.contains("1: 1") && !out.output.contains("4: 4"));
        assert!(
            out.output
                .contains("Showing lines 2-3 of 5. Use offset=4 to continue."),
            "got {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn an_offset_past_end_of_file_is_refused_with_the_line_count() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "only one line").expect("the fixture writes");

        let refused = ReadTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap(), "offset": 50 }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("an offset beyond the file is out of range");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("out of range") && message.contains("(1 lines)")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_file_at_the_default_offset_reads_as_zero_lines_not_an_error() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").expect("the fixture writes");

        let out = ReadTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("an empty file is not an out-of-range offset");

        assert!(
            out.output.contains("(End of file - total 0 lines)"),
            "got {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn an_explicit_zero_offset_falls_back_to_one_like_an_absent_offset() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\ntwo").expect("the fixture writes");

        let out = ReadTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap(), "offset": 0 }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("offset 0 is treated as absent, per upstream's `offset || 1`");

        assert!(
            out.output.contains("1: one\n2: two"),
            "got {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn a_missing_file_names_itself_and_suggests_lookalikes() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("readme.txt"), "x").expect("the fixture writes");
        let missing = dir.path().join("readme.tx");

        let refused = ReadTool
            .run(
                serde_json::json!({ "filePath": missing.to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("the file does not exist");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("File not found") && message.contains("readme.txt")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_file_with_no_lookalikes_gets_the_plain_message() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let missing = dir.path().join("nothing-close-to-this.txt");

        let refused = ReadTool
            .run(
                serde_json::json!({ "filePath": missing.to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("the file does not exist and the directory is empty");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message == &format!("File not found: {}", missing.display())),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn a_directory_lists_its_entries_sorted_with_trailing_slashes_on_subdirectories() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("b.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("a-dir")).unwrap();
        std::fs::write(dir.path().join("c.txt"), "x").unwrap();

        let out = ReadTool
            .run(
                serde_json::json!({ "filePath": dir.path().to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a directory is read as a listing");

        assert!(out.output.contains("<type>directory</type>"));
        assert!(
            out.output.contains("a-dir/\nb.txt\nc.txt"),
            "entries should be sorted with a trailing slash on the directory: {:?}",
            out.output
        );
        assert!(out.output.contains("(3 entries)"));
    }

    #[tokio::test]
    async fn a_binary_extension_is_refused_without_being_opened_for_content() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("archive.zip");
        // Deliberately text content: the extension alone must be enough to
        // refuse it, matching upstream's extension fast-path.
        std::fs::write(&path, "not actually binary").expect("the fixture writes");

        let refused = ReadTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("a binary extension is refused");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("Cannot read binary file")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn a_file_with_null_bytes_is_refused_as_binary() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("data.txt");
        std::fs::write(&path, [b'a', 0, b'b']).expect("the fixture writes");

        let refused = ReadTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("a NUL byte marks the file binary");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("Cannot read binary file"))
        );
    }

    #[tokio::test]
    async fn a_png_signature_is_reported_rather_than_refused_or_faked() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("pic.png");
        let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0u8; 32]);
        std::fs::write(&path, &bytes).expect("the fixture writes");

        let out = ReadTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("an image is a successful call, not a failure");

        assert!(
            out.output.contains("Image read successfully"),
            "got {:?}",
            out.output
        );
        assert_eq!(out.metadata["mime"], "image/png");
    }

    #[tokio::test]
    async fn a_line_longer_than_the_budget_is_cut_with_the_upstream_suffix() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("long.txt");
        std::fs::write(&path, "x".repeat(3_000)).expect("the fixture writes");

        let out = ReadTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap() }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a long line is truncated, not refused");

        assert!(
            out.output.contains("... (line truncated to 2000 chars)"),
            "got {:?}",
            out.output
        );
    }

    #[tokio::test]
    async fn a_read_file_may_then_be_overwritten() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello").expect("the fixture writes");
        let context = ctx(dir.path().to_owned());

        ReadTool
            .run(
                serde_json::json!({ "filePath": path.to_str().unwrap() }),
                &context,
            )
            .await
            .expect("the file reads");

        context
            .files
            .check_fresh(&path)
            .expect("a read records the file as fresh for a follow-up write");
    }

    #[test]
    fn the_schema_requires_only_the_file_path() {
        let schema = serde_json::to_value(ReadTool.schema()).expect("a schema is JSON");

        assert_eq!(schema["required"], serde_json::json!(["filePath"]));
        for name in ["filePath", "offset", "limit"] {
            assert!(
                schema["properties"][name].is_object(),
                "missing {name}: {schema}"
            );
        }
    }

    #[tokio::test]
    async fn ganjas_credential_store_is_refused_by_absolute_path() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let store = store_in(dir.path());

        let refused = ReadTool
            .run(
                serde_json::json!({ "filePath": store.to_str().unwrap() }),
                &guarding(dir.path().to_owned(), &store),
            )
            .await
            .expect_err("the credential store is never readable");

        let ToolError::Failed(message) = &refused else {
            panic!("well-formed arguments are not an argument error: {refused:?}");
        };
        assert!(
            message.contains("is ganja's credential store"),
            "got {message}"
        );
        assert!(message.contains("provider API keys"), "got {message}");
        assert!(
            message.contains("retrying will not help"),
            "a model that thinks the refusal is retryable will retry: {message}"
        );
    }

    #[tokio::test]
    async fn ganjas_credential_store_is_refused_through_a_relative_route_onto_it() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let store = store_in(dir.path());
        let directory = store.parent().expect("the store lives in a directory");
        let name = store.file_name().expect("the store is a file");

        let refused = ReadTool
            .run(
                serde_json::json!({ "filePath": name.to_str().unwrap() }),
                &guarding(directory.to_owned(), &store),
            )
            .await
            .expect_err("a relative route onto the store is still the store");

        let ToolError::Failed(message) = &refused else {
            panic!("well-formed arguments are not an argument error: {refused:?}");
        };
        assert!(
            message.contains("is ganja's credential store"),
            "got {message}"
        );
    }

    #[tokio::test]
    async fn a_project_file_that_only_shares_the_stores_name_still_reads() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let namesake = dir.path().join("auth.json");
        std::fs::write(&namesake, "not a credential store").expect("the fixture writes");

        let out = ReadTool
            .run(
                serde_json::json!({ "filePath": namesake.to_str().unwrap() }),
                &guarding(dir.path().to_owned(), &store_in(dir.path())),
            )
            .await
            .expect("the guard is identity-based: any other auth.json still reads");

        assert!(
            out.output.contains("1: not a credential store"),
            "got {:?}",
            out.output
        );
    }
}
