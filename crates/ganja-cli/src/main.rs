//! `ganja` — a terminal-first AI coding agent.
//!
//! Running the binary with no subcommand starts the terminal UI, which is what
//! the tool is for; the subcommands exist to set it up and to answer questions
//! about it without taking the screen over.

// `main`'s dispatch `match` covers every subcommand in one async fn, and the
// generated state machine's layout computation — which only clippy's fuller
// analysis walks, not a plain build — sits close enough to the default 128
// query-depth limit that one more subcommand arm growing by a few lines
// overflows it. Raising the limit is the standard fix for a large async fn's
// state machine (rustc suggests exactly this); splitting the match is a
// larger, unrelated refactor this file's growth does not otherwise call for.
#![recursion_limit = "256"]

use std::{
    io::{self, IsTerminal as _, Read as _, Write as _},
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use ganja_core::{McpStatus, SessionInfo, Storage, auth, catalog};
use ganja_permission::Project;
use ganja_protocol::Usage;
use secrecy::{SecretString, zeroize::Zeroize as _};
use tracing_appender::non_blocking::WorkerGuard;

mod assemble;
mod import;
mod login;
mod mcp;
mod plugin;
mod run;
mod serve;

// A plain comment, and above the doc comment rather than below it: clap
// renders a doc comment as the help a person reads — every line of it — and
// why a flag combination is refused is a note to whoever edits this file. A
// comment between the doc comment and the derive would break the association
// clap needs and drop the summary below to the manifest's description.
//
// `args_conflicts_with_subcommands` is what stops `ganja --continue models`
// from parsing: a resume flag describes the session a UI run opens, so an
// invocation that is not a UI run has no use for one, and quietly ignoring it
// would look like it had been honored.
/// Terminal-first AI coding agent.
#[derive(Debug, Parser)]
// No bare `about`: that spelling takes the manifest's `description`, which
// says what the crate *is* to a package index, where `--help` should say what
// the binary is for. The doc comment above is that sentence, and the only one.
#[command(name = "ganja", version, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Absent means the interactive UI, which is the point of the binary.
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    resume: ResumeArgs,
    #[command(flatten)]
    select: SelectArgs,
    #[command(flatten)]
    bypass: BypassArgs,
    /// Write the log file at debug level instead of info.
    ///
    /// What that buys is the provider wires' own account of a turn — the model
    /// and endpoint each request chose, the status that came back, and the
    /// body of a refusal — which is what makes a failure diagnosable after the
    /// fact rather than only while it is happening.
    ///
    /// RUST_LOG still wins wherever it is set: it can name one module and one
    /// level where this flag has only the one setting, so a flag that overrode
    /// it would take away the only way to ask for less than everything. This
    /// replaces the default, and nothing more.
    ///
    /// Write it after the subcommand — `ganja models -v` — or on its own for
    /// the UI: `ganja -v models` is refused, because
    /// `args_conflicts_with_subcommands` negates *every* argument written
    /// before a subcommand and clap has no exemption for a global one
    /// (`clap_builder/src/parser/parser.rs:484`, which branches on nothing but
    /// "some argument matched").
    // `global` is what puts it on every subcommand rather than only on the UI
    // run, and it is the whole reason the flag can be written at all beside
    // `models`, `run` or `serve`.
    #[arg(long, short = 'v', global = true)]
    verbose: bool,
}

/// Which stored session the UI opens, if any.
///
/// `multiple = false` makes clap itself refuse `--continue --session x`: the
/// two name different sessions, and a hand-written check would have to invent
/// which of them wins.
#[derive(Debug, Args)]
#[group(multiple = false)]
struct ResumeArgs {
    /// Resume the most recently updated session of this project.
    // `continue` is a keyword, so the field is raw-identified rather than
    // renamed: what the flag is called is upstream's `--continue`/`-c`.
    #[arg(long, short = 'c')]
    r#continue: bool,
    /// Resume the session with this id, as `ganja sessions` lists it.
    #[arg(long, short = 's', value_name = "ID")]
    session: Option<String>,
}

impl ResumeArgs {
    /// What the flags ask the UI to open. Absent is a fresh session.
    fn wanted(self) -> Option<ganja_tui::Resume> {
        if self.r#continue {
            return Some(ganja_tui::Resume::Latest);
        }

        self.session.map(ganja_tui::Resume::Session)
    }
}

