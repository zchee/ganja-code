use std::collections::BTreeMap;
use std::path::Path;

use serde_json::json;

use super::{
    ANALYST, AgentError, BUILD, CRITIC, DEBUGGER, EXECUTOR, EXPLORE, EXPLORE_PROMPT, GENERAL, PLAN,
    Registry, VERIFIER,
};
use crate::config::{AgentConfig, AgentMode, Config, Overrides};
use crate::permission::{Action, PermissionConfig, Permissions, Rule};

/// The agents a config defines, by name — the type [`Config::agent`] holds,
/// spelled once so a case can build one without naming the map type.
type Definitions = BTreeMap<String, AgentConfig>;

/// A `permission` block, parsed the way a config file would produce one.
fn permission(value: serde_json::Value) -> PermissionConfig {
    serde_json::from_value(value).expect("the fixture is a permission block")
}

/// The action the assembled rules give `permission`/`pattern`, by the same
/// last-match-wins walk the gate does.
fn decides(rules: &[Rule], permission: &str, pattern: &str) -> Option<Action> {
    rules
        .iter()
        .rev()
        .find(|rule| {
            crate::permission::matches(permission, &rule.permission)
                && crate::permission::matches(pattern, &rule.pattern)
        })
        .map(|rule| rule.action.clone())
}

/// The registry `config` resolves with **no** file tier at all.
///
/// Every case below that is about the config says so by going through
/// here: a definition directory nobody handed in cannot be read, so what
/// these assert does not depend on what is in the config home of whoever
/// is running the suite.
fn registry(config: &Config) -> Registry {
    Registry::from_dirs(config, &[]).expect("the fixture config resolves an agent")
}

#[test]
fn the_builtins_are_the_four_upstream_agents_this_build_can_run_and_ganjas_own_five() {
    let registry = registry(&Config::default());
    let names: Vec<&str> = registry.agents().iter().map(|agent| agent.name.as_str()).collect();

    assert_eq!(
        names,
        vec![BUILD, PLAN, GENERAL, EXPLORE, ANALYST, EXECUTOR, VERIFIER, CRITIC, DEBUGGER],
        "upstream's four, then the five `/team`'s stage routing names",
    );
    assert_eq!(registry.default_agent(), BUILD, "and the five changed nothing about that");
}

/// The five carry prompts of their own, and none of them carries the team
/// work-protocol text: the pipeline's stages, claim discipline and shutdown
/// belong to `/team`'s template, so a copy here would be a second place for
/// them to drift — and an agent spawned outside a team would be reading
/// instructions about a team it is not in.
#[test]
fn the_five_team_roles_carry_their_own_prompts_and_no_protocol() {
    let registry = registry(&Config::default());

    for name in [ANALYST, EXECUTOR, VERIFIER, CRITIC, DEBUGGER] {
        let agent = registry.get(name).unwrap_or_else(|| panic!("{name} is builtin"));
        let prompt = agent.prompt.as_deref().unwrap_or_else(|| panic!("{name} has a prompt"));

        assert!(prompt.starts_with("You are a"), "{name}: {prompt}");
        assert!(
            agent.description.as_deref().is_some_and(|line| !line.is_empty()),
            "{name} is offered to the task tool by a line the model reads",
        );
        for protocol in ["task_create", "task_list", "shutdown_request", "team-exec"] {
            assert!(
                !prompt.contains(protocol),
                "{name}'s prompt says {protocol}, which is `/team`'s template's to say",
            );
        }
    }
}

#[test]
fn only_the_subagents_may_be_spawned_and_only_the_others_may_be_selected() {
    let registry = registry(&Config::default());
    let split: Vec<(&str, bool, bool)> = registry
        .agents()
        .iter()
        .map(|agent| (agent.name.as_str(), agent.spawnable(), agent.selectable()))
        .collect();

    assert_eq!(
        split,
        vec![
            (BUILD, false, true),
            (PLAN, false, true),
            (GENERAL, true, false),
            (EXPLORE, true, false),
            // The five are subagents for the same reason `general` is: a
            // `/team` run delegates to them, and nobody switches a session to
            // one.
            (ANALYST, true, false),
            (EXECUTOR, true, false),
            (VERIFIER, true, false),
            (CRITIC, true, false),
            (DEBUGGER, true, false),
        ]
    );
}

