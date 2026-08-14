//! Language servers, and the diagnostics they hand back to the model.
//!
//! Spec: upstream `packages/opencode/src/lsp/lsp.ts` (lifecycle, matching,
//! the touch), with the per-tool append at `tool/edit.ts:197-201` and
//! `tool/write.ts:74-90`.
//!
//! # Opt-in, and lazy after that
//!
//! No `lsp` key means no language server ever starts — upstream's posture, and
//! the right one: a coding agent that silently spawns rust-analyzer on the
//! first file it reads has made a decision about somebody's laptop that
//! nobody asked it to make. With the key set, a server still starts only when
//! a file it claims is touched.
//!
//! # Failure is silence, never a failed turn
//!
//! Every path in here swallows. A server that will not start, a file that will
//! not read, a publish that never comes: each of them costs the model a
//! diagnostics block it would have got, and costs the turn nothing. This is
//! the house rule about tool results being information, applied to a
//! subsystem whose entire output is advice.
//!
//! A `(root, server)` pair that failed to start is **never retried for the
//! life of the session** (`lsp.ts:220-241`). One warning says so; a session
//! that retried a missing binary on every edit would say so a hundred times
//! and start a hundred processes that cannot start.
//!
//! # What is not here
//!
//! Upstream's experimental `lsp` tool — hover, definition, references and six
//! more, exposed to the model behind a flag — is not ported (deviation:
//! lsp-tool-unported). Nor are the diagnostics carried in tool metadata
//! (deviation: lsp-no-metadata-map): a frontend renders what the tool said,
//! and the tool says it in its text. Workspace-wide diagnostic pulls are not
//! ported either; see [`client`] for which half of that machinery is here and
//! why.

pub(crate) mod client;
pub(crate) mod diagnostic;
pub(crate) mod language;
pub mod server;

// [`Lsp::diagnostics`] hands back `lsp_types::Diagnostic`, so anything outside
// this crate that reads one has to be able to name it. Re-exported rather than
// wrapped: the protocol's own type is the honest one, and a parallel struct
// would be a second thing to keep in step with the specification.
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use client::Client;
use futures::future;
pub use lsp_types;
use lsp_types::Diagnostic;
use serde_json::Value;
use server::Spec;
use tokio::{sync::OnceCell, time::Instant};

use crate::config::LspConfig;

/// What `edit` prefixes its own file's diagnostics with (`tool/edit.ts:201`).
const OWN_FILE: &str = "\n\nLSP errors detected in this file, please fix:\n";

/// What `write` prefixes each *other* file's diagnostics with
/// (`tool/write.ts:88`). Repeated per file, as upstream repeats it.
const OTHER_FILES: &str = "\n\nLSP errors detected in other files:\n";

/// How many files besides the written one may contribute a block
/// (`tool/write.ts:18`).
const MAX_PROJECT_FILES: usize = 5;

/// Tools whose file argument is worth showing a language server.
///
/// Named here rather than asked of each tool, because the seam that appends
/// this is in the session loop and not in the tools: the observable output is
/// identical either way, and one list beats three tools each remembering to
/// call the same thing (deviation: lsp-append-at-the-seam).
const EDIT: &str = "edit";
/// See [`EDIT`].
const WRITE: &str = "write";
/// See [`EDIT`]. A read only warms a server up; it waits for nothing and
/// appends nothing (`tool/read.ts:117-120`).
const READ: &str = "read";

/// A client that has been started, or has permanently failed to start.
///
/// The [`None`] inside is the broken set: a cell is initialized once, so a
/// failure is remembered forever and a second toucher of the same `(root,
/// server)` pair waits on the first one's attempt instead of racing it.
type Slot = Arc<OnceCell<Option<Arc<Client>>>>;