/// Which model, agent and config file the interactive UI starts from.
///
/// The flag tier of the config precedence: these outrank the environment,
/// which outranks the files — `ganja-core` owns that ordering, these only
/// carry the words in.
#[derive(Debug, Args)]
struct SelectArgs {
    /// Ask this model, spelled "provider/model" the way the config's `model` key is.
    #[arg(long, value_name = "PROVIDER/MODEL")]
    model: Option<String>,
    /// Start on this agent instead of the roster's default.
    #[arg(long, value_name = "NAME")]
    agent: Option<String>,
    /// Merge exactly this config file, outranking `GANJA_CONFIG` and discovery.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

impl SelectArgs {
    /// The flag tier, in the shape `ganja-core` merges above everything else.
    fn overrides(self) -> ganja_core::config::Overrides {
        ganja_core::config::Overrides {
            model: self.model,
            agent: self.agent,
            config_file: self.config,
        }
    }
}

/// Whether the interactive session answers its own permission dialogs.
///
/// The same trio `ganja run` carries (`run.rs`), and deliberately the same
/// three words: one entry point that took `--yolo` and another that took only
/// `--auto` would be two flag languages for one decision, and whoever wrote
/// the alias into a shell function does not think of the UI and the headless
/// run as different products.
///
/// Not `global`: `args_conflicts_with_subcommands` negates every argument
/// written before a subcommand, and there is nothing a subcommand could mean
/// by this that it does not already carry itself — `run` has its own trio, and
/// nothing else in the table opens a dialog at all.
#[derive(Debug, Args)]
struct BypassArgs {
    /// Answer every permission dialog with "allow once" instead of asking.
    ///
    /// Dangerous, and the reason it is spelled out rather than hidden behind
    /// the alias below: the session stops asking before it runs anything, so
    /// an "allow" here is an allow for whatever the model decided to run.
    /// Nothing is remembered — no answer reaches the project's stored rules —
    /// so it lasts exactly this run and no longer (**D479**).
    ///
    /// A rule that already says *deny* still denies. This answers the dialogs
    /// the rules raise, and a denial raises none: a standing "no" written in a
    /// config is the user's own word, and a flag on one invocation does not
    /// get to overrule it.
    ///
    /// `question` and the plan doors still ask. A person is sitting in front
    /// of this session — which is exactly what `ganja run` does not have — so
    /// what this bypasses is *permission*, never *conversation*.
    #[arg(long)]
    auto: bool,
    // Two hidden spellings of `--auto`, carried for the reason `run`'s are
    // (`run.rs:236-240`): upstream carries them and scripts written against it
    // pass them. `--yolo` is also the spelling the other terminal agents use,
    // and the one this build was asked for by name.
    #[arg(long, hide = true)]
    yolo: bool,
    #[arg(long = "dangerously-skip-permissions", hide = true)]
    dangerously_skip_permissions: bool,
}

impl BypassArgs {
    /// Whether any of the three spellings was written.
    fn wanted(self) -> bool {
        self.auto || self.yolo || self.dangerously_skip_permissions
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the API keys providers are authenticated with.
    Auth {
        #[command(subcommand)]
        action: Auth,
    },
    /// Work with ganja's configuration files.
    Config {
        #[command(subcommand)]
        action: Config,
    },
    /// Show the configured MCP servers and the tools they lend, or manage one.
    ///
    /// `add`, `get` and `remove` edit the `mcp` table of a config file;
    /// `list` (also the bare word) connects every enabled server and reports
    /// what it found; `login` runs an OAuth flow for one remote server.
    ///
    /// With no further word: every enabled server is connected, so the
    /// standing reported is one this build actually reached rather than one
    /// the config merely asked for, and every connection is closed again
    /// before this returns.
    Mcp {
        #[command(subcommand)]
        action: Option<McpAction>,
    },
    /// List the models this build knows how to size and price.
    Models {
        /// List only the models this provider serves.
        #[arg(value_name = "PROVIDER")]
        provider: Option<String>,
        /// Fetch the published catalog first, however recently it was fetched.
        #[arg(long)]
        refresh: bool,
    },
    /// Manage installed plugins and the marketplaces they come from.
    ///
    /// A plugin's skills, agents, hooks, MCP servers and LSP entries join
    /// the config at the next session start. Installing is explicit on
    /// purpose: hooks and servers run with your own authority, and the typed
    /// command is the consent.
    Plugin {
        #[command(subcommand)]
        action: plugin::PluginAction,
    },
    /// Send one message and print the turn it produces, without the UI.
    ///
    /// Everything a session normally has is here — the agents, the tools, the
    /// permission rules, the stored history — except a person to answer a
    /// dialog, so a call that would have opened one is refused instead. Use
    /// `--auto` to allow them, and mean it: nobody is watching.
    Run(run::RunArgs),
    /// Serve the engine over HTTP until SIGINT or SIGTERM.
    ///
    /// The same engine the UI drives, behind REST routes and an SSE event
    /// stream instead of a screen. Loopback by default; binding anything
    /// else requires GANJA_SERVER_PASSWORD, and every route then asks for it.
    Serve(serve::ServeArgs),
    /// List the stored sessions of the project this was run in.
    Sessions,
}

#[derive(Debug, Subcommand)]
enum McpAction {
    /// Write one server into a config file's `mcp` table.
    ///
    /// `--url` makes a remote server and a trailing `-- <cmd> [args…]` makes
    /// a local one; exactly one of the two. The entry is refused before
    /// anything is written if this build could not read it back, and a
    /// `ganja.jsonc` at the target tier is refused by name rather than
    /// rewritten without its comments.
    Add(mcp::AddArgs),
    /// Show the configured servers and the tools they lend.
    ///
    /// The bare `ganja mcp` does exactly this; the word exists so the surface
    /// reads add/list/get/remove.
    List,
    /// Show one server as it resolved, and which file it came from.
    Get {
        /// The server's name, as `ganja mcp list` shows it.
        name: String,
    },
    /// Delete one server from a config file's `mcp` table.
    Remove(mcp::RemoveArgs),
    /// Start an OAuth login for one remote server configured with `oauth`.
    ///
    /// Discovery, registration and the browser wait all run here — the same
    /// flow the `/mcp` dialog's Login action drives — and a completed login
    /// is stored under `mcp:<server>`, ready for the next connect. Refused by
    /// name for a server that is not configured, is local, or names no
    /// `oauth`: there is nothing this build could mean by a login for it.
    Login {
        /// The server's name, as configured under `mcp`.
        server: String,
    },
}

#[derive(Debug, Subcommand)]
enum Config {
    /// Translate an opencode config into a ganja one.
    ///
    /// Config keys only, in one direction: what maps is written to a new
    /// `ganja.json`, and everything else is listed with the reason it was left
    /// out. An API key is never written, and `{env:…}`/`{file:…}` is never
    /// expanded.
    ImportOpencode {
        /// Import exactly this file, instead of looking for opencode's.
        ///
        /// Nothing else is read, so `--global` then only decides where the
        /// result lands.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        /// Read only opencode's global config, and write ganja's global one.
        ///
        /// Without this the project's own files are read too, and the result
        /// lands at the project root.
        #[arg(long)]
        global: bool,
        /// Print what would be imported and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum Auth {
    /// Store a provider's credential, by logging in or by giving a key.
    ///
    /// A key is taken from `--key`, else from standard input when it is piped
    /// in, else from a prompt the terminal does not echo. A provider with a
    /// login of its own runs that instead — see `--method`.
    ///
    /// Storing a credential is all this does. Which models then run on it is a
    /// separate question, and a login that succeeded is not an answer to it.
    Login {
        /// Provider the credential belongs to: one this build ships
        /// (anthropic, openai, openrouter, opencode, opencode-go, grok,
        /// github-copilot), or an id this project's config declares under
        /// `provider`.
        #[arg(
            long,
            value_parser = named_provider,
            default_value = "anthropic",
            value_name = "PROVIDER"
        )]
        provider: NamedProvider,
        /// The key itself.
        ///
        /// Every process on the machine can read another's command line, so
        /// prefer piping the key in or typing it at the prompt.
        #[arg(long, value_name = "KEY")]
        key: Option<String>,
        /// How to log in, instead of being asked.
        ///
        /// `api` is a key; `browser` opens one on this machine; `device` shows
        /// a code to type into a browser anywhere. Each provider has only some
        /// of them, and naming one it has not is refused.
        #[arg(long, short = 'm', value_enum, value_name = "METHOD")]
        method: Option<login::Method>,
        /// Which GitHub a Copilot login is against, instead of being asked.
        ///
        /// `public` is github.com and needs nothing else, which is what makes
        /// the common Copilot login runnable with nobody at the keyboard;
        /// `enterprise` still needs an address, from `--enterprise-url` or from
        /// the question that follows.
        #[arg(long, value_enum, value_name = "DEPLOYMENT")]
        deployment: Option<login::DeploymentKind>,
        /// The GitHub Enterprise deployment a Copilot login is against.
        ///
        /// Answers both of the questions the login would otherwise ask, which
        /// is what makes it runnable with nobody at the keyboard. A domain or a
        /// URL: `company.ghe.com` and `https://company.ghe.com/` name the same
        /// deployment.
        #[arg(long, value_name = "URL")]
        enterprise_url: Option<String>,
    },
    /// Show which providers have a credential, of what kind, and where it comes
    /// from.
    List,
    /// Forget a provider's stored credential.
    Logout {
        /// Provider to forget: one this build ships, or an id this project's
        /// config declares under `provider`.
        #[arg(long, value_parser = named_provider, value_name = "PROVIDER")]
        provider: NamedProvider,
    },
}

/// A provider named on a command line.
///
/// Two tiers, because that is what a session may run as: the ones this build
/// ships, and the endpoints a config declares. The split is in the type rather
/// than in a bare string so that every place downstream has to say which it is
/// holding — an OAuth flow exists only for the first, and only the second can
/// fail to exist at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NamedProvider {
    /// One this build authenticates itself.
    Builtin(ProviderId),
    /// An id a config's `provider` table declares. Checked when the command
    /// runs rather than when the argument parses: clap knows nothing about a
    /// file that has not been read, and reading one to validate a flag would
    /// make `--help` depend on the working directory.
    Configured(String),
}

