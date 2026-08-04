//! `ganja` — a terminal-first AI coding agent.
//!
//! Running the binary with no subcommand starts the terminal UI, which is what
//! the tool is for; the subcommands exist to set it up and to answer questions
//! about it without taking the screen over.

use std::{
    io::{self, IsTerminal as _, Read as _, Write as _},
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use ganja_core::{SessionInfo, Storage, auth, catalog};
use ganja_permission::Project;
use ganja_protocol::Usage;
use secrecy::{SecretString, zeroize::Zeroize as _};
use tracing_appender::non_blocking::WorkerGuard;

mod import;

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
    /// List the models this build knows how to size and price.
    Models {
        /// List only the models this provider serves.
        #[arg(value_name = "PROVIDER")]
        provider: Option<String>,
        /// Fetch the published catalog first, however recently it was fetched.
        #[arg(long)]
        refresh: bool,
    },
    /// List the stored sessions of the project this was run in.
    Sessions,
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
    /// Store a provider's API key.
    ///
    /// The key is taken from `--key`, else from standard input when it is
    /// piped in, else from a prompt the terminal does not echo.
    Login {
        /// Provider the key belongs to.
        #[arg(long, value_enum, default_value_t = ProviderId::Anthropic)]
        provider: ProviderId,
        /// The key itself.
        ///
        /// Every process on the machine can read another's command line, so
        /// prefer piping the key in or typing it at the prompt.
        #[arg(long, value_name = "KEY")]
        key: Option<String>,
    },
    /// Show which providers have a credential, and where it comes from.
    List,
    /// Forget a provider's stored API key.
    Logout {
        /// Provider to forget.
        #[arg(long, value_enum)]
        provider: ProviderId,
    },
}

/// The providers this build can authenticate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ProviderId {
    Anthropic,
    #[value(name = "openai")]
    OpenAi,
}

impl ProviderId {
    /// The identifier `ganja-core` knows the provider by.
    fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
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

/// Where a project's sessions live, under its data directory.
///
/// Pinned to the directory `ganja-tui` opens `Engine::persistent` on: the two
/// crates naming it separately is what the frozen seam asks for, and a listing
/// that read a different one would show nothing and look correct doing it.
const STORAGE: &str = "storage";

/// What a session with no title is listed as. Most of them: a title is earned
/// by a completed turn, and the fake provider never earns one.
const UNTITLED: &str = "(untitled)";

#[tokio::main]
async fn main() -> Result<()> {
    // Parsed before the log is installed so that `--version`, `--help` and a
    // usage error do not create a log directory for a run that never started.
    let cli = Cli::parse();
    // Held until `main` returns: dropping the guard stops the appender's
    // worker thread, and whatever it had not written is lost.
    let _logging = install_logging();

    match cli.command {
        None => ganja_tui::run(cli.resume.wanted(), cli.select.overrides()).await,
        Some(Command::Auth { action }) => auth_command(action),
        Some(Command::Config { action }) => config_command(action),
        Some(Command::Models { provider, refresh }) => models_command(provider, refresh).await,
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
/// The returned guard flushes the appender's worker thread when it drops.
#[must_use]
fn install_logging() -> Option<WorkerGuard> {
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
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_FILTER));
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

    let printable: String = title
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let trimmed = printable.trim();

    if trimmed.is_empty() {
        UNTITLED.to_owned()
    } else {
        trimmed.to_owned()
    }
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

fn auth_command(action: Auth) -> Result<()> {
    match action {
        Auth::Login { provider, key } => login(provider, key),
        Auth::List => list(),
        Auth::Logout { provider } => logout(provider),
    }
}

fn login(provider: ProviderId, key: Option<String>) -> Result<()> {
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

    auth::set_credential(provider.as_str(), key)
        .with_context(|| format!("failed to store the {provider} key"))?;

    println!(
        "stored the {provider} key {tail} in {}",
        auth::store_path()?.display()
    );
    warn_if_shadowed(provider)
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

    println!("{:<10}  {:<9}  SOURCE", "PROVIDER", "KEY");
    for entry in entries {
        println!(
            "{:<10}  {:<9}  {}",
            entry.provider_id, entry.tail, entry.source
        );
    }

    Ok(())
}

fn logout(provider: ProviderId) -> Result<()> {
    let forgotten = auth::remove_credential(provider.as_str())
        .with_context(|| format!("failed to forget the {provider} key"))?;

    if forgotten {
        println!("forgot the stored {provider} key");
    } else {
        println!("there was no stored {provider} key to forget");
    }

    warn_if_shadowed(provider)
}

/// Says so when an environment variable outranks whatever is stored, because
/// otherwise a login that appears to have worked changes nothing.
fn warn_if_shadowed(provider: ProviderId) -> Result<()> {
    let shadowing = auth::list_providers()
        .context("failed to read stored credentials")?
        .into_iter()
        .find(|entry| entry.provider_id == provider.as_str())
        .and_then(|entry| match entry.source {
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
fn read_key(provider: ProviderId) -> Result<Option<SecretString>> {
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
fn prompt_for_key(provider: ProviderId) -> Result<Option<SecretString>> {
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
/// was named.
///
/// The cached catalog is adopted here rather than left to the first lookup:
/// the disk tier is a layer somebody installs, and a listing that skipped
/// installing it would answer from the compiled-in snapshot however recently
/// the UI had fetched a newer one.
///
/// # Errors
///
/// When `provider` names nobody in the table. Nothing else here fails: the
/// catalog always answers, at worst from the snapshot compiled into the
/// binary.
async fn models_command(provider: Option<String>, refresh: bool) -> Result<()> {
    catalog::load_cached();
    if refresh {
        refreshed().await;
    }

    let table: Vec<Arc<catalog::ModelInfo>> = catalog::models().collect();
    let listed = matching(&table, provider.as_deref());
    if let Some(wanted) = provider.as_deref()
        && listed.is_empty()
    {
        // A header over no rows would read as "this provider serves nothing",
        // which is indistinguishable from the typo it usually is. Naming what
        // the table does carry makes the fix one keystroke.
        bail!(
            "no models here are served by `{wanted}`; this table carries {}",
            providers(&table).join(", ")
        );
    }

    println!(
        "{:<10}  {:<17}  {:>8}  {:>8}  {:>10}  {:>10}",
        "PROVIDER", "MODEL", "CONTEXT", "MAX OUT", "$/MTOK IN", "$/MTOK OUT"
    );

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

    use super::{Cli, Command, UNTITLED, age, billed_tokens, matching, per_mtok, providers, title};

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

    #[test]
    fn a_subcommand_given_the_ui_flags_is_refused_not_ignored() {
        // `args_conflicts_with_subcommands` covers the new flags the same way
        // it already covered the resume pair: the shape fails to parse.
        assert!(
            Cli::try_parse_from(["ganja", "--model", "x/y", "models"]).is_err(),
            "a listing that read like it honored --model would be lying"
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
