//! One engine assembly for the two headless subcommands.
//!
//! The same assembly `ganja-tui` performs, in the same order and for the same
//! reasons, minus everything about a screen: no themes, no key bindings, and
//! no catalog refresh loop behind the frame — a headless engine prices from
//! whatever is already cached. What `run` and `serve` legitimately do
//! differently stays at their call sites: `run` installs its auto-refuse
//! permission rules and skips the file watcher, `serve` watches and keeps
//! dialogs interactive, and both dial MCP themselves so `run`'s rules land
//! before the dial's tool-set rebuild can. This is only the half that must
//! never drift apart — it existed twice, and a tool added to one copy was a
//! tool the other silently never offered.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use ganja_core::{
    AgentRegistry, Engine, Storage, catalog,
    config::{Config, Overrides},
    instruction, provider,
};
use ganja_permission::Project;

use crate::STORAGE;

/// The engine either subcommand drives, and every handle a caller may need to
/// keep: the MCP server handles whose processes a shutdown ends, the storage
/// handle read-only routes answer from, and the paths and config the
/// informational routes serve. `run` takes the engine, the servers and the
/// config; the paths and the storage handle go.
pub(crate) struct Assembled {
    pub(crate) engine: Engine,
    pub(crate) servers: Arc<ganja_core::McpServers>,
    pub(crate) storage: Storage,
    pub(crate) root: PathBuf,
    pub(crate) data: PathBuf,
    pub(crate) config: Config,
}

/// Builds the engine a headless subcommand drives.
pub(crate) fn assemble(cwd: &Path, overrides: &Overrides) -> Result<Assembled> {
    let config = Config::load_with(cwd, overrides).context("failed to read the configuration")?;
    // Adopted before anything sizes a request: the disk tier is what the UI
    // last fetched, and an engine that skipped it would compact against the
    // compiled-in snapshot's numbers instead.
    catalog::load_cached();
    let selection = provider::select(&config).context("failed to select a provider")?;
    if let Some(notice) = &selection.notice {
        // stderr, so it cannot land in the middle of an nd-JSON stream.
        eprintln!("note: {notice}");
    }
    let project = Project::resolve(cwd);
    // The project root, like every other roster resolved here: an agent
    // definition file under `.ganja/` belongs to the checkout rather than to
    // whichever subdirectory this process was started in.
    let agents = Arc::new(
        AgentRegistry::build(&config, project.root()).context("failed to resolve the agents")?,
    );
    let data = project
        .data_dir()
        .context("failed to locate the project's data directory")?;
    let storage = Storage::open(data.join(STORAGE));
    let commands = Arc::new(ganja_core::command::Registry::build(
        &config,
        project.root(),
    ));
    let servers = ganja_core::McpServers::new(config.mcp.clone(), project.root());
    let lsp = ganja_core::Lsp::new(config.lsp.as_ref(), project.root());
    let snapshots = Arc::new(ganja_core::Snapshots::new(
        &project,
        config.snapshots_enabled(),
    ));
    let mut tools = ganja_core::tool::Registry::with_builtins();
    if config.webfetch_allows_private() {
        tools = tools.with(Arc::new(
            ganja_core::tool::webfetch::WebfetchTool::allowing_private(),
        ));
    }
    // Over the top of the roster's rootless one, out of the **same** value the
    // prompt's `<available_skills>` block is built from below: a session that
    // is offered a skill has to be able to load it, and only a caller holding
    // the config and the directory can resolve where either half looks.
    let skill_roots = instruction::skill_roots(&config, cwd);
    tools = tools.with(Arc::new(ganja_core::tool::skill::SkillTool::over(
        skill_roots.clone(),
    )));

    let mut engine = Engine::persistent(
        selection.provider,
        selection.model,
        Arc::new(tools),
        ganja_permission::Permissions::load(cwd),
        storage.clone(),
    )
    .with_agents(agents)
    .with_commands(commands)
    .with_mcp(Arc::clone(&servers))
    .with_snapshots(snapshots)
    .with_concurrency(config.agents.concurrency())
    .with_defer_threshold(config.defer_threshold())
    // The admission gate's two config knobs (**D523**, **D524**), the
    // screen's own line kept in the half that must never drift. Inert today:
    // a headless `run` or `serve` installs no teammates and leads no team,
    // so nothing can arrive to gate (the plan's M8 line) — wired anyway so a
    // later serve-led team reads the person's policy rather than the unset
    // default. The D479 classification seed is deliberately *not* here:
    // `--auto` is `run`'s own state, seeded at its call site beside its
    // auto-refuse permission rules, so the seed has exactly one road.
    .with_inbound_policy(config.inbound_policy(), config.dialog_expiry())
    .with_small_model(config.small_model.clone())
    // The same value the skill tool above was installed over, so a `$name`
    // invocation and a `skill` call load from one list.
    .with_skill_roots(skill_roots);
    if let Some(lsp) = lsp {
        engine = engine.with_lsp(lsp);
    }
    // Here rather than at either call site, which is this module's whole
    // purpose: a headless turn fires the same hooks a screen does, and a hook
    // installed in one frontend and forgotten in another is the drift this
    // assembly exists to prevent.
    if let Some(hooks) = ganja_core::hook::Hooks::new(&config.hooks, project.root()) {
        engine = engine.with_hooks(hooks);
    }
    let model = engine.model();
    let engine = engine
        .with_system_parts(
            Some(instruction::base_prompt(&model).to_owned()),
            instruction::suffix(&config, cwd, &model),
        )
        .with_environment({
            let config = config.clone();
            let cwd = cwd.to_owned();
            move |model| instruction::suffix(&config, &cwd, model)
        });

    Ok(Assembled {
        engine,
        servers,
        storage,
        root: project.root().to_owned(),
        data,
        config,
    })
}

#[cfg(test)]
#[path = "assemble_tests.rs"]
mod tests;