#[test]
fn the_search_agent_carries_upstreams_prompt_verbatim() {
    let registry = registry(&Config::default());
    let explore = registry.get(EXPLORE).expect("the search agent is builtin");

    assert_eq!(explore.prompt.as_deref(), Some(EXPLORE_PROMPT));
    assert!(EXPLORE_PROMPT.starts_with("You are a file search specialist."));
    assert!(registry.get(BUILD).expect("build is builtin").prompt.is_none());
}

/// The `.env` rules are the shared default every agent inherits, and the
/// example file is the exception that comes after them.
#[test]
fn every_agent_stops_to_ask_before_reading_a_dotenv_file() {
    let registry = registry(&Config::default());

    for name in [BUILD, PLAN, GENERAL] {
        let rules = &registry.get(name).expect("a builtin").rules;
        assert_eq!(decides(rules, "read", ".env"), Some(Action::Ask), "{name}");
        assert_eq!(decides(rules, "read", "config/.env.local"), Some(Action::Ask), "{name}");
        assert_eq!(decides(rules, "read", ".env.example"), Some(Action::Allow), "{name}");
        assert_eq!(decides(rules, "read", "src/main.rs"), None, "{name}");
    }
}

/// The memory door (**D478**): three rules, all patterned on the one
/// directory, and each of them answering a question the memory root would
/// otherwise raise — the location gate, and the two tools that record a
/// fact. Asserted over a directory handed in, because what the rules say
/// is the behaviour and where the directory came from is not.
#[test]
fn the_memory_door_opens_that_one_directory_and_nothing_around_it() {
    let directory = std::path::PathBuf::from("/data/ganja/project/api-1/memory");
    let mut rules = super::defaults();
    rules.extend(super::memory_rules(&directory));

    let index = directory.join("MEMORY.md").display().to_string();
    let topic = directory.join("topics").join("style.md").display().to_string();
    for pattern in [index.as_str(), topic.as_str()] {
        assert_eq!(
            decides(&rules, "write", pattern),
            Some(Action::Allow),
            "recording a fact must not interrupt anybody: {pattern}"
        );
        assert_eq!(decides(&rules, "edit", pattern), Some(Action::Allow));
        assert_eq!(
            decides(&rules, crate::permission::EXTERNAL_DIRECTORY, &format!("{pattern}/*")),
            Some(Action::Allow),
            "and neither must the location gate raised beside it"
        );
    }

    // One directory up is still outside the worktree, and still asks.
    let sibling = "/data/ganja/project/api-1/permissions.json";
    assert_eq!(decides(&rules, "write", sibling), None, "{sibling}");
    assert_eq!(
        decides(&rules, crate::permission::EXTERNAL_DIRECTORY, "/data/ganja/project/api-1/*"),
        Some(Action::Ask),
        "the shared default still governs everywhere else outside"
    );
}

/// And a session that never asked for memory carries no trace of it: the
/// default-off divergence, asserted where the rules are minted rather than
/// only where the prompt is composed.
#[test]
fn a_config_that_says_nothing_about_memory_opens_no_door() {
    let registry = registry(&Config::default());

    for name in [BUILD, PLAN, GENERAL, EXPLORE] {
        let rules = &registry.get(name).expect("a builtin").rules;
        assert!(
            !rules.iter().any(|rule| rule.pattern.contains("memory")),
            "{name} carries a memory rule nobody asked for: {rules:?}"
        );
    }
}

/// Who the door is written for: the agents a person runs a session as. A
/// subagent's ruleset is its own agent's, so a door in the shared defaults
/// would reach `general` and `explore` before inheritance could withhold
/// it — which is why it is not there.
#[test]
fn only_the_agents_a_person_runs_carry_the_memory_door() {
    let registry = registry(&Config { memory: Some(true), ..Config::default() });
    let holds_a_door = |name: &str| {
        registry
            .get(name)
            .expect("a builtin")
            .rules
            .iter()
            .any(|rule| rule.pattern.ends_with("memory/*"))
    };

    assert!(holds_a_door(BUILD), "the agent that acts keeps the memory");
    assert!(
        holds_a_door(PLAN),
        "and so does the other primary one, whose own write-deny still refuses it"
    );
    for name in [GENERAL, EXPLORE] {
        assert!(!holds_a_door(name), "{name} runs unwatched");
    }
}

