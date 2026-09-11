use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;

use super::{
    Argv, BASE, ChildEnv, NEVER_ANYWHERE, NEVER_ON_CONVERSATION, OneShot, PERMISSION_MODE, SET,
    Spawn, forbidden,
};

/// Every `Spawn` shape a builder is ever handed: the default model, a named
/// model, each with and without an effort.
fn conversations() -> Vec<Spawn> {
    let mut rows = Vec::new();
    for model in ["default", "claude-opus-5"] {
        for effort in [None, Some("high")] {
            rows.push(Spawn {
                session_id: "01998a00-0000-7000-8000-00000000000a".to_owned(),
                model: model.to_owned(),
                effort: effort.map(str::to_owned),
            });
        }
    }
    rows
}

fn one_shots() -> Vec<OneShot> {
    conversations()
        .into_iter()
        .map(|spawn| OneShot {
            session_id: spawn.session_id,
            model: spawn.model,
            effort: spawn.effort,
        })
        .collect()
}

fn spelled(argv: &[OsString]) -> Vec<String> {
    argv.iter().map(|token| token.to_string_lossy().into_owned()).collect()
}

/// The token after `flag`, or [`None`] when the flag is absent.
fn value_after(argv: &[OsString], flag: &str) -> Option<String> {
    let spelled = spelled(argv);
    let at = spelled.iter().position(|token| token == flag)?;

    spelled.get(at + 1).cloned()
}

#[test]
fn the_base_argv_is_twenty_one_tokens() {
    // The one place the count is checkable. A flag silently dropped from
    // `BASE` changes what every process this wire spawns is, and no other
    // test in the suite would name it.
    assert_eq!(BASE.len(), 21);
}

#[test]
fn every_conversation_argv_carries_the_tokens_the_wire_cannot_work_without() {
    for spawn in conversations() {
        let argv = spelled(&Argv::conversation(&spawn).expect("a built argv"));

        for token in [
            "--verbose",
            "-p",
            "--input-format",
            "--output-format",
            "--permission-prompt-tool",
            "--permission-mode",
            "--tools",
            "--setting-sources",
            "--strict-mcp-config",
        ] {
            assert!(argv.contains(&token.to_owned()), "{token} missing from {argv:?}");
        }

        assert_eq!(argv.iter().filter(|token| *token == "stream-json").count(), 2);
        assert_eq!(
            value_after(
                &Argv::conversation(&spawn).expect("a built argv"),
                "--permission-prompt-tool"
            )
            .as_deref(),
            Some("stdio")
        );
        // `--tools ""` and `--setting-sources ""` are a flag and an EMPTY
        // value, not a bare flag: the empty string is what removes the CLI's
        // own tools and its settings sources, and a builder that dropped it
        // would leave the flag consuming the next token instead.
        assert_eq!(
            value_after(&Argv::conversation(&spawn).expect("a built argv"), "--tools").as_deref(),
            Some("")
        );
        assert_eq!(
            value_after(&Argv::conversation(&spawn).expect("a built argv"), "--setting-sources")
                .as_deref(),
            Some("")
        );
    }
}

#[test]
fn every_one_shot_argv_carries_the_same_tokens() {
    for one_shot in one_shots() {
        let argv = spelled(&Argv::one_shot(&one_shot).expect("a built argv"));

        for token in [
            "--verbose",
            "-p",
            "--permission-prompt-tool",
            "--permission-mode",
            "--tools",
            "--setting-sources",
            "--strict-mcp-config",
        ] {
            assert!(argv.contains(&token.to_owned()), "{token} missing from {argv:?}");
        }
        assert_eq!(argv.iter().filter(|token| *token == "stream-json").count(), 2);
    }
}

#[test]
fn a_conversation_argv_is_disjoint_from_the_whole_conversation_never_list() {
    for spawn in conversations() {
        let argv = Argv::conversation(&spawn).expect("a built argv");

        assert_eq!(forbidden(&argv, NEVER_ON_CONVERSATION), None, "{:?}", spelled(&argv));
    }
}

#[test]
fn a_one_shot_argv_is_disjoint_from_the_wide_never_list() {
    for one_shot in one_shots() {
        let argv = Argv::one_shot(&one_shot).expect("a built argv");

        assert_eq!(forbidden(&argv, NEVER_ANYWHERE), None, "{:?}", spelled(&argv));
    }
}