/// Every language server this session may run, and every one it is running.
pub struct Lsp {
    /// The servers, resolved from config once. Sorted by id.
    servers: Vec<Spec>,
    /// Where the project starts. Every upward search for a root stops here,
    /// and a file outside it is not this session's business.
    directory: PathBuf,
    /// The bound on the one search allowed above [`Lsp::directory`] — the rust
    /// workspace walk.
    worktree: PathBuf,
    clients: std::sync::Mutex<HashMap<(PathBuf, String), Slot>>,
}

impl Lsp {
    /// The language servers `config` asked for, or [`None`] when it asked for
    /// none.
    ///
    /// [`None`] is the common case and is not a degraded one: absent and
    /// `false` both mean a session with no LSP, and the engine holds no
    /// service at all rather than an inert one.
    #[must_use]
    pub fn new(config: Option<&LspConfig>, root: &Path) -> Option<Arc<Self>> {
        let entries = match config? {
            LspConfig::Enabled(false) => return None,
            LspConfig::Enabled(true) => BTreeMap::new(),
            LspConfig::Servers(entries) => entries.clone(),
        };
        let servers = server::resolve(&entries);
        if servers.is_empty() {
            return None;
        }

        Some(Arc::new(Self {
            servers,
            directory: root.to_owned(),
            worktree: root.to_owned(),
            clients: std::sync::Mutex::default(),
        }))
    }

    /// Shows `path` to every server that claims it, and waits for what they
    /// then say when `wait` is set.
    ///
    /// Never fails. Two budgets bound it, and only the second is the wait:
    /// the **first** touch of a session starts the server it needs and its
    /// handshake is bounded by [`client::INITIALIZE_TIMEOUT`], after which
    /// waiting for what the server says is bounded by
    /// [`client::DOCUMENT_WAIT_TIMEOUT`]. An `edit` whose annotation awaits
    /// this inline can therefore sit for both.
    pub async fn touch(&self, path: &Path, wait: bool) {
        if !self.contains(path) {
            return;
        }
        // One stamp for the whole batch, taken before any of it: a publish is
        // fresh if it arrived after the *touch*, and reading the clock per
        // client would move the bar for every server after the first.
        let after = Instant::now();

        let touches = self
            .servers
            .iter()
            .filter(|spec| spec.matches(path))
            .map(|spec| self.touch_one(spec, path, after, wait));

        future::join_all(touches).await;
    }

    /// One server's half of [`Lsp::touch`]. Every failure here is a return.
    async fn touch_one(&self, spec: &Spec, path: &Path, after: Instant, wait: bool) {
        let Some(root) = server::root(spec, path, &self.directory, &self.worktree) else {
            return;
        };
        let Some(client) = self.client(spec, root).await else {
            return;
        };
        let version = match client.open(path).await {
            Ok(version) => version,
            Err(error) => {
                tracing::debug!(server = spec.id.as_str(), %error, "the file was not synced");

                return;
            }
        };
        if wait {
            client.wait_for_diagnostics(path, version, after).await;
        }
    }

    /// The client for `spec` at `root`, starting it if this is the first ask.
    ///
    /// [`None`] is a `(root, server)` pair that cannot be started. It is
    /// cached: the next caller gets the same [`None`] without another attempt.
    async fn client(&self, spec: &Spec, root: PathBuf) -> Option<Arc<Client>> {
        let slot = {
            let mut clients = self
                .clients
                .lock()
                .expect("the language server clients are never poisoned");

            Arc::clone(clients.entry((root.clone(), spec.id.clone())).or_default())
        };

        slot.get_or_init(|| async {
            match Client::start(spec, &root).await {
                Ok(client) => Some(Arc::new(client)),
                Err(error) => {
                    // Once per session per pair, because the cell this sits in
                    // is only ever initialized once.
                    tracing::warn!(
                        server = spec.id.as_str(),
                        root = %root.display(),
                        %error,
                        "the language server will not be used in this session"
                    );

                    None
                }
            }
        })
        .await
        .clone()
    }

