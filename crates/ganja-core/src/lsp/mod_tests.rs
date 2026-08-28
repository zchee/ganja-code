use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

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
            start: Position { line: 0, character: 0 },
            end: Position { line: 0, character: 1 },
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
    let entries =
        BTreeMap::from([("rust".to_owned(), disabled()), ("gopls".to_owned(), disabled())]);

    assert!(Lsp::new(Some(&LspConfig::Servers(entries)), std::path::Path::new("/p")).is_none());
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
    assert_eq!(resolve(std::path::Path::new("/p"), "src/main.rs"), PathBuf::from("/p/src/main.rs"));
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

    let own = appended.find(OWN_FILE).expect("the written file is reported");
    let first_other = appended.find(OTHER_FILES).expect("the others are reported");
    assert!(own < first_other, "the written file leads: {appended}");
    assert_eq!(
        appended.matches("LSP errors detected in other files").count(),
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
        diagnostics.insert(PathBuf::from(format!("/p/src/f{index}.rs")), vec![error("broken")]);
    }

    let appended = append("write", &written, &diagnostics);

    assert_eq!(appended.matches("LSP errors detected in other files").count(), MAX_PROJECT_FILES);
    assert_eq!(appended.matches(OWN_FILE).count(), 1);
}

#[test]
fn a_file_whose_only_diagnostics_are_warnings_adds_no_section() {
    let written = PathBuf::from("/p/src/main.rs");
    let warning =
        Diagnostic { severity: Some(DiagnosticSeverity::WARNING), ..error("unused import") };
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
        .annotate("read", &json!({ "filePath": "/p/src/main.rs" }), std::path::Path::new("/p"))
        .await;

    assert_eq!(appended, "", "a read never carries diagnostics");
}

#[tokio::test]
async fn a_tool_with_no_lsp_interest_is_never_annotated() {
    let lsp = service(std::path::Path::new("/p"));

    for tool in ["bash", "glob", "grep", "todowrite", "webfetch", "task"] {
        let appended = lsp
            .annotate(tool, &json!({ "file_path": "/p/src/main.rs" }), std::path::Path::new("/p"))
            .await;

        assert_eq!(appended, "", "{tool} appends nothing");
    }
}

#[tokio::test]
async fn a_call_with_no_file_argument_is_never_annotated() {
    let lsp = service(std::path::Path::new("/p"));

    let appended =
        lsp.annotate("edit", &json!({ "pattern": "*.rs" }), std::path::Path::new("/p")).await;

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
            format!("#!/bin/sh\necho attempt >> {}\nexit 1\n", attempts.display()),
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
