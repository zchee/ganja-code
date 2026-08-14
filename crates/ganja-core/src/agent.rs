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
//! `question`, `doom_loop`, `lsp` — are not ported.
//! A rule about a tool that cannot be called decides nothing, and carrying it
//! would suggest the tool exists. The one exception is `task`, kept on [`PLAN`]
//! so that the agent whose point is "do not act" already denies the subagent
//! that would act for it, the day the task tool lands. `websearch` came off
//! that list with the tool: [`EXPLORE`] allows it, exactly as upstream's does
//! — and both plan doors came off it with the tools behind them: the shared
//! defaults deny `plan_exit` and `plan_enter`, [`PLAN`] alone allows the exit
//! and [`BUILD`] alone the enter, which is exactly upstream's own pair of
//! deltas. Denied tools are still not hidden, so build's model sees
//! `plan_exit` in its schema — and plan's sees `plan_enter` — and a call comes
//! back as refusal text it reads.
//!
//! Upstream also *hides* a tool from the model's schema when the last rule
//! matching it is `"*": "deny"` (`permission/index.ts`, `disabled`). That is
//! not ported: a denied call still reaches the gate and comes back as a
//! refusal the model reads, which is the same outcome by the route this port
//! already has for every other refusal.

use std::path::Path;

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
    // `attended` is what decides whether the memory door is opened for an
    // agent: a person is watching a primary agent's turn and nobody is
    // watching a subagent's, and memory is written by whoever is being
    // watched. See [`memory_door`].
    let assemble = |attended: bool, own: Vec<Rule>| {
        let mut rules = defaults();
        if attended {
            rules.extend(memory_door(config));
        }
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
            // The second is adopted now that the door exists (**D477**): the
            // one agent that could usefully stop and plan is the one that
            // would otherwise start implementing, and the shared default
            // denies it to everybody else. Subagents inherit refusals and
            // never allows, so no child ever carries this one down.
            //
            // `question: allow` is still not adopted, and would change
            // nothing: its un-ruled baseline is already *allow* — `decide()`
            // falls through to allow for a tool no rule and no
            // ask-by-default entry names, and `question` appears in neither.
            // Whether it should instead gain an explicit rule is a decision
            // deliberately not taken here.
            rules: assemble(true, vec![rule("plan_enter", ANY, Action::Allow)]),
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
            rules: assemble(
                true,
                vec![
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
                    // The one agent whose finished plan has somewhere to go:
                    // upstream's `plan_exit: allow`, over the shared default deny.
                    // Subagents inherit refusals and never allows, so no child
                    // ever carries this one down.
                    rule("plan_exit", ANY, Action::Allow),
                ],
            ),
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
            rules: assemble(false, vec![rule("todowrite", ANY, Action::Deny)]),
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
            rules: assemble(
                false,
                vec![
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
                ],
            ),
        },
    ]
}

/// The ruleset every agent starts from, ported from upstream's `defaults`.
///
/// Upstream's `"*": "allow"` is deliberately absent — see the module docs. So
/// are its whitelisted directories: they name upstream's temporary, skill and
/// reference directories, none of which this build has. The one directory
/// this build ever whitelists is a session's own memory root, and it is not
/// here but beside the agents that may reach it — [`memory_door`] is where
/// that is argued.
fn defaults() -> Vec<Rule> {
    vec![
        // Ganja already asks about this one by default; written out because
        // the agents below turn it off and on again, and a rule that is only
        // implied cannot be put back.
        rule(EXTERNAL_DIRECTORY, ANY, Action::Ask),
        // Upstream defaults `plan_exit` to deny and allows it on plan alone.
        // The deny must be written here, not merely implied: `plan_exit` is
        // not in the ask-by-default table, so an un-ruled call would fall
        // through `decide()` to *allow* and build could leave a mode it was
        // never in, unasked.
        rule("plan_exit", ANY, Action::Deny),
        // And the mirror, upstream's `plan_enter: "deny"` (agent.ts:127),
        // allowed on build alone (**D477**). Written out for exactly the
        // reason above: not hidden, just refused — and the fallthrough is
        // allow, so an implied deny would be no deny at all.
        rule("plan_enter", ANY, Action::Deny),
        // Upstream's comment: mirrors github/gitignore's Node.gitignore
        // patterns for .env files. Reading is otherwise free, so this is the
        // one place a read stops to ask.
        rule("read", "*.env", Action::Ask),
        rule("read", "*.env.*", Action::Ask),
        rule("read", "*.env.example", Action::Allow),
    ]
}

