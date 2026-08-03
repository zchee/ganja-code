//! `ganja` — a terminal-first AI coding agent.
//!
//! Running the binary with no subcommand starts the terminal UI, which is what
//! the tool is for; the subcommands exist to set it up and to answer questions
//! about it without taking the screen over.

use std::io::{self, IsTerminal as _, Read as _, Write as _};

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use ganja_core::{auth, catalog};
use secrecy::{SecretString, zeroize::Zeroize as _};

/// Terminal-first AI coding agent.
#[derive(Debug, Parser)]
#[command(name = "ganja", version, about)]
struct Cli {
    /// Absent means the interactive UI, which is the point of the binary.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the API keys providers are authenticated with.
    Auth {
        #[command(subcommand)]
        action: Auth,
    },
    /// List the models this build knows how to size and price.
    Models,
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

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        None => ganja_tui::run().await,
        Some(Command::Auth { action }) => auth_command(action),
        Some(Command::Models) => {
            models_command();
            Ok(())
        }
    }
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

fn models_command() {
    println!(
        "{:<10}  {:<17}  {:>8}  {:>8}  {:>10}  {:>10}",
        "PROVIDER", "MODEL", "CONTEXT", "MAX OUT", "$/MTOK IN", "$/MTOK OUT"
    );

    let mut defaulted = false;
    for model in catalog::models() {
        let default = catalog::default_model(model.provider_id) == Some(model.id);
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

#[cfg(test)]
mod tests {
    use super::per_mtok;

    #[test]
    fn a_price_keeps_every_digit_that_means_something() {
        assert_eq!(per_mtok(10.0), "$10");
        assert_eq!(per_mtok(4.5), "$4.5");
        assert_eq!(per_mtok(0.075), "$0.075");
        assert_eq!(per_mtok(0.0), "$0");
    }
}
