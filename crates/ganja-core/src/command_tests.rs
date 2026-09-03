use std::path::Path;
use std::sync::Arc;

use super::{
    Definition, ESCAPE_TAIL, INIT, INIT_TEMPLATE, MAX_COMMAND_FILE_BYTES, MAX_SPEC_MEMBERS,
    Misdirected, NOTHING_NAMED, PATH_PLACEHOLDER, Registry, RosterAnswer, TEAM, TeamInvocation,
    TeamSpecError, file_commands, fill_template, mentions, misdirected, parse_team, render_members,
    shell_substitutions, split_range, tokenize,
};
use crate::teammate::{BACKENDS, UnknownBackend, backend_name};
use crate::tool::{Credentials, FileTimes, ToolCtx};

/// The session every expansion here runs under. Carries a `$` and a brace so
/// that a substitution which ever became pattern-matching rather than a plain
/// replace would show up as a mangled prompt rather than as nothing.
const SESSION: &str = "session-${odd}-01";

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
        init.template.starts_with("Create or update `AGENTS.md` for this repository."),
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
        ("fix $1 because $2", "auth it broke again", "fix auth because it broke again"),
        // …even when it is not the last one written.
        ("$2 — fix $1", "auth it broke", "it broke — fix auth"),
        // A position past the last token is empty rather than an error.
        ("fix $1 and $2", "auth", "fix auth and"),
        ("focus: $ARGUMENTS", "the tests", "focus: the tests"),
        // Raw and untokenized: quotes survive `$ARGUMENTS`.
        (r#"focus: $ARGUMENTS"#, r#""two words""#, r#"focus: "two words""#),
        // Neither placeholder, so the arguments are appended.
        ("review the diff", "only src/", "review the diff\n\nonly src/"),
        // Neither placeholder and no arguments: nothing is appended.
        ("review the diff", "", "review the diff"),
        // A quoted span is one token.
        (r#"say $1 to $2"#, r#""good morning" world"#, "say good morning to world"),
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
        let commands =
            matches.iter().map(|substitution| substitution.command.as_str()).collect::<Vec<_>>();
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
        let paths = found.iter().map(|mention| mention.path.as_str()).collect::<Vec<_>>();
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
        ("a.rs#99999999999999999999", ("a.rs#99999999999999999999", None, None)),
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
        tasks: None,
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
        builtin: false,
    };

    let echoed = command(r#"!`echo hi`"#)
        .expand("", SESSION, &ctx)
        .await
        .expect("an ordinary command expands");
    assert_eq!(echoed.prompt, "hi");

    let failed = command(r#"!`printf still-here; exit 7`"#)
        .expand("", SESSION, &ctx)
        .await
        .expect("expands");
    assert_eq!(
        failed.prompt, "still-here",
        "a non-zero exit still substitutes what the command wrote"
    );

    let attached = command("read @present.md and ask @alice")
        .expand("", SESSION, &ctx)
        .await
        .expect("expands");
    assert_eq!(attached.prompt, "read @present.md and ask @alice");
    assert_eq!(
        attached.mentions,
        vec![crate::protocol::Mention { path: "present.md".to_owned(), start: None, end: None }],
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
    assert_eq!(review.template, "review $ARGUMENTS\n", "the body is the template verbatim");
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
    let hinted: Vec<Option<&str>> =
        commands.iter().map(|command| command.argument_hint.as_deref()).collect();
    assert!(
        hinted.contains(&Some("<issue>")),
        "the hint slot should carry the file's argument-hint: {hinted:?}"
    );
    assert_eq!(
        described,
        vec![("hint", Some("<issue>")), ("kept", Some("quoted, and capitalised")),],
        "unknown keys, comments and stray lines are skipped, not fatal"
    );
    let kept = commands
        .iter()
        .find(|command| command.name == "kept")
        .expect("the tolerated file is a command");
    assert_eq!(kept.agent, None, "a key with nothing after it says nothing at all");
    assert_eq!(kept.template, "body\n");
}

#[test]
fn a_file_this_build_will_not_read_is_skipped_rather_than_half_parsed() {
    let oversized = vec![b'x'; usize::try_from(MAX_COMMAND_FILE_BYTES).expect("a usize") + 1];
    let dir = commands_dir(&[
        // A block that opens and never closes: the header is not a prompt.
        ("unterminated.md", b"---\ndescription: half a header\nand then a body\n"),
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
    let names: Vec<&str> = commands.iter().map(|command| command.name.as_str()).collect();
    assert_eq!(names, vec!["good"], "every hostile file is absent from the roster: {commands:?}");
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
        tasks: None,
        ask: None,
        switch: None,
        jobs: None,
    };

    let commands = file_commands(dir.path());
    let expanded =
        commands[0].expand("the port", SESSION, &ctx).await.expect("a file command expands");

    assert_eq!(expanded.prompt, "hi about the port beside @present.md");
    assert_eq!(
        expanded.mentions,
        vec![crate::protocol::Mention { path: "present.md".to_owned(), start: None, end: None }],
        "a file command attaches what a config command's template would"
    );
}

/// **Bead 2m46.** `/teammate`'s three subcommands are refused where `/team`
/// would have expanded, so a line typed at the command D544 renamed costs no
/// turn and no model round trip — and the refusal hands back the line that was
/// meant rather than describing it.
#[tokio::test]
async fn the_roster_subcommands_are_refused_by_team_with_the_line_that_was_meant() {
    let root = tempfile::TempDir::new().expect("a temporary project is creatable");
    let ctx = expansion_ctx(root.path());
    let team = Registry::builtin(root.path()).get(TEAM).expect("team is builtin").clone();

    for (typed, meant) in [
        ("spawn w1 --backend ganja", "/teammate spawn w1 --backend ganja"),
        ("shutdown w2", "/teammate shutdown w2"),
        ("shutdown", "/teammate shutdown"),
        ("list", "/teammate list"),
    ] {
        assert_eq!(
            team.expand(typed, SESSION, &ctx).await,
            Err(Misdirected { meant: meant.to_owned() }),
            "`/team {typed}` is a roster line, and nothing about it needs a model",
        );
    }
}

/// The refusal is conservative on purpose: `list` takes no arguments of its
/// own, so a `list` with a tail is a task somebody wants done, and a first word
/// that merely starts with one of the three is not one of them.
#[tokio::test]
async fn a_task_that_only_reads_like_a_roster_line_still_expands() {
    let root = tempfile::TempDir::new().expect("a temporary project is creatable");
    let ctx = expansion_ctx(root.path());
    let team = Registry::builtin(root.path()).get(TEAM).expect("team is builtin").clone();

    for typed in ["list the config keys", "3 spawn-free task text", "port the loader", ""] {
        let expanded = team
            .expand(typed, SESSION, &ctx)
            .await
            .unwrap_or_else(|refused| panic!("`/team {typed}` is a task: {refused:?}"));
        assert!(
            expanded.prompt.contains(typed),
            "and what it expands to carries what was typed: {}",
            expanded.prompt,
        );
        // `contains("")` holds of anything, so the empty row is proved an
        // expansion by the template's own opening sentence instead.
        assert!(
            expanded.prompt.starts_with("You are running a team pipeline."),
            "and it is the template that was expanded, not the line echoed: {}",
            expanded.prompt,
        );
    }
}

/// The context an expansion runs in, at its dullest: a project directory and
/// nothing lent.
fn expansion_ctx(cwd: &Path) -> ToolCtx {
    ToolCtx {
        cwd: cwd.to_owned(),
        cancel: tokio_util::sync::CancellationToken::new(),
        call_id: String::new(),
        files: Arc::new(FileTimes::default()),
        credentials: Credentials::Unguarded,
        spawn: None,
        postbox: None,
        tasks: None,
        ask: None,
        switch: None,
        jobs: None,
    }
}

/// The roster the grammar table is judged against: three spawnable agents, two
/// primaries, and nothing else known.
///
/// A closure rather than a built [`crate::agent::Registry`] because that is the
/// whole point of the injected predicate — the grammar is a pure function, so a
/// table of thirty-three rows costs no fixture home and no engine.
fn fixture_roster(name: &str) -> RosterAnswer {
    match name {
        "critic" | "executor" | "explore" => RosterAnswer::Spawnable,
        "build" | "plan" => RosterAnswer::Primary,
        _ => RosterAnswer::Unknown,
    }
}

/// A session that was handed no agent registry at all (**R8**): every name is
/// unknown, so the bare-name arm can never fire.
fn no_roster(_: &str) -> RosterAnswer {
    RosterAnswer::Unknown
}

/// What one row of the plan's grammar table expects.
#[derive(Debug)]
enum Expect {
    /// The parse succeeds with these members — each spelled
    /// `name agent backend`, with `-` for a row that named no agent — this
    /// standing surface, and this task text.
    Parsed(&'static [&'static str], Option<&'static str>, &'static str),
    /// The parse refuses, spelled the way the plan's table spells the variant.
    Refused(&'static str),
    /// The line never reaches the parse: bead 2m46's door answers it first.
    Misdirected,
}

/// One member per line, in the spelling the table's members column uses.
fn spelled(invocation: &TeamInvocation) -> Vec<String> {
    invocation
        .members
        .iter()
        .map(|member| {
            format!(
                "{} {} {}",
                member.name,
                member.agent.as_deref().unwrap_or("-"),
                backend_name(member.backend)
            )
        })
        .collect()
}

/// A refusal in the plan's own `Variant{payload}` notation, so a table row and
/// the document it came from read the same.
fn refusal(error: &TeamSpecError) -> String {
    match error {
        TeamSpecError::UnknownAgent { name } => format!("UnknownAgent{{{name}}}"),
        TeamSpecError::NotSpawnable { name } => format!("NotSpawnable{{{name}}}"),
        TeamSpecError::UnknownBackend(unknown) => format!("UnknownBackend{{{}}}", unknown.value),
        TeamSpecError::BackendWithAt { segment } => format!("BackendWithAt{{{segment}}}"),
        TeamSpecError::FewerMembersThanSurfaces { members, surfaces } => {
            format!("FewerMembersThanSurfaces{{{members},{surfaces}}}")
        }
        TeamSpecError::ZeroCount { segment } => format!("ZeroCount{{{segment}}}"),
        TeamSpecError::TooMany { asked, cap } => format!("TooMany{{{asked},{cap}}}"),
        TeamSpecError::Malformed { token } => format!("Malformed{{{token}}}"),
        TeamSpecError::MissingBackendValue => "MissingBackendValue".to_owned(),
        TeamSpecError::RepeatedBackend => "RepeatedBackend".to_owned(),
    }
}

/// **D549's grammar table, one row per line**, in the plan's own order
/// (`.omc/plans/2026-09-03-team-segment-grammar.md`, "The grammar table").
///
/// The members column here is strictly the **parse result**; the one row the
/// plan marks `†` — a bare count with no task — differs only in how
/// [`render_members`] draws it, which
/// `render_members_says_which_of_the_three_cases_it_is` pins instead.
///
/// Four rows past the thirty-three are the W1 review's, marked where they
/// start. They are kept in this test rather than in one of their own because
/// they are the same kind of claim, and kept **after** the plan's own rows so
/// that the correspondence with the document stays readable line by line.
#[test]
fn the_head_token_is_a_spec_exactly_when_the_grammar_says_so() {
    let rows: &[(&str, Expect)] = &[
        // Nothing that looks like a spec: the whole line is the task.
        ("port the loader", Expect::Parsed(&[], None, "port the loader")),
        ("explore the loader", Expect::Parsed(&[], None, "explore the loader")),
        ("critic review this", Expect::Parsed(&[], None, "critic review this")),
        ("fix,the,bug", Expect::Parsed(&[], None, "fix,the,bug")),
        // A bare count names members without naming an agent.
        (
            "3 port the loader",
            Expect::Parsed(
                &["worker-1 - ganja", "worker-2 - ganja", "worker-3 - ganja"],
                None,
                "port the loader",
            ),
        ),
        (
            "3",
            Expect::Parsed(&["worker-1 - ganja", "worker-2 - ganja", "worker-3 - ganja"], None, ""),
        ),
        (
            "3:critic port the loader",
            Expect::Parsed(
                &["critic-1 critic ganja", "critic-2 critic ganja", "critic-3 critic ganja"],
                None,
                "port the loader",
            ),
        ),
        (
            "3:critic --backend claude x",
            Expect::Parsed(
                &["critic-1 critic claude", "critic-2 critic claude", "critic-3 critic claude"],
                Some("claude"),
                "x",
            ),
        ),
        // The bare-name arm: a spawnable name whose next raw token is the flag.
        (
            "critic --backend claude x",
            Expect::Parsed(&["critic-1 critic claude"], Some("claude"), "x"),
        ),
        (
            "critic@claude,critic@codex,critic@grok x",
            Expect::Parsed(
                &["critic-1 critic claude", "critic-2 critic codex", "critic-3 critic grok"],
                None,
                "x",
            ),
        ),
        (
            "2:critic@claude,2:critic@codex,3:critic@grok x",
            Expect::Parsed(
                &[
                    "critic-1 critic claude",
                    "critic-2 critic claude",
                    "critic-3 critic codex",
                    "critic-4 critic codex",
                    "critic-5 critic grok",
                    "critic-6 critic grok",
                    "critic-7 critic grok",
                ],
                None,
                "x",
            ),
        ),
        (
            "critic,critic,critic x",
            Expect::Parsed(
                &["critic-1 critic ganja", "critic-2 critic ganja", "critic-3 critic ganja"],
                None,
                "x",
            ),
        ),
        (
            "critic,critic,critic --backend claude x",
            Expect::Parsed(
                &["critic-1 critic claude", "critic-2 critic claude", "critic-3 critic claude"],
                Some("claude"),
                "x",
            ),
        ),
        // No spec at all: one surface stands, a list of them makes members.
        (
            "--backend claude,codex x",
            Expect::Parsed(&["worker-1 - claude", "worker-2 - codex"], None, "x"),
        ),
        ("--backend=grok x", Expect::Parsed(&[], Some("grok"), "x")),
        (
            "--backend codex read the wire and report",
            Expect::Parsed(&[], Some("codex"), "read the wire and report"),
        ),
        (
            "port the loader --backend claude",
            Expect::Parsed(&[], Some("claude"), "port the loader"),
        ),
        (
            "3:critic --backend claude,codex x",
            Expect::Parsed(
                &["critic-1 critic claude", "critic-2 critic codex", "critic-3 critic claude"],
                None,
                "x",
            ),
        ),
        (
            "--backend claude 3:critic fix it",
            Expect::Parsed(
                &["critic-1 critic claude", "critic-2 critic claude", "critic-3 critic claude"],
                Some("claude"),
                "fix it",
            ),
        ),
        // Every refusal row.
        (
            "2:critic --backend claude,codex,grok x",
            Expect::Refused("FewerMembersThanSurfaces{2,3}"),
        ),
        ("2:critic@claude --backend codex x", Expect::Refused("BackendWithAt{2:critic@claude}")),
        ("0:critic x", Expect::Refused("ZeroCount{0:critic}")),
        ("17:critic x", Expect::Refused("TooMany{17,16}")),
        ("99999999999999999999:critic x", Expect::Refused("TooMany{99999999999999999999,16}")),
        ("3:build x", Expect::Refused("NotSpawnable{build}")),
        ("critic@nope x", Expect::Refused("UnknownBackend{nope}")),
        ("nosuch@claude x", Expect::Refused("UnknownAgent{nosuch}")),
        ("TODO:fix the parser", Expect::Refused("Malformed{TODO:fix}")),
        ("fix the --backend flag docs", Expect::Refused("UnknownBackend{flag}")),
        ("3:critic --backend x --backend y z", Expect::Refused("RepeatedBackend")),
        ("3:critic --backend", Expect::Refused("MissingBackendValue")),
        // Bead 2m46's door, which runs before any of this.
        ("spawn w1 --backend ganja", Expect::Misdirected),
        (
            "2:critic list",
            Expect::Parsed(&["critic-1 critic ganja", "critic-2 critic ganja"], None, "list"),
        ),
        // ── Past the plan's table: rows the W1 review found unpinned. The
        // grammar already decided every one of them; what was not pinned was a
        // refusal's *payload* or an arm's reachability.
        //
        // M1: a piece that cannot name itself is refused under the head token,
        // never under the empty string it split into — and this is PM-1's own
        // class of input, prose opening with a roster name and a comma.
        ("critic, then fix it", Expect::Refused("Malformed{critic,}")),
        // M2 (Dv-1): both arms ask one question, so a primary followed by this
        // command's own flag is the spec the person meant and is refused by
        // name, exactly as `build,build x` already was.
        ("build --backend claude x", Expect::Refused("NotSpawnable{build}")),
        // M4: the same argument as M1, in the flag's own vocabulary.
        ("--backend claude, x", Expect::Refused("UnknownBackend{claude,}")),
        // L4: R3's narrowing wins over R1's `segment := … | count`, so a
        // comma list of bare counts is prose. Pinned so that nobody later
        // "restores" the grammar and silently turns this into five workers.
        ("2,3 fix it", Expect::Parsed(&[], None, "2,3 fix it")),
    ];

    for (typed, expected) in rows {
        if matches!(expected, Expect::Misdirected) {
            assert!(
                misdirected(typed).is_some(),
                "`/team {typed}` is a roster line, answered before the spec parse"
            );
            continue;
        }
        assert!(
            misdirected(typed).is_none(),
            "`/team {typed}` is not a roster line, so the spec parse is what answers it"
        );

        match (expected, parse_team(typed, &fixture_roster)) {
            (Expect::Parsed(members, standing, task), Ok(invocation)) => {
                assert_eq!(spelled(&invocation), *members, "the members of `/team {typed}`");
                assert_eq!(
                    invocation.standing.map(backend_name),
                    *standing,
                    "the standing surface of `/team {typed}`"
                );
                assert_eq!(invocation.task, *task, "the task text of `/team {typed}`");
            }
            (Expect::Refused(refused), Err(error)) => {
                assert_eq!(&refusal(&error), refused, "the refusal of `/team {typed}`");
            }
            (expected, parsed) => {
                panic!("`/team {typed}` expected {expected:?} and parsed {parsed:?}")
            }
        }
    }
}

/// **AC-2.** Names are `<agent>-<n>`, `n` 1-based per agent name across the
/// whole spec, so seven critics spread over three surfaces are still
/// `critic-1`…`critic-7` — and a spec naming two agents counts each of them
/// from one.
#[test]
fn names_are_assigned_per_agent_in_spec_order() {
    let seven = parse_team("2:critic@claude,2:critic@codex,3:critic@grok x", &fixture_roster)
        .expect("a spec of seven critics parses");
    let names: Vec<&str> = seven.members.iter().map(|member| member.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["critic-1", "critic-2", "critic-3", "critic-4", "critic-5", "critic-6", "critic-7"],
        "one counter per agent name, not one per segment"
    );

    let mixed = parse_team("2:critic,2:executor,2 x", &fixture_roster)
        .expect("a spec naming two agents and a bare count parses");
    let names: Vec<&str> = mixed.members.iter().map(|member| member.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["critic-1", "critic-2", "executor-1", "executor-2", "worker-1", "worker-2"],
        "each agent counts from one, and an agent-less row counts under `worker`"
    );
}

/// **AC-5.** `members[i] ← surfaces[i % k]` indexes **members**, not segments:
/// a two-segment spec over two surfaces alternates rather than giving each
/// segment a surface of its own.
#[test]
fn the_round_robin_indexes_members_not_segments() {
    let surfaces = |typed: &str| {
        parse_team(typed, &fixture_roster)
            .expect("the spec parses")
            .members
            .iter()
            .map(|member| backend_name(member.backend))
            .collect::<Vec<_>>()
    };

    assert_eq!(
        surfaces("3:critic --backend claude,codex x"),
        vec!["claude", "codex", "claude"],
        "three members over two surfaces wrap"
    );
    assert_eq!(
        surfaces("2:critic,2:executor --backend claude,codex x"),
        vec!["claude", "codex", "claude", "codex"],
        "and `i` counts members: per segment it would be claude, claude, codex, codex"
    );
}

/// **R1.** A count over the cap and a digit run too large to be a count take
/// the same door, because the sentence a person needs is the same one.
#[test]
fn a_count_over_the_cap_and_one_that_overflows_are_refused_alike() {
    for (typed, asked) in [
        ("17:critic x", "17"),
        ("99999999999999999999:critic x", "99999999999999999999"),
        // Summed across the spec, not per segment.
        ("9:critic,8:critic x", "17"),
    ] {
        let refused = parse_team(typed, &fixture_roster).expect_err("the cap refuses");
        assert_eq!(
            refusal(&refused),
            format!("TooMany{{{asked},{MAX_SPEC_MEMBERS}}}"),
            "`/team {typed}`"
        );
    }

    let capped = parse_team("16:critic x", &fixture_roster).expect("the cap itself is allowed");
    assert_eq!(capped.members.len(), MAX_SPEC_MEMBERS);
}

/// **R8/AC-24.** With no agent registry every name is unknown, so the bare-name
/// arm never fires: `critic --backend claude x` is task text on a standing
/// surface, while a `@`-shaped head is still refused by name.
#[test]
fn an_engine_with_no_roster_never_fires_the_bare_name_arm() {
    let bare = parse_team("critic --backend claude x", &no_roster)
        .expect("a bare word is task text when no roster knows it");
    assert!(bare.members.is_empty(), "{:?}", bare.members);
    assert_eq!(bare.standing.map(backend_name), Some("claude"));
    assert_eq!(bare.task, "critic x", "the head token stays in the task it was a word of");

    let listed = parse_team("critic,critic,critic x", &no_roster)
        .expect("a comma list holding no roster name is task text too");
    assert!(listed.members.is_empty(), "{:?}", listed.members);
    assert_eq!(listed.task, "critic,critic,critic x", "including its head token");

    let shaped = parse_team("critic@claude x", &no_roster).expect_err("a `@` head is still a spec");
    assert_eq!(refusal(&shaped), "UnknownAgent{critic}");
}

/// **AC-3/PM-1.** Every refusal ends with the one escape sentence, so a person
/// whose task text was eaten is told the way back rather than left to guess it.
#[test]
fn every_refusal_names_the_way_back_to_task_text() {
    let every: [(TeamSpecError, &[&str]); 10] = [
        (TeamSpecError::UnknownAgent { name: "nosuch".to_owned() }, &["nosuch"]),
        (TeamSpecError::NotSpawnable { name: "build".to_owned() }, &["build"]),
        (TeamSpecError::UnknownBackend(UnknownBackend { value: "nope".to_owned() }), &["nope"]),
        (TeamSpecError::BackendWithAt { segment: "critic@claude".to_owned() }, &["critic@claude"]),
        (TeamSpecError::FewerMembersThanSurfaces { members: 2, surfaces: 3 }, &["2", "3"]),
        (TeamSpecError::ZeroCount { segment: "0:critic".to_owned() }, &["0:critic"]),
        (TeamSpecError::TooMany { asked: "17".to_owned(), cap: MAX_SPEC_MEMBERS }, &["17", "16"]),
        (TeamSpecError::Malformed { token: "TODO:fix".to_owned() }, &["TODO:fix"]),
        (TeamSpecError::MissingBackendValue, &["--backend"]),
        (TeamSpecError::RepeatedBackend, &["--backend"]),
    ];

    for (error, payload) in &every {
        let sentence = error.to_string();
        assert!(sentence.ends_with(ESCAPE_TAIL), "a refusal with no way back: {sentence}");
        // Against what it says **before** the tail, because the tail itself
        // names `--backend`, a `:` and an `@`: measuring the whole sentence
        // would let a refusal that named nothing pass on the escape's own words.
        let said = &sentence[..sentence.len() - ESCAPE_TAIL.len()];
        for named in *payload {
            assert!(said.contains(named), "a refusal that never names {named:?}: {sentence}");
        }
    }

    // **AC-8.** The surface refusal is `teammate::UnknownBackend`'s own words,
    // and the list is the whole point of them — a `Display` that stopped
    // spelling the six would keep every other assertion in this file green.
    let unknown = every[2].0.to_string();
    for backend in BACKENDS {
        assert!(unknown.contains(backend), "the surface refusal drops {backend}: {unknown}");
    }
}

/// **AC-12.** The three renderings are told apart by the parse, and the
/// empty-task override picks the second whatever was parsed.
#[test]
fn render_members_says_which_of_the_three_cases_it_is() {
    let resolved = render_members(
        &parse_team("2:critic@claude,critic@codex x", &fixture_roster).expect("a spec parses"),
    );
    assert!(
        resolved.contains("1. critic-1 — critic on claude"),
        "a resolved roster is one line per member: {resolved}"
    );
    assert!(resolved.contains("3. critic-3 — critic on codex"), "{resolved}");

    let nothing =
        render_members(&parse_team("port the loader", &fixture_roster).expect("task text parses"));
    assert_eq!(nothing, NOTHING_NAMED, "nothing named and no surface is one sentence");

    let standing = render_members(
        &parse_team("--backend codex read the wire", &fixture_roster).expect("a flag alone parses"),
    );
    assert!(standing.starts_with(NOTHING_NAMED), "the third case is the second plus a clause");
    assert!(standing.contains("codex"), "which names the surface: {standing}");

    // The override: template case 1 prints usage and spawns nobody, so a parsed
    // roster with no task is drawn as nothing named.
    let empty = render_members(&parse_team("3", &fixture_roster).expect("a bare count parses"));
    assert_eq!(empty, NOTHING_NAMED, "an empty task renders as nothing named whatever it parsed");
}

/// **AC-13.** A spec-less `--backend` must not lose the surface: it reaches the
/// model as a standing one rather than as nothing at all.
#[test]
fn a_spec_less_surface_reaches_the_model_as_a_standing_surface() {
    let invocation = parse_team("--backend codex read the wire and report", &fixture_roster)
        .expect("a spec-less flag parses");

    assert!(invocation.members.is_empty(), "{:?}", invocation.members);
    assert_eq!(invocation.standing.map(backend_name), Some("codex"));
    assert_eq!(invocation.task, "read the wire and report");
    assert!(render_members(&invocation).contains("codex"));
}

/// **R4.** Both spellings of the flag are read wherever they appear, and each
/// way it can be wrong is refused by its own name.
#[test]
fn the_backend_flag_is_read_in_both_spellings_and_refused_once_per_way_it_can_be_wrong() {
    for typed in ["--backend=grok x", "--backend grok x", "x --backend=grok"] {
        let invocation = parse_team(typed, &fixture_roster).expect("the flag parses");
        assert_eq!(invocation.standing.map(backend_name), Some("grok"), "`/team {typed}`");
        assert_eq!(invocation.task, "x", "and the flag is stripped from `/team {typed}`");
    }

    for (typed, refused) in [
        ("3:critic --backend x --backend y z", "RepeatedBackend"),
        // L2: a second flag is never the first one's surface, so the sentence
        // is the one about naming one list rather than one about a surface
        // called `--backend`.
        ("--backend --backend x", "RepeatedBackend"),
        ("3:critic --backend", "MissingBackendValue"),
        ("3:critic --backend=", "MissingBackendValue"),
        ("2:critic@claude --backend codex x", "BackendWithAt{2:critic@claude}"),
        ("fix the --backend flag docs", "UnknownBackend{flag}"),
    ] {
        let error = parse_team(typed, &fixture_roster).expect_err("the flag refuses");
        assert_eq!(refusal(&error), refused, "`/team {typed}`");
    }

    // R2's bare-name arm asks whether the next raw token **is** the flag, which
    // is the same question the extraction pass asks — one predicate, so the two
    // cannot come to disagree. "Begins `--backend`" was written to admit the
    // joined spelling, not a longer word that happens to open the same way.
    let joined = parse_team("critic --backend=claude x", &fixture_roster)
        .expect("the joined spelling still opens the bare-name arm");
    assert_eq!(spelled(&joined), vec!["critic-1 critic claude"]);
    assert_eq!(joined.task, "x");

    let prose = parse_team("critic --backends foo", &fixture_roster)
        .expect("a word that only starts like the flag is a word");
    assert!(prose.members.is_empty(), "so the head stays a word too: {:?}", prose.members);
    assert_eq!(prose.standing, None);
    assert_eq!(prose.task, "critic --backends foo", "and the whole line is the task");
}