/// What a child gets of the door: the location gate travels, because every
/// `external_directory` rule does, and the two tool allows do not. So a
/// subagent may read the memory it was shown and cannot rewrite it
/// unwatched — "inherits refusals, never allows", stated over this
/// feature's own rules.
#[test]
fn a_subagent_does_not_inherit_the_door_that_lets_memory_be_written() {
    let directory = std::path::PathBuf::from("/data/ganja/project/api-1/memory");
    let mut parent = Permissions::default();
    let mut rules = super::defaults();
    rules.extend(super::memory_rules(&directory));
    parent.set_baseline(rules);

    let child = parent.inherited_by_subagent();
    let index = directory.join("MEMORY.md").display().to_string();

    assert_eq!(
        decides(&child, "write", &index),
        None,
        "the write allow must not travel: {child:?}"
    );
    assert_eq!(decides(&child, "edit", &index), None, "nor the edit allow: {child:?}");
    assert_eq!(
        decides(&child, crate::permission::EXTERNAL_DIRECTORY, &format!("{index}/*")),
        Some(Action::Allow),
        "the location gate travels, which is what lets a child read it"
    );
}

#[test]
fn the_planning_agent_denies_both_of_the_tools_that_write() {
    let registry = registry(&Config::default());
    let rules = &registry.get(PLAN).expect("plan is builtin").rules;

    assert_eq!(decides(rules, "edit", "src/main.rs"), Some(Action::Deny));
    assert_eq!(decides(rules, "write", "src/main.rs"), Some(Action::Deny));
    assert_eq!(decides(rules, "task", GENERAL), Some(Action::Deny));
    // Reading and searching are what a plan is made of.
    assert_eq!(decides(rules, "grep", "fn main"), None);
}

#[test]
fn the_search_agent_allows_only_what_a_search_needs() {
    let registry = registry(&Config::default());
    let rules = &registry.get(EXPLORE).expect("explore is builtin").rules;

    for allowed in ["grep", "glob", "read", "webfetch", "websearch"] {
        assert_eq!(decides(rules, allowed, ANY_CALL), Some(Action::Allow));
    }
    // Shell access is undenied, not ungated.
    assert_eq!(decides(rules, "bash", "ls"), Some(Action::Ask));
    assert_eq!(decides(rules, crate::permission::EXTERNAL_DIRECTORY, "/tmp/*"), Some(Action::Ask));
    for denied in ["edit", "write", "todowrite"] {
        assert_eq!(decides(rules, denied, ANY_CALL), Some(Action::Deny));
    }
}

/// **The correction.** Upstream's `explore` allow-list names seven
/// permissions and `skill` is not among them (`agent/agent.ts:200-211`),
/// so a search agent may not load a skill — and a skill is a file of
/// instructions the model would then follow, fetched from a directory the
/// user may not have looked in. Pinned as its own case because the rule
/// enforcing it is an *absence*, and an absence is exactly what a later
/// edit adds to by accident.
#[test]
fn the_search_agent_may_not_load_a_skill() {
    let registry = registry(&Config::default());

    assert_eq!(
        decides(&registry.get(EXPLORE).expect("explore is builtin").rules, "skill", "porting"),
        Some(Action::Deny)
    );
    // And the agents upstream leaves at its `"*": "allow"` default say
    // nothing about it, so ganja's own default — not in `ASK_BY_DEFAULT`,
    // therefore allowed — is what decides.
    for name in [BUILD, PLAN, GENERAL] {
        assert_eq!(
            decides(&registry.get(name).expect("a builtin").rules, "skill", "porting"),
            None,
            "{name}"
        );
    }
}

/// A pattern that stands for whatever the tool was handed.
const ANY_CALL: &str = "*";

/// Upstream's `plan_exit: allow` delta, over the shared default deny that
/// keeps the un-ruled fallthrough — which is allow — from deciding.
#[test]
fn the_planning_agent_alone_may_leave_planning() {
    let registry = registry(&Config::default());

    assert_eq!(
        decides(&registry.get(PLAN).expect("plan is builtin").rules, "plan_exit", ANY_CALL),
        Some(Action::Allow)
    );
}

#[test]
fn the_build_agent_is_refused_the_exit_it_does_not_need() {
    let registry = registry(&Config::default());

    assert_eq!(
        decides(&registry.get(BUILD).expect("build is builtin").rules, "plan_exit", ANY_CALL),
        Some(Action::Deny)
    );
}

