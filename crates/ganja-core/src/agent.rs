//! Who the model is being asked to be: its prompt, and what it may do.
//!
//! Spec: upstream `packages/opencode/src/agent/agent.ts` (the v1 stack; the
//! `packages/core` mirror is not the one the session pipeline runs).
//!
//! An agent is four things a turn reads: a **prompt** that replaces the base
//! one for the model's family, a **model** it prefers, a **mode** saying who
//! may run it, and a **ruleset** that is the entire tool-enable mechanism —
//! upstream has no `tools` field at runtime, and neither does this.
//!
//! The rules of every agent are assembled in one order, and the order *is* the
//! precedence, because evaluation is last-match-wins ([`crate::permission`]):
//!
//! ```text
//! shared defaults  <  the agent's own delta  <  config `permission`  <  the
//! agent's own config `permission`   …and, at the session, the stored rules
//! ```
//!
//! The last tier lives in [`crate::permission::Permissions`], layered over
//! whatever [`Agent::rules`] an engine installs as its baseline.
//!
//! # What is adapted, and why
//!
//! Upstream's defaults open with `"*": "allow"`, which is the whole reason its
//! `build` agent runs a shell command without asking. Ganja's baseline is the
//! other way round — [`crate::permission::ASK_BY_DEFAULT`] names the tools that
//! change the world, and everything else already runs unasked — so porting that
//! rule literally would erase P3's hardening in one line. It is therefore *not*
//! emitted: ganja's ask-by-default table is the `"*": "allow"`-equivalent, and
//! the two agree everywhere except on the tools ganja deliberately gates.
//!
//! Upstream rules naming permissions this build has no tool for — `list`,
//! `question`, `doom_loop`, `plan_enter`, `plan_exit`, `lsp` — are not ported.
//! A rule about a tool that cannot be called decides nothing, and carrying it
//! would suggest the tool exists. The one exception is `task`, kept on [`PLAN`]
//! so that the agent whose point is "do not act" already denies the subagent
//! that would act for it, the day the task tool lands. `websearch` came off
//! that list with the tool: [`EXPLORE`] allows it, exactly as upstream's does.
//!
//! Upstream also *hides* a tool from the model's schema when the last rule
//! matching it is `"*": "deny"` (`permission/index.ts`, `disabled`). That is
//! not ported: a denied call still reaches the gate and comes back as a
//! refusal the model reads, which is the same outcome by the route this port
//! already has for every other refusal.

use crate::{
    config::{AgentConfig, AgentMode, Config},
    permission::{Action, EXTERNAL_DIRECTORY, Rule},
};

/// The pattern that covers every call to a permission.
const ANY: &str = "*";

/// Name of the agent that may act. Upstream keys the plan/build reminders off
/// these two names rather than off a mode, and so does this.
pub const BUILD: &str = "build";

/// Name of the read-only agent.
pub const PLAN: &str = "plan";

/// Name of the general-purpose subagent.
pub const GENERAL: &str = "general";

/// Name of the search subagent.
pub const EXPLORE: &str = "explore";

/// The search subagent's prompt, ported verbatim (MIT; see
/// `THIRD_PARTY_NOTICES.md`).
const EXPLORE_PROMPT: &str = include_str!("prompt/explore.txt");

/// Injected as a synthetic user part on every turn the [`PLAN`] agent runs,
/// ported verbatim from upstream `session/prompt/plan.txt`.
pub const PLAN_REMINDER: &str = include_str!("prompt/plan.txt");

/// Injected once when a session that was planning starts building, ported
/// verbatim from upstream `session/prompt/build-switch.txt`.
pub const BUILD_SWITCH_REMINDER: &str = include_str!("prompt/build-switch.txt");

