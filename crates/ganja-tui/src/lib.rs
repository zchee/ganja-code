//! ratatui frontend for ganja.
//!
//! The crate owns every pixel and no engine logic: it turns terminal events
//! into [`Command`](ganja_protocol::Command)s and
//! [`Event`](ganja_protocol::Event)s into frames.

pub mod app;
pub mod binder;
pub mod clipboard;
pub mod command;
pub mod component;
pub mod escrepair;
pub mod event;
pub mod external;
pub mod graphics;
pub mod history;
pub mod keybind;
pub mod lister;
pub(crate) mod markdown;
pub mod member;
pub mod mention;
pub mod notify;
pub mod theme;
pub mod transcript;

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use ganja_core::config::{Config, Overrides, ThemeMode};
use ganja_core::teammate::TeammateRegistry;
use ganja_core::teammate::agy::Agy;
use ganja_core::teammate::claude::ClaudePane;
use ganja_core::teammate::codex::Codex;
use ganja_core::teammate::grok::Grok;
use ganja_core::teammate::pane::{GanjaPane, PaneShare, PaneShell};
use ganja_core::teammate::shim_tui::ShimTui;
use ganja_core::{
    AgentRegistry, Backends, Engine, SessionId, Storage, catalog, instruction, provider,
};
use ganja_permission::Project;
use ganja_protocol::Message;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use tokio_util::sync::CancellationToken;

use crate::app::App;
use crate::history::History;
use crate::keybind::Keybinds;
use crate::theme::{Mode, Themes};

/// Directory the session store lives in, under the project's data directory.
const STORAGE: &str = "storage";

/// Separates the things the status bar shows on its left.
pub(crate) const NOTICE_SEPARATOR: &str = " \u{b7} ";

/// Which stored session a run opens, when it opens one.
///
/// Naming a session is the caller's way of saying it wants *that*
/// conversation; nothing here quietly substitutes another one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resume {
    /// The most recently updated session in this project's store.
    Latest,
    /// The session with this stored id.
    Session(String),
}

/// The surfaces this build can spawn a teammate onto, except the engine's own
/// (**D538**).
///
/// Assembled here because a pane needs a tmux server and the shell a spawn
/// splits into, and a foreign CLI's TUI needs those plus a binary on `PATH` —
/// none of which an engine holds. `Engine::with_teammates` adds the in-process
/// implementation it *can* build, out of that session's own provider, tool set
/// and store.
///
/// **D512 (P28)**: all three shim slots open the CLI's own native TUI in a
/// pane, spoken to through bracketed paste, and **no spawn door in this build
/// reaches the headless `teammate::shim::ShimBackend`** any more — that
/// machinery stays in the tree, unit-tested, reachable only by the tests that
/// drive it against a fake CLI. Which is also why `teammates.shim_turn_timeout`
/// is not read here: a pane-mode shim has no per-turn deadline (the module doc
/// owns why), and the key governs only the headless machinery it was written
/// for (**D509**).
///
/// These slots search the real `PATH`; a test that reached one would spawn the
/// developer's own CLI. Tests assemble their backends through
/// `ganja_testkit`, never through this.
fn local_backends(shell: PaneShell, share: PaneShare) -> Backends {
    Backends::new()
        .with(Arc::new(GanjaPane::new(shell.clone(), share)))
        .with(Arc::new(ClaudePane::new(shell.clone(), share)))
        .with(Arc::new(ShimTui::new(Arc::new(Codex::new()), shell.clone(), share)))
        .with(Arc::new(ShimTui::new(Arc::new(Agy::new()), shell.clone(), share)))
        .with(Arc::new(ShimTui::new(Arc::new(Grok::new()), shell, share)))
}