/// The rules that let a session keep its own memory, or none at all when the
/// config never asked for memory (**D478**, declared at
/// [`crate::instruction::memory_dir`]).
///
/// # Why any rule is needed
///
/// The memory root is outside the worktree, and this build asks about
/// everything out there — twice over: the tool's own permission (`write` and
/// `edit` are both in [`crate::permission::ASK_BY_DEFAULT`]) and the location
/// gate raised beside it. That posture is right for the general case and
/// exactly wrong for this one: the model is being *told*, in its own system
/// prompt, to maintain files in a directory it would then have to interrupt
/// the user about on every single fact it records.
///
/// # Why it is this narrow
///
/// Three rules, all patterned on the memory directory alone:
///
/// - the location gate for that directory, which is what `read` needs too —
///   reading is otherwise free, but the gate is raised for a read outside the
///   project like everything else;
/// - `write` and `edit` under it, which is what recording a fact is.
///
/// Nothing else outside the worktree moves: a `write` one directory up from
/// the memory root still asks exactly as it did before this feature existed.
/// The alternative shapes were a dedicated memory tool (a second way to write
/// a file, so that a permission rule could name it) and a blessed root on
/// `ToolCtx` (a path the tools trust, which is a gate outside the gate).
/// Both add a mechanism; this adds three rules to a table that already
/// expresses exactly this.
///
/// # Who gets it: the attended agents, and only them
///
/// The door is added per agent rather than to [`defaults`], and only for the
/// **primary** ones — the agents a person runs a session as. A subagent's
/// ruleset is its *own* agent's rules, not the parent's, so a door written
/// into the shared defaults would arrive at `general` and `explore` complete,
/// and inheritance would never get a chance to withhold it. Two things then
/// hold together: the location rule the parent carries still travels
/// ([`crate::permission::Permissions::inherited_by_subagent`] passes every
/// `external_directory` rule down), so a child may *read* the memory its
/// prompt showed it, and the two tool rules reach no child at all, so its
/// `write` falls back to asking. That is "a subagent inherits refusals and
/// never allows", made true of this feature by construction.
///
/// A config-defined agent in mode `all` gets no door either, for the same
/// reason: an agent that may be spawned may be spawned unwatched. Declaring
/// one `primary` is what asks for the door, and writing the rule out by hand
/// in a `permission` block is what asks for anything else.
///
/// # Where the directory comes from
///
/// The process's own working directory, resolved through the same
/// [`crate::instruction::memory_dir`] the prompt is composed from, so the door
/// and the block cannot name two different places. Threading a `cwd` into
/// [`Registry::build`] was the tidier alternative and was not taken: three
/// frontends call it, every one of them from the directory the process was
/// started in, and the engine reads its own `cwd` from exactly there too.
fn memory_door(config: &Config) -> Vec<Rule> {
    if !config.memory_enabled() {
        return Vec::new();
    }

    let Ok(cwd) = std::env::current_dir() else {
        tracing::warn!("the working directory is unreadable, so project memory has no door");
        return Vec::new();
    };
    let Some(directory) = crate::instruction::memory_dir(&cwd) else {
        return Vec::new();
    };

    memory_rules(&directory)
}