/// An agent could not be resolved.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentError {
    /// A name was configured and no agent answers to it.
    #[error("agent \"{name}\" does not exist")]
    Unknown {
        /// What was asked for.
        name: String,
    },
    /// A name was configured and names a subagent, which only the task tool
    /// may run.
    #[error("agent \"{name}\" is a subagent")]
    Subagent {
        /// What was asked for.
        name: String,
    },
    /// A name was configured and names a hidden agent.
    #[error("agent \"{name}\" is hidden")]
    Hidden {
        /// What was asked for.
        name: String,
    },
    /// Every agent left is a subagent or hidden, so a session has nothing to
    /// start on. Upstream's `no primary visible agent found`; reachable only
    /// by disabling or hiding the builtins.
    #[error("no visible primary agent is configured")]
    NoneVisible,
}

/// One agent, resolved: everything a turn running as it needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    /// What it is called, in a picker and in `default_agent`.
    pub name: String,
    /// One line the task tool shows the model when it lists the agents it may
    /// spawn.
    pub description: Option<String>,
    /// Who may run it.
    pub mode: AgentMode,
    /// Hidden agents exist and run; they are only left out of the pickers, of
    /// `default_agent`, and of whatever cycles through agents. Upstream's
    /// `hidden` is exactly this narrow.
    pub hidden: bool,
    /// System prompt, which **replaces** the base prompt for the model's
    /// family rather than adding to it.
    pub prompt: Option<String>,
    /// Model it prefers. Switching to an agent that names one switches the
    /// model with it, which is upstream's behaviour.
    pub model: Option<String>,
    /// Rules this agent's calls are judged by, beneath whatever the session
    /// has stored. Assembled in precedence order; see the module docs.
    pub rules: Vec<Rule>,
}

impl Agent {
    /// Whether the task tool may spawn this one — upstream's
    /// `mode !== "primary"` (`tool/registry.ts`), so `all` qualifies too.
    #[must_use]
    pub fn spawnable(&self) -> bool {
        self.mode != AgentMode::Primary
    }

    /// Whether a user may switch to it: not a subagent, and not hidden.
    #[must_use]
    pub fn selectable(&self) -> bool {
        self.mode != AgentMode::Subagent && !self.hidden
    }
}

/// Every agent a session may run as, in the order they were defined.
///
/// Order is load-bearing twice: it decides which agent a config with no
/// `default_agent` starts on, and it is the order a picker lists. The builtins
/// come first, `build` at the front, and config-defined agents follow.
#[derive(Clone, Debug)]
pub struct Registry {
    agents: Vec<Agent>,
    default: String,
}

impl Registry {
    /// The agents `config` describes: the builtins, overlaid with its `agent`
    /// map.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when `default_agent` — or the `--agent` flag
    /// above it — names an agent that does not exist, is a subagent, or is
    /// hidden, and when nothing visible is left to start on. Upstream throws in
    /// exactly these four places (`agent.ts`, `defaultInfo`).
    pub fn build(config: &Config) -> Result<Self, AgentError> {
        let mut agents = builtins(config);
        overlay(&mut agents, config);

        let default = resolve_default(&agents, config)?;

        Ok(Self { agents, default })
    }

    /// A registry holding exactly `agents`, starting on the first selectable
    /// one.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::NoneVisible`] when none of them is selectable.
    pub fn new(agents: Vec<Agent>) -> Result<Self, AgentError> {
        let default = agents
            .iter()
            .find(|agent| agent.selectable())
            .map(|agent| agent.name.clone())
            .ok_or(AgentError::NoneVisible)?;

        Ok(Self { agents, default })
    }

    /// The agent named `name`, or nothing.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    /// Every agent, in definition order.
    #[must_use]
    pub fn agents(&self) -> &[Agent] {
        &self.agents
    }

    /// The agent a session starts on.
    #[must_use]
    pub fn default_agent(&self) -> &str {
        &self.default
    }
}

