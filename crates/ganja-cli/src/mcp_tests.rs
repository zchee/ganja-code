#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use serde_json::json;
use toml_edit::{DocumentMut, Item};

use super::{AddArgs, Tier, check_name, document, entry, pairs, put, servers, shaped, validate};

/// The shape a test asks for, with every optional flag unset.
fn args(name: &str) -> AddArgs {
    AddArgs {
        name: name.to_owned(),
        global: false,
        force: false,
        url: None,
        header: Vec::new(),
        oauth: false,
        env: Vec::new(),
        cwd: None,
        timeout: None,
        output_limit: None,
        disabled: false,
        command: Vec::new(),
    }
}

/// A document with `name` added to it, printed — the whole write path bar the
/// disk.
fn added(text: &str, name: &str, asked: &AddArgs) -> String {
    let mut document = text.parse::<DocumentMut>().expect("the fixture is a TOML document");
    let built = entry(asked).expect("the entry is buildable");
    validate(name, &built).expect("the loader would read it back");
    let table = servers(&mut document, Path::new("ganja.toml")).expect("the table is reachable");
    put(table, name, shaped(name, &built).expect("the entry shapes"));

    document.to_string()
}

#[test]
fn a_local_entry_carries_its_command_and_nothing_it_was_not_given() {
    let mut asked = args("docs");
    asked.command = vec!["bun".to_owned(), "server.ts".to_owned()];

    let built = entry(&asked).expect("the entry is buildable");

    assert_eq!(built, json!({"type": "local", "command": ["bun", "server.ts"]}));
    validate("docs", &built).expect("the loader would read it back");
}

#[test]
fn a_local_entry_carries_the_cwd_and_environment_it_was_given() {
    let mut asked = args("docs");
    asked.command = vec!["bun".to_owned()];
    asked.cwd = Some("./tools".to_owned());
    asked.env = vec!["TOKEN=a=b".to_owned(), "MODE=quiet".to_owned()];
    asked.disabled = true;
    asked.timeout = Some(9_000);

    let built = entry(&asked).expect("the entry is buildable");

    assert_eq!(
        built,
        json!({
            "type": "local",
            "command": ["bun"],
            "cwd": "./tools",
            // The value keeps its own `=`: split on the first, never the last.
            "environment": {"TOKEN": "a=b", "MODE": "quiet"},
            "enabled": false,
            "timeout": 9_000,
        })
    );
    validate("docs", &built).expect("the loader would read it back");
}

#[test]
fn a_remote_entry_carries_its_url_and_headers() {
    let mut asked = args("hosted");
    asked.url = Some("https://mcp.example/api".to_owned());
    asked.header = vec!["Authorization=Bearer x".to_owned()];
    asked.output_limit = Some(4_096);

    let built = entry(&asked).expect("the entry is buildable");

    assert_eq!(
        built,
        json!({
            "type": "remote",
            "url": "https://mcp.example/api",
            "headers": {"Authorization": "Bearer x"},
            "output_limit": 4_096,
        })
    );
    validate("hosted", &built).expect("the loader would read it back");
}

/// The marker `ganja mcp login` gates on: without it, login refuses the
/// server by name — which is exactly what the field hit when `add` had no
/// way to write it.
#[test]
fn oauth_marks_a_remote_for_login_and_the_loader_reads_it_back() {
    let mut asked = args("hosted");
    asked.url = Some("https://mcp.example/api".to_owned());
    asked.oauth = true;

    let built = entry(&asked).expect("the entry is buildable");

    assert_eq!(
        built,
        json!({
            "type": "remote",
            "url": "https://mcp.example/api",
            "oauth": {},
        })
    );
    validate("hosted", &built).expect("the loader would read it back");
}