/// Runs the interactive terminal UI until the user quits.
///
/// `resume` opens a stored session instead of starting a fresh one, and
/// `overrides` carries what the command line decided — the tier above every
/// config file and above the environment between them. `yolo` is the bypass
/// trio's one bool (**D479**): the session answers its own permission dialogs
/// with "allow once" instead of raising them, and remembers none of it.
/// `member` is §4.1's launch line, when a lead started this process as a pane
/// teammate of its team: the session then reads its own inbox instead of
/// leading a team, and tells the lead when its turns end (§10.3). `binder` is
/// how a **lead** session's socket gets bound (**D505**, [`binder`]): the
/// binary that links the server hands one in, and this crate — which may not
/// name that server — decides when it is asked, which is only for a session
/// that leads a team; a pane member and a build with no config home hand it
/// back unused, and a caller with no server passes [`None`]. `lister` is the
/// `@` menu's and the send resolver's live-session listing (**D529** Axis 5,
/// **D530**'s re-derived gate, [`lister`]): every **interactive non-member**
/// session is handed one — team or none, wider than `binder`'s lead-only
/// gate — and a pane member or a caller with no server passes [`None`].
/// `name` is `--name`'s value, already validated by the CLI boundary
/// (**D527**'s grammar) — this function only asserts that in a debug build,
/// never re-refuses it; [`None`] falls back to the project root's derived
/// basename (REVISION-3 P5). `socket_dir` is the hidden `--socket-dir`
/// override, seeded onto the identity resolver so the binder, the lister and
/// a name's resolution all read the one directory; [`None`] leaves the
/// well-known default standing.
///
/// Everything that can refuse does so *before* the terminal is taken over: a
/// config file that will not parse, a key binding this build cannot read, a
/// provider it does not ship, an agent roster that leaves nothing to start on,
/// a resume naming a session that is not there. All of them reach the shell as
/// a readable error rather than flashing past inside the alternate screen.
///
/// The terminal is restored on every exit path, including a panic: the hook
/// installed here undoes bracketed paste and mouse capture and then defers to
/// the one [`ratatui::try_init`] installed, which leaves raw mode and the
/// alternate screen. MCP servers are **not** part of that hook — its work has
/// to be synchronous and closing them is not — so a panic leaves a local
/// server's process group standing until it notices its stdin has closed.
///
/// # Errors
///
/// Returns an error for any of the refusals above, and if the terminal cannot
/// be initialized, drawn to, read from, or restored.
///
/// Eight independent startup knobs, each documented above and each optional
/// on its own: a struct would not shorten a call site that already names
/// every one of them, and would only hide which knob a diff actually
/// touched.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    resume: Option<Resume>,
    overrides: Overrides,
    yolo: bool,
    member: Option<member::Flags>,
    binder: Option<Box<dyn binder::Binder>>,
    lister: Option<Box<dyn lister::Lister>>,
    name: Option<String>,
    socket_dir: Option<PathBuf>,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Resolved first, and refused readably before anything else is built: a
    // pane teammate is launched by a program rather than a person, and a bad
    // launch line has to reach the lead's log as a sentence rather than as a
    // pane that flashed and died. The teams root is the lead's own — asked of
    // the same registry a lead builds, so both sides read one directory — and
    // a launch line with no config home has nowhere to read from at all.
    //
    // The posture is the lead's dialog, full stop: the record carries no such
    // field — Claude's shape holds `planModeRequired` and nothing else about
    // it — and since **D513** a lead composes no posture onto the line either,
    // so `yolo` here is a person's own `--auto` about this session (D479) and
    // is not read as a posture. The record itself — the model this teammate
    // was spawned to run, and whether it must start in plan mode — is read
    // off the team file with a bounded wait
    // (`member::Membership::await_record`): the lead writes it before it types
    // this launch line, so the wait covers a lead that died in between, not
    // the ordinary path. Before the config loads, because plan mode is an
    // agent here, and the agent a session starts on is the config's to
    // resolve.
    let membership = match member {
        Some(flags) => {
            let home = ganja_core::config::config_home()
                .context("a pane teammate needs a config home to find its team in")?;
            let pane = std::env::var(member::TMUX_PANE).ok();
            let membership = member::Membership::resolve(flags, &home, &cwd, pane)?;
            let record = membership
                .await_record(member::RECORD_WAIT)
                .await
                .context("this pane teammate's lead never registered it")?;

            Some((membership, record))
        }
        None => None,
    };
    // `planModeRequired` is the `plan` agent: read-only rules and the
    // `plan_exit` door, which is what plan mode is in this build. Below the
    // flag tier, so a hand-typed `--agent` still wins over the record.
    let overrides = match &membership {
        Some((_, record))
            if record.plan_mode_required.unwrap_or(false) && overrides.agent.is_none() =>
        {
            Overrides { agent: Some(ganja_core::agent::PLAN.to_owned()), ..overrides }
        }
        _ => overrides,
    };
    let config = Config::load_with(&cwd, &overrides).context("failed to read the configuration")?;
    let keys =
        Keybinds::from_config(&config.keybinds).context("failed to read the key bindings")?;
    let selection = provider::select(&config).context("failed to select a provider")?;
    // Captured before the provider is handed to the engine: the model list is
    // narrowed to this provider, and `Selection` gives it up on the move.
    let provider_id = selection.provider.id().to_owned();
    // A pane teammate runs the model its spawn decided — the record's, a bare
    // id the lead's own engine was serving — over the provider this build's
    // config selects; `--model` is not on §4.1's line, and a record naming no
    // model leaves the selection's default standing.
    let model =
        membership.as_ref().and_then(|(_, record)| record.model.clone()).unwrap_or(selection.model);
    // Sessions live per project, so opening `src/` and opening the repository
    // root reach the same history.
    let project = Project::resolve(&cwd);
    // The **project root** again: an agent definition file under `.ganja/`
    // belongs to the checkout, not to whichever subdirectory was opened.
    let agents = Arc::new(
        AgentRegistry::build(&config, project.root()).context("failed to resolve the agents")?,
    );
    let data = project.data_dir().context("failed to locate the project's data directory")?;
    let storage = Storage::open(data.join(STORAGE));
    // `/init`'s template names the worktree it is being run in, so the roster
    // is resolved against the project root rather than against whichever
    // subdirectory the terminal happened to be opened in.
    let commands = Arc::new(ganja_core::command::Registry::build(&config, project.root()));
    // Configured MCP servers, none of them dialled yet. The **project root**
    // is what a relative `cwd` in an entry resolves against, not the directory
    // this process happens to have been started in: a server configured once
    // for the project has to start in the same place whichever subdirectory
    // its owner opened the terminal in.
    let servers = ganja_core::McpServers::new(config.mcp.clone(), project.root());
    // The **project root** again, and for the same reason: a language server's
    // idea of a workspace has to be the project's, not whichever subdirectory
    // the terminal was opened in. `None` when the config asked for no LSP,
    // which leaves the engine doing no LSP work rather than inert LSP work.
    let lsp = ganja_core::Lsp::new(config.lsp.as_ref(), project.root());
    // Probed here, before the first frame: whether this project can be
    // snapshotted at all decides what the status bar has to say about `/undo`,
    // and the answer costs one synchronous `git` probe.
    let snapshots = Arc::new(ganja_core::Snapshots::new(&project, config.snapshots_enabled()));
    let snapshot_notice = snapshots.notice().map(str::to_owned);
    // The registry carries every builtin tool the agent loop can execute;
    // permission rules load for the project the terminal was opened in.
    let mut tools = ganja_tool::Registry::with_builtins();
    // `webfetch` refuses a private address unless this session said otherwise,
    // and the config is the only place that can say so — which address on a
    // private network is a legitimate one to fetch is a question only the
    // person running the session can answer.
    if config.webfetch_allows_private() {
        tools = tools.with(Arc::new(ganja_tool::webfetch::WebfetchTool::allowing_private()));
    }
    // Over the top of the roster's rootless one, out of the **same** value the
    // prompt's `<available_skills>` block is built from below: a session that
    // is offered a skill has to be able to load it, and only a caller holding
    // the config and the directory can resolve where either half looks.
    let skill_roots = instruction::skill_roots(&config, &cwd);
    tools = tools.with(Arc::new(ganja_tool::skill::SkillTool::over(skill_roots.clone())));
    let mut engine = Engine::persistent(
        selection.provider,
        model,
        Arc::new(tools),
        ganja_permission::Permissions::load(&cwd),
        // Cloned, not moved: the socket a lead serves answers its session
        // routes from the same store, handed over in the lead arm below.
        storage.clone(),
    )
    .with_agents(agents)
    .with_commands(commands)
    .with_mcp(Arc::clone(&servers))
    .with_snapshots(snapshots)
    // The screen's copy of the headless assembly's line: this frontend builds
    // its own engine, so a knob wired into `ganja-cli`'s `assemble` alone
    // would be a knob every interactive session still ignored.
    .with_concurrency(config.agents.concurrency())
    .with_defer_threshold(config.defer_threshold())
    // The admission gate's two config knobs (**D523**, **D524**): the
    // explicit `cross_session_inbound` with the tier that won it, and the
    // review window a parity hold's timer runs on. Wired here for the
    // concurrency knob's reason — this frontend builds its own engine, so a
    // gate wired only into `ganja-cli`'s assembly would leave every
    // interactive session running the unset default.
    .with_inbound_policy(config.inbound_policy(), config.dialog_expiry())
    // And the D479 trio's classification seed: a `--yolo` session is
    // bypass-classed at the receiver, which is exactly the session whose
    // unset-policy inbound holds rather than delivering into a run nobody
    // is gating. Classification only — dialog auto-answering stays the
    // App's own, and the approval dialog it must not answer is B1's.
    .with_inbound_bypass(yolo)
    .with_small_model(config.small_model.clone())
    // The same value the skill tool above was installed over, so a `$name`
    // invocation and a `skill` call load from one list.
    .with_skill_roots(skill_roots);
    if let Some(lsp) = lsp {
        engine = engine.with_lsp(lsp);
    }
    // The **project root** for the same reason the language server takes one: a
    // hook that runs `git status` means the checkout, not whichever
    // subdirectory the terminal was opened in. `None` when the config asked for
    // no hooks, which leaves the engine doing no hook work at all.
    if let Some(hooks) = ganja_core::hook::Hooks::new(&config.hooks, project.root()) {
        engine = engine.with_hooks(hooks);
    }
    // Composed from the engine and not from the selection, and only once the
    // agents are on it: the default agent may have named a model of another
    // family, and the prompt has to be that model's.
    let (base, suffix) = system_parts(&engine, &config, &cwd);
    let engine = engine
        .with_system_parts(base, suffix)
        .with_environment({
            // Owned copies, because this outlives the startup that composed the
            // first one: the environment block names the model, and the model
            // moves when somebody picks another one mid-session.
            let config = config.clone();
            let cwd = cwd.clone();
            move |model| instruction::suffix(&config, &cwd, model)
        })
        // And the other half moves with it: the base prompt is chosen by the
        // model's family, so a switch from a `claude` model to a `gpt` one has
        // to change this too or the new model reads the old family's
        // instructions. Nothing is handed over — the engine composes this half
        // from the model's name alone.
        .with_base_for_model();

    // **D527/D530, REVISION-3 P5**: the self-name every registration, every
    // `@` menu label and every solo send reads (`Engine::self_name`,
    // `App::self_name_source`), resolved once here. `--name` is the
    // person's own choice, already vetted at the CLI boundary before it
    // reaches this signature — asserted, never re-refused, since a second
    // refusal here would be a second grammar to keep in step with
    // `registry::vet_name`'s own. Absent one, the project root's basename
    // stands in, run through the same grammar sanitizer a typed name would
    // be, falling back to [`ganja_core::tool::registry::FALLBACK_NAME`] on
    // nothing usable — the [`ganja_core::tool::registry::NameSource`] that
    // came out is what tells a registration record `user` from `derived`
    // (AC-4).
    let (resolved_name, name_source) = match name {
        Some(name) => {
            debug_assert!(
                ganja_core::tool::registry::vet_name(&name).is_ok(),
                "the CLI already validated --name; this asserts it stayed valid"
            );

            (name, ganja_core::tool::registry::NameSource::User)
        }
        None => (
            ganja_core::tool::registry::sanitize(
                project.root().file_name().and_then(std::ffi::OsStr::to_str).unwrap_or_default(),
            ),
            ganja_core::tool::registry::NameSource::Derived,
        ),
    };
    // Every seam below is scoped to an interactive **non-member** assembly
    // (**D530**'s gate, restated at each): a pane member speaks through its
    // own `MemberPostbox`, never registers, and is handed no lister — its
    // self-name cell, socket directory and `teamless_send` posture go
    // unread, so seeding them would be work with no reader.
    let interactive = membership.is_none();
    // The identity resolver's own directory (**D528**), seeded **before**
    // anything below captures `&Engine::identity` — `with_teammates`' lead
    // postbox and `with_solo_postbox`'s solo one both do, in the match
    // that follows, so this has to run first or either would capture the
    // engine's un-seeded default instead of the hidden `--socket-dir`
    // override.
    let engine = if interactive {
        engine
            .with_socket_directory(socket_dir.clone().unwrap_or_else(ganja_tool::socket::directory))
            .with_teamless_send(config.teamless_send())
    } else {
        engine
    };
    if interactive {
        engine.set_self_name(resolved_name);
    }

    // Dialled from here on, in the background: the first turn is offered
    // whichever servers have answered by the time it starts, and a server
    // that never answers costs its tools and a line of the status bar rather
    // than the startup this call returns straight out of (**R3**).
    engine.connect_mcp();
    // The session is open, so whatever a `SessionStart` hook has to say is
    // collected now and delivered to the first turn that asks the model. Before
    // the resume below, so a `--continue` fires `startup` and then `resume`, in
    // the order they happened.
    engine.session_start().await;
    // Filesystem events for the files this session reads, so a file edited in
    // another window is refused before the model acts on what it read and is
    // named to it at the top of the next turn. A watcher that will not start
    // is one warning and nothing else.
    engine.watch_files();
    // Held apart the same way `servers` is: `engine` moves into `App::new`
    // below, and a background job's own process group has to be ended
    // whichever way this run ends, exactly as every local MCP server's does.
    let jobs = Arc::clone(engine.jobs());

    let seed = match resume {
        Some(resume) => stored_transcript(&engine, resume).await?,
        None => Vec::new(),
    };

    // **After the resume**, and that is the whole of why it is here rather
    // than up in the builder chain: §2.1 names a team after the session that
    // leads it, and a resume replaces the id the engine was minted with. A
    // team decided before that point would name a conversation nobody opened,
    // and `--continue` would join a different team every launch instead of
    // rejoining the one it left. The team name is a pure function of the id
    // (`teammate::session_team`), so the same session always finds the same
    // directory.
    //
    // Once, and never again: `Engine::with_teammates`' own doc says the team a
    // session leads is decided before anything can be streaming. A `/new`
    // therefore keeps this team rather than minting one — the lead is the
    // process, and its teammates outlive the conversation that started them.
    // Re-minting on `NewSession` would need a seam the engine does not have
    // and would strand every running teammate in a team nothing was reading.
    //
    // A build with no config home has nowhere to keep a team, so it leads
    // none: `Engine::teammates()` answers `None` and the frontend's whole
    // lead side is inert. Since **D530** it still speaks, though: the solo
    // postbox below opens `send_message` to named live sessions and `uds:`
    // addresses, sending as `<self-name>@solo` with the one-way note — a
    // sender, never an addressee, because only a socket-binding lead
    // registers a name.
    //
    // **A pane teammate leads no team either** (§10.3): it is a member of the
    // one that launched it, and a teammate is not a place to nest a second
    // team — the same line the engine draws for an in-process one. So the
    // registry is skipped outright, which is also what keeps the lead's
    // `send_message` from being offered under the lead's name to a process
    // that is not the lead.
    let (engine, teammates, socket) =
        match ganja_core::config::config_home().filter(|_| membership.is_none()) {
            Some(home) => {
                let id = engine.session_id();
                let registry = Arc::new(TeammateRegistry::for_session(&home, id.as_str(), &cwd));
                // Resolved **once**, here, and handed to the backends that read
                // them rather than to the registry (**D538**, keeping **D520**'s
                // intent): the idle shell a pane is split into and how wide the
                // teammates' column opens are properties of this *runtime*, and
                // a backend must name no config type — so they cross as
                // `ganja-core`'s own value types.
                let shell =
                    config.teammates.pane_shell().map(PaneShell::configured).unwrap_or_default();
                let share =
                    config.teammates.pane_share().map(PaneShare::configured).unwrap_or_default();
                let backends = local_backends(shell, share);
                // **D506**: panes a previous lead of this team left running,
                // before this one spawns anything of its own. Best-effort by
                // construction — it returns a `Swept` and never an error, and a
                // session outside tmux sweeps nothing — so it is awaited here
                // rather than guarded: the one thing it must be is *before* the
                // first spawn, since that is what makes a pane member found in
                // the file certainly not this lead's.
                //
                // The behavioural witness is `ganja-cli/tests/teammate_reaper.rs`
                // (AC-12), which drives `reaper::sweep_on` against a private tmux
                // server: `run` opens a real terminal, so there is no headless
                // seam in this file to assert the call from.
                let swept = ganja_core::teammate::reaper::sweep(&registry).await;
                if !swept.is_empty() {
                    tracing::info!(?swept, "a previous lead's panes were swept at startup");
                }
                // **D508**: and the shim children a previous lead left,
                // which is a *separate* call rather than a branch inside the
                // one above. `sweep` is gated on there being a tmux server to
                // look at — correct for panes, fatal for shims, whose common
                // case has no tmux at all — and hoisting that gate would
                // change the pane arm's own contract. Unconditional here, and
                // asserted so at the function level by
                // `ganja-core/tests/teammate_shim_sweep.rs`; the call itself
                // has the same no-headless-seam gap the pane sweep's does.
                let orphans = ganja_core::teammate::reaper::sweep_shims(
                    &registry,
                    ganja_core::teammate::shim::default_directory(),
                )
                .await;
                if !orphans.is_empty() {
                    tracing::info!(
                        ?orphans,
                        "a previous lead's foreign-CLI children were swept at startup"
                    );
                }

                (
                    engine.with_teammates(Arc::clone(&registry), backends),
                    Some(registry),
                    // The lead's socket rides the same gate as its team
                    // (**D505**): a session that leads is one a peer session
                    // has a reason to reach, and the socket's team routes have
                    // something to answer. Not bound here — the app binds on
                    // its first pass, so the id it binds under is the one the
                    // resume above installed.
                    binder.map(|binder| {
                        (
                            binder,
                            binder::Served {
                                directory: cwd.clone(),
                                root: project.root().to_path_buf(),
                                data: Some(data),
                                storage: Some(storage),
                                config: Some(config.clone()),
                            },
                        )
                    }),
                )
            }
            // A member speaks as itself: its `send_message` posts through the
            // postbox stamped with the name its launch line carried, over the
            // same teams root its lead writes into — the roster read off the team
            // file per call, the lead always addressable, and this session still
            // leading no team of its own. And it binds no socket: a member is
            // addressed through its lead's team, by the same line that keeps
            // it from leading one (**D505**).
            None if let Some((membership, _)) = &membership => (
                engine.with_postbox(Arc::new(ganja_core::teammate::member::MemberPostbox::new(
                    membership.name().clone(),
                    membership.team().clone(),
                    membership.root().clone(),
                ))),
                None,
                None,
            ),
            None => {
                tracing::warn!(
                    "no config home, so this session leads no team and cannot spawn \
                     teammates; cross-session sending stays open through the solo \
                     postbox (D530)"
                );

                (engine.with_solo_postbox(), None, None)
            }
        };
    // What the status bar says about who this process is, beside the provider
    // and theme notices: a person looking at a pane should be able to tell it
    // from the lead's window at a glance.
    let member_notice = membership.as_ref().map(|(membership, _)| {
        tracing::info!(
            team = membership.team().as_str(),
            name = membership.name().as_str(),
            parent = membership.parent_session_id(),
            "running as a pane teammate"
        );

        match membership.color() {
            Some(color) => format!(
                "teammate {} ({color}) of {}",
                membership.name().as_str(),
                membership.team().as_str()
            ),
            None => {
                format!("teammate {} of {}", membership.name().as_str(), membership.team().as_str())
            }
        }
    });
    // The builtins, the user's own themes, and the theme they last picked —
    // then whatever the config asks for on top, because a `theme` written in a
    // file outranks a runtime pick permanently rather than until the next one.
    let mut themes = Themes::load();
    let theme_notice = configure_themes(&mut themes, &config);

    // Prices come off the disk before the first frame — adoption happens on
    // this thread — and are kept current behind the loop for as long as the app
    // runs. Deliberately not a refusal: a catalog that could not be fetched
    // leaves the compiled-in snapshot standing, which is a session that prices
    // slightly stale rather than a session that does not start.
    let background = CancellationToken::new();
    catalog::spawn_refresh_loop(background.clone());

    // After the resume, because the config is the default and the stored row
    // is the choice: a continued session runs under the effort it was left
    // under, and only a session carrying none takes the configured one. And
    // after the catalog call above, whose first act is the synchronous disk
    // read — the seed validates its name against the active model's catalog
    // row, and seeded any earlier it read a model the compiled snapshot
    // predates as carrying no efforts at all, clearing a configured effort at
    // every launch (2026-08-15; the CLI paths were never wrong, because
    // `assemble` loads the cache before either seeds). The frame this
    // announces into is still the first one the app draws.
    engine.seed_effort(config.effort.clone()).await;

    // Spilled tool output older than a week is nobody's context any more, and
    // nothing else on this machine ever deletes it.
    ganja_tool::truncate::spawn_sweep_loop(background.clone());

    let mut terminal = ratatui::try_init().context("failed to initialize the terminal")?;
    let outcome = match capture_input() {
        Ok(()) => {
            // After `capture_input` and before the first `EventStream` poll,
            // per the probe's own ordering constraint.
            let kitty = capture_keys();
            let focused = initial_focus().await;
            // The model is the engine's to answer for, not the selection's:
            // the default agent may have named one of its own, and a resumed
            // session restores the one it was left on.
            let mut app = App::new(
                engine,
                notice(&[selection.notice, theme_notice, snapshot_notice, member_notice]),
                themes,
            )
            .with_provider(provider_id)
            .with_keybinds(keys)
            // Inline image previews, only where the environment says a kitty
            // ancestor will actually draw them (2026-08-15).
            .with_graphics(graphics::Emitter::detect())
            // The `tui` table's moments, written to the same stdout the frame
            // rides — the notifier the app's focus gate emits through
            // (**D468**).
            .with_notifier(notify::Notifier::to_stdout(config.tui.clone()))
            // The `tui.statusline` roster, when the config wrote one; absent,
            // the bar keeps its fixed default layout (**D469**).
            .with_statusline(config.tui.statusline.as_ref())
            // The bypass, from the command line and from nowhere else: no
            // config key turns this on, because a flag is written once by
            // somebody who meant it and a file is written once and forgotten
            // (**D479**).
            .with_yolo(yolo)
            // The kitty verdict: with the protocol active the split-Esc
            // ambiguity cannot occur, so the repair runs in passthrough
            // (**D516**, **D517**).
            .with_kitty_keys(kitty)
            .with_focused(focused)
            // The one place the prompt history reaches the disk: the default
            // store is inert, so a test that does not opt in never touches the
            // machine's own history.
            .with_history(History::load())
            .with_root(project.root())
            // The `@` file menu walks from here, so a mention resolves against
            // the directory the user opened rather than the project root: what
            // they typed is relative to where they are standing.
            .with_cwd(cwd)
            // The registration record's own source column (**D527**, AC-4):
            // `user` for `--name`, `derived` for the project-basename
            // fallback — the same resolution [`Engine::set_self_name`] was
            // seeded from, above.
            .with_self_name_source(name_source)
            .watching_mcp(config.mcp.len());
            // The lister's gate is wider than the socket's (**D530**): every
            // interactive session that is not a pane member gets one — team
            // or none — so it is read off `membership` before that value
            // moves into `with_member` below.
            let is_member = membership.is_some();
            // The member's inbox, on the tick that already polls everything
            // else here; a session nobody launched as a teammate installs
            // nothing and reads nothing (§10.3).
            if let Some((membership, _)) = membership {
                app = app.with_member(member::Inbox::new(membership));
            }
            if let Some((binder, served)) = socket {
                app = app.with_socket(binder, served);
            }
            if !is_member && let Some(lister) = lister {
                app = app.with_lister(lister);
            }
            // A **teamless** session binds no socket, so its own collision
            // scan has no bound path to read a directory off — an explicit
            // `--socket-dir` reaches it only through this seam (a lead's own
            // scan already gets the override for free, off its bound path).
            if !is_member && let Some(socket_dir) = socket_dir {
                app = app.with_registry_directory(socket_dir);
            }
            app.seed(seed);
            // `SessionEnd` fires at the tail of this call rather than beside
            // `jobs.shutdown()` below: `run` consumes the app, and the engine
            // it consumed is the only thing that knows which session to name.
            app.run(&mut terminal).await
        }
        Err(error) => Err(error),
    };
    // Nothing is waiting on the loops, but a background task that outlives the
    // screen it was feeding is a leak whichever way the run ended.
    background.cancel();
    // Teammates first, and the order is a requirement rather than a
    // preference: a shutdown **settles** each teammate's turn rather than
    // killing it, because its transcript is a session somebody may open
    // tomorrow — and a turn still settling may still be calling an MCP tool.
    // Held apart for the reason `jobs` is: the engine moved into the app.
    if let Some(teammates) = teammates {
        teammates.shutdown().await;
    }
    // Every local server's process group ends here. Through this handle rather
    // than through `Engine::shutdown_mcp`, which is the same call one layer
    // down: the engine moved into the app, and `App::run` consumes it.
    servers.shutdown().await;
    jobs.shutdown().await;
    let restored = restore();

    outcome.and(restored)
}