/// The mirror delta, on the agent upstream's `agent.ts:147-150` gives it
/// to: build alone may ask to stop and plan (**D477**).
#[test]
fn the_build_agent_alone_may_ask_to_start_planning() {
    let registry = registry(&Config::default());

    assert_eq!(
        decides(&registry.get(BUILD).expect("build is builtin").rules, "plan_enter", ANY_CALL),
        Some(Action::Allow)
    );
}

/// The shared default, which is what makes the delta above mean
/// something: every other agent is refused the door, including the one
/// already standing behind it.
#[test]
fn the_planning_agent_is_refused_the_entrance_it_is_already_through() {
    let registry = registry(&Config::default());

    for name in [PLAN, GENERAL, EXPLORE] {
        assert_eq!(
            decides(&registry.get(name).expect("a builtin").rules, "plan_enter", ANY_CALL),
            Some(Action::Deny),
            "{name}"
        );
    }
}

/// A subagent's ruleset is its own rules plus what the parent session
/// insists on — and only refusals travel down, so a child spawned from a
/// *planning* session still may not call the exit its parent is allowed.
#[test]
fn a_subagent_inherits_the_refusal_and_not_the_plan_agents_allow() {
    let registry = registry(&Config::default());
    let mut parent = Permissions::default();
    parent.set_baseline(registry.get(PLAN).expect("plan is builtin").rules.clone());

    for name in [GENERAL, EXPLORE] {
        let mut child = registry.get(name).expect("a builtin subagent").rules.clone();
        child.extend(parent.inherited_by_subagent());

        assert_eq!(decides(&child, "plan_exit", ANY_CALL), Some(Action::Deny), "{name}");
    }
}

/// And the mirror: a child spawned from a *building* session may not walk
/// through the door its parent is allowed either (**D477**). Nobody is
/// watching a subagent's turn, so nothing a subagent does may put a
/// question in front of a person or move the session it belongs to.
#[test]
fn a_subagent_inherits_the_refusal_and_not_the_build_agents_allow() {
    let registry = registry(&Config::default());
    let mut parent = Permissions::default();
    parent.set_baseline(registry.get(BUILD).expect("build is builtin").rules.clone());

    for name in [GENERAL, EXPLORE] {
        let mut child = registry.get(name).expect("a builtin subagent").rules.clone();
        child.extend(parent.inherited_by_subagent());

        assert_eq!(decides(&child, "plan_enter", ANY_CALL), Some(Action::Deny), "{name}");
    }
}

#[test]
fn a_config_rule_is_appended_after_the_agents_own() {
    let config = Config { permission: permission(json!({ "edit": "allow" })), ..Config::default() };
    let registry = registry(&config);

    assert_eq!(
        decides(&registry.get(PLAN).expect("plan").rules, "edit", "a.rs"),
        Some(Action::Allow),
        "the user's rule is the last word, even over an agent's own deny"
    );
}

#[test]
fn a_per_agent_config_rule_comes_after_the_global_one() {
    let mut agent = Definitions::new();
    agent.insert(
        PLAN.to_owned(),
        AgentConfig { permission: permission(json!({ "edit": "deny" })), ..AgentConfig::default() },
    );
    let config =
        Config { permission: permission(json!({ "edit": "allow" })), agent, ..Config::default() };

    assert_eq!(
        decides(&registry(&config).get(PLAN).expect("plan").rules, "edit", "a.rs"),
        Some(Action::Deny)
    );
}

#[test]
fn a_disabled_agent_is_gone_and_an_unknown_name_becomes_a_new_one() {
    let mut agent = Definitions::new();
    agent.insert(PLAN.to_owned(), AgentConfig { disable: Some(true), ..AgentConfig::default() });
    agent.insert(
        "reviewer".to_owned(),
        AgentConfig { prompt: Some("you review".to_owned()), ..AgentConfig::default() },
    );
    let config = Config { agent, ..Config::default() };
    let registry = registry(&config);

    assert!(registry.get(PLAN).is_none());
    let reviewer = registry.get("reviewer").expect("a config agent exists");
    assert_eq!(reviewer.mode, AgentMode::All, "upstream's default for one");
    assert_eq!(reviewer.prompt.as_deref(), Some("you review"));
    assert!(reviewer.spawnable(), "mode all is spawnable");
    assert!(reviewer.selectable(), "and selectable");
}