    /// Everything every live client currently believes, merged by path.
    ///
    /// Two clients with something to say about one file have both of their
    /// opinions concatenated, which is upstream's behavior (`lsp.ts:364-375`)
    /// and the honest one: they are different servers and neither is wrong.
    #[must_use]
    pub fn diagnostics(&self) -> HashMap<PathBuf, Vec<Diagnostic>> {
        let mut merged: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();
        for client in self.live() {
            for (path, issues) in client.store().diagnostics() {
                merged.entry(path).or_default().extend(issues);
            }
        }

        merged
    }

    /// Ends every server this session started.
    pub fn shutdown(&self) {
        for client in self.live() {
            client.shutdown();
        }
    }

    /// Whether starting `id` at `root` has already been tried and failed.
    ///
    /// Exists for the tests that pin the never-retried rule; nothing in the
    /// engine asks.
    #[cfg(test)]
    #[must_use]
    pub fn is_broken(&self, root: &Path, id: &str) -> bool {
        self.clients
            .lock()
            .expect("the language server clients are never poisoned")
            .get(&(root.to_owned(), id.to_owned()))
            .and_then(|slot| slot.get())
            .is_some_and(Option::is_none)
    }

    /// Every client that started and is still held.
    fn live(&self) -> Vec<Arc<Client>> {
        self.clients
            .lock()
            .expect("the language server clients are never poisoned")
            .values()
            .filter_map(|slot| slot.get().cloned().flatten())
            .collect()
    }

    /// Whether `path` is this session's business (`lsp.ts:210`).
    fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.directory) || path.starts_with(&self.worktree)
    }

    /// What a completed call to `tool` adds to its own output.
    ///
    /// The whole of the LSP's model-facing surface, in one place, keyed by
    /// tool id. An empty string is the answer for every tool that is not one
    /// of the three named here — and for the three, whenever the servers had
    /// nothing to report.
    ///
    /// `read` returns immediately: its touch is a warm-up, forked so that the
    /// first edit in a session does not also pay for rust-analyzer's startup
    /// (`tool/read.ts:117-120`).
    pub async fn annotate(self: &Arc<Self>, tool: &str, args: &Value, cwd: &Path) -> String {
        let Some(path) = file_argument(args).map(|named| resolve(cwd, &named)) else {
            return String::new();
        };

        match tool {
            READ => {
                let lsp = Arc::clone(self);
                tokio::spawn(async move { lsp.touch(&path, false).await });

                String::new()
            }
            EDIT | WRITE => {
                self.touch(&path, true).await;

                // Read once and formatted from that one reading, as upstream
                // does: two reads could disagree, and a write's own-file
                // section and cross-file section describing different moments
                // is a transcript that contradicts itself.
                append(tool, &path, &self.diagnostics())
            }
            _ => String::new(),
        }
    }
}

/// What `tool` adds to its output, given everything the servers believe.
///
/// Pure, and separated from the service for that reason: this is where the
/// model-facing behavior lives — which sections appear, in which order, under
/// which headers, and how many — and none of it needs a language server to
/// be true.
fn append(tool: &str, path: &Path, diagnostics: &HashMap<PathBuf, Vec<Diagnostic>>) -> String {
    let mut appended = diagnostics
        .get(path)
        .and_then(|issues| diagnostic::report(&path.to_string_lossy(), issues))
        .map(|block| format!("{OWN_FILE}{block}"))
        .unwrap_or_default();

    if tool != WRITE {
        return appended;
    }

    // Upstream walks its diagnostics object in insertion order, which is the
    // order servers happened to publish in. Sorting by path instead makes one
    // write's output the same text twice (deviation: lsp-other-files-sorted)
    // — a transcript that differs run to run is a transcript nobody can diff.
    let mut others: Vec<&PathBuf> = diagnostics.keys().filter(|other| *other != path).collect();
    others.sort();

    appended.extend(
        others
            .into_iter()
            .filter_map(|other| {
                diagnostic::report(&other.to_string_lossy(), &diagnostics[other])
                    .map(|block| format!("{OTHER_FILES}{block}"))
            })
            .take(MAX_PROJECT_FILES),
    );

    appended
}