/// The two halves of the system prompt `engine` runs under.
///
/// The model is asked of the engine rather than taken from the provider
/// selection, and that is the whole reason this takes an [`Engine`] instead of
/// a name. Both halves are written against a model id — the base half picks a
/// prompt by family (`claude` / `gpt` / neither), and the suffix's environment
/// block names the model twice — so composing them against the launch model
/// while the engine had already adopted the default agent's own would tell a
/// `gpt` session it was Claude, in a block that also states its model as fact.
/// This must therefore run **after** `with_agents`.
///
/// The base half is handed over **explicitly** rather than left to [`None`].
/// `Engine::system_for` resolves an agent's own prompt *or* the base one, and
/// both agents a session can start on — `build` and `plan` — deliberately have
/// no prompt of their own: what makes `plan` plan is a reminder injected per
/// turn, not a system prompt. Passing [`None`] here would leave every one of
/// their turns carrying the environment block and nothing else.
///
/// The suffix is the half no agent replaces, so an agent switch swaps the
/// first and keeps this one. This composes the pair the session starts on;
/// keeping each half current across a later model switch is the engine's
/// business — `Engine::with_environment` for the suffix and
/// `Engine::with_base_for_model` for the base — and the caller installs both
/// beside this.
fn system_parts(engine: &Engine, config: &Config, cwd: &Path) -> (Option<String>, Option<String>) {
    let model = engine.model();

    (Some(instruction::base_prompt(&model).to_owned()), instruction::suffix(config, cwd, &model))
}

