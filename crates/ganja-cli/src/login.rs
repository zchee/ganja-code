//! Which login a provider gets, and running the ones that are not a key.
//!
//! Spec: upstream `packages/opencode/src/cli/cmd/providers.ts:39-205`, whose
//! `handlePluginAuth` is what this ports — a method chosen (skipped when the
//! provider has one), the method's own prompts asked in order, the URL and the
//! code shown, then a wait that ends in `Login successful` or `Failed to
//! authorize`.
//!
//! **Nothing here stores anything.** Every path returns a credential or an
//! error, and the caller in `main.rs` is what writes it down. That is what
//! makes "a login that was cancelled left nothing behind" a fact about which
//! functions exist rather than a claim about which branches were taken — the
//! same property [`ganja_provider::auth::grok`] and its two siblings are built
//! on.
//!
//! **A login that succeeds is a credential stored, and nothing more.** Whether
//! a model then runs on it is a separate question with a separate answer per
//! provider, so nothing printed here may suggest otherwise.

use std::io::{self, BufRead as _, IsTerminal as _, Write as _};

use anyhow::{Context as _, Result, bail};
use clap::ValueEnum;
use ganja_provider::auth::copilot::{self, Deployment};
use ganja_provider::auth::device::{DeviceFlow, Tokens};
use ganja_provider::auth::{self, OauthCredential, cursor, grok, openai};
use tokio_util::sync::CancellationToken;

use crate::ProviderId;

/// Where the login endpoints are reached, when something has redirected them.
///
/// A test needs a login it can complete, which means endpoints it controls,
/// and this module is where the flow objects are built — so there is nowhere
/// else in the binary to put that seam. `ganja_provider::auth` already offers the
/// injectable constructors this reaches for; what it cannot offer is a way for
/// a *subprocess* to be told.
///
/// **Only a loopback origin is honoured.** The value decides where a device
/// code and then a pair of tokens are sent, so an origin that could name
/// somewhere else would make one exported variable enough to collect somebody
/// else's login. Loopback cannot leave the machine, which makes the variable
/// useless for anything but the thing it exists for.
const ISSUER_ENV: &str = "GANJA_AUTH_ISSUER";

/// How a provider is logged in to.
///
/// Upstream selects by a method's *label* (`providers.ts:56-58`, matched
/// case-insensitively against `plugin.auth.methods`); these are the same three
/// kinds under names short enough to type, because ganja has no plugin
/// registry to read labels out of (deviation: `login-methods-are-a-value-enum`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Method {
    /// A key typed, piped or passed in — what `ganja auth login` has always
    /// done.
    Api,
    /// A browser on this machine, answering to a loopback redirect.
    Browser,
    /// A code typed into a browser that may be on another machine entirely.
    Device,
}

impl std::fmt::Display for Method {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Api => "api",
            Self::Browser => "browser",
            Self::Device => "device",
        })
    }
}

/// Which GitHub a Copilot login is against, named rather than asked for.
///
/// Upstream's `deploymentType` prompt has two answers (`copilot.ts:186-207`)
/// and only one of them used to have a flag: `--enterprise-url` names the
/// enterprise branch *and* its address, while the public branch — the common
/// one — had no way to be answered by anything but a person at a terminal. So
/// the login every headless machine actually wants was the one that could not
/// run unattended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum DeploymentKind {
    /// github.com.
    Public,
    /// A GitHub Enterprise deployment, whose address comes from
    /// `--enterprise-url` or from the question that follows.
    Enterprise,
}

/// What this invocation said about the deployment, before anything is asked.
///
/// The two flags together rather than separately, because they answer one
/// question between them and the combinations that contradict each other have
/// to be refused somewhere that can see both.
#[derive(Clone, Debug, Default)]
pub(crate) struct DeploymentAnswer {
    /// `--deployment`, when one was named.
    pub(crate) kind: Option<DeploymentKind>,
    /// `--enterprise-url`, which names the enterprise branch and its address at
    /// once.
    pub(crate) enterprise_url: Option<String>,
}