#[test]
fn a_plain_http_remote_is_refused_and_loopback_is_not() {
    let mut asked = args("hosted");
    asked.url = Some("http://mcp.example/api".to_owned());
    let built = entry(&asked).expect("the entry is buildable");
    let refusal = validate("hosted", &built).expect_err("plain http elsewhere is refused");
    assert!(refusal.to_string().contains("https"), "the refusal names the rule: {refusal}");
    assert!(
        !refusal.to_string().contains("mcp.example"),
        "the refusal never quotes the URL: {refusal}"
    );

    for allowed in [
        "http://127.0.0.1:9000/mcp",
        "http://localhost:9000/mcp",
        "http://[::1]:9000/mcp",
        "https://mcp.example/api",
    ] {
        asked.url = Some(allowed.to_owned());
        let built = entry(&asked).expect("the entry is buildable");
        validate("hosted", &built).unwrap_or_else(|error| panic!("{allowed} is allowed: {error}"));
    }

    // The bypasses a text match would take: a host that merely *contains*
    // an address belongs to whoever registered it.
    for refused in [
        "http://127.0.0.1.evil.example/mcp",
        "http://127.0.0.1@evil.example/mcp",
        "http://localhost.evil.example/mcp",
    ] {
        asked.url = Some(refused.to_owned());
        let built = entry(&asked).expect("the entry is buildable");
        validate("hosted", &built).expect_err(refused);
    }
}

#[test]
fn an_empty_command_and_a_zero_output_limit_are_both_refused() {
    let empty = entry(&args("docs")).expect("the entry is buildable");
    assert!(
        validate("docs", &empty)
            .expect_err("a server with no program is not a server")
            .to_string()
            .contains("empty command")
    );

    let mut asked = args("docs");
    asked.command = vec!["bun".to_owned()];
    asked.output_limit = Some(0);
    let built = entry(&asked).expect("the entry is buildable");
    assert!(
        validate("docs", &built)
            .expect_err("a budget of nothing refuses every result")
            .to_string()
            .contains("output_limit of 0")
    );
}

#[test]
fn a_zero_timeout_is_refused_by_the_config_type_itself() {
    let mut asked = args("docs");
    asked.command = vec!["bun".to_owned()];
    asked.timeout = Some(0);
    let built = entry(&asked).expect("the entry is buildable");

    let refusal = validate("docs", &built).expect_err("a request budget of nothing is refused");
    assert!(refusal.to_string().contains("docs"), "the refusal names the server: {refusal}");
}

#[test]
fn a_pair_without_an_equals_sign_is_refused_by_the_flag_that_took_it() {
    let refusal = pairs(&["Authorization".to_owned()], "--header")
        .expect_err("a word with no value is not a pair");

    assert!(refusal.to_string().contains("--header"), "{refusal}");
    assert!(pairs(&["=value".to_owned()], "--env").is_err(), "a value with no key names nothing");
}

#[test]
fn a_name_that_is_a_path_or_nothing_at_all_is_refused() {
    check_name("docs").expect("an ordinary name is a name");
    check_name("docs.v2").expect("a dot is not a separator");
    assert!(check_name("").is_err(), "an entry needs a name");
    assert!(check_name("   ").is_err(), "whitespace is not a name");
    assert!(check_name("a/b").is_err(), "a name is not a path");
    assert!(check_name("a\\b").is_err(), "a name is not a path");
}

#[test]
fn the_project_tier_writes_at_the_worktree_root() {
    let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = Tier::Project.directory(cwd).expect("a worktree always resolves");

    assert!(
        cwd.starts_with(&directory),
        "the target directory is at or above the working directory"
    );
}

#[test]
fn an_absent_file_reads_as_an_empty_document_and_a_broken_one_refuses() {
    let missing = Path::new("/nonexistent-ganja-mcp-test/ganja.toml");
    let empty = document(missing).expect("an absent file is nothing to merge");
    assert_eq!(empty.to_string(), "", "an absent file is an empty document, not an error");

    let directory = tempfile::tempdir().expect("a temporary directory is creatable");
    let path = directory.path().join(super::CONFIG_FILE);
    std::fs::write(&path, "theme = ").expect("the fixture is writable");
    assert!(document(&path).is_err(), "a file that does not parse is never treated as empty");
}

