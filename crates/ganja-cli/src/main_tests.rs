use std::{
    process::Command as ProcessCommand,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use clap::Parser;
use ganja_core::{
    SessionId, SessionInfo,
    catalog::{ModelInfo, ModelStatus, Pricing},
    storage::VERSION,
};
use ganja_protocol::Usage;
use jiff::{Timestamp, tz::TimeZone};

use super::{
    Cli, Command, UNTITLED, age, billed_tokens, matching, per_mtok, printable_session, providers,
    resolve_filter, title,
};

/// The session column of `sessions
/// --live` is a peer's word, and a peer that is not a session could put
/// an escape sequence in it; the column shows an id untouched and
/// anything else as `?` per character, cut to an id's width.
#[test]
fn a_peers_session_id_is_printed_plain_or_not_at_all() {
    assert_eq!(
        printable_session("01998ad0-0000-7000-8000-00000000d505"),
        "01998ad0-0000-7000-8000-00000000d505",
        "an id this build mints passes untouched"
    );
    assert_eq!(
        printable_session("\x1b[31mred\x1b[0m\n"),
        "?[31mred?[0m?",
        "escapes and newlines are shown as ?"
    );
    assert_eq!(
        printable_session(&"a".repeat(100)).chars().count(),
        36,
        "and the column is cut to an id's width"
    );
}

/// The log's name carries the machine's own civil date, zero-padded so
/// the directory sorts by age (2026-08-15, retiring the stock appender's
/// UTC stamp).
#[test]
fn a_log_is_named_by_the_zero_padded_date() {
    assert_eq!(super::log_name((2026, 8, 15)), "ganja.2026-08-15.log");
    assert_eq!(super::log_name((2026, 1, 2)), "ganja.2026-01-02.log");
}

/// A child process owns `TZ`, so this remains race-free under plain
/// `cargo test` while still exercising the system timezone lookup.
#[test]
fn a_local_date_uses_the_configured_timezone_at_a_fixed_instant() {
    const CHILD: &str = "GANJA_TEST_LOCAL_DATE_CHILD";
    if let Some(expected) = std::env::var_os(CHILD) {
        let instant = UNIX_EPOCH
            .checked_add(Duration::from_secs(1_786_926_600))
            .expect("the fixed instant is representable");
        let expected = match expected.to_str() {
            Some("local") => (2026, 8, 16),
            Some("fallback") => (2026, 8, 17),
            value => panic!("unknown child case: {value:?}"),
        };
        assert_eq!(super::local_date_at(instant), expected);
        return;
    }

    for (case, timezone) in [
        ("local", "America/Los_Angeles"),
        ("fallback", "/definitely/missing/ganja-zoneinfo"),
    ] {
        let output =
            ProcessCommand::new(std::env::current_exe().expect("the test binary has a path"))
                .args([
                    "--exact",
                    "tests::a_local_date_uses_the_configured_timezone_at_a_fixed_instant",
                    "--nocapture",
                ])
                .env(CHILD, case)
                .env("TZ", timezone)
                .output()
                .expect("the isolated timezone test starts");

        assert!(
            output.status.success(),
            "the isolated {case} timezone pin failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

/// The UTC fallback, against dates a calendar can check — the epoch
/// itself, a leap-adjacent day, and the report's own.
#[test]
fn the_utc_fallback_date_matches_the_calendar() {
    for (days, expected) in [
        (0_i64, (1970, 1, 1)),
        (11_017, (2000, 3, 1)),
        (20_680, (2026, 8, 15)),
    ] {
        let timestamp = Timestamp::from_second(days * 86_400).expect("the fixture is in range");
        assert_eq!(super::date_in(timestamp, TimeZone::UTC), expected);
    }
}

#[test]
fn the_ui_flags_map_onto_the_override_tier() {
    let cli = Cli::parse_from([
        "ganja",
        "--model",
        "anthropic/claude-sonnet-5",
        "--agent",
        "plan",
        "--config",
        "/tmp/override.jsonc",
    ]);

    let overrides = cli.select.overrides();
    assert_eq!(
        overrides.model.as_deref(),
        Some("anthropic/claude-sonnet-5")
    );
    assert_eq!(overrides.agent.as_deref(), Some("plan"));
    assert_eq!(
        overrides.config_file.as_deref(),
        Some(std::path::Path::new("/tmp/override.jsonc"))
    );
}

/// All three spellings mean one thing (**D479**), which is the whole
/// reason `ganja run` carries them too: a script that says `--yolo` and a
/// person who says `--auto` have asked for the same session.
#[test]
fn every_spelling_of_the_bypass_resolves_to_the_one_decision() {
    for spelled in ["--auto", "--yolo", "--dangerously-skip-permissions"] {
        let cli = Cli::try_parse_from(["ganja", spelled])
            .unwrap_or_else(|error| panic!("{spelled} has to parse: {error}"));

        assert!(cli.bypass.wanted(), "{spelled} asked for the bypass");
    }

    assert!(
        !Cli::parse_from(["ganja"]).bypass.wanted(),
        "a session that asked for nothing keeps every dialog it always had"
    );
}

/// §4.1's launch line, as another process composes it: the five spellings
/// parse together, ride beside `--model`/`--agent`, and are the value the
/// UI is handed. `--agent-color` is the one that may be left off.
#[test]
fn the_spawn_flags_parse_together_and_reach_the_ui() {
    let cli = Cli::parse_from([
        "ganja",
        "--model",
        "anthropic/claude-sonnet-5",
        "--agent-id",
        "w1@session-224cbeab",
        "--agent-name",
        "w1",
        "--team-name",
        "session-224cbeab",
        "--agent-color",
        "blue",
        "--parent-session-id",
        "224cbeab-4e62-497c-aa8f-d05cc33ce7ba",
    ]);

    assert_eq!(
        cli.member.wanted(),
        Some(ganja_tui::member::Flags {
            agent_id: "w1@session-224cbeab".to_owned(),
            name: "w1".to_owned(),
            team: "session-224cbeab".to_owned(),
            color: Some("blue".to_owned()),
            parent_session_id: "224cbeab-4e62-497c-aa8f-d05cc33ce7ba".to_owned(),
        })
    );

    let uncolored = Cli::parse_from([
        "ganja",
        "--agent-id",
        "w1@session-224cbeab",
        "--agent-name",
        "w1",
        "--team-name",
        "session-224cbeab",
        "--parent-session-id",
        "224cbeab-4e62-497c-aa8f-d05cc33ce7ba",
    ]);
    assert_eq!(
        uncolored.member.wanted().map(|flags| flags.color),
        Some(None),
        "the colour is the one optional member of the set"
    );
    assert_eq!(
        Cli::parse_from(["ganja"]).member.wanted(),
        None,
        "a session a person started is nobody's teammate"
    );
}

/// The five arrive together or not at all: a launch line missing one of
/// the required four is refused by clap rather than half-honored, and a
/// companion flag without `--agent-id` is refused too.
#[test]
fn a_partial_spawn_line_is_refused_not_half_honored() {
    for partial in [
        vec!["ganja", "--agent-id", "w1@session-224cbeab"],
        vec![
            "ganja",
            "--agent-id",
            "w1@session-224cbeab",
            "--agent-name",
            "w1",
            "--team-name",
            "session-224cbeab",
        ],
        vec!["ganja", "--agent-name", "w1"],
        vec!["ganja", "--team-name", "session-224cbeab"],
        vec!["ganja", "--agent-color", "blue"],
        vec!["ganja", "--parent-session-id", "224cbeab"],
    ] {
        assert!(
            Cli::try_parse_from(&partial).is_err(),
            "{partial:?} is a partial launch line and has to be refused"
        );
    }
}

/// Hidden exactly as the bypass aliases are: nothing a person does not
/// type appears in the help a person reads.
#[test]
fn the_spawn_flags_are_hidden_from_help() {
    use clap::CommandFactory as _;

    let mut help = Vec::new();
    Cli::command()
        .write_long_help(&mut help)
        .expect("the help renders");
    let help = String::from_utf8(help).expect("the help is UTF-8");

    for flag in [
        "--agent-id",
        "--agent-name",
        "--team-name",
        "--agent-color",
        "--parent-session-id",
        "--yolo",
    ] {
        assert!(
            !help.contains(flag),
            "{flag} is hidden from --help:\n{help}"
        );
    }
    assert!(help.contains("--model"), "and the visible flags still show");
}

/// The live listing's directory door is hidden for the spawn flags'
/// reason — nothing a person types — and meaningless without `--live`,
/// so clap refuses the pair rather than this code ignoring half of it.
#[test]
fn the_socket_directory_door_is_hidden_and_needs_live() {
    use clap::CommandFactory as _;

    let mut help = Vec::new();
    Cli::command()
        .find_subcommand_mut("sessions")
        .expect("sessions is a subcommand")
        .write_long_help(&mut help)
        .expect("the help renders");
    let help = String::from_utf8(help).expect("the help is UTF-8");
    assert!(
        !help.contains("--socket-dir"),
        "--socket-dir is hidden from --help:\n{help}"
    );
    assert!(help.contains("--live"), "and the visible flag still shows");

    assert!(
        Cli::try_parse_from(["ganja", "sessions", "--socket-dir", "/tmp/x"]).is_err(),
        "a directory to list live sockets in, without --live, is refused"
    );
    let Ok(Cli {
        command: Some(Command::Sessions(args)),
        ..
    }) = Cli::try_parse_from(["ganja", "sessions", "--live", "--socket-dir", "/tmp/x"])
    else {
        panic!("the pair parses");
    };
    assert!(args.live);
    assert_eq!(
        args.socket_dir.as_deref(),
        Some(std::path::Path::new("/tmp/x"))
    );
}

/// AC-5's CLI half: `--name` is validated at parse, against the same
/// grammar `/rename` runs mid-session, with the refusal's own sentence
/// — every clause of it, not only one.
#[test]
fn the_name_flag_is_vetted_at_parse_by_the_d527_grammar() {
    let Ok(Cli { name, .. }) = Cli::try_parse_from(["ganja", "--name", "worker-1"]) else {
        panic!("a name the grammar admits parses");
    };
    assert_eq!(name.as_deref(), Some("worker-1"));

    for (spelled, refusal) in [
        ("", "empty"),
        ("a b", "carries no whitespace"),
        ("*", "broadcast token"),
        ("name@scope", "carries no `@`"),
        ("uds:name", "carries no `:`"),
        ("/leading", "does not begin with `/`"),
    ] {
        let error = Cli::try_parse_from(["ganja", "--name", spelled])
            .expect_err(&format!("{spelled:?} is refused by the grammar"));
        let rendered = error.to_string();
        assert!(
            rendered.contains(refusal),
            "{spelled:?} names its own clause: {rendered}"
        );
    }
}

#[test]
fn a_subcommand_given_the_ui_flags_is_refused_not_ignored() {
    // `args_conflicts_with_subcommands` covers the new flags the same way
    // it already covered the resume pair: the shape fails to parse.
    assert!(
        Cli::try_parse_from(["ganja", "--model", "x/y", "models"]).is_err(),
        "a listing that read like it honored --model would be lying"
    );
}

/// `global = true` is what puts the flag on every subcommand rather than
/// only on the UI run — a log level means the same thing for a listing as
/// for a session, where a resume flag means nothing at all.
#[test]
fn every_invocation_takes_the_verbose_flag() {
    for spelled in [
        vec!["ganja", "-v"],
        vec!["ganja", "--verbose"],
        vec!["ganja", "models", "-v"],
        vec!["ganja", "sessions", "--verbose"],
        vec!["ganja", "run", "-v", "what does this crate do"],
    ] {
        let cli = Cli::try_parse_from(&spelled)
            .unwrap_or_else(|error| panic!("{spelled:?} has to parse: {error}"));

        assert!(cli.verbose, "{spelled:?} asked for the debug log");
    }

    assert!(
        !Cli::parse_from(["ganja", "models"]).verbose,
        "the flag is off unless it was passed"
    );
}

/// The position the flag's doc comment promises, pinned so that a clap
/// release which *does* exempt a global argument shows up here as a failing
/// assertion rather than as documentation that quietly stopped being true.
#[test]
fn the_verbose_flag_is_written_after_the_subcommand_not_before_it() {
    assert!(
        Cli::try_parse_from(["ganja", "-v", "models"]).is_err(),
        "`args_conflicts_with_subcommands` negates every argument written \
             before a subcommand, global ones included"
    );
}

/// The precedence the flag's own doc comment promises, in both directions:
/// the flag moves the default, and an explicit `RUST_LOG` outranks it.
#[test]
fn rust_log_outranks_the_verbose_flag_and_the_flag_outranks_the_default() {
    use tracing_subscriber::filter::LevelFilter;

    assert_eq!(
        resolve_filter(None, false).max_level_hint(),
        Some(LevelFilter::INFO),
        "without the flag nothing about today's default may move"
    );

    let verbose = resolve_filter(None, true);
    assert_eq!(
        verbose.max_level_hint(),
        Some(LevelFilter::DEBUG),
        "the flag has to reach debug or it buys nothing"
    );
    assert!(
        verbose.to_string().contains("ganja=debug"),
        "debug is for this workspace's crates, not for hyper's socket \
             bookkeeping: {verbose}"
    );

    for flag in [false, true] {
        assert_eq!(
            resolve_filter(Some("warn"), flag).max_level_hint(),
            Some(LevelFilter::WARN),
            "an explicit RUST_LOG wins whether or not -v was passed"
        );
    }

    // A variable that will not parse is not an instruction, so it falls
    // through to whatever the flag asked for — which is what the filter
    // did with an unreadable RUST_LOG before the flag existed.
    assert_eq!(
        resolve_filter(Some("=not a filter="), true).max_level_hint(),
        Some(LevelFilter::DEBUG)
    );
}

#[test]
fn the_models_arguments_parse_as_a_filter_and_a_forced_fetch() {
    let cli = Cli::parse_from(["ganja", "models", "anthropic", "--refresh"]);

    let Some(Command::Models { provider, refresh }) = cli.command else {
        panic!("`models` has to parse as itself");
    };
    assert_eq!(provider.as_deref(), Some("anthropic"));
    assert!(refresh);

    // Both are optional, and the bare form is the one every earlier
    // invocation of this command used.
    let cli = Cli::parse_from(["ganja", "models"]);
    let Some(Command::Models { provider, refresh }) = cli.command else {
        panic!("`models` has to parse as itself");
    };
    assert_eq!(provider, None);
    assert!(!refresh);
}

/// A table row, differing from the next only in what a listing filters on.
fn model(provider_id: &str, id: &str) -> Arc<ModelInfo> {
    Arc::new(ModelInfo {
        id: id.to_owned(),
        provider_id: provider_id.to_owned(),
        name: id.to_owned(),
        context_window: 200_000,
        max_output: 8_000,
        input_limit: None,
        pricing: Pricing {
            input: 1.0,
            output: 2.0,
            cache_read: 0.1,
            cache_write: None,
        },
        family: None,
        release_date: None,
        tool_call: true,
        status: ModelStatus::Active,
        reasoning: false,
        reasoning_options: None,
        npm: None,
        variants: Default::default(),
    })
}

/// Two providers, one of them serving two models, so a filter that matched
/// on the wrong field or kept the first row of each provider would show.
fn table() -> Vec<Arc<ModelInfo>> {
    vec![
        model("anthropic", "claude-sonnet-5"),
        model("anthropic", "claude-haiku-4-5"),
        model("openai", "gpt-5.6"),
    ]
}

#[test]
fn a_named_provider_is_the_only_one_a_listing_shows() {
    let listed = matching(&table(), Some("anthropic"));

    assert_eq!(listed.len(), 2, "both of that provider's rows are listed");
    assert!(
        listed.iter().all(|model| model.provider_id == "anthropic"),
        "another provider's rows reached a filtered listing"
    );
}

#[test]
fn naming_no_provider_lists_the_whole_table_in_the_order_it_came_in() {
    let table = table();
    let listed = matching(&table, None);

    let ids: Vec<&str> = listed.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["claude-sonnet-5", "claude-haiku-4-5", "gpt-5.6"]);
}

/// A provider that serves nothing here has to be told apart from one that
/// serves nothing at all — the refusal this feeds is the whole difference.
#[test]
fn a_provider_the_table_never_heard_of_matches_nothing() {
    assert!(matching(&table(), Some("anthropi")).is_empty());
    // The comparison is on the provider, not on anything that merely looks
    // like one: a model id is not a provider id.
    assert!(matching(&table(), Some("gpt-5.6")).is_empty());
}

#[test]
fn the_providers_a_table_carries_are_named_once_each_in_listing_order() {
    assert_eq!(providers(&table()), ["anthropic", "openai"]);
    assert!(providers(&[]).is_empty());
}

const SECOND: u64 = 1_000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// The moment every fixture is aged against, so a test asserts on the
/// interval it asked for rather than on whatever the clock says.
const NOW: u64 = 1_000 * DAY;

/// A stored session that differs from the next only in what it is called,
/// which is all [`title`] reads.
fn info(name: Option<&str>) -> SessionInfo {
    SessionInfo {
        effort: None,
        id: SessionId::from("ses_1".to_owned()),
        version: VERSION,
        title: name.map(str::to_owned),
        created: 0,
        updated: NOW,
        usage: Usage::default(),
        context_tokens: 0,
        summary: None,
        agent: None,
        model: None,
        activated_tools: std::collections::BTreeSet::new(),
        parent: None,
        revert: None,
    }
}

#[test]
fn a_price_keeps_every_digit_that_means_something() {
    assert_eq!(per_mtok(10.0), "$10");
    assert_eq!(per_mtok(4.5), "$4.5");
    assert_eq!(per_mtok(0.075), "$0.075");
    assert_eq!(per_mtok(0.0), "$0");
}

/// A title is written by a model, so it is untrusted text on its way to a
/// terminal that would *execute* an escape sequence in it — the same threat
/// the picker in `ganja-tui` is pinned against, on the surface that has no
/// `ratatui` filtering underneath it. `println!` writes straight to the
/// tty, so this function is the only thing standing between the two.
#[test]
fn a_title_the_model_wrote_cannot_move_the_terminals_cursor() {
    let listed = title(&info(Some(
        "\u{1b}[2J\u{1b}[31mporting storage\u{7}\r\nsecond row",
    )));

    let leaked: Vec<char> = listed
        .chars()
        .filter(|character| character.is_control())
        .collect();
    assert!(
        leaked.is_empty(),
        "control characters reached a printed row: {leaked:?} in {listed:?}"
    );
    // Without this the assertion above would also pass on an empty string.
    assert!(
        listed.contains("porting storage"),
        "the printable remainder still has to be listed: {listed:?}"
    );
    // A newline would have broken one row of the table into two.
    assert!(
        !listed.contains('\n') && listed.contains("second row"),
        "a newline has to become a space, not a row break: {listed:?}"
    );
}

#[test]
fn a_title_with_nothing_printable_left_falls_back_to_untitled() {
    assert_eq!(title(&info(None)), UNTITLED);
    assert_eq!(title(&info(Some(""))), UNTITLED);
    assert_eq!(title(&info(Some("   "))), UNTITLED);
    // Every character here is replaced by a space, and the row would then
    // be blank rather than merely odd.
    assert_eq!(title(&info(Some("\u{1b}\u{7}\r\n\t"))), UNTITLED);
}

#[test]
fn a_title_is_listed_without_the_whitespace_around_it() {
    assert_eq!(title(&info(Some("  porting storage  "))), "porting storage");
}

/// The picker in `ganja-tui` renders the same ages from its own copy of
/// this arithmetic — deliberately, so neither crate has to reach into the
/// other. Deliberate duplication still drifts, so these mirror the
/// assertions in `component/sessions.rs` one for one. Note the arguments
/// are in the opposite order there, which is the cheapest way for the two
/// to start disagreeing without anyone noticing.
#[test]
fn ages_round_to_the_unit_they_are_reported_in() {
    assert_eq!(age(NOW, NOW), "just now");
    assert_eq!(age(NOW - 59 * SECOND, NOW), "just now");
    assert_eq!(age(NOW - 5 * MINUTE, NOW), "5m ago");
    assert_eq!(age(NOW - 3 * HOUR, NOW), "3h ago");
    assert_eq!(age(NOW - 2 * DAY, NOW), "2d ago");
    // A clock that moved backwards between runs, not a session recorded in
    // the future.
    assert_eq!(age(NOW + DAY, NOW), "just now");
}

/// Each bucket's first and last moment, because an off-by-one here reads
/// as "60m ago" or "24h ago" — a listing that is wrong in a way a user
/// would notice but not be able to explain.
#[test]
fn each_age_bucket_ends_where_the_next_one_begins() {
    assert_eq!(age(NOW - (MINUTE - 1), NOW), "just now");
    assert_eq!(age(NOW - MINUTE, NOW), "1m ago");
    assert_eq!(age(NOW - (HOUR - 1), NOW), "59m ago");
    assert_eq!(age(NOW - HOUR, NOW), "1h ago");
    assert_eq!(age(NOW - (DAY - 1), NOW), "23h ago");
    assert_eq!(age(NOW - DAY, NOW), "1d ago");
}

/// Reasoning tokens are a slice of `output_tokens` rather than a count
/// beside them, so billing them again would report the same thinking
/// twice. That reasoning lives in a doc comment; this is what keeps it
/// true.
#[test]
fn the_billed_total_counts_what_was_paid_for_and_counts_it_once() {
    let usage = Usage {
        input_tokens: 1,
        output_tokens: 20,
        reasoning_tokens: 8,
        cache_read_tokens: 300,
        cache_write_tokens: 4_000,
    };

    assert_eq!(billed_tokens(&usage), 1 + 20 + 300 + 4_000);
    assert_eq!(billed_tokens(&Usage::default()), 0);

    // The exclusion has to be the rule rather than an accident of the
    // numbers above: thinking harder must not move the bill.
    let thinking_harder = Usage {
        reasoning_tokens: 19,
        ..usage
    };
    assert_eq!(
        billed_tokens(&thinking_harder),
        billed_tokens(&usage),
        "reasoning_tokens is already inside output_tokens"
    );
}