impl Drop for Lsp {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The file a tool call names.
///
/// All three tools that matter here spell it `filePath` on the wire — their
/// argument structs are `rename_all = "camelCase"`, because the names are
/// upstream's and upstream's are what the model was trained against. Read off
/// the raw arguments rather than by re-parsing each tool's struct: this seam
/// knows the three tool ids and one field name, and nothing else about them.
fn file_argument(args: &Value) -> Option<String> {
    args.get("filePath")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// `named` against `cwd` when it is relative, which the tools also do.
fn resolve(cwd: &Path, named: &str) -> PathBuf {
    let path = Path::new(named);
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        path::PathBuf,
        sync::Arc,
    };

    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    use serde_json::json;

    use super::{Lsp, MAX_PROJECT_FILES, OTHER_FILES, OWN_FILE, append, file_argument, resolve};
    use crate::config::{LspConfig, LspEntry};

    /// An `Lsp` with no servers configured but a live diagnostics surface, so
    /// the append can be tested without anything to start.
    fn service(root: &std::path::Path) -> Arc<Lsp> {
        Arc::new(Lsp {
            servers: Vec::new(),
            directory: root.to_owned(),
            worktree: root.to_owned(),
            clients: std::sync::Mutex::default(),
        })
    }

    fn error(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            message: message.to_owned(),
            ..Diagnostic::default()
        }
    }

    #[test]
    fn no_lsp_key_is_no_language_servers() {
        assert!(Lsp::new(None, std::path::Path::new("/p")).is_none());
    }

    #[test]
    fn an_lsp_set_to_false_is_no_language_servers() {
        assert!(Lsp::new(Some(&LspConfig::Enabled(false)), std::path::Path::new("/p")).is_none());
    }

    #[test]
    fn an_lsp_set_to_true_is_the_builtins() {
        let lsp = Lsp::new(Some(&LspConfig::Enabled(true)), std::path::Path::new("/p"))
            .expect("the builtins are servers");

        let ids: Vec<&str> = lsp.servers.iter().map(|spec| spec.id.as_str()).collect();
        assert_eq!(ids, ["gopls", "rust"]);
    }

    #[test]
    fn a_config_that_disables_every_builtin_is_no_language_servers() {
        let entries = BTreeMap::from([
            ("rust".to_owned(), disabled()),
            ("gopls".to_owned(), disabled()),
        ]);

        assert!(
            Lsp::new(
                Some(&LspConfig::Servers(entries)),
                std::path::Path::new("/p")
            )
            .is_none()
        );
    }

    fn disabled() -> LspEntry {
        LspEntry {
            command: None,
            extensions: None,
            disabled: true,
            env: BTreeMap::new(),
            initialization: None,
        }
    }

    #[test]
    fn a_file_argument_is_read_off_the_wire_name_the_tools_use() {
        assert_eq!(
            file_argument(&json!({ "filePath": "/p/a.rs" })).as_deref(),
            Some("/p/a.rs"),
            "which is camelCase for all three of read, edit and write"
        );
        assert_eq!(file_argument(&json!({ "pattern": "*.rs" })), None);
        assert_eq!(
            file_argument(&json!({ "filePath": 7 })),
            None,
            "a file path that is not a string names no file"
        );
    }

    #[test]
    fn a_relative_file_argument_resolves_against_the_working_directory() {
        assert_eq!(
            resolve(std::path::Path::new("/p"), "src/main.rs"),
            PathBuf::from("/p/src/main.rs")
        );
        assert_eq!(
            resolve(std::path::Path::new("/p"), "/elsewhere/main.rs"),
            PathBuf::from("/elsewhere/main.rs")
        );
    }