impl NamedProvider {
    /// The identifier `ganja-core` knows the provider by, which for a
    /// configured one is the id its entry was written under.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Builtin(builtin) => builtin.as_str(),
            Self::Configured(id) => id,
        }
    }

    /// The builtin this names, where it names one — the flows that exist per
    /// provider rather than per credential, which is every OAuth login.
    fn builtin(&self) -> Option<ProviderId> {
        match self {
            Self::Builtin(builtin) => Some(*builtin),
            Self::Configured(_) => None,
        }
    }
}

impl std::fmt::Display for NamedProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Parses a provider argument, preferring the builtins.
///
/// The `ValueEnum` still decides what a shipped name means, so `github-copilot`
/// keeps resolving to the provider rather than to a string that happens to
/// match one. Anything else is carried as a configured id and validated
/// against the loaded config by [`configured_provider_exists`] — where the
/// refusal can name the config's own entries, which is the whole point of
/// deferring it.
fn named_provider(spelled: &str) -> Result<NamedProvider, String> {
    if let Ok(builtin) = ProviderId::from_str(spelled, false) {
        return Ok(NamedProvider::Builtin(builtin));
    }
    if spelled.trim().is_empty() {
        return Err("a provider name cannot be blank".to_owned());
    }

    Ok(NamedProvider::Configured(spelled.to_owned()))
}

/// Refuses a configured provider this project's config does not declare.
///
/// A builtin is nothing to check — clap already did. For anything else the
/// config is the authority, and the refusal names **both** tiers, because
/// somebody who mistyped their own entry needs to see what they actually
/// wrote.
fn configured_provider_exists(provider: &NamedProvider) -> Result<()> {
    let NamedProvider::Configured(id) = provider else {
        return Ok(());
    };

    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let config = ganja_core::config::Config::load(&cwd).context("failed to read the config")?;
    if config.provider.contains_key(id) {
        return Ok(());
    }

    let declared = config
        .provider
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "no provider `{id}`; this build ships {}{}",
        ganja_core::provider::PROVIDERS.join(", "),
        if declared.is_empty() {
            ", and this project's config declares none".to_owned()
        } else {
            format!(", and this project's config declares {declared}")
        }
    )
}

/// The providers this build can authenticate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ProviderId {
    Anthropic,
    // Without this clap would derive `open-ai`, which is nobody's name for it.
    // The two below derive the names their providers already have.
    #[value(name = "openai")]
    OpenAi,
    // And this one would derive `open-router`, which is nobody's name for it
    // either — the vendor, the catalog and the credential file all spell it as
    // one word.
    #[value(name = "openrouter")]
    OpenRouter,
    // The vendor's gateway, under the vendor's own ids. `opencode` naming a
    // provider *inside an opencode port* is confusing and deliberate: it is
    // what the catalog files the rows under, and any other spelling would cost
    // them their sizing and pricing.
    #[value(name = "opencode")]
    Opencode,
    #[value(name = "opencode-go")]
    OpencodeGo,
    Grok,
    GithubCopilot,
    // Parses so its refusal can name the deferral; a name clap rejected would
    // read as a typo rather than as the stub it is.
    Cursor,
}

impl ProviderId {
    /// The identifier `ganja-core` knows the provider by.
    ///
    /// The constants rather than literals wherever the module that owns the
    /// name exports one: a command-line argument, a config key and a provider
    /// id all mean the same provider, and a login that wrote under one spelling
    /// while a request read another would read as a storage bug rather than as
    /// the naming one it is.
    ///
    /// **`grok` and not `xai`**, deliberately: `xai` is what the credential is
    /// stored under so that a shared `auth.json` keeps working, and
    /// [`auth::storage_key`] is the single place that translation happens.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => auth::openai::PROVIDER_ID,
            Self::OpenRouter => ganja_core::provider::openrouter::ID,
            Self::Opencode => ganja_core::provider::opencode::ZEN_ID,
            Self::OpencodeGo => ganja_core::provider::opencode::GO_ID,
            Self::Grok => auth::grok::PROVIDER_ID,
            Self::GithubCopilot => auth::copilot::PROVIDER_ID,
            Self::Cursor => ganja_core::provider::cursor::ID,
        }
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Directory ganja keeps its state in, under the XDG data home. Spelled here
/// rather than asked of `ganja-core`, which resolves the same directory
/// privately in two places already and exports neither.
const DIRECTORY: &str = "ganja";

/// Directory rolling log files land in, under [`DIRECTORY`]. Not per project:
/// a log is about the run, and a run that could not resolve a project still
/// has something worth writing down.
const LOGS: &str = "log";

/// What every log file is named before the appender's date, and the extension
/// after it: `ganja.2026-08-04.log`.
const LOG_NAME: &str = "ganja";
const LOG_EXTENSION: &str = "log";

/// Days of logs kept. A rotation with no bound is a disk leak on a machine
/// nobody is watching; a week is enough to still have the run being asked
/// about.
const LOG_FILES: usize = 7;

/// What is traced when `RUST_LOG` says nothing, or says something the filter
/// cannot parse.
const DEFAULT_FILTER: &str = "info";

/// What `-v` traces instead, when `RUST_LOG` says nothing.
///
/// This workspace's crates at debug and everything else left at info, rather
/// than a bare `debug`: the point of the flag is the provider wires' account of
/// a turn, and hyper, h2 and rustls at debug bury it under socket bookkeeping
/// nobody asked for.
///
/// One directive covers all nine crates because an `EnvFilter` target is
/// matched as a **raw string prefix** (`filter/env/directive.rs`), and every
/// target here — `ganja_core::session`, `ganja_provider::provider::responses`,
/// and the rest — begins with those five letters.
const VERBOSE_FILTER: &str = "info,ganja=debug";

/// Where a project's sessions live, under its data directory.
///
/// Pinned to the directory `ganja-tui` opens `Engine::persistent` on: the two
/// crates naming it separately is what the frozen seam asks for, and a listing
/// that read a different one would show nothing and look correct doing it.
const STORAGE: &str = "storage";

/// What a session with no title is listed as. Most of them: a title is earned
/// by a completed turn, and the fake provider never earns one.
const UNTITLED: &str = "(untitled)";

/// What a line hanging under a row of the MCP listing starts with.
const INDENT: &str = "    ";