#[test]
fn the_table_is_created_empty_and_a_non_table_mcp_key_is_refused() {
    let path = Path::new("ganja.toml");
    let missing = Path::new("/nonexistent-ganja-mcp-test/ganja.toml");

    let mut document = document(missing).expect("an absent file is an empty document");
    let table = servers(&mut document, path).expect("an absent table is created");
    let mut asked = args("docs");
    asked.command = vec!["bun".to_owned()];
    let built = entry(&asked).expect("the entry is buildable");
    put(table, "docs", shaped("docs", &built).expect("the entry shapes"));
    assert_eq!(
        document.to_string(),
        "[mcp.docs]\ncommand = [\"bun\"]\ntype = \"local\"\n",
        "the entry lands under a table this created, and no empty `[mcp]` \
         header is written above it"
    );

    // Whatever this is, it is not this command's to throw away.
    let mut hostile =
        "mcp = [\"not\", \"a\", \"table\"]\n".parse::<DocumentMut>().expect("the fixture parses");
    assert!(servers(&mut hostile, path).is_err());
}

/// toml_edit quotes a key that needs it, which is the whole reason an entry is
/// serialized rather than composed: a name with a dot in it is two keys if it
/// is written bare, and `ganja mcp add` accepts one (`check_name` refuses only
/// path separators).
#[test]
fn a_name_that_needs_quoting_arrives_quoted() {
    let mut asked = args("docs.v2");
    asked.command = vec!["bun".to_owned()];

    let written = added("", "docs.v2", &asked);

    assert_eq!(
        written, "[mcp.\"docs.v2\"]\ncommand = [\"bun\"]\ntype = \"local\"\n",
        "the dot is inside the name rather than a path through two tables"
    );
    assert!(
        written
            .parse::<DocumentMut>()
            .expect("what this printed parses")
            .get("mcp")
            .and_then(Item::as_table_like)
            .is_some_and(|table| table.contains_key("docs.v2")),
        "and reading it back finds the name that was typed: {written}"
    );
}