    #[test]
    fn an_edit_reports_its_own_file_and_says_nothing_about_any_other() {
        let edited = PathBuf::from("/p/src/main.rs");
        let diagnostics = HashMap::from([
            (edited.clone(), vec![error("mismatched types")]),
            (PathBuf::from("/p/src/other.rs"), vec![error("also broken")]),
        ]);

        let appended = append("edit", &edited, &diagnostics);

        assert_eq!(
            appended,
            "\n\nLSP errors detected in this file, please fix:\n\
             <diagnostics file=\"/p/src/main.rs\">\n\
             ERROR [1:1] mismatched types\n\
             </diagnostics>"
        );
        assert!(
            !appended.contains("other.rs"),
            "an edit is told about the file it edited, and no more: {appended}"
        );
    }

    #[test]
    fn a_write_reports_its_own_file_first_and_then_the_others() {
        let written = PathBuf::from("/p/src/main.rs");
        let diagnostics = HashMap::from([
            (written.clone(), vec![error("mismatched types")]),
            (PathBuf::from("/p/src/b.rs"), vec![error("b is broken")]),
            (PathBuf::from("/p/src/a.rs"), vec![error("a is broken")]),
        ]);

        let appended = append("write", &written, &diagnostics);

        let own = appended
            .find(OWN_FILE)
            .expect("the written file is reported");
        let first_other = appended.find(OTHER_FILES).expect("the others are reported");
        assert!(own < first_other, "the written file leads: {appended}");
        assert_eq!(
            appended
                .matches("LSP errors detected in other files")
                .count(),
            2,
            "the header repeats per file, as upstream repeats it: {appended}"
        );
        assert!(
            appended.find("a.rs").expect("a is reported")
                < appended.find("b.rs").expect("b is reported"),
            "sorted, so the same write produces the same text twice: {appended}"
        );
    }

    #[test]
    fn a_write_reports_at_most_five_other_files() {
        let written = PathBuf::from("/p/src/main.rs");
        let mut diagnostics = HashMap::from([(written.clone(), vec![error("mine")])]);
        for index in 0..MAX_PROJECT_FILES + 4 {
            diagnostics.insert(
                PathBuf::from(format!("/p/src/f{index}.rs")),
                vec![error("broken")],
            );
        }

        let appended = append("write", &written, &diagnostics);

        assert_eq!(
            appended
                .matches("LSP errors detected in other files")
                .count(),
            MAX_PROJECT_FILES
        );
        assert_eq!(appended.matches(OWN_FILE).count(), 1);
    }

    #[test]
    fn a_file_whose_only_diagnostics_are_warnings_adds_no_section() {
        let written = PathBuf::from("/p/src/main.rs");
        let warning = Diagnostic {
            severity: Some(DiagnosticSeverity::WARNING),
            ..error("unused import")
        };
        let diagnostics = HashMap::from([
            (written.clone(), vec![warning.clone()]),
            (PathBuf::from("/p/src/other.rs"), vec![warning]),
        ]);

        assert_eq!(append("write", &written, &diagnostics), "");
        assert_eq!(append("edit", &written, &diagnostics), "");
    }

    #[test]
    fn a_clean_run_appends_nothing() {
        let written = PathBuf::from("/p/src/main.rs");

        assert_eq!(append("write", &written, &HashMap::new()), "");
        assert_eq!(append("edit", &written, &HashMap::new()), "");
    }

    #[tokio::test]
    async fn a_read_warms_a_server_up_without_waiting_or_appending() {
        let lsp = service(std::path::Path::new("/p"));

        let appended = lsp
            .annotate(
                "read",
                &json!({ "filePath": "/p/src/main.rs" }),
                std::path::Path::new("/p"),
            )
            .await;

        assert_eq!(appended, "", "a read never carries diagnostics");
    }