#[tokio::main]
async fn main() -> Result<()> {
    // Parsed before the log is installed so that `--version`, `--help` and a
    // usage error do not create a log directory for a run that never started.
    let cli = Cli::parse();
    // Held until `main` returns: dropping the guard stops the appender's
    // worker thread, and whatever it had not written is lost. The flag comes
    // from the already-parsed `Cli` because this runs before any subcommand is
    // dispatched, and a level decided after the fact would miss the startup it
    // was set to watch.
    let _logging = install_logging(cli.verbose);

    match cli.command {
        None => {
            ganja_tui::run(
                cli.resume.wanted(),
                cli.select.overrides(),
                cli.bypass.wanted(),
            )
            .await
        }
        Some(Command::Auth { action }) => auth_command(action).await,
        Some(Command::Config { action }) => config_command(action),
        Some(Command::Mcp { action }) => mcp_command(action).await,
        Some(Command::Models { provider, refresh }) => models_command(provider, refresh).await,
        Some(Command::Plugin { action }) => plugin::plugin_command(action),
        Some(Command::Run(args)) => run::run(args).await,
        Some(Command::Serve(args)) => serve::serve(args).await,
        Some(Command::Sessions) => sessions_command(),
    }
}

fn config_command(action: Config) -> Result<()> {
    match action {
        Config::ImportOpencode {
            file,
            global,
            dry_run,
        } => import::import_opencode(file, global, dry_run),
    }
}

/// Sends `tracing` output to a rolling file under the data home.
///
/// A file is the only sink, and that is the point rather than a default: the
/// UI owns the alternate screen for as long as it runs, so a subscriber
/// writing to stdout or stderr would draw over the thing being diagnosed.
/// Subcommands have the same claim on stdout, which callers capture.
///
/// Nothing here is fatal. A log is a diagnostic, and a run that cannot write
/// one is still a run worth having, so a data home that will not resolve or a
/// directory that will not be created costs the run its log and says so on
/// stderr — safe, because this happens before the terminal is taken over.
///
/// `verbose` is `-v`, and it moves the *default* level only: see
/// [`resolve_filter`] for the precedence and [`VERBOSE_FILTER`] for what it
/// turns on.
///
/// The returned guard flushes the appender's worker thread when it drops.
#[must_use]
fn install_logging(verbose: bool) -> Option<WorkerGuard> {
    let directory = match log_directory() {
        Ok(directory) => directory,
        Err(error) => return declined(&format!("{error:#}")),
    };

    // Created here rather than left to the appender, which opens its file only
    // *after* pruning the old ones — and pruning reads a directory that, on a
    // first run, nothing has made yet. The appender recovers and creates it,
    // but not before writing its own complaint to stderr, and it lands on the
    // one run where a user has the least context for reading it as harmless.
    if let Err(error) = std::fs::create_dir_all(&directory) {
        return declined(&format!(
            "{} could not be created: {error}",
            directory.display()
        ));
    }

    let appender = match tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(LOG_NAME)
        .filename_suffix(LOG_EXTENSION)
        .max_log_files(LOG_FILES)
        .build(&directory)
    {
        Ok(appender) => appender,
        Err(error) => {
            return declined(&format!("{} is not writable: {error}", directory.display()));
        }
    };

    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = resolve_filter(
        std::env::var(tracing_subscriber::EnvFilter::DEFAULT_ENV)
            .ok()
            .as_deref(),
        verbose,
    );
    let installed = tracing_subscriber::fmt()
        .with_writer(writer)
        // A file is not a terminal, so colour codes in it are noise every
        // reader has to filter back out.
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();

    match installed {
        // The guard is what keeps the worker thread alive, so it only means
        // anything once something is actually feeding it.
        Ok(()) => Some(guard),
        Err(error) => declined(&format!("a subscriber is already installed: {error}")),
    }
}

/// What the run traces, given whatever `RUST_LOG` said and whether `-v` was
/// passed.
///
/// **`RUST_LOG` outranks the flag.** The variable names a module and a level
/// where the flag has only its one setting, so a flag that overrode it would
/// take away the only way to ask for less than everything — or for more than
/// this workspace's own crates. The flag moves the default, which is the level
/// a run has when nobody said anything.
///
/// `configured` is passed in rather than read here so the precedence can be
/// exercised without a test mutating the environment of the process it shares
/// with every other test in this binary.
fn resolve_filter(configured: Option<&str>, verbose: bool) -> tracing_subscriber::EnvFilter {
    // A value that will not parse falls through to the default, which is what
    // `try_from_default_env` did before the flag existed. An *empty* value is a
    // value: it parses, to a filter that enables nothing, and honouring that is
    // the same behaviour this had when it read the variable directly.
    configured
        .and_then(|spelled| tracing_subscriber::EnvFilter::try_new(spelled).ok())
        .unwrap_or_else(|| {
            tracing_subscriber::EnvFilter::new(if verbose {
                VERBOSE_FILTER
            } else {
                DEFAULT_FILTER
            })
        })
}

/// Says why there will be no log this run, and answers [`None`] so the caller
/// reads as one expression.
fn declined(reason: &str) -> Option<WorkerGuard> {
    eprintln!("note: not logging to a file: {reason}");

    None
}

/// `<data home>/ganja/log`.
fn log_directory() -> Result<PathBuf> {
    use etcetera::base_strategy::{BaseStrategy as _, Xdg};

    // XDG conventions on every platform, macOS included, matching how
    // `ganja_core::auth` and `ganja_permission::project` resolve their own
    // paths.
    let base = Xdg::new().context("the home directory holding the log could not be located")?;

    Ok(base.data_dir().join(DIRECTORY).join(LOGS))
}

/// Shows every configured MCP server, where connecting to it got, and the
/// tools it lends the agent loop.
///
/// Spec: upstream `packages/opencode/src/cli/cmd/mcp.ts` — its `McpListCommand`
/// is what this ports, one row per configured server carrying a standing and
/// the command or URL it was reached at. Upstream spells the listing `opencode
/// mcp list`, under a parent whose other children — `add`, `auth`, `logout`,
/// `debug` — are all about OAuth or about writing config, and none of which
/// this build has. A parent with one child would be a menu with one item, so
/// the listing *is* the subcommand here, the way `sessions` and `models` are
/// (deviation: mcp-listing-is-the-subcommand). Adding those siblings later
/// nests this under `list`, which is a rename of one word.
///
/// Upstream's listing stops at the standing. The tools are ganja's addition and
/// the reason to run this at all: what a server contributes is what the model
/// is offered, under names the permission rules are written against, and there
/// is otherwise nowhere to read them.
///
/// **Servers are dialled.** A status nothing has tried is not a status — the
/// question is what this build makes of the config, and only a connect answers
/// it. No credential is needed for any of it: an MCP server is a peer of the
/// session and not of the model provider, so the config is read directly and no
/// [`ganja_core::Engine`] is built, for the reason `sessions` reads the store
/// directly.
///
/// **Everything is read before the shutdown, and the shutdown always runs.**
/// Closing a connection takes its client and clears its definitions, so a
/// listing that shut down first would report every server as lending nothing.
async fn mcp_command(action: Option<McpAction>) -> Result<()> {
    // The three file-editing actions and the login return before anything is
    // dialled: writing a config file has nothing to learn from connecting,
    // and `get` reports what the loader resolved rather than what a server
    // answered. Only `list` — the word, and the bare `ganja mcp` — connects.
    match action {
        Some(McpAction::Add(args)) => return mcp::add(&args),
        Some(McpAction::Get { name }) => return mcp::get(&name),
        Some(McpAction::Remove(args)) => return mcp::remove(&args),
        Some(McpAction::Login { server }) => return mcp_login_command(&server).await,
        Some(McpAction::List) | None => {}
    }

    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let config = ganja_core::config::Config::load(&cwd).context("failed to read the config")?;

    if config.mcp.is_empty() {
        println!("no MCP servers configured; add one under `mcp` in this project's ganja.json");

        return Ok(());
    }

    // The **project root**, not this process's working directory, because that
    // is what a relative `cwd` in an entry resolves against — the same root
    // `ganja-tui` dials from, so that what this reports is what a session
    // would get.
    let root = Project::resolve(&cwd);
    let servers = ganja_core::McpServers::new(config.mcp.clone(), root.root());
    servers.connect_all().await;

    let standing = servers.status();
    let counts = servers.tool_counts();
    let lent: Vec<String> = servers
        .tools()
        .iter()
        .map(|tool| tool.id().to_owned())
        .collect();
    servers.shutdown().await;

    println!("{:<20}  {:<9}  {:<5}  ADDRESS", "SERVER", "STATUS", "TOOLS");
    // Driven by the config rather than by the statuses, which deliberately omit
    // a server nothing has finished trying: after an awaited `connect_all` there
    // is no such server, and a row that could silently vanish is worse than one
    // that reports it has no standing.
    for (name, entry) in &config.mcp {
        let status = standing.get(name);
        println!(
            "{:<20}  {:<9}  {:<5}  {}",
            name,
            word(status),
            tools_column(counts.get(name).copied()),
            address(entry)
        );

        // A failed server's reason and a connected server's tools both hang
        // under the row, and no server is ever both.
        if let Some(McpStatus::Failed { error }) = status {
            println!("{INDENT}{}", printable(error));
        }
        if status == Some(&McpStatus::Connected) {
            report(&lent, name);
        }
    }

    Ok(())
}