/// AC-3.21's builder half. `--resume` is the token posture C exists to
/// forbid, and it is checked by name rather than only through the never-list
/// so that a reordering of that list cannot make this pass vacuously.
#[test]
fn no_argv_this_wire_builds_contains_resume() {
    assert!(NEVER_ANYWHERE.contains(&"--resume"));

    for spawn in conversations() {
        assert!(
            !spelled(&Argv::conversation(&spawn).expect("a built argv"))
                .contains(&"--resume".to_owned())
        );
    }
    for one_shot in one_shots() {
        assert!(
            !spelled(&Argv::one_shot(&one_shot).expect("a built argv"))
                .contains(&"--resume".to_owned())
        );
    }
}

/// W2's M4 dropped it, and rev 7 forbade it — the same enforcement `--resume`
/// gets, so the two dropped flags cannot diverge in how they are held out.
#[test]
fn no_argv_this_wire_builds_contains_include_partial_messages() {
    assert!(NEVER_ANYWHERE.contains(&"--include-partial-messages"));

    for spawn in conversations() {
        assert!(
            !spelled(&Argv::conversation(&spawn).expect("a built argv"))
                .contains(&"--include-partial-messages".to_owned())
        );
    }
    for one_shot in one_shots() {
        assert!(
            !spelled(&Argv::one_shot(&one_shot).expect("a built argv"))
                .contains(&"--include-partial-messages".to_owned())
        );
    }
}

/// Stronger than forbidding the dangerous spellings: `auto` and `acceptEdits`
/// are choices too, and this wire makes none of them.
#[test]
fn the_permission_mode_is_manual_on_every_row_of_both_builders() {
    for spawn in conversations() {
        assert_eq!(
            value_after(&Argv::conversation(&spawn).expect("a built argv"), "--permission-mode")
                .as_deref(),
            Some(PERMISSION_MODE)
        );
    }
    for one_shot in one_shots() {
        assert_eq!(
            value_after(&Argv::one_shot(&one_shot).expect("a built argv"), "--permission-mode")
                .as_deref(),
            Some(PERMISSION_MODE)
        );
    }
    assert_eq!(PERMISSION_MODE, "manual");
}

/// AC-3.11: the flag belongs to exactly one builder, and a test reddens on
/// either inversion.
#[test]
fn only_a_one_shot_is_told_not_to_persist_its_record() {
    for one_shot in one_shots() {
        assert!(
            spelled(&Argv::one_shot(&one_shot).expect("a built argv"))
                .contains(&"--no-session-persistence".to_owned()),
            "a one-shot must say its record is worth nothing"
        );
    }
    for spawn in conversations() {
        assert!(
            !spelled(&Argv::conversation(&spawn).expect("a built argv"))
                .contains(&"--no-session-persistence".to_owned()),
            "a held process's record is the continuity this wire has"
        );
    }
    assert!(NEVER_ON_CONVERSATION.contains(&"--no-session-persistence"));
}

#[test]
fn the_session_id_is_the_only_id_flag_either_builder_writes() {
    let spawn = Spawn {
        session_id: "01998a00-0000-7000-8000-00000000000a".to_owned(),
        model: "default".to_owned(),
        effort: None,
    };
    let argv = spelled(&Argv::conversation(&spawn).expect("a built argv"));

    assert_eq!(
        value_after(&Argv::conversation(&spawn).expect("a built argv"), "--session-id").as_deref(),
        Some(spawn.session_id.as_str())
    );
    assert_eq!(argv.iter().filter(|token| *token == "--session-id").count(), 1);
}

/// `default` is the CLI's own word for "whatever you would choose", so
/// passing it as a literal would replace the vendor's choice with a string.
#[test]
fn the_default_model_is_left_unnamed_and_every_other_model_is_named() {
    let mut spawn = Spawn {
        session_id: "01998a00-0000-7000-8000-00000000000a".to_owned(),
        model: "default".to_owned(),
        effort: None,
    };
    assert_eq!(value_after(&Argv::conversation(&spawn).expect("a built argv"), "--model"), None);

    spawn.model = "claude-opus-5".to_owned();
    assert_eq!(
        value_after(&Argv::conversation(&spawn).expect("a built argv"), "--model").as_deref(),
        Some("claude-opus-5")
    );
}

#[test]
fn an_effort_is_named_only_when_the_turn_runs_under_one() {
    let mut spawn = Spawn {
        session_id: "01998a00-0000-7000-8000-00000000000a".to_owned(),
        model: "default".to_owned(),
        effort: None,
    };
    assert_eq!(value_after(&Argv::conversation(&spawn).expect("a built argv"), "--effort"), None);

    spawn.effort = Some("high".to_owned());
    assert_eq!(
        value_after(&Argv::conversation(&spawn).expect("a built argv"), "--effort").as_deref(),
        Some("high")
    );
}

