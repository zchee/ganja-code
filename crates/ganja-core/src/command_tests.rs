use std::{path::Path, sync::Arc};

use super::{
    Definition, INIT, INIT_TEMPLATE, MAX_COMMAND_FILE_BYTES, PATH_PLACEHOLDER, Registry,
    file_commands, fill_template, mentions, shell_substitutions, split_range, tokenize,
};
use crate::tool::{Credentials, FileTimes, ToolCtx};

#[test]
fn the_init_template_is_upstreams_with_ganjas_identity_and_the_worktree_filled_in() {
    let registry = Registry::builtin(Path::new("/repo/ganja"));
    let init = registry.get(INIT).expect("init is builtin");

    assert!(
        INIT_TEMPLATE.contains(PATH_PLACEHOLDER),
        "the ported file should still carry the placeholder"
    );
    assert!(
        !init.template.contains(PATH_PLACEHOLDER),
        "and the resolved template should not: {}",
        init.template
    );
    assert!(init.template.contains("/repo/ganja"));
    assert!(
        init.template
            .starts_with("Create or update `AGENTS.md` for this repository."),
        "the template is upstream's prose, identity substituted: {}",
        init.template
    );
    assert_eq!(init.description.as_deref(), Some("guided AGENTS.md setup"));
}

#[test]
fn a_template_fills_its_placeholders_the_way_upstream_fills_them() {
    let cases = [
        // (template, arguments, expected)
        ("fix $1", "auth", "fix auth"),
        // The highest-numbered placeholder is greedy: `$2` takes the rest.
        (
            "fix $1 because $2",
            "auth it broke again",
            "fix auth because it broke again",
        ),
        // …even when it is not the last one written.
        ("$2 — fix $1", "auth it broke", "it broke — fix auth"),
        // A position past the last token is empty rather than an error.
        ("fix $1 and $2", "auth", "fix auth and"),
        ("focus: $ARGUMENTS", "the tests", "focus: the tests"),
        // Raw and untokenized: quotes survive `$ARGUMENTS`.
        (
            r#"focus: $ARGUMENTS"#,
            r#""two words""#,
            r#"focus: "two words""#,
        ),
        // Neither placeholder, so the arguments are appended.
        (
            "review the diff",
            "only src/",
            "review the diff\n\nonly src/",
        ),
        // Neither placeholder and no arguments: nothing is appended.
        ("review the diff", "", "review the diff"),
        // A quoted span is one token.
        (
            r#"say $1 to $2"#,
            r#""good morning" world"#,
            "say good morning to world",
        ),
        // A `$` that names nothing is left alone.
        ("costs $5.00 and $x", "", "costs .00 and $x"),
        // Trimmed, as upstream trims.
        ("  spaced  ", "", "spaced"),
    ];

    for (template, arguments, expected) in cases {
        assert_eq!(
            fill_template(template, arguments),
            expected,
            "expanding {template:?} with {arguments:?}"
        );
    }
}