/// The three rules themselves, over a directory handed in.
///
/// Split from [`memory_door`] so that what they say can be asserted without a
/// test having to own the process's working directory to say it.
fn memory_rules(directory: &Path) -> Vec<Rule> {
    // Resolved and suffixed the way the gate spells a directory it is asked
    // about (`permission`'s own `covering`), because a rule written in another
    // spelling of the same place answers nothing. `*` spans separators here,
    // so one pattern covers the topic files as well as the index.
    let pattern = crate::permission::resolve(directory)
        .join(ANY)
        .to_string_lossy()
        .into_owned();

    vec![
        rule(EXTERNAL_DIRECTORY, &pattern, Action::Allow),
        rule("write", &pattern, Action::Allow),
        rule("edit", &pattern, Action::Allow),
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
        permission::{Action, PermissionConfig, Permissions, Rule},
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
        let topic = directory
            .join("topics")
            .join("style.md")
            .display()
            .to_string();
        for pattern in [index.as_str(), topic.as_str()] {
            assert_eq!(
                decides(&rules, "write", pattern),
                Some(Action::Allow),
                "recording a fact must not interrupt anybody: {pattern}"
            );
            assert_eq!(decides(&rules, "edit", pattern), Some(Action::Allow));
            assert_eq!(
                decides(
                    &rules,
                    crate::permission::EXTERNAL_DIRECTORY,
                    &format!("{pattern}/*")
                ),
                Some(Action::Allow),
                "and neither must the location gate raised beside it"
            );
        }

        // One directory up is still outside the worktree, and still asks.
        let sibling = "/data/ganja/project/api-1/permissions.json";
        assert_eq!(decides(&rules, "write", sibling), None, "{sibling}");
        assert_eq!(
            decides(
                &rules,
                crate::permission::EXTERNAL_DIRECTORY,
                "/data/ganja/project/api-1/*"
            ),
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
        let registry = registry(&Config {
            memory: Some(true),
            ..Config::default()
        });
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
        assert_eq!(
            decides(&child, "edit", &index),
            None,
            "nor the edit allow: {child:?}"
        );
        assert_eq!(
            decides(
                &child,
                crate::permission::EXTERNAL_DIRECTORY,
                &format!("{index}/*")
            ),
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

    /// Upstream's `plan_exit: allow` delta, over the shared default deny that
    /// keeps the un-ruled fallthrough — which is allow — from deciding.
    #[test]
    fn the_planning_agent_alone_may_leave_planning() {
        let registry = registry(&Config::default());

        assert_eq!(
            decides(
                &registry.get(PLAN).expect("plan is builtin").rules,
                "plan_exit",
                ANY_CALL
            ),
            Some(Action::Allow)
        );
    }

    #[test]
    fn the_build_agent_is_refused_the_exit_it_does_not_need() {
        let registry = registry(&Config::default());

        assert_eq!(
            decides(
                &registry.get(BUILD).expect("build is builtin").rules,
                "plan_exit",
                ANY_CALL
            ),
            Some(Action::Deny)
        );
    }

    /// The mirror delta, on the agent upstream's `agent.ts:147-150` gives it
    /// to: build alone may ask to stop and plan (**D477**).
    #[test]
    fn the_build_agent_alone_may_ask_to_start_planning() {
        let registry = registry(&Config::default());

        assert_eq!(
            decides(
                &registry.get(BUILD).expect("build is builtin").rules,
                "plan_enter",
                ANY_CALL
            ),
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
                decides(
                    &registry.get(name).expect("a builtin").rules,
                    "plan_enter",
                    ANY_CALL
                ),
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
            let mut child = registry
                .get(name)
                .expect("a builtin subagent")
                .rules
                .clone();
            child.extend(parent.inherited_by_subagent());

            assert_eq!(
                decides(&child, "plan_exit", ANY_CALL),
                Some(Action::Deny),
                "{name}"
            );
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
            let mut child = registry
                .get(name)
                .expect("a builtin subagent")
                .rules
                .clone();
            child.extend(parent.inherited_by_subagent());

            assert_eq!(
                decides(&child, "plan_enter", ANY_CALL),
                Some(Action::Deny),
                "{name}"
            );
        }
    }

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