/// Applies `config`'s theme and mode, answering with anything worth saying
/// about it.
///
/// A `theme` naming something this build does not have leaves the default in
/// place and says so, rather than failing the run: a custom theme file the
/// user deleted should cost them that theme, exactly as a malformed one does,
/// and not their session (deviation: config-theme-unknown-is-a-notice).
fn configure_themes(themes: &mut Themes, config: &Config) -> Option<String> {
    // The mode goes first so that the selection below resolves in the arm the
    // config asked for rather than in the default one.
    if let Some(mode) = config.theme_mode {
        themes.set_mode(match mode {
            ThemeMode::Dark => Mode::Dark,
            ThemeMode::Light => Mode::Light,
        });
    }

    let name = config.theme.as_deref()?;
    if themes.select(name).is_none() {
        return Some(format!("no theme named {name:?}; using {}", themes.active()));
    }

    None
}

/// The status bar's opening line: whatever startup had to say, in one string.
///
/// Everything that has something to say gets a say, in the order it is passed:
/// a session can start on the fake provider, in a theme it could not find, in
/// a directory it cannot snapshot, and none of those three is a reason to
/// swallow the others.
fn notice(parts: &[Option<String>]) -> Option<String> {
    let said: Vec<&str> = parts.iter().filter_map(Option::as_deref).collect();

    (!said.is_empty()).then(|| said.join(NOTICE_SEPARATOR))
}