impl ProviderId {
    /// The logins this provider has, in the order a menu offers them.
    ///
    /// Upstream's order for ChatGPT (`openai.ts:44`, `:101`: browser first,
    /// then headless), with the API key last because it is the one that needs
    /// somebody to have fetched a key first.
    fn methods(self) -> &'static [Method] {
        match self {
            // No OAuth flow of its own in the pin, so there is nothing to
            // choose between.
            Self::Anthropic => &[Method::Api],
            // A key from the vendor's console, and only that. OpenRouter does
            // publish a PKCE flow that provisions a key for a signed-in
            // account, and it is a recorded follow-up rather than something
            // guessed at here (`provider::openrouter`).
            Self::OpenRouter => &[Method::Api],
            // A key pasted from the vendor's console, for both. There *is* an
            // OAuth device flow in that vendor's client, and it authenticates
            // the **console** rather than the model path — the gateway takes a
            // plain key — so cloning it here would be a login that buys a
            // session nothing.
            Self::Opencode | Self::OpencodeGo => &[Method::Api],
            Self::OpenAi => &[Method::Browser, Method::Device, Method::Api],
            // Upstream's own order for xAI too (`xai.ts:551`, `:594`, `:619`):
            // the loopback method first, because somebody sitting in front of a
            // browser is who a terminal login usually belongs to, then the
            // device grant that needs no browser here at all.
            Self::Grok => &[Method::Browser, Method::Device, Method::Api],
            // One method upstream too (`copilot.ts:182-185`). The API-key entry
            // is ganja's: `auth.json` is a key-value file that a shared
            // opencode install also reads, and refusing to write a line into it
            // would be a refusal invented here rather than ported.
            Self::GithubCopilot => &[Method::Device, Method::Api],
            // One login, an OAuth flow with no loopback and no code to type:
            // the browser is sent to cursor.com carrying a pairing id, and
            // the terminal long-polls until the person finishes there. The
            // login lands ahead of its wire deliberately — a stored
            // credential is real value the day the wire arrives, the
            // precedent three P7 logins set. No API-key entry on purpose:
            // cursor's backend runs on subscription tokens, and a stored key
            // would be a credential nothing ever sends.
            Self::Cursor => &[Method::Browser],
        }
    }

    /// What this provider does when nothing named a method and there is
    /// somebody at the terminal to ask.
    ///
    /// [`None`] means ask. The two providers with an OAuth flow of each kind do:
    /// a browser login and a device login are genuinely different situations —
    /// whether there is a browser on *this* machine — and nothing here can tell
    /// which one somebody is in. The other two have one login each worth
    /// offering, and a menu with one item is a keystroke charged for nothing
    /// (upstream skips the prompt at `providers.ts:47` for the same reason).
    fn only_login(self) -> Option<Method> {
        match self {
            Self::Anthropic | Self::OpenRouter | Self::Opencode | Self::OpencodeGo => {
                Some(Method::Api)
            }
            Self::GithubCopilot => Some(Method::Device),
            Self::Grok | Self::OpenAi => None,
            // One login worth offering, like Copilot's: a menu with one item
            // is a keystroke charged for nothing.
            Self::Cursor => Some(Method::Browser),
        }
    }
}