/// `fd5v`: readable thinking is asked for by a turn that asked for reasoning,
/// and by no other. The effort is that ask — the one reasoning signal a
/// `ChatRequest` carries on this wire — so the pair follows it both ways, on
/// both model spellings, and never reaches a one-shot, whose text is read and
/// whose reasoning is not.
#[test]
fn readable_thinking_is_asked_for_only_when_the_turn_runs_under_an_effort() {
    for spawn in conversations() {
        let argv = Argv::conversation(&spawn).expect("a built argv");
        let display = value_after(&argv, "--thinking-display");

        match spawn.effort {
            Some(_) => assert_eq!(
                display.as_deref(),
                Some("summarized"),
                "an effort-bearing turn must ask for the thinking text: {:?}",
                spelled(&argv)
            ),
            None => assert_eq!(
                display,
                None,
                "a turn with no effort asked for no reasoning and must not pay for its text: {:?}",
                spelled(&argv)
            ),
        }
    }

    for one_shot in one_shots() {
        let argv = Argv::one_shot(&one_shot).expect("a built argv");
        assert_eq!(
            value_after(&argv, "--thinking-display"),
            None,
            "a title or a summary reads text only: {:?}",
            spelled(&argv)
        );
    }
}

// ---------------------------------------------------------------- the env

/// The whole environment posture, read off `Command::get_envs()` — no
/// process spawned, and no value of the real environment read.
fn child_env() -> Vec<(String, Option<String>)> {
    let mut command = tokio::process::Command::new("/nonexistent");
    ChildEnv { cwd: PathBuf::from("/nonexistent/cwd") }.apply(&mut command);

    command
        .as_std()
        .get_envs()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

/// The one name on both lists.
///
/// The CLI strips it from a child and this wire then chooses its value, so
/// the child receives it **set** — the recording's own posture, its header
/// listing the name under `SET (9)` and `REMOVED (87)` alike. Pinned as a set
/// so that a later widening of either list cannot quietly create a second
/// overlap nobody decided about.
fn strip_and_set_overlap() -> BTreeSet<&'static str> {
    let set: BTreeSet<&str> = SET.iter().map(|(name, _)| *name).collect();

    super::STRIP.iter().copied().filter(|name| set.contains(name)).collect()
}

#[test]
fn the_strip_list_is_the_eighty_seven_name_union() {
    assert_eq!(super::STRIP.len(), 87);

    let unique: BTreeSet<&str> = super::STRIP.iter().copied().collect();
    assert_eq!(unique.len(), 87, "the union carries no duplicate");
}

#[test]
fn exactly_one_name_is_both_stripped_and_set() {
    assert_eq!(
        strip_and_set_overlap().into_iter().collect::<Vec<_>>(),
        vec!["CLAUDE_CODE_QUESTION_PREVIEW_FORMAT"]
    );
}

#[test]
fn every_stripped_name_reaches_the_command_as_a_removal() {
    let envs = child_env();
    let overlap = strip_and_set_overlap();

    for name in super::STRIP.iter().filter(|name| !overlap.contains(*name)) {
        assert!(
            envs.contains(&((*name).to_owned(), None)),
            "{name} is not removed from the child's environment"
        );
    }
}

#[test]
fn the_two_credentials_that_would_outrank_the_clis_login_are_removed_by_name() {
    let envs = child_env();

    // Pre-mortem 5, named separately from the bulk assertion above because
    // this is the one whose failure bills a platform key in silence.
    assert!(envs.contains(&("ANTHROPIC_API_KEY".to_owned(), None)));
    assert!(envs.contains(&("ANTHROPIC_AUTH_TOKEN".to_owned(), None)));
}

#[test]
fn every_set_name_reaches_the_command_with_its_value() {
    let envs = child_env();

    assert_eq!(SET.len(), 9);
    for (name, value) in SET {
        assert!(
            envs.contains(&((*name).to_owned(), Some((*value).to_owned()))),
            "{name} is not set on the child's environment"
        );
    }
}

#[test]
fn the_overlapping_name_reaches_the_child_set_rather_than_removed() {
    let envs = child_env();

    assert!(envs.contains(&(
        "CLAUDE_CODE_QUESTION_PREVIEW_FORMAT".to_owned(),
        Some("markdown".to_owned())
    )));
}

#[test]
fn the_client_app_name_is_this_builds_own_user_agent() {
    let named = SET
        .iter()
        .find(|(name, _)| *name == "CLAUDE_AGENT_SDK_CLIENT_APP")
        .expect("the client app name is set");

    assert_eq!(named.1, crate::auth::device::GANJA_USER_AGENT);
    assert!(named.1.starts_with("ganja-code/"), "the wire names itself, never the vendor's client");
}