/// Installs the session `resume` names and hands back its transcript.
///
/// Split out of [`run`] because it is the one part of the startup path worth
/// testing on its own: everything around it needs a terminal, and what has to
/// be true here — that a resume either produces the session that was asked for
/// or fails loudly — is exactly what a silent fallback would break.
async fn stored_transcript(engine: &Engine, resume: Resume) -> Result<Vec<Message>> {
    let id = match resume {
        Resume::Latest => engine
            .sessions()
            .await
            .context("failed to list the stored sessions")?
            .into_iter()
            // `sessions()` answers newest first, so the latest is the first.
            .next()
            .map(|info| info.id)
            .context("there is no stored session to continue in this project")?,
        Resume::Session(id) => SessionId::from(id),
    };

    engine.resume(&id).await.context("failed to resume the session")
}

/// Turns on wheel reporting, bracketed paste and focus reporting, and extends
/// the panic hook to turn all three back off.
///
/// Bracketed paste is what makes a pasted paragraph one event instead of a
/// stream of keystrokes — without it, the newline in the middle of a paste is
/// an Enter, and Enter here sends the prompt. Left enabled on the way out it
/// would leave the user's shell wrapping their pastes in escapes nothing there
/// reads (**R13**). Focus reporting is how the notifier's gate learns whether
/// anybody is watching (**D468**), and it holds to the same discipline: left
/// on, the user's shell would be fed focus escapes nothing there reads.
fn capture_input() -> Result<()> {
    execute!(stdout(), EnableMouseCapture).context("failed to enable mouse reporting")?;
    execute!(stdout(), EnableBracketedPaste).context("failed to enable bracketed paste")?;
    execute!(stdout(), EnableFocusChange).context("failed to enable focus reporting")?;

    let installed = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Pop before the disables, mirroring push-last; popping an empty
        // stack is a no-op by the kitty spec, so a session that never pushed
        // loses nothing here.
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
        let _ = execute!(stdout(), DisableFocusChange);
        let _ = execute!(stdout(), DisableBracketedPaste);
        let _ = execute!(stdout(), DisableMouseCapture);
        installed(info);
    }));

    Ok(())
}