/// Which login this invocation runs.
///
/// The order is the whole of it, and each step is somebody's existing
/// invocation:
///
/// 1. `--key` is the API-key path spelled out.
/// 2. `--method` is the answer to the question below, given in advance.
/// 3. **A provider whose one login is not a key runs it, terminal or not.**
///    That flow *is* the provider's headless login — Copilot's device grant is
///    a code typed into a browser somewhere else entirely — so a pipe deciding
///    it was a key instead put the only unattended login out of reach of the
///    only unattended invocation.
/// 4. **Standard input that is not a terminal is a key.** `pass show … | ganja
///    auth login` predates every OAuth login here, and an invocation being fed
///    rather than typed at has nobody to answer a menu.
/// 5. A provider with one login runs it.
/// 6. Otherwise, ask.
///
/// Steps 3 and 5 are the same question asked either side of the pipe rule, and
/// the split is what separates them: a provider whose sole login *is* a key —
/// anthropic — answers step 5, so the piped-key case it has always had is
/// untouched. Nothing above 3 changed either, which is why `--key` and
/// `--method api` still store a piped key for every provider.
///
/// # Errors
///
/// When `method` names a login the provider does not have, or when a menu was
/// needed and standard input ended before an answer arrived.
pub(crate) fn chosen(
    provider: ProviderId,
    has_key: bool,
    method: Option<Method>,
) -> Result<Method> {
    if has_key {
        return accepted(provider, Method::Api);
    }
    if let Some(method) = method {
        return accepted(provider, method);
    }

    let only = provider.only_login();
    if let Some(only) = only.filter(|only| *only != Method::Api) {
        return Ok(only);
    }
    if !io::stdin().is_terminal() {
        return Ok(Method::Api);
    }
    if let Some(only) = only {
        return Ok(only);
    }

    let offered = provider.methods();
    let labels: Vec<String> = offered.iter().map(|method| label(provider, *method)).collect();
    let chosen = choose("Login method", &labels)?;

    Ok(offered[chosen])
}

/// `method`, when `provider` has it.
///
/// The refusals are real rather than a policy: there is no Anthropic OAuth
/// flow in the pin and no browser flow for Copilot, whose only OAuth method
/// upstream is the device grant — so an invocation naming one is asking for
/// something that does not exist.
fn accepted(provider: ProviderId, method: Method) -> Result<Method> {
    if provider.methods().contains(&method) {
        return Ok(method);
    }

    bail!(
        "{provider} has no `{method}` login; it has {}",
        provider
            .methods()
            .iter()
            .map(|method| format!("`{method}`"))
            .collect::<Vec<_>>()
            .join(" and ")
    )
}

/// What a menu calls one of a provider's logins.
///
/// Upstream's own labels where it has one (`openai.ts:44`, `:101`; `xai.ts:552`,
/// `:594`), because the words on the screen are the part somebody recognises
/// from having read its documentation.
fn label(provider: ProviderId, method: Method) -> String {
    match (provider, method) {
        (ProviderId::OpenAi, Method::Browser) => "ChatGPT Pro/Plus (browser)".to_owned(),
        (ProviderId::OpenAi, Method::Device) => "ChatGPT Pro/Plus (headless)".to_owned(),
        (ProviderId::Grok, Method::Browser) => "xAI Grok OAuth (SuperGrok Subscription)".to_owned(),
        (ProviderId::Grok, Method::Device) => "xAI Grok OAuth (Headless / Remote / VPS)".to_owned(),
        (_, Method::Api) => "Manually enter API Key".to_owned(),
        (_, method) => format!("{provider} ({method})"),
    }
}

/// Runs `method` against `provider` and hands back what it produced.
///
/// # Errors
///
/// Whatever the flow failed with, always saying that nothing was stored —
/// which is structural here rather than a claim, since storing is the caller's
/// next step and it never runs.
pub(crate) async fn oauth(
    provider: ProviderId,
    method: Method,
    answer: DeploymentAnswer,
) -> Result<OauthCredential> {
    let cancel = CancellationToken::new();
    // Held for the length of the login: dropping it stops the task watching for
    // the keystroke, which has nothing left to cancel once the flow returns.
    let _interrupt = Interrupt::watching(cancel.clone());

    match (provider, method) {
        (ProviderId::Grok, Method::Browser) => {
            let tokens = grok_browser(&cancel).await?;

            Ok(grok::credential_from(&tokens))
        }
        (ProviderId::Grok, Method::Device) => {
            let tokens = device(&grok_flow()?, &cancel).await?;

            Ok(grok::credential_from(&tokens))
        }
        (ProviderId::GithubCopilot, Method::Device) => {
            // The two questions before the flow, in upstream's order
            // (`copilot.ts:186-221`): which deployment, and then its address
            // only when the answer was an enterprise one — each skipped when
            // the invocation already answered it.
            let deployment = deployment(answer)?;
            let tokens = device(&copilot_flow(&deployment)?, &cancel).await?;

            Ok(copilot::credential_from(&tokens, &deployment))
        }
        (ProviderId::OpenAi, Method::Browser) => chatgpt_browser(&cancel).await,
        (ProviderId::OpenAi, Method::Device) => chatgpt_device(&cancel).await,
        (ProviderId::Cursor, Method::Browser) => {
            // The same two-step shape every flow here has: the URL reaches
            // the screen before anything blocks on it having been opened.
            // Nothing is bound first — the poll is the return path, so there
            // is no socket for a browser to race.
            let login = cursor_flow()?.start().map_err(nothing_stored)?;
            announce(login.url(), "");

            login.poll(&cancel).await.map_err(nothing_stored)
        }
        // `chosen` is the only way to reach here and it refuses every other
        // pairing by name, so this is the shape of the match rather than a
        // case anybody can produce.
        (provider, method) => bail!("{provider} has no `{method}` login"),
    }
}