/// Runs an OAuth login for `server` and stores what it produces.
///
/// The same flow the `/mcp` dialog's Login action drives — discovery,
/// registration, PKCE, the loopback wait — reached here instead through
/// [`ganja_provider::auth::mcp_oauth`] directly, since a one-shot CLI process
/// has no engine to ask and no reason to build one: an MCP server's
/// credential is a peer of the session, not of a model provider, the same
/// reasoning [`mcp_command`]'s own listing is built on.
///
/// **Nothing here writes a credential until the login has actually
/// succeeded** — the same "a login that was cancelled left nothing behind"
/// property [`login::oauth`] and its own flows are built on.
///
/// # Errors
///
/// Refused by name when `server` is not configured, is a local server, or
/// names no `oauth`; otherwise whatever discovery, registration or the
/// exchange failed with.
async fn mcp_login_command(server: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let config = ganja_core::config::Config::load(&cwd).context("failed to read the config")?;

    let entry = config
        .mcp
        .get(server)
        .with_context(|| format!("mcp server \"{server}\" is not configured"))?;
    let ganja_core::config::McpServer::Remote(remote) = entry else {
        bail!("mcp server \"{server}\" is a local server; oauth is for remote servers only");
    };
    if remote.oauth.is_none() {
        bail!("mcp server \"{server}\" has no `oauth` configured");
    }

    let cancel = tokio_util::sync::CancellationToken::new();
    let _interrupt = login::Interrupt::watching(cancel.clone());

    let login = ganja_provider::auth::mcp_oauth::Login::new(&remote.url)
        .with_context(|| format!("mcp server \"{server}\" could not start a login"))?;
    let browser = login
        .browser()
        .await
        .with_context(|| format!("mcp server \"{server}\": discovery or registration failed"))?;
    eprintln!("Go to: {}", browser.url());
    eprintln!("Waiting for authorization...");

    let credential = browser
        .wait(ganja_provider::auth::mcp_oauth::CALLBACK_DEADLINE, &cancel)
        .await
        .with_context(|| format!("mcp server \"{server}\": nothing was stored"))?;
    ganja_provider::auth::set_oauth(&format!("mcp:{server}"), &credential)
        .context("the credential could not be stored")?;

    println!("mcp server \"{server}\": login stored");

    Ok(())
}

/// What hangs under a connected server's row: the tools it lends, or the fact
/// that it lends none.
///
/// A connected server that contributed nothing is worth a line of its own —
/// silence there is indistinguishable from a listing that forgot to look.
fn report(lent: &[String], server: &str) {
    // Matched on the prefix the engine builds these names with rather than by
    // asking which server each came from, which is not on offer: a tool knows
    // the name it is called, and that name is `mcp__<server>__<tool>`. Two
    // configured names that sanitize to one prefix would group together — the
    // engine already refuses the colliding tools and says so in the log, so
    // what reaches here cannot be ambiguous about anything but which of two
    // pathological names lent it.
    let prefix = ganja_core::mcp::tool_name(server, "");
    let mut lending = lent.iter().filter(|id| id.starts_with(&prefix)).peekable();

    if lending.peek().is_none() {
        println!("{INDENT}(no tools)");

        return;
    }
    for id in lending {
        println!("{INDENT}{id}");
    }
}

/// The TOOLS column's value for a server: how many tools it lends, or a dash
/// for one that cannot be lending any — disabled, failed, or never reached
/// (**tool-counts**, alongside `word`'s STATUS column and `report`'s own
/// per-tool detail underneath a connected row).
fn tools_column(count: Option<usize>) -> String {
    count.map_or_else(|| "-".to_owned(), |count| count.to_string())
}

/// What a server's standing is called in the listing.
fn word(status: Option<&McpStatus>) -> &'static str {
    match status {
        Some(McpStatus::Connected) => "connected",
        Some(McpStatus::Disabled) => "disabled",
        Some(McpStatus::Failed { .. }) => "failed",
        // Unreachable after an awaited connect, and rendered rather than
        // skipped so that the listing never drops a configured server.
        None => "unknown",
    }
}

/// Where a server was reached, as the config spells it.
fn address(server: &ganja_core::config::McpServer) -> String {
    match server {
        ganja_core::config::McpServer::Local(local) => printable(&local.command.join(" ")),
        ganja_core::config::McpServer::Remote(remote) => printable(&remote.url),
    }
}

/// Lists the stored sessions of the project this was run in, newest first.
///
/// The store is read directly rather than through an [`ganja_core::Engine`],
/// because building one selects a provider and a provider wants a credential:
/// asking what was worked on yesterday is not a reason to need an API key.
///
/// Roots only, as the picker in `ganja-tui` lists them: a session carrying a
/// parent belongs to the `task` call that spawned it, and resuming into one
/// would open a delegated turn with nothing on the screen saying what asked
/// for it. Filtering before the count is what makes a project whose every
/// session is a child read as one that has none, rather than as a table with
/// no rows.
fn sessions_command() -> Result<()> {
    let sessions: Vec<SessionInfo> = session_storage()?
        .list_sessions()
        .context("failed to read the stored sessions")?
        .into_iter()
        .filter(|session| session.parent.is_none())
        .collect();

    if sessions.is_empty() {
        println!("no sessions here yet; run `ganja` in this project and send a prompt");

        return Ok(());
    }

    println!(
        "{:<21}  {:>9}  {:>7}  TITLE",
        "SESSION", "UPDATED", "TOKENS"
    );

    let now = millis_now();
    for session in sessions {
        println!(
            "{:<21}  {:>9}  {:>7}  {}",
            session.id.as_str(),
            age(session.updated, now),
            catalog::compact_tokens(billed_tokens(&session.usage)),
            title(&session),
        );
    }

    Ok(())
}