/// The position and the comment above a replaced entry are the two things a
/// remove-and-append would lose, and both live on the table this writes into
/// the slot the old one held.
#[test]
fn a_replaced_entry_keeps_its_place_in_the_file_and_the_comment_above_it() {
    let before = "\
# Servers.

# Reads the design tokens. Do not point this at staging.
[mcp.tokens]
command = [\"cat\", \"tokens.json\"]
type = \"local\"

[mcp.notes]
command = [\"cat\"]
type = \"local\"

[tui]
notifications = true
";
    let mut asked = args("tokens");
    asked.command = vec!["cat".to_owned(), "moved.json".to_owned()];

    let after = added(before, "tokens", &asked);

    assert_eq!(
        after,
        before.replace("tokens.json", "moved.json"),
        "only the one value moved: the comment, the blank lines and the two \
         tables after it are where they were"
    );
}

#[test]
fn an_entry_the_file_spelled_inline_is_replaced_inline() {
    let before = "[mcp]\n# the one server\ntokens = { command = [\"cat\"], type = \"local\" }\n";
    let mut asked = args("tokens");
    asked.command = vec!["cat".to_owned(), "again.json".to_owned()];

    let after = added(before, "tokens", &asked);

    assert_eq!(
        after,
        "[mcp]\n# the one server\ntokens = { command = [\"cat\", \"again.json\"], type = \"local\" }\n",
        "promoting it to a `[mcp.tokens]` header would move it out of the \
         table somebody wrote it in"
    );
}

/// `RLIMIT_FSIZE` at zero, so that writing any byte to any file in this
/// process fails — with `SIGXFSZ` ignored, because its default
/// disposition kills the process rather than letting the write return
/// `EFBIG`. It is the cheapest real write failure there is: no fixture
/// filesystem, no injected error type, no seam in production code that
/// exists only for a test.
///
/// Both settings are process-wide, and nextest gives every test its own
/// process. The restore on drop is what keeps that from being the only
/// thing holding the line — under a plain `cargo test`, where a binary's
/// tests share one process across threads, the window is at least closed
/// rather than left open for the rest of the run.
#[cfg(unix)]
struct NoFileMayGrow {
    limit: libc::rlimit,
    signal: libc::sighandler_t,
}

#[cfg(unix)]
impl NoFileMayGrow {
    fn take() -> Self {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: each call is handed a pointer to a live local of the
        // type it documents, and nothing outlives this frame.
        unsafe {
            assert_eq!(
                libc::getrlimit(libc::RLIMIT_FSIZE, &raw mut limit),
                0,
                "the current file-size limit is readable"
            );
            let signal = libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let forbidden = libc::rlimit { rlim_cur: 0, rlim_max: limit.rlim_max };
            assert_eq!(
                libc::setrlimit(libc::RLIMIT_FSIZE, &raw const forbidden),
                0,
                "lowering the file-size limit is always permitted"
            );

            Self { limit, signal }
        }
    }
}

#[cfg(unix)]
impl Drop for NoFileMayGrow {
    fn drop(&mut self) {
        // SAFETY: the values restored are the ones the constructor read
        // out of this same process.
        unsafe {
            libc::setrlimit(libc::RLIMIT_FSIZE, &raw const self.limit);
            libc::signal(libc::SIGXFSZ, self.signal);
        }
    }
}

#[cfg(unix)]
#[test]
fn a_write_that_fails_leaves_no_staged_file_beside_the_config() {
    let directory = tempfile::tempdir().expect("a temporary directory is creatable");
    let path = directory.path().join(super::CONFIG_FILE);
    let original = "# a note\ntheme = \"ganja\"\n";
    std::fs::write(&path, original).expect("the fixture is writable");
    let parsed = document(&path).expect("the fixture parses");

    let refused = {
        let _forbidden = NoFileMayGrow::take();

        super::write(&path, &parsed).expect_err("no byte may be written")
    };

    // Asserted before the message, because it is what the name promises
    // and what the old staging loop got wrong.
    let left: Vec<_> = std::fs::read_dir(directory.path())
        .expect("the directory is readable")
        .map(|entry| entry.expect("the entry is readable").file_name())
        .collect();
    assert_eq!(
        left,
        vec![std::ffi::OsString::from(super::CONFIG_FILE)],
        "a failed write is litter-free: only the config it could not edit is left"
    );
    assert!(
        format!("{refused:#}").contains(&path.display().to_string()),
        "the failure names the file somebody asked to edit: {refused:#}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("the config is readable"),
        original,
        "and the config it could not edit is the bytes it was"
    );
}

#[cfg(unix)]
#[test]
fn a_fresh_config_is_private_to_its_owner() {
    let directory = tempfile::tempdir().expect("a temporary directory is creatable");
    let path = directory.path().join(super::CONFIG_FILE);
    let parsed = document(&path).expect("an absent file is an empty document");

    super::write(&path, &parsed).expect("the config is writable");

    assert_eq!(
        std::fs::symlink_metadata(&path).expect("the config has metadata").permissions().mode()
            & 0o777,
        0o600,
        "a newly staged config carries no group or other access"
    );
}

#[cfg(unix)]
#[test]
fn a_config_rewrite_keeps_the_documents_existing_mode() {
    let directory = tempfile::tempdir().expect("a temporary directory is creatable");
    let path = directory.path().join(super::CONFIG_FILE);
    std::fs::write(&path, "theme = \"ganja\"\n").expect("the fixture is writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
        .expect("the fixture mode is settable");
    let parsed = document(&path).expect("the fixture parses");

    super::write(&path, &parsed).expect("the config is rewritable");

    assert_eq!(
        std::fs::symlink_metadata(&path).expect("the config has metadata").permissions().mode()
            & 0o777,
        0o640,
        "rewriting a regular file preserves the access its owner chose"
    );
}

/// `entry` never builds one this large, but the serializer is what stands
/// between a config's own numbers and an `f64` round trip, and 2^53 + 1 is
/// where that round trip starts lying.
#[test]
fn a_number_reaches_the_document_as_the_digits_it_arrived_as() {
    let shaped = shaped("docs", &json!({"n": 9_007_199_254_740_993_u64}))
        .expect("a JSON number shapes into TOML");

    assert_eq!(shaped.to_string(), "n = 9007199254740993\n");
}