#[test]
fn a_config_definition_replaces_field_by_field() {
    let mut agent = Definitions::new();
    agent.insert(
        BUILD.to_owned(),
        AgentConfig {
            model: Some("anthropic/claude-haiku-4.5".to_owned()),
            hidden: Some(true),
            mode: Some(AgentMode::All),
            ..AgentConfig::default()
        },
    );
    let config = Config { agent, ..Config::default() };
    let registry = registry(&config);
    let build = registry.get(BUILD).expect("build survives being redefined");

    assert_eq!(build.model.as_deref(), Some("anthropic/claude-haiku-4.5"));
    assert!(build.hidden);
    assert_eq!(build.mode, AgentMode::All);
    assert_eq!(
        build.description.as_deref(),
        Some("The default agent. Executes tools based on configured permissions."),
        "a field the config left out keeps what it had"
    );
    assert_eq!(registry.default_agent(), PLAN, "a hidden build is not what a session starts on");
}

#[test]
fn a_named_default_agent_that_cannot_be_honoured_is_an_error() {
    let hidden = {
        let mut agent = Definitions::new();
        agent
            .insert(BUILD.to_owned(), AgentConfig { hidden: Some(true), ..AgentConfig::default() });
        agent
    };

    let cases = [
        (
            Config { default_agent: Some("nope".to_owned()), ..Config::default() },
            AgentError::Unknown { name: "nope".to_owned() },
        ),
        (
            Config { default_agent: Some(EXPLORE.to_owned()), ..Config::default() },
            AgentError::Subagent { name: EXPLORE.to_owned() },
        ),
        (
            Config { default_agent: Some(BUILD.to_owned()), agent: hidden, ..Config::default() },
            AgentError::Hidden { name: BUILD.to_owned() },
        ),
    ];

    for (config, expected) in cases {
        assert_eq!(Registry::from_dirs(&config, &[]).err(), Some(expected));
    }
}

#[test]
fn the_agent_flag_outranks_the_configured_default() {
    let config = Config {
        default_agent: Some(BUILD.to_owned()),
        overrides: Overrides { agent: Some(PLAN.to_owned()), ..Overrides::default() },
        ..Config::default()
    };

    assert_eq!(registry(&config).default_agent(), PLAN);

    // And it is validated the same way the config key is.
    let config = Config {
        overrides: Overrides { agent: Some("nope".to_owned()), ..Overrides::default() },
        ..Config::default()
    };
    assert_eq!(
        Registry::from_dirs(&config, &[]).err(),
        Some(AgentError::Unknown { name: "nope".to_owned() })
    );
}

#[test]
fn a_config_that_leaves_nothing_visible_refuses_to_resolve() {
    let mut agent = Definitions::new();
    for name in [BUILD, PLAN] {
        agent
            .insert(name.to_owned(), AgentConfig { disable: Some(true), ..AgentConfig::default() });
    }
    let config = Config { agent, ..Config::default() };

    assert_eq!(Registry::from_dirs(&config, &[]).err(), Some(AgentError::NoneVisible));
}

// ---- Agent definition files (**D482**) ------------------------------
//
// Every case here hands the directories in rather than resolving them from
// a root, so none of it reads — or depends on the emptiness of — the
// config home of whoever is running the suite. That the two homes are the
// ones resolved is `tests/agent_files.rs`, which owns `GANJA_CONFIG_HOME`
// in a binary of its own.

/// A definition directory holding `files`, as `(file name, contents)`.
fn homes(files: &[(&str, &str)]) -> (tempfile::TempDir, Vec<std::path::PathBuf>) {
    let directory = tempfile::tempdir().expect("a temporary directory");
    for (name, contents) in files {
        std::fs::write(directory.path().join(name), contents).expect("the fixture is written");
    }
    let path = vec![directory.path().to_path_buf()];

    (directory, path)
}

/// The registry `config` resolves with `files` in its one definition
/// directory.
fn with_files(config: &Config, files: &[(&str, &str)]) -> (tempfile::TempDir, Registry) {
    let (directory, dirs) = homes(files);
    let registry = Registry::from_dirs(config, &dirs).expect("the fixture resolves an agent");

    (directory, registry)
}