/// The two names this wire deliberately **inherits**: where the person's own
/// `claude` keeps its records, and the login this wire is not holding.
/// Stripping either would make the child a second, differently-configured
/// CLI rather than the user's own.
#[test]
fn the_config_dir_and_the_oauth_token_are_on_neither_list() {
    for name in ["CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN"] {
        assert!(!super::STRIP.contains(&name), "{name} must be inherited");
        assert!(!SET.iter().any(|(set, _)| *set == name), "{name} must be inherited, not chosen");
    }

    let envs = child_env();
    assert!(!envs.iter().any(|(name, _)| name == "CLAUDE_CONFIG_DIR"));
    assert!(!envs.iter().any(|(name, _)| name == "CLAUDE_CODE_OAUTH_TOKEN"));
}

/// AC-3.22's builder half: the cwd is on the `Command`, and it is the
/// scratch directory it was handed rather than whatever this process is in.
#[test]
fn the_child_runs_in_the_scratch_directory_it_was_handed() {
    let mut command = tokio::process::Command::new("/nonexistent");
    ChildEnv { cwd: PathBuf::from("/tmp/scratch/key") }.apply(&mut command);

    assert_eq!(command.as_std().get_current_dir(), Some(std::path::Path::new("/tmp/scratch/key")));
}

// ------------------------------------------- the never-list, in every build

/// The two free-form values that reach this command line are `model` and
/// `effort`, and both arrive from outside the crate — a checked-in
/// `ganja.toml`, `GANJA_MODEL`, a `POST /session/{id}/model` that validates
/// nothing. So the never-list has to hold against a value somebody typed, and
/// hold in a **release** build, where the `debug_assert!` it used to be
/// expanded to nothing (CC-4).
#[test]
fn a_model_that_spells_a_forbidden_flag_is_refused_naming_the_token() {
    let spawn = Spawn {
        session_id: "01998a00-0000-7000-8000-00000000000a".to_owned(),
        model: "--resume".to_owned(),
        effort: None,
    };

    let refused = Argv::conversation(&spawn).expect_err("a forbidden token is refused");

    assert!(refused.to_string().contains("--resume"), "the token is named: {refused}");
    assert!(
        Argv::one_shot(&OneShot {
            session_id: spawn.session_id.clone(),
            model: spawn.model.clone(),
            effort: None,
        })
        .is_err(),
        "and the other builder refuses it too"
    );
}

/// The same for an effort, which reaches the builder as a raw string read
/// back out of `request.effort_options` rather than off the `/effort` roster.
#[test]
fn an_effort_that_spells_a_forbidden_flag_is_refused_too() {
    let spawn = Spawn {
        session_id: "01998a00-0000-7000-8000-00000000000a".to_owned(),
        model: "default".to_owned(),
        effort: Some("--sdk-url".to_owned()),
    };

    assert!(Argv::conversation(&spawn).is_err());
}

/// A joined `--flag=value` carries both in one word, so a whole-token
/// comparison had every such spelling outside the list (CC-4).
#[test]
fn the_never_list_covers_the_joined_spelling_of_a_forbidden_flag() {
    for joined in ["--sdk-url=https://elsewhere.example", "--append-system-prompt=be somebody else"]
    {
        let argv = vec![OsString::from("-p"), OsString::from(joined)];

        assert_eq!(
            forbidden(&argv, NEVER_ON_CONVERSATION).as_deref(),
            Some(joined),
            "a joined spelling is still the flag it names"
        );
    }

    // And a value that merely contains an `=` is not a flag at all.
    let ordinary = vec![OsString::from("--model"), OsString::from("a=b")];
    assert_eq!(forbidden(&ordinary, NEVER_ANYWHERE), None);
}

/// The builders themselves still produce none of it — the row this file
/// already had, now reading the refusal rather than a `debug_assert!`.
#[test]
fn every_argv_a_builder_produces_is_accepted_by_its_own_never_list() {
    for spawn in conversations() {
        assert!(Argv::conversation(&spawn).is_ok(), "{spawn:?}");
    }
    for one_shot in one_shots() {
        assert!(Argv::one_shot(&one_shot).is_ok(), "{one_shot:?}");
    }
    assert!(
        Argv::listing(&super::Listing {
            session_id: "01998a00-0000-7000-8000-00000000000a".to_owned(),
        })
        .is_ok()
    );
}