/// A device grant: what to do, then the wait for it to have been done.
///
/// **Two calls with a print between them, and that is the point.** `start`
/// returns the code and the address, `poll` blocks until somebody has typed the
/// one into the other, and a single call would leave a person watching a
/// terminal that has told them nothing.
async fn device(flow: &DeviceFlow, cancel: &CancellationToken) -> Result<Tokens> {
    let started = flow.start(cancel).await.map_err(nothing_stored)?;
    announce(started.browser_url(), &started.user_code);

    flow.poll(&started, cancel).await.map_err(nothing_stored)
}

/// xAI through a browser on this machine (`xai.ts:551-584`).
///
/// The same two-step shape a device grant has, and for the same reason: the URL
/// has to be on the screen before anything blocks on somebody having opened it.
/// The socket is bound by `start` — before the URL exists — so a browser opened
/// here can never finish the login into a port nothing is listening on.
async fn grok_browser(cancel: &CancellationToken) -> Result<Tokens> {
    let browser = grok_browser_flow()?.start().await.map_err(nothing_stored)?;
    announce(browser.url(), "");

    browser.wait(grok::CALLBACK_DEADLINE, cancel).await.map_err(nothing_stored)
}

/// ChatGPT through a browser on this machine (`openai.ts:39-94`).
async fn chatgpt_browser(cancel: &CancellationToken) -> Result<OauthCredential> {
    let browser = chatgpt()?.browser().await.map_err(nothing_stored)?;
    announce(browser.url(), "");

    browser.wait(openai::CALLBACK_DEADLINE, cancel).await.map_err(nothing_stored)
}

/// ChatGPT through a code typed on whatever device has a browser
/// (`openai.ts:95-148`).
async fn chatgpt_device(cancel: &CancellationToken) -> Result<OauthCredential> {
    let started = chatgpt()?.device().await.map_err(nothing_stored)?;
    announce(started.url(), started.user_code());

    started.wait(openai::DEVICE_DEADLINE, cancel).await.map_err(nothing_stored)
}

/// Says where to go and what to type there, before anything blocks on it
/// having been done.
///
/// Upstream's own words (`providers.ts:96` and `openai.ts:116`), on stderr for
/// the reason the API-key prompt is: these are instructions to a person, and
/// stdout is what a caller captures. An empty `code` is the browser flow, which
/// has nothing to type.
///
/// No flush: Rust's stderr is unbuffered, so these have already been written by
/// the time the caller blocks. Anything that changes that has to put one back.
fn announce(url: &str, code: &str) {
    eprintln!("Go to: {url}");
    if !code.is_empty() {
        eprintln!("Enter code: {code}");
    }
    eprintln!("Waiting for authorization...");
}

/// Upstream's first Copilot prompt (`copilot.ts:186-207`).
const DEPLOYMENT_QUESTION: &str = "Select GitHub deployment type";

/// Its second, asked only when the first answer needs one (`copilot.ts:208`).
const ENTERPRISE_QUESTION: &str = "Enter your GitHub Enterprise URL or domain";