/// The whole shape in one file: a name it does not state, a description
/// and a model it does, and a body that becomes the prompt.
#[test]
fn a_definition_file_is_an_agent_named_after_it() {
    let (_directory, registry) = with_files(
        &Config::default(),
        &[(
            "reviewer.md",
            "---\ndescription: Reviews a diff.\nmodel: anthropic/claude-haiku-4.5\n---\n\
                 You review changes and say what is wrong with them.\n",
        )],
    );
    let reviewer = registry.get("reviewer").expect("the file defines an agent");

    assert_eq!(reviewer.description.as_deref(), Some("Reviews a diff."));
    assert_eq!(reviewer.model.as_deref(), Some("anthropic/claude-haiku-4.5"));
    assert_eq!(
        reviewer.prompt.as_deref(),
        Some("You review changes and say what is wrong with them."),
        "the body is the prompt, and a prompt replaces the base one"
    );
    assert_eq!(reviewer.mode, AgentMode::All, "the same default a config-defined agent gets");
    assert!(reviewer.selectable(), "so a person may switch to it");
    assert!(reviewer.spawnable(), "and the task tool may spawn it");
}

/// A `description` written as a block scalar keeps every line of itself.
///
/// The reader here handles `|`, `|-`, `>` and `>-` — seventeen lines of it
/// — and no fixture in this crate had ever fed it one, so the whole branch
/// could be deleted, or quietly swapped for `command.rs`'s simpler reader,
/// with a green suite. What that would change is what an agent file's
/// `description:` *means*, which is the roster line a person picks an
/// agent from and the sentence the task tool judges one by.
#[test]
fn a_definition_files_block_scalar_description_keeps_its_whole_text() {
    let (_directory, registry) = with_files(
        &Config::default(),
        &[
            ("literal.md", "---\ndescription: |\n  first\n  second\n---\nBe brief.\n"),
            ("folded.md", "---\ndescription: >-\n  first\n  second\n---\nBe brief.\n"),
        ],
    );

    assert_eq!(
        registry.get("literal").expect("the file defines an agent").description.as_deref(),
        Some("first\nsecond"),
        "a literal block keeps its line breaks"
    );
    assert_eq!(
        registry.get("folded").expect("the file defines an agent").description.as_deref(),
        Some("first second"),
        "and a folded one joins them with a space"
    );
}

/// The frontmatter's own `name` outranks the file it is in, and the
/// vocabulary for `mode` and `hidden` is the config's.
#[test]
fn a_stated_name_outranks_the_file_name() {
    let (_directory, registry) = with_files(
        &Config::default(),
        &[("any-file-name.md", "---\nname: auditor\nmode: subagent\nhidden: true\n---\nAudit.\n")],
    );
    let auditor = registry.get("auditor").expect("the frontmatter named it");

    assert!(registry.get("any-file-name").is_none());
    assert_eq!(auditor.mode, AgentMode::Subagent);
    assert!(auditor.hidden);
}

/// The refusals, each of them a skip that names the file and leaves the
/// session startable.
#[test]
fn a_definition_nobody_could_act_on_is_skipped_with_a_reason() {
    let cases = [
        ("---\nname: team/reviewer\n---\nbody", "path separator", "holds a path separator"),
        ("---\nmode: occasionally\n---\nbody", "an unknown mode", "`primary`, `subagent`, `all`"),
        ("---\nhidden: sometimes\n---\nbody", "an unknown hidden", "`true` or `false`"),
    ];

    for (text, what, expected) in cases {
        let reason = super::Definition::parse(Path::new("/agents/a.md"), text)
            .expect_err(&format!("{what} is refused"));
        assert!(reason.contains(expected), "{what}: {reason}");
    }
}

/// **AC5.** Claude's `opus`/`sonnet`/`haiku` name nothing this build can
/// resolve, so a file carrying one is refused by name rather than
/// silently ignored — and the message says what a model id looks like
/// here.
#[test]
fn a_model_alias_is_refused_and_the_full_form_is_named() {
    let reason =
        super::Definition::parse(Path::new("/agents/reviewer.md"), "---\nmodel: opus\n---\nbody")
            .expect_err("an alias is not a model id");

    assert!(reason.contains("opus"), "{reason}");
    assert!(reason.contains("provider/model"), "{reason}");
    assert!(reason.contains("anthropic/"), "{reason}");

    // And the session still starts: the file is skipped, nothing else is.
    let (_directory, registry) =
        with_files(&Config::default(), &[("reviewer.md", "---\nmodel: opus\n---\nbody")]);
    assert!(registry.get("reviewer").is_none());
    assert_eq!(registry.default_agent(), BUILD);
}