/// The seven-agent roster upstream ships, minus the three ganja does not have.
///
/// `compaction`, `title` and `summary` are hidden agents upstream because its
/// pipeline runs *every* request through an agent. Ganja's title and
/// compaction requests are direct — they carry their own prompt and no tools —
/// so there is nothing for an agent to add (**D7**).
fn builtins(config: &Config) -> Vec<Agent> {
    let user = config.permission.rules();
    let assemble = |own: Vec<Rule>| {
        let mut rules = defaults();
        rules.extend(own);
        rules.extend(user.iter().cloned());
        rules
    };

    vec![
        Agent {
            name: BUILD.to_owned(),
            description: Some(
                "The default agent. Executes tools based on configured permissions.".to_owned(),
            ),
            mode: AgentMode::Primary,
            hidden: false,
            prompt: None,
            model: None,
            // Upstream's delta is `question: allow` and `plan_enter: allow`.
            // `question` has a tool behind it now and `ask` is what an
            // interactive session wants for it; `plan_enter` still names
            // nothing. Either way the delta is not adopted, so the baseline
            // stays what it was.
            rules: assemble(Vec::new()),
        },
        Agent {
            name: PLAN.to_owned(),
            description: Some("Plan mode. Disallows all edit tools.".to_owned()),
            mode: AgentMode::Primary,
            hidden: false,
            // Upstream's plan agent has no prompt of its own: what makes it
            // plan is the reminder injected on every one of its turns.
            prompt: None,
            model: None,
            rules: assemble(vec![
                // Inert until the task tool exists, and kept anyway: the rule
                // that stops a planning session delegating its way into an
                // edit should already be there when the tool arrives.
                rule("task", GENERAL, Action::Deny),
                // Upstream denies `edit` and then carves two exceptions for
                // the file a plan is written to. Ganja has no plans directory
                // — a plan is prose in the transcript — so the deny stands
                // alone. `write` is denied beside it because upstream's rule
                // reaches it through an alias table (`edit|write|apply_patch`)
                // that this port does not have (deviation: plan-denies-write).
                rule("edit", ANY, Action::Deny),
                rule("write", ANY, Action::Deny),
            ]),
        },
        Agent {
            name: GENERAL.to_owned(),
            description: Some(
                "General-purpose agent for researching complex questions and executing \
                 multi-step tasks. Use this agent to execute multiple units of work in parallel."
                    .to_owned(),
            ),
            mode: AgentMode::Subagent,
            hidden: false,
            prompt: None,
            model: None,
            rules: assemble(vec![rule("todowrite", ANY, Action::Deny)]),
        },
        Agent {
            name: EXPLORE.to_owned(),
            description: Some(
                "Fast agent specialized for exploring codebases. Use this when you need to \
                 quickly find files by patterns (eg. \"src/components/**/*.tsx\"), search code \
                 for keywords (eg. \"API endpoints\"), or answer questions about the codebase \
                 (eg. \"how do API endpoints work?\"). When calling this agent, specify the \
                 desired thoroughness level: \"quick\" for basic searches, \"medium\" for \
                 moderate exploration, or \"very thorough\" for comprehensive analysis across \
                 multiple locations and naming conventions."
                    .to_owned(),
            ),
            mode: AgentMode::Subagent,
            hidden: false,
            prompt: Some(EXPLORE_PROMPT.to_owned()),
            model: None,
            rules: assemble(vec![
                // An allow-list, spelled the way upstream spells it: deny
                // everything, then name what is left. `list` is in upstream's
                // list and has no tool here.
                //
                // **`skill` is not in that list**, and its absence is the
                // whole point of the deny above: upstream allows this agent
                // seven permissions and `skill` is not one of them
                // (`agent/agent.ts:200-211`), so a search agent cannot load a
                // skill and act on instructions nobody in this session read.
                // The blanket deny is what enforces that, which is why no
                // `skill` rule appears below — a rule saying `deny` here would
                // read as the *only* thing stopping it.
                rule(ANY, ANY, Action::Deny),
                rule("grep", ANY, Action::Allow),
                rule("glob", ANY, Action::Allow),
                rule("read", ANY, Action::Allow),
                rule("webfetch", ANY, Action::Allow),
                rule("websearch", ANY, Action::Allow),
                // Upstream writes `bash: "allow"`, which on top of its
                // `"*": "allow"` default changes nothing there and would
                // change a great deal here: it would hand a subagent
                // unattended shell access that no other ganja agent has.
                // What the rule has to do is undo the blanket deny above, and
                // `ask` does that while leaving the gate exactly where every
                // other agent has it (deviation: explore-bash-asks).
                rule("bash", ANY, Action::Ask),
                // Upstream's `readonlyExternalDirectory`, which is the shared
                // default put back after the blanket deny; without it a
                // command naming somewhere outside the project would be
                // refused outright rather than asked about.
                rule(EXTERNAL_DIRECTORY, ANY, Action::Ask),
            ]),
        },
    ]
}