/// Which GitHub a Copilot login is against.
///
/// Every combination the two flags can arrive in, so that each of upstream's
/// prompts is skipped exactly when the invocation already answered it:
/// `--deployment public` is the public branch outright, `--enterprise-url`
/// remains the enterprise branch *and* its address, and `--deployment
/// enterprise` alone still needs the address from somewhere.
///
/// **Both branches are now nameable, which is the point.** A flag existed for
/// the enterprise answer alone, so the public login — the common one — could
/// not complete without a terminal, and the obvious workaround was a trap: a
/// piped `1` is a non-terminal standard input, which [`chosen`] used to read as
/// an API key and store the menu answer as a credential. Step 3 there is what
/// closed that; this is what makes the login somebody wanted runnable at all.
///
/// # Errors
///
/// When the two flags name different deployments, when an enterprise
/// deployment ends up with a blank address, or when a question had to be asked
/// and standard input ended before it was answered.
fn deployment(answer: DeploymentAnswer) -> Result<Deployment> {
    match (answer.kind, answer.enterprise_url) {
        (Some(DeploymentKind::Public), None) => Ok(Deployment::Public),
        // Refused rather than resolved by precedence: whichever this build
        // picked would be the one somebody's other flag said not to.
        (Some(DeploymentKind::Public), Some(url)) => bail!(
            "`--deployment public` and `--enterprise-url {url}` name different \
             deployments; nothing was stored"
        ),
        (Some(DeploymentKind::Enterprise) | None, Some(url)) => enterprise(&url),
        (Some(DeploymentKind::Enterprise), None) => enterprise(&asked(ENTERPRISE_QUESTION)?),
        (None, None) => prompted(),
    }
}

/// Upstream's two prompts, for an invocation that named neither answer.
///
/// Line-based like every other question here, so a piped answer drives it — the
/// shape `the_enterprise_address_is_asked_for_only_when_the_deployment_is_one`
/// pins. What a pipe may no longer do is arrive *as a credential*, which is
/// [`chosen`]'s doing rather than this function's.
fn prompted() -> Result<Deployment> {
    let public =
        choose(DEPLOYMENT_QUESTION, &["GitHub.com".to_owned(), "GitHub Enterprise".to_owned()])
            .with_context(named_up_front)?
            == 0;
    if public {
        return Ok(Deployment::Public);
    }

    enterprise(&asked(ENTERPRISE_QUESTION)?)
}

/// [`ask`], with the flag that would have answered it named in the failure.
///
/// Without this a login run by a machine fails with "there was no answer",
/// which is true and says nothing about what to do instead.
fn asked(question: &str) -> Result<String> {
    ask(question).with_context(named_up_front)
}

/// What to have passed instead of answering a question nobody was there for.
fn named_up_front() -> String {
    "the deployment was not named and the question could not be answered; pass \
     `--deployment public` for github.com, or `--enterprise-url <url>` for a \
     GitHub Enterprise deployment"
        .to_owned()
}

/// An enterprise deployment at `url`, refusing the one spelling that names
/// nothing.
///
/// Upstream validates the same emptiness and then parses the value
/// (`copilot.ts:211-220`); [`Deployment::enterprise`] already normalises every
/// spelling that parses, so what is left to refuse is a blank answer — which
/// would otherwise become `https:///login/device/code`.
fn enterprise(url: &str) -> Result<Deployment> {
    if url.trim().is_empty() {
        bail!("a GitHub Enterprise deployment needs a URL or a domain; nothing was stored");
    }

    Ok(Deployment::enterprise(url.trim()))
}

/// The grok device flow, against xAI or against whatever redirected it.
///
/// The paths are the ones xAI publishes (`xai.ts:12`, `:20`), so a redirected
/// login exercises the same routing the real one does.
fn grok_flow() -> Result<DeviceFlow> {
    let Some(origin) = issuer()? else {
        return grok::device_flow().map_err(nothing_stored);
    };

    grok::device_flow_at(format!("{origin}/oauth2/device/code"), format!("{origin}/oauth2/token"))
        .map_err(nothing_stored)
}

