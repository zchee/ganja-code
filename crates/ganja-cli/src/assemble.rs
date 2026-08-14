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
    tools = tools.with(Arc::new(ganja_core::tool::skill::SkillTool::over(
        instruction::skill_roots(&config, cwd),
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
    .with_concurrency(config.agents.concurrency());
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
mod tests {
    use ganja_core::config::Overrides;

    use super::assemble;

    /// The cap a config names reaches the engine this seam builds.
    ///
    /// `ganja-core`'s own suite pins what the cap *does* — two children at a
    /// time and never more — over an engine it builds by hand
    /// (`tests/parallel_subagents.rs`). What that suite cannot see is whether
    /// a real session is ever handed the number, which is the half that was
    /// missing: `agents.concurrency` was parsed, validated and documented
    /// while every assembled engine ran at the default.
    ///
    /// The three redirects are what make an assembly hermetic: the global
    /// config tier, the data home a project's storage hangs under, and the
    /// provider the environment would otherwise choose. Without them this
    /// reads whatever config the machine running the suite happens to hold.
    #[test]
    fn the_configured_cap_reaches_an_assembled_engine() {
        let data = tempfile::TempDir::new().expect("a temporary directory is creatable");
        let home = tempfile::TempDir::new().expect("a temporary directory is creatable");
        let project = tempfile::TempDir::new().expect("a temporary directory is creatable");
        // SAFETY: process-wide, so this belongs to a test that runs alone in
        // its process — which `nextest` gives every test, and which the rest
        // of this binary's unit tests do not contend for: none of them reads
        // the environment.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", data.path());
            std::env::set_var("GANJA_CONFIG_HOME", home.path());
            std::env::remove_var("GANJA_PROVIDER");
            std::env::remove_var("GANJA_MODEL");
        }
        std::fs::write(
            project.path().join("ganja.json"),
            r#"{"agents": {"concurrency": 3}}"#,
        )
        .expect("the fixture config is writable");

        let assembled = assemble(project.path(), &Overrides::default())
            .expect("a project holding one config key assembles");

        assert_eq!(
            assembled.engine.concurrency(),
            3,
            "the assembled engine runs at the cap the config named"
        );
    }
}