/// Enables the kitty keyboard protocol where the terminal offers it
/// (**D517**), answering whether it did.
///
/// The probe writes a query to the tty and blocks up to two seconds for an
/// answer (`crossterm`'s own recommended detection), so `GANJA_DISABLE_TERM_PROBE`
/// skips it wholesale — the kill switch for a terminal that never answers,
/// and what every pty drill sets, because a test harness is exactly such a
/// terminal. Only `DISAMBIGUATE_ESCAPE_CODES` is pushed: key events stay
/// Press-shaped and every existing match arm holds; what changes is that Esc
/// arrives as `CSI 27 u`, which no read boundary can split into a phantom
/// key — the ambiguity [`escrepair`] exists to repair, removed at the
/// protocol level, which is why a session that pushed the flag runs the
/// repair in passthrough.
///
/// Must run before the first `EventStream` poll: the probe reads its answer
/// off the same internal queue the stream consumes.
/// Whether the terminal this ganja came up in is being looked at.
///
/// Focus is learned from changes — crossterm's `FocusGained`/`FocusLost` —
/// and a change presumes a starting state. Outside tmux the only honest one
/// is "looked at". Inside tmux the state is a question tmux answers
/// (`Server::focused`), and it has to be asked: measured on next-3.8
/// (2026-08-25), a pane that enables focus reporting is sent nothing about
/// the state it starts in, and a ganja spawned into a pane beside the lead —
/// every teammate — would take itself for looked-at until the first change
/// and announce nothing (**D468**). A tmux that will not answer leaves the
/// default.
async fn initial_focus() -> bool {
    use ganja_core::teammate::tmux::{Server, TMUX_PANE};

    let Ok(pane) = std::env::var(TMUX_PANE) else {
        return true;
    };
    let Ok(server) = Server::current() else {
        return true;
    };
    server.focused(&pane).await.unwrap_or(true)
}