/// Two files in one directory cannot both be `reviewer`; the first by file
/// name keeps it. (Across *directories* the later one wins, which is the
/// precedence case below.)
#[test]
fn a_second_file_claiming_a_name_in_one_directory_is_skipped() {
    let (_directory, registry) = with_files(
        &Config::default(),
        &[
            ("a-reviewer.md", "---\nname: reviewer\n---\nfirst"),
            ("b-reviewer.md", "---\nname: reviewer\n---\nsecond"),
        ],
    );

    assert_eq!(registry.get("reviewer").expect("one of them won").prompt, Some("first".to_owned()));
}

/// **AC4, at the rules.** A listed tool is allowed, an unlisted one is
/// denied — and denied is not hidden: the call still reaches the gate.
#[test]
fn a_tools_list_allows_what_it_names_and_denies_the_rest() {
    let (_directory, registry) = with_files(
        &Config::default(),
        &[("searcher.md", "---\ntools: read, grep\n---\nYou search and report.\n")],
    );
    let rules = &registry.get("searcher").expect("the file defines it").rules;

    for allowed in ["read", "grep"] {
        assert_eq!(decides(rules, allowed, ANY_CALL), Some(Action::Allow));
    }
    for denied in ["edit", "write", "bash", "task", "todowrite", "skill"] {
        assert_eq!(decides(rules, denied, ANY_CALL), Some(Action::Deny), "{denied}");
    }
    assert_eq!(
        decides(rules, "mcp__docs__search", ANY_CALL),
        Some(Action::Deny),
        "a namespace whose names are unknown until a server is dialled is closed by glob"
    );
    assert_eq!(
        decides(rules, crate::permission::EXTERNAL_DIRECTORY, "/tmp/*"),
        Some(Action::Ask),
        "and the location gate is left exactly where it was"
    );
}

/// The guard: a wall never closes the three tools a *conversation* is made
/// of. A `tools:` line on a file named after the build agent must not seal
/// the one door that agent has.
#[test]
fn a_tools_list_leaves_the_conversation_tools_where_it_found_them() {
    let (_directory, registry) = with_files(
        &Config::default(),
        &[("build.md", "---\ntools: read, plan_exit\n---\nYou build carefully.\n")],
    );
    let rules = &registry.get(BUILD).expect("the file overlaid a builtin").rules;

    assert_eq!(
        decides(rules, "plan_enter", ANY_CALL),
        Some(Action::Allow),
        "build's own door survives a roster that does not mention it"
    );
    assert_eq!(
        decides(rules, "plan_exit", ANY_CALL),
        Some(Action::Deny),
        "and listing a door neither opens it: the shared default still decides"
    );
    assert_eq!(
        decides(rules, "question", ANY_CALL),
        None,
        "asking the person watching is not work a roster describes"
    );
    assert_eq!(decides(rules, "edit", ANY_CALL), Some(Action::Deny));
}

/// No `tools:` key at all is today's semantics, unchanged: an agent with
/// the whole roster and not one rule about it.
#[test]
fn a_file_that_states_no_tools_builds_no_wall() {
    let (_directory, registry) =
        with_files(&Config::default(), &[("reviewer.md", "---\ndescription: r\n---\nReview.\n")]);
    let rules = &registry.get("reviewer").expect("the file defines it").rules;

    for tool in ["edit", "write", "bash", "task", "read"] {
        assert_eq!(
            decides(rules, tool, ANY_CALL),
            None,
            "{tool} is decided by the shared defaults, as it always was"
        );
    }
}

/// **AC4's second half.** A subagent spawned by a restricted agent runs
/// under the parent's refusals: the wall travels down, the allows do not.
#[test]
fn a_subagent_inherits_the_wall_a_tools_list_built() {
    let (_directory, registry) =
        with_files(&Config::default(), &[("searcher.md", "---\ntools: read\n---\nSearch.\n")]);
    let mut parent = Permissions::default();
    parent.set_baseline(registry.get("searcher").expect("the file defines it").rules.clone());

    let mut child = registry.get(GENERAL).expect("general is a builtin subagent").rules.clone();
    child.extend(parent.inherited_by_subagent());

    assert_eq!(
        decides(&child, "edit", ANY_CALL),
        Some(Action::Deny),
        "what the parent may not do, a child it spawns may not do either"
    );
    assert_eq!(
        decides(&child, "read", ANY_CALL),
        None,
        "and the parent's allow does not travel: the child is back on the defaults"
    );
}