/// The session store of the project this was run in.
fn session_storage() -> Result<Storage> {
    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let root = Project::resolve(&cwd)
        .data_dir()
        .context("failed to locate this project's data directory")?
        .join(STORAGE);

    Ok(Storage::open(root))
}

/// What a session is called in the listing, with anything that would move the
/// cursor taken out.
///
/// A title is written by a model, which makes it untrusted text: a newline
/// would break the row into pieces and an escape sequence would be *executed*
/// by the terminal printing it. Neither belongs in a column.
fn title(session: &SessionInfo) -> String {
    let Some(title) = session.title.as_deref() else {
        return UNTITLED.to_owned();
    };

    let trimmed = printable(title);
    let trimmed = trimmed.trim();

    if trimmed.is_empty() {
        UNTITLED.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// `text` with everything that would move the cursor replaced by a space.
///
/// Every caller is printing something somebody else wrote to a terminal that
/// would *execute* an escape sequence in it: a title a model chose, a command
/// out of a config file, or the words a remote MCP server failed with. A
/// newline would break one row of a table into two and an escape would repaint
/// the screen, so neither reaches `println!`.
fn printable(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Every token a session was billed for.
///
/// [`Usage::reasoning_tokens`] is deliberately left out: it counts a subset of
/// [`Usage::output_tokens`] rather than a count beside it, so adding it would
/// report the same thinking twice.
fn billed_tokens(usage: &Usage) -> u64 {
    usage.input_tokens + usage.output_tokens + usage.cache_read_tokens + usage.cache_write_tokens
}

/// Now, in milliseconds since the Unix epoch, which is what a stored session
/// records.
fn millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Renders how long ago `then` was, relative to `now`, both in milliseconds
/// since the Unix epoch.
///
/// An age rather than a timestamp, because the question a listing answers is
/// "which of these was I just in" — and because there is no date library in
/// the workspace manifest, and putting one there is not this crate's call.
///
/// A `then` in the future is a clock that moved rather than a session from the
/// future, so it reads as the present instead of as a negative age.
fn age(then: u64, now: u64) -> String {
    const MINUTE: u64 = 60 * 1_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let elapsed = now.saturating_sub(then);
    if elapsed < MINUTE {
        return "just now".to_owned();
    }
    if elapsed < HOUR {
        return format!("{}m ago", elapsed / MINUTE);
    }
    if elapsed < DAY {
        return format!("{}h ago", elapsed / HOUR);
    }

    format!("{}d ago", elapsed / DAY)
}

async fn auth_command(action: Auth) -> Result<()> {
    match action {
        Auth::Login {
            provider,
            key,
            method,
            deployment,
            enterprise_url,
        } => {
            login(
                provider,
                key,
                method,
                login::DeploymentAnswer {
                    kind: deployment,
                    enterprise_url,
                },
            )
            .await
        }
        Auth::List => list(),
        Auth::Logout { provider } => logout(&provider),
    }
}

/// Stores a credential for `provider`, by whichever route this invocation asked
/// for.
///
/// Spec: upstream `packages/opencode/src/cli/cmd/providers.ts:39-205`. What is
/// chosen and how it is run lives in [`login`](mod@login); what is written down
/// lives here, because storing is the step that must not happen when anything
/// above it failed.
async fn login(
    provider: NamedProvider,
    key: Option<String>,
    method: Option<login::Method>,
    deployment: login::DeploymentAnswer,
) -> Result<()> {
    configured_provider_exists(&provider)?;

    // A configured endpoint is key-authenticated and nothing else. Every OAuth
    // flow this build has is a set of endpoints written per provider — an
    // issuer, a client id, a token endpoint — and a config entry supplies none
    // of them, so naming one here is refused rather than attempted.
    let Some(builtin) = provider.builtin() else {
        if let Some(method) = method
            && method != login::Method::Api
        {
            bail!(
                "`{provider}` is a provider this config declares, and those are \
                 authenticated by a key; `--method {method}` needs a login flow that \
                 only a provider this build ships can have"
            );
        }

        return store_key(&provider, key);
    };

    match login::chosen(builtin, key.is_some(), method)? {
        login::Method::Api => store_key(&provider, key),
        oauth => {
            let credential = login::oauth(builtin, oauth, deployment).await?;
            let tail = credential.tail();

            warn_before_replacing(&provider)?;
            auth::set_oauth(provider.as_str(), &credential)
                .with_context(|| format!("failed to store the {provider} login"))?;

            // "Login successful" is upstream's word for this
            // (`providers.ts:128`); where it landed and what may be shown of it
            // are ganja's, and match what the key path has always printed.
            println!(
                "login successful; stored the {provider} credential {tail} in {}",
                auth::store_path()?.display()
            );
            warn_if_shadowed(&provider)
        }
    }
}

/// Stores a key taken from wherever this invocation put it.
fn store_key(provider: &NamedProvider, key: Option<String>) -> Result<()> {
    let key = match key {
        // A key given on the command line was already in the shell's history
        // and its process table entry before this ran; wrapping it is all that
        // is left to do about it.
        Some(key) => secret(key),
        None => read_key(provider)?,
    };
    let Some(key) = key else {
        bail!("no key was given; nothing was stored");
    };
    let tail = auth::RedactedTail::of_secret(&key);

    warn_before_replacing(provider)?;
    // Under the provider's own id, which for a configured endpoint is the id
    // its entry was written under: `auth::storage_key` passes an id it has no
    // alias for through unchanged, so this is exactly where `select` reads.
    auth::set_credential(provider.as_str(), key)
        .with_context(|| format!("failed to store the {provider} key"))?;

    println!(
        "stored the {provider} key {tail} in {}",
        auth::store_path()?.display()
    );
    warn_if_shadowed(provider)
}

/// Says what a login is about to overwrite, while it still exists.
///
/// A ChatGPT login and an OpenAI API key are stored under the same key, so each
/// replaces the other — upstream's behaviour at that key too, and `ganja-core`
/// pins it in both directions. Core cannot warn about it: it is handed a
/// credential and a provider, and has no way to know a person is watching. This
/// is the only place that does.
///
/// Nothing is refused. A replacement is what `login` is for, and the point is
/// that it not be silent.
fn warn_before_replacing(provider: &NamedProvider) -> Result<()> {
    let Some((kind, tail)) = login::stored(provider.as_str())? else {
        return Ok(());
    };

    eprintln!("note: this replaces the {kind} credential {tail} already stored for {provider}");

    Ok(())
}

/// Wraps a key and wipes the buffer it was assembled in.
///
/// Trimming happens here because a key pasted out of a file or a password
/// manager arrives with whitespace that would corrupt the request header, and
/// a key that is nothing but whitespace is not a key: [`None`] says so without
/// making the caller unwrap a secret to find out.
fn secret(mut key: String) -> Option<SecretString> {
    let trimmed = key.trim();
    let wrapped = (!trimmed.is_empty()).then(|| SecretString::from(trimmed));
    // The `String` is what the key was typed, piped or passed into, and nothing
    // else will clear it; the copy that matters from here on is the wrapped one.
    key.zeroize();

    wrapped
}

fn list() -> Result<()> {
    let entries = auth::list_providers().context("failed to read stored credentials")?;

    if entries.is_empty() {
        println!(
            "no credentials; run `ganja auth login` or set one of {}",
            auth::KEY_VARS
                .iter()
                .map(|(_, variable)| *variable)
                .collect::<Vec<_>>()
                .join(", ")
        );

        return Ok(());
    }

    // TYPE is not decoration: a login and a pasted key are stored under the
    // same provider key for at least one provider, so without this column the
    // listing shows the same row for two credentials that behave nothing alike
    // — one expires and renews itself, the other never changes. The column is
    // what makes "which of them is in there" a question the listing answers.
    println!("{:<16}  {:<5}  {:<9}  SOURCE", "PROVIDER", "TYPE", "KEY");
    for entry in entries {
        // A provider with a credential in both places gets a row each, and the
        // outranked one has to say so on its own line: two rows and no marker
        // would read as two credentials in use, which is the opposite of what a
        // person is being told. The variable comes from the entry rather than
        // from a lookup here, so the listing cannot disagree with the
        // precedence it is describing.
        let shadowed = entry
            .shadowed_by
            .map(|variable| format!(" (shadowed by {variable})"))
            .unwrap_or_default();
        println!(
            "{:<16}  {:<5}  {:<9}  {}{shadowed}",
            entry.provider_id, entry.kind, entry.tail, entry.source
        );
    }

    Ok(())
}

fn logout(provider: &NamedProvider) -> Result<()> {
    configured_provider_exists(provider)?;

    // Not every stored credential is a key, and `remove_credential` takes the
    // provider rather than the storage key — so forgetting `grok` really does
    // remove the entry filed under `xai`.
    let forgotten = auth::remove_credential(provider.as_str())
        .with_context(|| format!("failed to forget the {provider} credential"))?;

    if forgotten {
        println!("forgot the stored {provider} credential");
    } else {
        println!("there was no stored {provider} credential to forget");
    }

    warn_if_shadowed(provider)
}

/// Says so when an environment variable outranks whatever is stored, because
/// otherwise a login that appears to have worked changes nothing.
///
/// A no-op for a provider with no key variable, which is every OAuth one:
/// [`auth::list_providers`] only reports an environment entry for a provider in
/// [`auth::KEY_VARS`], so there is nothing there to shadow with. The comparison
/// is on the *storage* key because that is the name the listing reports, and a
/// provider ganja and the file disagree about — `grok`, filed as `xai` — would
/// otherwise silently never match.
///
/// The search is for the environment entry by name rather than for the
/// provider's first row: a provider now has a row per place it has a
/// credential, and taking whichever came first would make this depend on how
/// the listing happens to be ordered.
fn warn_if_shadowed(provider: &NamedProvider) -> Result<()> {
    let stored_as = auth::storage_key(provider.as_str());
    let shadowing = auth::list_providers()
        .context("failed to read stored credentials")?
        .into_iter()
        .filter(|entry| entry.provider_id == stored_as)
        .find_map(|entry| match entry.source {
            auth::Source::Environment(variable) => Some(variable),
            auth::Source::File => None,
        });

    if let Some(variable) = shadowing {
        eprintln!("note: {variable} is set, and it is used in preference to the stored key");
    }

    Ok(())
}

/// Reads a key from wherever the invocation put it.
///
/// Piped input is read whole so that `pass show … | ganja auth login` works;
/// otherwise the key is typed at a prompt.
fn read_key(provider: &NamedProvider) -> Result<Option<SecretString>> {
    let mut stdin = io::stdin();
    if stdin.is_terminal() {
        return prompt_for_key(provider);
    }

    let mut piped = String::new();
    let read = stdin.read_to_string(&mut piped);
    let key = secret(piped);
    read.context("failed to read the key from standard input")?;

    Ok(key)
}

/// Asks for a key at a terminal that does not echo it.
///
/// Echo is off because an echoed key survives the exchange: it sits in the
/// scrollback of a terminal that may be shared, recorded, or logged, and every
/// copy of it is as good as the original. Raw mode is the only way crossterm
/// offers to suppress it, which also means this loop has to handle Enter,
/// Backspace and Ctrl-C itself — in raw mode the terminal driver does none of
/// that, and Ctrl-C raises no signal.
fn prompt_for_key(provider: &NamedProvider) -> Result<Option<SecretString>> {
    use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    /// Leaves raw mode however the read ends, panic included: a terminal left
    /// in raw mode is unusable and the shell that owns it will not fix it.
    struct RawMode;

    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }

    // The prompt goes to stderr so that stdout stays a clean channel for
    // whatever a caller is capturing.
    eprint!("{provider} API key (not shown as you type): ");
    io::stderr().flush().ok();

    enable_raw_mode().context("failed to turn off terminal echo")?;
    let raw = RawMode;
    let key = read_unechoed();
    drop(raw);
    eprintln!();

    key
}

fn read_unechoed() -> Result<Option<SecretString>> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

    /// Wipes what was typed however the read ends, cancellation and panic
    /// included — a key abandoned half-way through is still a key.
    ///
    /// Best effort, and worth being precise about: `zeroize` clears the whole
    /// capacity of the buffer, so a backspaced character goes too, but a
    /// `String` that grew as it was typed reallocated, and the prefixes it left
    /// behind are not reachable to wipe.
    struct Typed(String);

    impl Drop for Typed {
        fn drop(&mut self) {
            self.0.zeroize();
        }
    }

    let mut typed = Typed(String::new());
    loop {
        let Event::Key(pressed) = event::read().context("failed to read the key")? else {
            continue;
        };
        if pressed.kind == KeyEventKind::Release {
            continue;
        }

        match pressed.code {
            KeyCode::Enter => return Ok(secret(std::mem::take(&mut typed.0))),
            KeyCode::Backspace => {
                typed.0.pop();
            }
            KeyCode::Char('c' | 'd') if pressed.modifiers.contains(KeyModifiers::CONTROL) => {
                bail!("cancelled; nothing was stored")
            }
            KeyCode::Char(character) => typed.0.push(character),
            // Arrows, function keys and the rest have no meaning in a secret.
            _ => {}
        }
    }
}

/// Lists what this build can size and price, filtered to `provider` when one
/// was named — or, for a provider whose wire carries its own roster, what the
/// stored credential is offered.
///
/// The cached catalog is adopted here rather than left to the first lookup:
/// the disk tier is a layer somebody installs, and a listing that skipped
/// installing it would answer from the compiled-in snapshot however recently
/// the UI had fetched a newer one.
///
/// # Errors
///
/// When `provider` names nobody in the table, or names a wire-listed provider
/// whose listing could not be fetched — no stored login, an unreachable
/// endpoint — in the wire's own words. The catalog half never fails: it
/// always answers, at worst from the snapshot compiled into the binary.
async fn models_command(provider: Option<String>, refresh: bool) -> Result<()> {
    // The wire-listed tier answers before the catalog machinery is touched: a
    // `Some` from the seam means the wire — not the table — knows what the
    // stored credential may name. `--refresh` is not consulted on this path
    // either way: cursor's roster is live on every call with no cache to force
    // past, and a ChatGPT seat's is pinned in the binary where no fetch can
    // reach it (**D476**). The notice each answer carries says which.
    if let Some(wanted) = provider.as_deref()
        && let Some(listing) = ganja_core::provider::wire_model_listing(wanted).await
    {
        let listed = listing?;
        // The notice is the seam's, not this command's: cursor's roster is
        // fetched live and unpriced while a ChatGPT seat's is pinned in the
        // binary and may well be cataloged, and one sentence about both would
        // be false about one of them (**D476**).
        println!("{wanted} models, {}", listed.notice);
        println!("\n{:<32}  NAME", "MODEL");
        for model in listed.models {
            println!("{:<32}  {}", model.id, model.name);
        }

        return Ok(());
    }

    catalog::load_cached();
    if refresh {
        refreshed().await;
    }

    let table: Vec<Arc<catalog::ModelInfo>> = catalog::models().collect();
    let listed = matching(&table, provider.as_deref());
    if let Some(wanted) = provider.as_deref()
        && listed.is_empty()
    {
        // Two different situations, and telling them apart is the whole of
        // what this command owes somebody who named a provider.
        //
        // A provider a session **can** run as, with no rows here, is the
        // uncataloged tier: real, usable, and unpriced. Refusing it would call
        // a working configuration a typo, and printing a bare header would
        // claim it serves nothing — so the header is printed and the
        // consequence is spelled out, which is what a person actually needs to
        // know before they run a turn on it.
        //
        // A name nothing can select is the typo it looks like, and keeps the
        // refusal it always had.
        if selectable_here(wanted)? {
            print_header();
            println!(
                "\n`{wanted}` has no catalog rows; sizing and cost display are off for it — \
                 a session runs, and names its own model with --model, {}, or the config's \
                 `model` key.",
                ganja_core::provider::MODEL_ENV
            );

            return Ok(());
        }

        bail!(
            "no models here are served by `{wanted}`; this table carries {}",
            providers(&table).join(", ")
        );
    }

    print_header();

    let mut defaulted = false;
    for model in listed {
        let default = catalog::default_model(&model.provider_id) == Some(model.id.as_str());
        defaulted |= default;

        println!(
            "{:<10}  {:<17}  {:>8}  {:>8}  {:>10}  {:>10}",
            model.provider_id,
            format!("{}{}", model.id, if default { "*" } else { "" }),
            catalog::compact_tokens(model.context_window),
            catalog::compact_tokens(model.max_output),
            per_mtok(model.pricing.input),
            per_mtok(model.pricing.output),
        );
    }

    if defaulted {
        println!("\n* the model its provider is asked for when none is named");
    }

    Ok(())
}

/// The listing's column header.
///
/// One function rather than a literal in two places: it is printed above the
/// rows, and *instead* of them for a provider the table has none for, and a
/// header that drifted from the row format would misalign the whole listing.
fn print_header() {
    println!(
        "{:<10}  {:<17}  {:>8}  {:>8}  {:>10}  {:>10}",
        "PROVIDER", "MODEL", "CONTEXT", "MAX OUT", "$/MTOK IN", "$/MTOK OUT"
    );
}

/// Whether a session in this project could run as `provider_id` — the
/// selectable tier, which is the builtins plus whatever the config declares.
///
/// Asked only when the catalog has nothing to list, because that is the one
/// moment "is this a typo or an unpriced endpoint" has two answers.
///
/// # Errors
///
/// When the working directory cannot be read, or the config cannot be.
fn selectable_here(provider_id: &str) -> Result<bool> {
    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let config = ganja_core::config::Config::load(&cwd).context("failed to read the config")?;

    Ok(ganja_core::provider::selectable(&config, provider_id))
}

/// Fetches the catalog before a listing reads it, saying why when it could
/// not.
///
/// Never fatal, whichever way it fails: every tier beneath the fetch still
/// answers, so what follows is at worst as current as the cache — and a
/// `--refresh` that exited non-zero over an unreachable endpoint would make a
/// table that is merely stale look like no table at all.
async fn refreshed() {
    match catalog::refresh(true).await {
        Ok(true) => {}
        // Forced, so the cache's five-minute debounce cannot be the reason
        // nothing was fetched; the switch is the only one left.
        Ok(false) => eprintln!(
            "note: {} is set, so the catalog was not fetched",
            catalog::DISABLE_FETCH_ENV
        ),
        Err(error) => eprintln!("note: the catalog was not refreshed: {error}"),
    }
}

/// The rows a listing shows: every one, or only those `provider` serves.
///
/// The match is exact, as upstream's is. Provider ids are published in
/// lowercase and the refusal above names them, so a forgiving comparison would
/// buy a little convenience at the price of a lookup that answers questions it
/// was not asked.
fn matching(
    table: &[Arc<catalog::ModelInfo>],
    provider: Option<&str>,
) -> Vec<Arc<catalog::ModelInfo>> {
    table
        .iter()
        .filter(|model| provider.is_none_or(|wanted| model.provider_id == wanted))
        .cloned()
        .collect()
}

/// The providers a table carries, each once and in the order it lists them.
///
/// Order rather than sorted: this is read back to somebody who asked for a
/// provider that is not here, and listing them as the table would have listed
/// them is what makes the answer checkable against the table itself.
fn providers(table: &[Arc<catalog::ModelInfo>]) -> Vec<String> {
    let mut named: Vec<String> = Vec::new();
    for model in table {
        if !named.iter().any(|provider| *provider == model.provider_id) {
            named.push(model.provider_id.clone());
        }
    }

    named
}

/// Renders a price per million tokens.
///
/// Trailing zeros are trimmed rather than padded to a fixed width, so a price
/// of a fraction of a cent shows as itself instead of being rounded into a
/// different number.
fn per_mtok(price: f64) -> String {
    let rendered = format!("{price:.3}");
    let trimmed = rendered.trim_end_matches('0').trim_end_matches('.');

    format!("${trimmed}")
}

/// What the sessions listing renders, exercised where it is written.
///
/// These live inside the binary because the helpers they cover are private to
/// it. That also puts the empty-store message out of reach: driving the built
/// binary needs `CARGO_BIN_EXE_ganja`, which cargo defines for integration
/// tests only, so that one assertion belongs in `tests/` instead.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clap::Parser;
    use ganja_core::{
        SessionId, SessionInfo,
        catalog::{ModelInfo, ModelStatus, Pricing},
        storage::VERSION,
    };
    use ganja_protocol::Usage;

    use super::{
        Cli, Command, UNTITLED, age, billed_tokens, matching, per_mtok, providers, resolve_filter,
        title,
    };

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
}