#[test]
fn shell_substitutions_match_complete_nonempty_commands_in_written_order() {
    let cases: &[(&str, &[&str])] = &[
        (r#"!`echo hi`"#, &["echo hi"]),
        (r#"!`first` between !`second`"#, &["first", "second"]),
        // An empty command does not satisfy the one-or-more grammar.
        (r#"!``"#, &[]),
        ("`", &[]),
        ("!", &[]),
        (r#"!`without a close"#, &[]),
        // The first backtick closes the match; it cannot be command text.
        (r#"!`echo `tail`"#, &["echo "]),
    ];

    for (text, expected) in cases {
        let matches = shell_substitutions(text);
        let commands = matches
            .iter()
            .map(|substitution| substitution.command.as_str())
            .collect::<Vec<_>>();
        assert_eq!(commands, *expected, "scanning {text:?}");
    }
}

#[test]
fn arguments_tokenize_with_quoted_spans_kept_whole() {
    let cases = [
        ("", Vec::new()),
        ("one two", vec!["one", "two"]),
        (r#""two words" three"#, vec!["two words", "three"]),
        (r#"'single quoted' rest"#, vec!["single quoted", "rest"]),
        // An unterminated quote is one token running to the end.
        (r#""unterminated rest"#, vec!["unterminated rest"]),
        ("  padded   out  ", vec!["padded", "out"]),
    ];

    for (arguments, expected) in cases {
        assert_eq!(tokenize(arguments), expected, "tokenizing {arguments:?}");
    }
}

#[test]
fn mentions_open_only_at_a_word_boundary_and_require_a_path() {
    let cases: &[(&str, &[&str])] = &[
        ("@a.rs", &["a.rs"]),
        ("look at @a.rs\nthen @b.rs", &["a.rs", "b.rs"]),
        ("mail me@example.com", &[]),
        ("an @ on its own", &[]),
        ("@#5", &[]),
    ];

    for (text, expected) in cases {
        let found = mentions(text);
        let paths = found
            .iter()
            .map(|mention| mention.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, *expected, "scanning {text:?}");
    }
}

#[test]
fn a_range_suffix_is_split_only_when_it_parses() {
    let cases = [
        ("a.rs", ("a.rs", None, None)),
        ("a.rs#5", ("a.rs", Some(5), None)),
        ("a.rs#5-9", ("a.rs", Some(5), Some(9))),
        // An empty end is a start alone.
        ("a.rs#5-", ("a.rs", Some(5), None)),
        // A reversed or flat range keeps its start only.
        ("a.rs#20-10", ("a.rs", Some(20), None)),
        ("a.rs#5-5", ("a.rs", Some(5), None)),
        // Line zero is what was typed; the read clamps it, not the scan.
        ("a.rs#0", ("a.rs", Some(0), None)),
        // The split is at the *last* `#`, so a path may contain one.
        ("we#ird.rs#5-9", ("we#ird.rs", Some(5), Some(9))),
        // Tails outside the grammar stay part of the path: `#` is a
        // character a file name may contain.
        ("notes#TODO", ("notes#TODO", None, None)),
        ("a.rs#", ("a.rs#", None, None)),
        ("a.rs#5-9-12", ("a.rs#5-9-12", None, None)),
        ("a.rs#-5", ("a.rs#-5", None, None)),
        ("a.rs#+5", ("a.rs#+5", None, None)),
        // A line number past `u32` is not a line number.
        (
            "a.rs#99999999999999999999",
            ("a.rs#99999999999999999999", None, None),
        ),
    ];

    for (mentioned, (path, start, end)) in cases {
        assert_eq!(split_range(mentioned), (path, start, end), "{mentioned:?}");
    }
}

#[test]
fn mentions_dedupe_by_path_and_range_together() {
    assert_eq!(mentions("@a.rs#5-9 and again @a.rs#5-9").len(), 1);

    let mentions = mentions("@a.rs#5-9 then @a.rs#30-40 then @a.rs");
    assert_eq!(mentions.len(), 3, "{mentions:?}");
}

#[tokio::test]
async fn template_expansion_runs_shells_and_attaches_only_files_that_exist() {
    let root = tempfile::TempDir::new().expect("a temporary project is creatable");
    std::fs::write(root.path().join("present.md"), "present")
        .expect("the mentioned fixture is writable");
    let ctx = ToolCtx {
        cwd: root.path().to_owned(),
        cancel: tokio_util::sync::CancellationToken::new(),
        call_id: String::new(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: None,
        postbox: None,
        ask: None,
        switch: None,
        jobs: None,
    };
    let command = |template: &str| Definition {
        name: "fixture".to_owned(),
        description: None,
        template: template.to_owned(),
        agent: None,
        model: None,
        argument_hint: None,
        source: None,
    };

    let echoed = command(r#"!`echo hi`"#).expand("", &ctx).await;
    assert_eq!(echoed.prompt, "hi");

    let failed = command(r#"!`printf still-here; exit 7`"#)
        .expand("", &ctx)
        .await;
    assert_eq!(
        failed.prompt, "still-here",
        "a non-zero exit still substitutes what the command wrote"
    );

    let attached = command("read @present.md and ask @alice")
        .expand("", &ctx)
        .await;
    assert_eq!(attached.prompt, "read @present.md and ask @alice");
    assert_eq!(
        attached.mentions,
        vec![crate::protocol::Mention {
            path: "present.md".to_owned(),
            start: None,
            end: None,
        }],
        "only the path that exists becomes a file part"
    );
}

/// A commands directory a test owns outright, so nothing here reads — or
/// depends on the absence of — whatever the machine running the suite keeps
/// in its own config home. The tier that *does* resolve that home is
/// exercised in `tests/command_files.rs`, which redirects it.
fn commands_dir(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("a temporary directory is creatable");
    for (name, contents) in files {
        std::fs::write(dir.path().join(name), contents).expect("the fixture is writable");
    }

    dir
}

#[test]
fn a_command_file_is_its_frontmatter_and_its_body() {
    let dir = commands_dir(&[(
        "review.md",
        b"---\n\
              description: review the diff\n\
              agent: plan\n\
              model: anthropic/claude-sonnet-4-5\n\
              argument-hint: <path>\n\
              ---\n\
              review $ARGUMENTS\n",
    )]);

    let commands = file_commands(dir.path());
    assert_eq!(commands.len(), 1, "{commands:?}");
    let review = &commands[0];
    assert_eq!(review.name, "review", "the name is the file's stem");
    assert_eq!(
        review.description.as_deref(),
        Some("review the diff — <path>"),
        "the hint rides the line a palette already shows"
    );
    assert_eq!(review.agent.as_deref(), Some("plan"));
    assert_eq!(review.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
    assert_eq!(
        review.template, "review $ARGUMENTS\n",
        "the body is the template verbatim"
    );
    assert_eq!(
        fill_template(&review.template, "src/"),
        "review src/",
        "so the expansion a config command gets is the expansion this gets"
    );
}

#[test]
fn a_file_with_no_frontmatter_is_all_template() {
    let dir = commands_dir(&[("hello.md", b"say hello to $1\n")]);

    let commands = file_commands(dir.path());
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert_eq!(commands[0].name, "hello");
    assert_eq!(commands[0].description, None);
    assert_eq!(commands[0].template, "say hello to $1\n");
}

#[test]
fn frontmatter_tolerates_what_it_does_not_understand() {
    let dir = commands_dir(&[
        (
            "kept.md",
            b"---\n\
                  # a comment somebody left\n\
                  allowed-tools: Bash(git status:*)\n\
                  not a key-value line at all\n\
                  Description: \"quoted, and capitalised\"\n\
                  agent:\n\
                  ---\n\
                  body\n",
        ),
        // A hint with no description of its own still says something.
        ("hint.md", b"---\nargument-hint: <issue>\n---\nfix it\n"),
    ]);

    let commands = file_commands(dir.path());
    let described: Vec<(&str, Option<&str>)> = commands
        .iter()
        .map(|command| (command.name.as_str(), command.description.as_deref()))
        .collect();
    // D518: the hint also travels on its own slot for the composer.
    let hinted: Vec<Option<&str>> = commands
        .iter()
        .map(|command| command.argument_hint.as_deref())
        .collect();
    assert!(
        hinted.contains(&Some("<issue>")),
        "the hint slot should carry the file's argument-hint: {hinted:?}"
    );
    assert_eq!(
        described,
        vec![
            ("hint", Some("<issue>")),
            ("kept", Some("quoted, and capitalised")),
        ],
        "unknown keys, comments and stray lines are skipped, not fatal"
    );
    let kept = commands
        .iter()
        .find(|command| command.name == "kept")
        .expect("the tolerated file is a command");
    assert_eq!(
        kept.agent, None,
        "a key with nothing after it says nothing at all"
    );
    assert_eq!(kept.template, "body\n");
}

#[test]
fn a_file_this_build_will_not_read_is_skipped_rather_than_half_parsed() {
    let oversized = vec![b'x'; usize::try_from(MAX_COMMAND_FILE_BYTES).expect("a usize") + 1];
    let dir = commands_dir(&[
        // A block that opens and never closes: the header is not a prompt.
        (
            "unterminated.md",
            b"---\ndescription: half a header\nand then a body\n",
        ),
        ("binary.md", &[0xff, 0xfe, b'n', 0x00, b'o']),
        ("huge.md", &oversized),
        // Not Markdown, so not meant for this directory.
        ("notes.txt", b"just notes"),
        ("good.md", b"this one is fine\n"),
    ]);
    std::fs::create_dir(dir.path().join("nested")).expect("a subdirectory is creatable");
    std::fs::write(dir.path().join("nested").join("deep.md"), b"not read yet")
        .expect("the nested fixture is writable");

    let commands = file_commands(dir.path());
    let names: Vec<&str> = commands
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["good"],
        "every hostile file is absent from the roster: {commands:?}"
    );
}

#[test]
fn a_missing_commands_directory_is_the_common_case_and_not_an_error() {
    let dir = tempfile::TempDir::new().expect("a temporary directory is creatable");

    assert!(file_commands(&dir.path().join("commands")).is_empty());
}

#[tokio::test]
async fn a_file_command_expands_through_the_one_expansion_path() {
    let root = tempfile::TempDir::new().expect("a temporary project is creatable");
    std::fs::write(root.path().join("present.md"), "present")
        .expect("the mentioned fixture is writable");
    let dir = commands_dir(&[(
        "brief.md",
        b"---\ndescription: brief me\n---\n!`printf hi` about $ARGUMENTS beside @present.md\n",
    )]);
    let ctx = ToolCtx {
        cwd: root.path().to_owned(),
        cancel: tokio_util::sync::CancellationToken::new(),
        call_id: String::new(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: None,
        postbox: None,
        ask: None,
        switch: None,
        jobs: None,
    };

    let commands = file_commands(dir.path());
    let expanded = commands[0].expand("the port", &ctx).await;

    assert_eq!(expanded.prompt, "hi about the port beside @present.md");
    assert_eq!(
        expanded.mentions,
        vec![crate::protocol::Mention {
            path: "present.md".to_owned(),
            start: None,
            end: None,
        }],
        "a file command attaches what a config command's template would"
    );
}