/// Precedence, pinned pairwise: the project directory wins the name over
/// the global one, and the config wins over both — field by field.
#[test]
fn a_project_file_outranks_a_global_one_and_the_config_outranks_both() {
    let global = tempfile::tempdir().expect("a temporary directory");
    let project = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        global.path().join("reviewer.md"),
        "---\ndescription: the global one\nmodel: anthropic/claude-haiku-4.5\n---\nglobal",
    )
    .expect("the fixture is written");
    std::fs::write(
        project.path().join("reviewer.md"),
        "---\ndescription: the project one\n---\nproject",
    )
    .expect("the fixture is written");
    let dirs = vec![global.path().to_path_buf(), project.path().to_path_buf()];

    let registry =
        Registry::from_dirs(&Config::default(), &dirs).expect("the fixture resolves an agent");
    let reviewer = registry.get("reviewer").expect("both files define it");
    assert_eq!(reviewer.description.as_deref(), Some("the project one"));
    assert_eq!(reviewer.prompt.as_deref(), Some("project"));
    assert_eq!(
        reviewer.model.as_deref(),
        Some("anthropic/claude-haiku-4.5"),
        "a field the project file left out keeps what the global one said"
    );

    let mut agent = Definitions::new();
    agent.insert(
        "reviewer".to_owned(),
        AgentConfig {
            description: Some("the configured one".to_owned()),
            ..AgentConfig::default()
        },
    );
    let config = Config { agent, ..Config::default() };
    let reviewer = Registry::from_dirs(&config, &dirs)
        .expect("the fixture resolves an agent")
        .get("reviewer")
        .cloned()
        .expect("the config kept it");
    assert_eq!(reviewer.description.as_deref(), Some("the configured one"));
    assert_eq!(
        reviewer.prompt.as_deref(),
        Some("project"),
        "and the config states nothing about the prompt, so the file's survives"
    );
}

/// **AC3's escape hatch.** The config is the last word, including the word
/// "no": a `disable: true` removes a file agent outright.
#[test]
fn a_config_disable_removes_a_file_agent() {
    let mut agent = Definitions::new();
    agent.insert(
        "reviewer".to_owned(),
        AgentConfig { disable: Some(true), ..AgentConfig::default() },
    );
    let config = Config { agent, ..Config::default() };
    let (_directory, dirs) = homes(&[("reviewer.md", "---\ndescription: r\n---\nReview.")]);

    let registry = Registry::from_dirs(&config, &dirs).expect("the builtins are still there");
    assert!(registry.get("reviewer").is_none());
}

/// What a directory of hostile files costs: nothing. Every one of them is
/// skipped and the agents around them resolve.
#[test]
fn a_directory_of_unreadable_files_leaves_the_session_startable() {
    let (_directory, registry) = with_files(
        &Config::default(),
        &[
            ("empty.md", ""),
            ("unterminated.md", "---\nname: nope\ndescription: no closing fence\n"),
            ("binary.md", "\u{0}\u{1}\u{2}not markdown"),
            ("not-markdown.txt", "---\nname: ignored\n---\nbody"),
            ("good.md", "---\ndescription: fine\n---\nWork.\n"),
        ],
    );

    assert!(registry.get("good").is_some(), "the readable one still lands");
    assert!(registry.get("ignored").is_none(), "a file that is not `.md` is not a definition");
    // A file with no frontmatter at all is not hostile, only terse: it is
    // an agent named after itself whose whole text is its prompt.
    for name in ["empty", "unterminated", "binary"] {
        let agent = registry.get(name).expect("named after its file");
        assert!(agent.description.is_none(), "{name}");
    }
    assert_eq!(registry.default_agent(), BUILD);
}

/// The one parser detail worth its own case: how a `tools:` value is
/// split, in every spelling a person or an editor writes it in.
#[test]
fn a_tools_value_is_read_in_every_spelling_it_is_written_in() {
    for value in ["read, grep", "read grep", "read,grep", "[read, grep]", "\"read\", 'grep'"] {
        assert_eq!(super::tool_names(value), vec!["read".to_owned(), "grep".to_owned()], "{value}");
    }
    assert!(super::tool_names("  ").is_empty());
}