/// The ruleset every agent starts from, ported from upstream's `defaults`.
///
/// Upstream's `"*": "allow"` is deliberately absent — see the module docs. So
/// are its whitelisted directories: they name upstream's temporary, skill and
/// reference directories, none of which this build has.
fn defaults() -> Vec<Rule> {
    vec![
        // Ganja already asks about this one by default; written out because
        // the agents below turn it off and on again, and a rule that is only
        // implied cannot be put back.
        rule(EXTERNAL_DIRECTORY, ANY, Action::Ask),
        // Upstream's comment: mirrors github/gitignore's Node.gitignore
        // patterns for .env files. Reading is otherwise free, so this is the
        // one place a read stops to ask.
        rule("read", "*.env", Action::Ask),
        rule("read", "*.env.*", Action::Ask),
        rule("read", "*.env.example", Action::Allow),
    ]
}

/// One rule, spelled out.
fn rule(permission: &str, pattern: &str, action: Action) -> Rule {
    Rule {
        permission: permission.to_owned(),
        pattern: pattern.to_owned(),
        action,
    }
}

/// Applies `config.agent` to `agents`, upstream's overlay loop
/// (`agent.ts:267-294`).
///
/// A disabled agent is removed; a name nothing answers to becomes a new agent
/// in mode `all`; every field the config states replaces the one below it, and
/// its rules are **appended**, so a config can only ever have the last word.
///
/// Upstream iterates its config object in insertion order. Ganja's `agent` map
/// is sorted by name, so two config-defined agents are defined alphabetically
/// (deviation: config-agent-order) — which decides only their order in a
/// picker, and which of them a config with no `default_agent` would start on
/// if every builtin were disabled.
fn overlay(agents: &mut Vec<Agent>, config: &Config) {
    for (name, definition) in &config.agent {
        if definition.disable == Some(true) {
            agents.retain(|agent| agent.name != *name);
            continue;
        }

        let position = match agents.iter().position(|agent| agent.name == *name) {
            Some(position) => position,
            None => {
                agents.push(fresh(name, config));
                agents.len() - 1
            }
        };
        apply(&mut agents[position], definition);
    }
}

/// An agent a config named and no builtin answers to, in upstream's shape: mode
/// `all`, and nothing but the shared defaults and the user's own rules under it.
fn fresh(name: &str, config: &Config) -> Agent {
    let mut rules = defaults();
    rules.extend(config.permission.rules());

    Agent {
        name: name.to_owned(),
        description: None,
        mode: AgentMode::All,
        hidden: false,
        prompt: None,
        model: None,
        rules,
    }
}

/// Overlays one config definition onto a resolved agent.
fn apply(agent: &mut Agent, definition: &AgentConfig) {
    if let Some(model) = &definition.model {
        agent.model = Some(model.clone());
    }
    if let Some(prompt) = &definition.prompt {
        agent.prompt = Some(prompt.clone());
    }
    if let Some(description) = &definition.description {
        agent.description = Some(description.clone());
    }
    if let Some(mode) = definition.mode {
        agent.mode = mode;
    }
    if let Some(hidden) = definition.hidden {
        agent.hidden = hidden;
    }

    // Appended rather than merged: upstream's `Permission.merge` is
    // concatenation and its order is its precedence, so a config's rule about
    // a call the agent already had a rule about simply comes later.
    agent.rules.extend(definition.permission.rules());
}