fn capture_keys() -> bool {
    let disabled = std::env::var("GANJA_DISABLE_TERM_PROBE").is_ok_and(|value| {
        let value = value.to_ascii_lowercase();
        value == "1" || value == "true"
    });
    if disabled {
        return false;
    }
    if !matches!(ratatui::crossterm::terminal::supports_keyboard_enhancement(), Ok(true)) {
        return false;
    }
    match execute!(
        stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    ) {
        Ok(()) => {
            tracing::info!("kitty keyboard protocol enabled");
            true
        }
        Err(error) => {
            tracing::warn!("failed to push keyboard enhancement flags: {error}");
            false
        }
    }
}

fn restore() -> Result<()> {
    // Placements outlive the cells they sat on; the broom runs before the
    // screen is handed back (2026-08-15). Best-effort by nature — a terminal
    // that never drew any ignores an APC it never learned.
    if let Some(emitter) = graphics::Emitter::detect() {
        use std::io::Write as _;
        let mut out = stdout();
        let _ = out.write_all(emitter.delete_all().as_bytes());
        let _ = out.flush();
    }
    // Pop first, mirroring push-last; a no-op when nothing was pushed.
    let keys =
        execute!(stdout(), PopKeyboardEnhancementFlags).context("failed to pop keyboard flags");
    let focus = execute!(stdout(), DisableFocusChange).context("failed to disable focus reporting");
    let paste =
        execute!(stdout(), DisableBracketedPaste).context("failed to disable bracketed paste");
    let mouse =
        execute!(stdout(), DisableMouseCapture).context("failed to disable mouse reporting");
    let terminal = ratatui::try_restore().context("failed to restore the terminal");

    keys.and(focus).and(paste).and(mouse).and(terminal)
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