    #[tokio::test]
    async fn a_tool_with_no_lsp_interest_is_never_annotated() {
        let lsp = service(std::path::Path::new("/p"));

        for tool in ["bash", "glob", "grep", "todowrite", "webfetch", "task"] {
            let appended = lsp
                .annotate(
                    tool,
                    &json!({ "file_path": "/p/src/main.rs" }),
                    std::path::Path::new("/p"),
                )
                .await;

            assert_eq!(appended, "", "{tool} appends nothing");
        }
    }

    #[tokio::test]
    async fn a_call_with_no_file_argument_is_never_annotated() {
        let lsp = service(std::path::Path::new("/p"));

        let appended = lsp
            .annotate(
                "edit",
                &json!({ "pattern": "*.rs" }),
                std::path::Path::new("/p"),
            )
            .await;

        assert_eq!(appended, "");
    }

    #[tokio::test]
    async fn a_server_that_will_not_start_is_never_started_again() {
        // A "server" that records having been run and then exits. Its stdout
        // closes, so `initialize` can never be answered and the client fails —
        // which is the interesting shape: not a missing binary, but one that
        // starts and is useless.
        let temp = tempfile::TempDir::new().expect("a temp dir");
        let root = temp.path().canonicalize().expect("the fixture resolves");
        let attempts = root.join("attempts");
        // The two platforms need different fixtures for one behaviour. A
        // `#!/bin/sh` file is not a program on Windows — nothing would spawn,
        // and the test would be counting a failure it never provoked — so
        // there the server is `cmd.exe` appending a line and exiting, which is
        // the same shape: a process that starts, says nothing an LSP client can
        // read, and goes. The echo carries no space, so nothing on the way to
        // `cmd` has to guess where its quoting ends.
        #[cfg(unix)]
        let command = {
            use std::os::unix::fs::PermissionsExt as _;

            let script = root.join("pretend-server");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\necho attempt >> {}\nexit 1\n",
                    attempts.display()
                ),
            )
            .expect("the script is written");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("the script is made runnable");

            vec![script.to_string_lossy().into_owned()]
        };
        #[cfg(not(unix))]
        let command = vec![
            "cmd.exe".to_owned(),
            "/c".to_owned(),
            format!("echo.attempt>>{}", attempts.display()),
        ];
        std::fs::write(root.join("main.rs"), "fn main() {}\n").expect("a file to touch");

        let lsp = Arc::new(Lsp {
            servers: vec![super::server::Spec {
                id: "pretend".to_owned(),
                extensions: vec![".rs".to_owned()],
                command: Some(command),
                root: super::server::Root::Directory,
                env: BTreeMap::new(),
                initialization: None,
            }],
            directory: root.clone(),
            worktree: root.clone(),
            clients: std::sync::Mutex::default(),
        });
        let file = root.join("main.rs");

        // Three touches, two of them at once, so the in-flight dedupe is
        // exercised as well as the permanence.
        lsp.touch(&file, true).await;
        tokio::join!(lsp.touch(&file, true), lsp.touch(&file, true));

        assert!(lsp.is_broken(&root, "pretend"), "the failure is remembered");
        let ran = std::fs::read_to_string(&attempts).unwrap_or_default();
        assert_eq!(
            ran.lines().count(),
            1,
            "a broken server is started exactly once a session, however often it is touched"
        );
        assert!(
            lsp.diagnostics().is_empty(),
            "and it contributes nothing, rather than failing anything"
        );
    }

    #[test]
    fn a_pair_nothing_has_tried_yet_is_not_broken() {
        let lsp = service(std::path::Path::new("/p"));

        assert!(!lsp.is_broken(std::path::Path::new("/p"), "rust"));
    }

    #[test]
    fn a_file_outside_the_project_is_not_this_sessions_business() {
        let lsp = service(std::path::Path::new("/p"));

        assert!(lsp.contains(std::path::Path::new("/p/src/main.rs")));
        assert!(!lsp.contains(std::path::Path::new("/elsewhere/main.rs")));
    }
}