/// The grok browser login, against xAI or against whatever redirected it.
///
/// The paths are the ones xAI publishes (`xai.ts:11`, `:12`), so a redirected
/// login exercises the same routing the real one does. **The callback port is
/// not redirected with them**: it is part of somebody else's client
/// registration rather than an address this build chose, so there is nothing
/// here for an override to mean.
fn grok_browser_flow() -> Result<grok::BrowserFlow> {
    let Some(origin) = issuer()? else {
        return grok::browser_flow().map_err(nothing_stored);
    };

    grok::browser_flow_at(format!("{origin}/oauth2/authorize"), format!("{origin}/oauth2/token"))
        .map_err(nothing_stored)
}

/// The Copilot device flow for `deployment`, or against whatever redirected it.
///
/// A redirected login still reads the deployment, because that is what decides
/// the `enterpriseUrl` the credential is stored with (`copilot.ts:301-303`);
/// only where the request goes is overridden.
fn copilot_flow(deployment: &Deployment) -> Result<DeviceFlow> {
    let Some(origin) = issuer()? else {
        return copilot::device_flow(deployment).map_err(nothing_stored);
    };

    copilot::device_flow_at(
        format!("{origin}/login/device/code"),
        format!("{origin}/login/oauth/access_token"),
    )
    .map_err(nothing_stored)
}

/// The cursor login, against cursor's own two hosts or against whatever
/// redirected it.
///
/// One origin redirects both hosts. In production the deep link's page and
/// the poll endpoint live on different hosts (`cursor.com`, `api2.cursor.sh`),
/// but a suite that owns the poll has to be able to assert the deep link's
/// shape too — so the override serves the pair from one roof, at the same
/// paths the real hosts use.
fn cursor_flow() -> Result<cursor::Flow> {
    let Some(origin) = issuer()? else {
        return cursor::login_flow().map_err(nothing_stored);
    };

    cursor::login_flow_at(&origin).map_err(nothing_stored)
}

/// A ChatGPT login against the real issuer, or against whatever redirected it.
fn chatgpt() -> Result<openai::Login> {
    match issuer()? {
        Some(origin) => openai::Login::with_issuer(&origin),
        None => openai::Login::new(),
    }
    .map_err(nothing_stored)
}

/// The origin every login endpoint hangs off, when one was named.
///
/// # Errors
///
/// When the variable is set to something that is not a loopback origin. A
/// refusal rather than a fallback to the real issuer: somebody who set this
/// meant to redirect the login, and quietly sending a device code to xAI
/// instead is the one outcome they cannot have wanted.
fn issuer() -> Result<Option<String>> {
    let Ok(value) = std::env::var(ISSUER_ENV) else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Ok(None);
    }
    if loopback_origin(value.trim()).is_none() {
        bail!(
            "{ISSUER_ENV} has to be a loopback origin such as `http://127.0.0.1:8080`, \
             because it decides where this login's code and tokens are sent"
        );
    }

    Ok(Some(value.trim().to_owned()))
}

/// `value` when it is `http://<loopback host>:<port>` and nothing else.
///
/// Deliberately a shape check rather than a URL parse: what has to be true is
/// that the *whole* value is an origin, so userinfo, a path, a query and a
/// fragment are all refused by there being nowhere for them to go. A prefix
/// match alone would accept
/// `http://127.0.0.1:80@elsewhere.example`, which resolves to `elsewhere`.
fn loopback_origin(value: &str) -> Option<&str> {
    let (host, port) = value.strip_prefix("http://")?.rsplit_once(':')?;

    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }

    (!port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())).then_some(value)
}

/// A flow's failure, with the fact that nothing was stored said out loud.
///
/// Structural rather than reassuring: storing is the caller's next step and an
/// error never reaches it. Saying so is what stops somebody who cancelled from
/// wondering whether half a credential is now on disk.
fn nothing_stored(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{error}; nothing was stored")
}