/// Which agent a session starts on.
///
/// The `--agent` flag outranks `default_agent`, which outranks the first
/// selectable agent there is. A name that was asked for and cannot be honoured
/// is an error rather than a fallback: silently starting on a different agent
/// than the one named would be a different session than the one asked for.
fn resolve_default(agents: &[Agent], config: &Config) -> Result<String, AgentError> {
    let named = config
        .overrides
        .agent
        .as_deref()
        .or(config.default_agent.as_deref());

    let Some(name) = named else {
        return agents
            .iter()
            .find(|agent| agent.selectable())
            .map(|agent| agent.name.clone())
            .ok_or(AgentError::NoneVisible);
    };

    let Some(agent) = agents.iter().find(|agent| agent.name == name) else {
        return Err(AgentError::Unknown {
            name: name.to_owned(),
        });
    };
    if agent.mode == AgentMode::Subagent {
        return Err(AgentError::Subagent {
            name: name.to_owned(),
        });
    }
    if agent.hidden {
        return Err(AgentError::Hidden {
            name: name.to_owned(),
        });
    }

    Ok(agent.name.clone())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{Agent, AgentError, BUILD, EXPLORE, EXPLORE_PROMPT, GENERAL, PLAN, Registry};
    use crate::{
        config::{AgentConfig, AgentMode, Config, Overrides},
        permission::{Action, PermissionConfig, Rule},
    };

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

    fn registry(config: &Config) -> Registry {
        Registry::build(config).expect("the fixture config resolves an agent")
    }

    #[test]
    fn the_builtins_are_the_four_upstream_agents_this_build_can_run() {
        let registry = registry(&Config::default());
        let names: Vec<&str> = registry
            .agents()
            .iter()
            .map(|agent| agent.name.as_str())
            .collect();

        assert_eq!(names, vec![BUILD, PLAN, GENERAL, EXPLORE]);
        assert_eq!(registry.default_agent(), BUILD);
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
            ]
        );
    }

    #[test]
    fn the_search_agent_carries_upstreams_prompt_verbatim() {
        let registry = registry(&Config::default());
        let explore = registry.get(EXPLORE).expect("the search agent is builtin");

        assert_eq!(explore.prompt.as_deref(), Some(EXPLORE_PROMPT));
        assert!(EXPLORE_PROMPT.starts_with("You are a file search specialist."));
        assert!(
            registry
                .get(BUILD)
                .expect("build is builtin")
                .prompt
                .is_none()
        );
    }

    /// The `.env` rules are the shared default every agent inherits, and the
    /// example file is the exception that comes after them.
    #[test]
    fn every_agent_stops_to_ask_before_reading_a_dotenv_file() {
        let registry = registry(&Config::default());

        for name in [BUILD, PLAN, GENERAL] {
            let rules = &registry.get(name).expect("a builtin").rules;
            assert_eq!(decides(rules, "read", ".env"), Some(Action::Ask), "{name}");
            assert_eq!(
                decides(rules, "read", "config/.env.local"),
                Some(Action::Ask),
                "{name}"
            );
            assert_eq!(
                decides(rules, "read", ".env.example"),
                Some(Action::Allow),
                "{name}"
            );
            assert_eq!(decides(rules, "read", "src/main.rs"), None, "{name}");
        }
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
        assert_eq!(
            decides(rules, crate::permission::EXTERNAL_DIRECTORY, "/tmp/*"),
            Some(Action::Ask)
        );
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
            decides(
                &registry.get(EXPLORE).expect("explore is builtin").rules,
                "skill",
                "porting"
            ),
            Some(Action::Deny)
        );
        // And the agents upstream leaves at its `"*": "allow"` default say
        // nothing about it, so ganja's own default — not in `ASK_BY_DEFAULT`,
        // therefore allowed — is what decides.
        for name in [BUILD, PLAN, GENERAL] {
            assert_eq!(
                decides(
                    &registry.get(name).expect("a builtin").rules,
                    "skill",
                    "porting"
                ),
                None,
                "{name}"
            );
        }
    }

    /// A pattern that stands for whatever the tool was handed.
    const ANY_CALL: &str = "*";

    #[test]
    fn a_config_rule_is_appended_after_the_agents_own() {
        let config = Config {
            permission: permission(json!({ "edit": "allow" })),
            ..Config::default()
        };
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
            AgentConfig {
                permission: permission(json!({ "edit": "deny" })),
                ..AgentConfig::default()
            },
        );
        let config = Config {
            permission: permission(json!({ "edit": "allow" })),
            agent,
            ..Config::default()
        };

        assert_eq!(
            decides(
                &registry(&config).get(PLAN).expect("plan").rules,
                "edit",
                "a.rs"
            ),
            Some(Action::Deny)
        );
    }

    #[test]
    fn a_disabled_agent_is_gone_and_an_unknown_name_becomes_a_new_one() {
        let mut agent = Definitions::new();
        agent.insert(
            PLAN.to_owned(),
            AgentConfig {
                disable: Some(true),
                ..AgentConfig::default()
            },
        );
        agent.insert(
            "reviewer".to_owned(),
            AgentConfig {
                prompt: Some("you review".to_owned()),
                ..AgentConfig::default()
            },
        );
        let config = Config {
            agent,
            ..Config::default()
        };
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
        let config = Config {
            agent,
            ..Config::default()
        };
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
        assert_eq!(
            registry.default_agent(),
            PLAN,
            "a hidden build is not what a session starts on"
        );
    }

    #[test]
    fn a_named_default_agent_that_cannot_be_honoured_is_an_error() {
        let hidden = {
            let mut agent = Definitions::new();
            agent.insert(
                BUILD.to_owned(),
                AgentConfig {
                    hidden: Some(true),
                    ..AgentConfig::default()
                },
            );
            agent
        };

        let cases = [
            (
                Config {
                    default_agent: Some("nope".to_owned()),
                    ..Config::default()
                },
                AgentError::Unknown {
                    name: "nope".to_owned(),
                },
            ),
            (
                Config {
                    default_agent: Some(EXPLORE.to_owned()),
                    ..Config::default()
                },
                AgentError::Subagent {
                    name: EXPLORE.to_owned(),
                },
            ),
            (
                Config {
                    default_agent: Some(BUILD.to_owned()),
                    agent: hidden,
                    ..Config::default()
                },
                AgentError::Hidden {
                    name: BUILD.to_owned(),
                },
            ),
        ];

        for (config, expected) in cases {
            assert_eq!(Registry::build(&config).err(), Some(expected));
        }
    }

    #[test]
    fn the_agent_flag_outranks_the_configured_default() {
        let config = Config {
            default_agent: Some(BUILD.to_owned()),
            overrides: Overrides {
                agent: Some(PLAN.to_owned()),
                ..Overrides::default()
            },
            ..Config::default()
        };

        assert_eq!(registry(&config).default_agent(), PLAN);

        // And it is validated the same way the config key is.
        let config = Config {
            overrides: Overrides {
                agent: Some("nope".to_owned()),
                ..Overrides::default()
            },
            ..Config::default()
        };
        assert_eq!(
            Registry::build(&config).err(),
            Some(AgentError::Unknown {
                name: "nope".to_owned()
            })
        );
    }

    #[test]
    fn a_config_that_leaves_nothing_visible_refuses_to_resolve() {
        let mut agent = Definitions::new();
        for name in [BUILD, PLAN] {
            agent.insert(
                name.to_owned(),
                AgentConfig {
                    disable: Some(true),
                    ..AgentConfig::default()
                },
            );
        }
        let config = Config {
            agent,
            ..Config::default()
        };

        assert_eq!(
            Registry::build(&config).err(),
            Some(AgentError::NoneVisible)
        );
    }

    #[test]
    fn a_registry_can_be_built_from_agents_alone() {
        let agents = vec![Agent {
            name: "solo".to_owned(),
            description: None,
            mode: AgentMode::Primary,
            hidden: false,
            prompt: Some("be brief".to_owned()),
            model: None,
            rules: Vec::new(),
        }];

        let registry = Registry::new(agents).expect("one selectable agent is enough");
        assert_eq!(registry.default_agent(), "solo");
        assert!(Registry::new(Vec::new()).is_err());
    }
}