/// Turns the interrupt keystroke into the cancellation the flows take.
///
/// A device login waits for minutes on somebody else's browser, and the only
/// thing that ends it early is this. The flows return promptly when the token
/// fires and hand back [`ganja_provider::auth::device::DeviceError::Cancelled`],
/// which is what makes the message truthful as well as prompt.
///
/// The task is aborted on the way out. Left running it would hold the process's
/// interrupt handler for a wait that has already finished, so the next `Ctrl-C`
/// would be swallowed by nobody.
///
/// `pub(crate)`: `ganja mcp login`'s browser wait in `main.rs` needs the same
/// keystroke-to-cancellation shape a provider login already has, and it is
/// not provider-specific in any way — nothing about it names a vendor.
pub(crate) struct Interrupt(tokio::task::JoinHandle<()>);

impl Interrupt {
    pub(crate) fn watching(cancel: CancellationToken) -> Self {
        Self(tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        }))
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Asks `question` with numbered answers, and returns which was chosen.
///
/// Line-based rather than the arrow-key menu upstream's prompt library draws:
/// raw mode in this binary exists to stop a *secret* being echoed, and a menu
/// choice is not one — so this stays a question anything can answer, including
/// a pipe. That is also what makes the prompts testable without a pty.
///
/// # Errors
///
/// When standard input ends before an answer arrives, which is the shape a
/// cancelled prompt has here.
fn choose(question: &str, options: &[String]) -> Result<usize> {
    loop {
        eprintln!("{question}");
        for (index, option) in options.iter().enumerate() {
            eprintln!("  {}) {option}", index + 1);
        }
        eprint!("> ");
        io::stderr().flush().ok();

        let answer = line(question)?;
        if let Ok(chosen) = answer.trim().parse::<usize>()
            && (1..=options.len()).contains(&chosen)
        {
            return Ok(chosen - 1);
        }

        eprintln!("that is not one of the answers");
    }
}

/// Asks `question` and returns what was typed.
///
/// # Errors
///
/// As [`choose`].
fn ask(question: &str) -> Result<String> {
    eprint!("{question}: ");
    io::stderr().flush().ok();

    line(question)
}

/// One line of standard input, or the fact that there will not be one.
fn line(question: &str) -> Result<String> {
    let mut answer = String::new();
    let read = io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|error| anyhow::anyhow!("{question}: the answer could not be read: {error}"))?;

    if read == 0 {
        bail!("{question}: there was no answer; nothing was stored");
    }

    Ok(answer)
}

/// What is already stored for `provider`, so a login can say what it replaces.
///
/// Two readers rather than one, because neither answers the whole question:
///
/// - [`auth::oauth_for`] reads the file directly, and is what distinguishes a
///   login from a key without interpreting either.
/// - [`auth::list_providers`] is what knows a *key* in the file is a key, and
///   what it may be shown as.
///
/// The `Source::File` filter is load-bearing and now reaches everything it
/// names: the listing used to drop a stored key whose variable was exported,
/// so a key about to be overwritten went unmentioned in exactly the case
/// somebody most needed to hear about it. It reports both rows, and this takes
/// the stored one.
///
/// The tail travels as a [`auth::RedactedTail`] rather than as a `String`,
/// because that type is the whole of this build's answer to how much of a
/// credential may be shown — flattening it to text at the boundary would make
/// the guarantee a convention again.
///
/// # Errors
///
/// When the credential file exists and cannot be read.
/// Takes the provider **id** rather than a [`ProviderId`], because a
/// configured endpoint is stored the same way and has none: `auth::storage_key`
/// passes an id it holds no alias for through unchanged, so this reads exactly
/// where `ganja auth login <id>` wrote.
pub(crate) fn stored(
    provider_id: &str,
) -> Result<Option<(auth::CredentialKind, auth::RedactedTail)>> {
    if let Some(credential) = auth::oauth_for(provider_id)? {
        return Ok(Some((auth::CredentialKind::Oauth, credential.tail())));
    }

    let key = auth::storage_key(provider_id);

    Ok(auth::list_providers()?
        .into_iter()
        .find(|entry| entry.provider_id == key && entry.source == auth::Source::File)
        .map(|entry| (entry.kind, entry.tail)))
}

#[cfg(test)]
#[path = "login_tests.rs"]
mod tests;
