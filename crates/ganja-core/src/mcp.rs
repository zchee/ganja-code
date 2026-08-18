//! Model Context Protocol servers, and the tools they lend the agent loop.
//!
//! Spec: upstream `packages/opencode/src/mcp/index.ts` (lifecycle, status) and
//! `packages/opencode/src/mcp/catalog.ts` (listing, naming, result shaping).
//!
//! A configured server is connected once, at startup, in a background task.
//! Everything about that is deliberate:
//!
//! - **Concurrently, and never blocking.** A server that is slow, missing or
//!   broken makes its own status say so and costs nobody else a turn. The
//!   engine starts, the first prompt runs, and the tools appear when they
//!   appear.
//! - **As information.** Nothing here fails a turn. A dead server is a tool
//!   the model is not offered; a tool that errors is error text the model
//!   reads.
//!
//! # Reconnect (**D463**)
//!
//! Upstream never reconnects (`index.ts:443-455`): a transport that closes
//! marks the server [`Status::Failed`] for good, and that was this port's own
//! posture through P13's first four waves too. It retires here. Claude Code
//! does reconnect — a `/mcp` dialog offers it by name
//! (`docs/references/claude.ja.md:168`) — and a person watching a long
//! session is in a better position to say "try that one again" than a
//! transport that only ever closes once is to say it for them. Three doors,
//! all riding the same `Servers::connect` a startup uses:
//!
//! - [`Servers::reconnect`] — the `/mcp` dialog's Reconnect action and
//!   [`crate::engine::Engine::reconnect_mcp`], for a server the dialog shows
//!   as [`Status::Failed`]. Refused, naming why, for a server reconnect does
//!   not mean anything about: not configured, disabled, already connected, or
//!   still on its very first dial.
//! - [`Servers::retry_once`] — a server whose first-ever dial never succeeded
//!   gets exactly one automatic re-dial, spawned at the [`Servers::reap`] seam
//!   [`crate::engine::Engine`] already calls once per turn start
//!   (`refresh_mcp`) and never awaited there, so a dead server costs at most
//!   one background connect timeout across a whole session rather than one
//!   every turn.
//! - A connection that later closes — [`Servers::reap`] notices it, same as
//!   always — still only leaves through the two doors above; nothing reconnects
//!   it on its own.
//!
//! Either door bumps [`Servers::generation`] on success, exactly as a first
//! connect does, so the *next* turn — never the one already streaming — is
//! the one that sees the revived tools ([`crate::engine::Engine`]'s
//! once-per-turn `refresh_mcp` contract, unmoved).
//!
//! A `ganja mcp reconnect <name>` CLI subcommand was deliberately not built
//! alongside these: W5a kept the CLI's own scope to the read-only listing,
//! so the `/mcp` dialog and [`crate::engine::Engine::reconnect_mcp`] remain
//! the only two doors above. A named follow-up, not an oversight.
//!
//! # Output caps (**D464**)
//!
//! Claude's `MAX_MCP_OUTPUT_TOKENS` names the same worry a server's own result
//! can raise that any other tool's can — flooding the context window — and
//! this build spells the budget in bytes rather than tokens, matching every
//! other clamp in the tree ([`ganja_tool::truncate`]). `render` clamps
//! through [`ganja_tool::truncate::clamp_bytes`], the spill-file posture every
//! one-shot tool here already uses (`clamp_with`'s: the full result is
//! written to a file and the model is told where), at the budget
//! [`crate::config::McpServer::output_limit`] names for that server —
//! [`ganja_tool::truncate::MAX_CHARS`] for one that names none.
//!
//! # What a server's tools are called
//!
//! `mcp__<server>__<tool>`, each half through upstream's sanitizer
//! (`[^a-zA-Z0-9_-]` → `_`, `catalog.ts:117-119`). Upstream joins the two
//! halves with a single `_` and no prefix; the namespace here is
//! plan-mandated and collision-proof against the builtins, which a bare
//! `<server>_<tool>` is not — a server called `web` lending a tool called
//! `fetch` would otherwise be spelled `web_fetch` and sit one underscore away
//! from `webfetch` (deviation: mcp-namespace-prefix).
//!
//! Because that name is also the permission key, and because it cannot be
//! known before a server has been asked, [`crate::tool::Tool::id`] borrows
//! from `&self` rather than returning a `'static` literal.
//!
//! # OAuth (**D466**)
//!
//! A remote entry's `oauth: {}` ([`crate::config::McpRemote::oauth`]) turns on
//! [`ganja_provider::auth::mcp_oauth`]: RFC 8414 discovery, RFC 7591 dynamic
//! registration, then the crate's own PKCE and loopback machinery, reused
//! wholesale rather than forked. What lands here is the two seams that
//! module's own doc names as somebody else's job:
//!
//! - **Dial-time bearer.** `Servers::dial`'s Remote branch, for an entry
//!   with `oauth` set, asks [`ganja_provider::auth::Refresher::shared`] for a
//!   usable access token — refreshed first if the stored deadline says it is
//!   due — and sends it as `Authorization: Bearer …`, layered onto whatever
//!   static `headers` the entry also names.
//! - **Refresh-then-redial on a 401.** A connect attempt that still fails
//!   with an authorization challenge (`rmcp`'s own
//!   `ClientInitializeError::is_authorization_required`) forces one more
//!   refresh — bypassing the stored deadline's own due-check, because the
//!   server's answer is what said the token was bad — and retries the dial
//!   exactly once. Static-headers-at-dial is the transport's shape
//!   ([`rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig`] carries no
//!   per-request hook), so this is the honest v1; per-request reactive
//!   re-authorization is a named follow-up, not an oversight. **`rmcp` itself
//!   only recognizes the challenge on a `401` that also carries a
//!   `WWW-Authenticate` header** (RFC 6750 §3's own requirement on a
//!   bearer-auth refusal); a server answering `401` without one is an
//!   ordinary connect failure and never earns the retry — a real,
//!   spec-compliant resource server sends the header, and the fixture in
//!   `tests/mcp_oauth.rs` is built to prove exactly that path.
//! - **Login, two doors.** [`Servers::start_login`] runs discovery and
//!   registration inline and returns once the browser URL is ready —
//!   [`Servers::login_url`] is how a caller shows it — then finishes the wait
//!   for the callback in the background; a successful login stores under
//!   `mcp:<name>` and re-dials through `Servers::connect`. The `/mcp`
//!   dialog's Login action and `ganja mcp login <server>` are both this one
//!   function.
//!
//! # What is not here
//!
//! Prompts and resources are not ported: neither the `mcp.prompts()` surface
//! nor upstream's three built-in resource tools.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::future;
use ganja_provider::auth::RefreshOauth as _;
use rmcp::{
    ClientHandler, RoleClient, ServiceExt as _,
    model::{
        CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, ContentBlock,
        Implementation, PaginatedRequestParams, ResourceContents,
    },
    service::{NotificationContext, RunningService},
    transport::{StreamableHttpClientTransport, TokioChildProcess},
};
use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt as _;

use crate::{
    config::{MCP_CALL_TIMEOUT, MCP_CONNECT_TIMEOUT, MCP_LIST_TIMEOUT, McpServer},
    permission::MCP_PREFIX,
    tool::{Tool, ToolCtx, ToolError, ToolOutput},
};

/// A live connection to one server.
type Client = RunningService<RoleClient, Handler>;

/// How many `tools/list` pages are followed before the listing is abandoned.
///
/// Upstream's cap (`catalog.ts:18-36`). A server that keeps handing out fresh
/// cursors is not paginating, and following it forever is how a startup never
/// finishes.
const MAX_PAGES: usize = 1_000;

/// What a tool call that failed says when the server sent no text with it
/// (`catalog.ts:68-74`).
const UNSPOKEN_ERROR: &str = "MCP tool returned an error";

/// What a server whose transport went away is marked with, spelled as upstream
/// spells it (`index.ts:443-455`).
const CLOSED: &str = "Connection closed";

/// How long a local server's process group is given to end itself after
/// `SIGTERM` before `SIGKILL` follows — the same grace `tool/shell.rs`'s own
/// kill sequence gives a command tree, and for the same reason: only the unix
/// path signals a group at all.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(200);

/// Where one configured server stands.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Status {
    /// Connected, and its tools are in the registry.
    Connected,
    /// Configured with `enabled: false`. Never connected, and not an error.
    Disabled,
    /// It could not be reached, or it stopped being reachable.
    Failed {
        /// What went wrong, as the user is shown it.
        error: String,
    },
}

/// One server's live state.
#[derive(Default)]
struct Server {
    status: Option<Status>,
    client: Option<Arc<Client>>,
    /// The definitions `tools/list` handed back, in the order it handed them.
    defs: Vec<rmcp::model::Tool>,
    /// What the server said about itself at initialize, trimmed.
    instructions: Option<String>,
    /// The stdio child's process group, so shutdown can end the whole of it
    /// and not just the process that was spawned.
    group: Option<u32>,
    /// Whether [`Servers::connect`] has ever landed [`Status::Connected`] for
    /// this server. What [`Servers::retry_once`] reads to tell "the first
    /// dial never succeeded" apart from "it succeeded once and later closed"
    /// — only the former gets an automatic retry; the latter waits for a
    /// person to ask, through [`Servers::reconnect`].
    ever_connected: bool,
}

/// Every MCP server this session was configured with.
///
/// Built from the config and then connected by [`Servers::connect_all`], which
/// is a plain async function so that it is drivable from a test with no
/// terminal and no engine. The engine only spawns it.
pub struct Servers {
    /// What the config said, by name. `BTreeMap` because the iteration order
    /// is load-bearing: it is the order servers contribute tools in, and a
    /// registry that rebuilt in a different order every time would offer the
    /// model a different tool list every time.
    config: BTreeMap<String, McpServer>,
    /// Where the project starts, which a relative `cwd` resolves against.
    root: PathBuf,
    state: Mutex<BTreeMap<String, Server>>,
    /// Set by [`Servers::shutdown`], and checked by [`Servers::connect`] at
    /// the one place a connection lands in `state`. Both sides serialize
    /// through the `state` lock — `shutdown` sets this before it takes the
    /// lock to drain, `connect` reads it after taking the lock to insert —
    /// so whichever runs second always sees what the other already did:
    /// either the drain finds the connection `connect` just installed, or
    /// `connect` finds shutdown already happened and never installs it.
    closed: std::sync::atomic::AtomicBool,
    /// Bumped whenever the set of tools this would contribute changes. The
    /// engine compares it against what it last installed; nothing else about
    /// the rebuild needs a signal.
    generation: AtomicU64,
    /// Servers [`Servers::retry_once`] has already spent its one automatic
    /// re-dial on, so a server that keeps failing costs at most one extra
    /// connect timeout across the whole session rather than one at every turn
    /// start. Manual [`Servers::reconnect`] is a different door and does not
    /// consult or update this.
    retried: Mutex<HashSet<String>>,
    /// Server names currently mid-[`Servers::start_login`], and the URL a
    /// caller should show for each — ephemeral UI state, cleared the moment
    /// the login ends, success or failure. Nothing here is a credential; see
    /// this module's "OAuth" doc section.
    logins: Mutex<BTreeMap<String, String>>,
}

impl Servers {
    /// The servers `config` describes, none of them connected yet.
    #[must_use]
    pub fn new(config: BTreeMap<String, McpServer>, root: &Path) -> Arc<Self> {
        let state = config
            .iter()
            .map(|(name, server)| {
                let status = (!server.enabled()).then_some(Status::Disabled);

                (
                    name.clone(),
                    Server {
                        status,
                        ..Server::default()
                    },
                )
            })
            .collect();

        Arc::new(Self {
            config,
            root: root.to_owned(),
            state: Mutex::new(state),
            closed: std::sync::atomic::AtomicBool::new(false),
            generation: AtomicU64::new(0),
            retried: Mutex::new(HashSet::new()),
            logins: Mutex::new(BTreeMap::new()),
        })
    }

    /// Whether any server was configured at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.config.is_empty()
    }

    /// Every server this session was configured with, by name, sorted —
    /// including one still on its very first dial, which [`Servers::status`]'s
    /// map does not carry until that resolves. What a `/mcp` row list needs to
    /// show every configured server rather than only the ones with an opinion
    /// so far.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.config.keys().cloned().collect()
    }

    /// Connects every enabled server, all at once.
    ///
    /// Returns when the last of them has either connected or failed. Nothing
    /// it does can fail: a server that could not be reached is one whose
    /// [`Status`] says so.
    pub async fn connect_all(self: &Arc<Self>) {
        let connects = self
            .config
            .iter()
            .filter(|(_, server)| server.enabled())
            .map(|(name, server)| self.connect(name, server));

        future::join_all(connects).await;
    }

    /// Connects one server and records what happened to it.
    async fn connect(self: &Arc<Self>, name: &str, server: &McpServer) {
        // The connect budget is fixed rather than the entry's `timeout`, which
        // governs requests only. Upstream documents its `timeout` as covering
        // this and then does not use it here either (`index.ts:38`).
        let budget = Duration::from_millis(MCP_CONNECT_TIMEOUT);
        let outcome = match tokio::time::timeout(budget, self.dial(name, server)).await {
            Ok(outcome) => outcome,
            Err(_) => Err(format!("timed out after {MCP_CONNECT_TIMEOUT}ms")),
        };

        let (client, group) = match outcome {
            Ok(connected) => connected,
            Err(error) => {
                tracing::warn!(server = name, %error, "an MCP server could not be connected");
                self.mark(name, Status::Failed { error });
                return;
            }
        };

        // Listed after the connect and under its own budget, because a server
        // that answers `initialize` and then never answers `tools/list` is a
        // server with no tools rather than a hung startup.
        let defs = match self.list_tools(name, server, &client).await {
            Ok(defs) => defs,
            Err(error) => {
                tracing::warn!(server = name, %error, "an MCP server's tools could not be listed");
                self.mark(name, Status::Failed { error });
                return;
            }
        };

        let instructions = client
            .peer_info()
            .and_then(|info| info.instructions.clone())
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty());

        let mut state = self.state();
        // Checked under the same lock `shutdown` drains under: if `shutdown`
        // already ran, installing this connection would revive a session
        // that is over, with a live client and a live child nothing is ever
        // going to cancel or kill again.
        if self.closed.load(Ordering::Acquire) {
            drop(state);
            tracing::debug!(
                server = name,
                "an MCP connection finished after shutdown; ending it instead of installing it"
            );
            client.cancellation_token().cancel();
            #[cfg(unix)]
            if let Some(group) = group {
                ganja_tool::shell::signal_group(group, libc::SIGTERM);
                tokio::time::sleep(KILL_GRACE).await;
                ganja_tool::shell::signal_group(group, libc::SIGKILL);
            }

            return;
        }

        tracing::info!(server = name, tools = defs.len(), "an MCP server connected");
        state.insert(
            name.to_owned(),
            Server {
                status: Some(Status::Connected),
                client: Some(Arc::new(client)),
                defs,
                instructions,
                group,
                ever_connected: true,
            },
        );
        drop(state);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Opens the transport `server` describes and speaks `initialize` over it.
    ///
    /// The second half of the pair is the child's process group, for a local
    /// server: rmcp's own cleanup kills the process it spawned, and a server
    /// that spawned helpers of its own would leave them behind.
    async fn dial(
        self: &Arc<Self>,
        name: &str,
        server: &McpServer,
    ) -> Result<(Client, Option<u32>), String> {
        let handler = Handler {
            servers: Arc::downgrade(self),
            name: name.to_owned(),
        };

        match server {
            McpServer::Local(local) => {
                let (transport, stderr, group) = self.spawn(name, local)?;
                if let Some(stderr) = stderr {
                    drain(name.to_owned(), stderr);
                }

                let client = handler
                    .serve(transport)
                    .await
                    .map_err(|error| error.to_string())?;

                Ok((client, group))
            }
            McpServer::Remote(remote) => {
                let mut headers = Self::static_headers(remote)?;
                if remote.oauth.is_some() {
                    headers.insert(
                        reqwest::header::AUTHORIZATION,
                        self.bearer_header(name).await?,
                    );
                }

                let url = remote.url.clone();
                let transport = |headers: HashMap<_, _>| {
                    StreamableHttpClientTransport::with_client(
                        reqwest::Client::new(),
                        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                            url.clone(),
                        )
                        .custom_headers(headers),
                    )
                };

                match handler.clone().serve(transport(headers.clone())).await {
                    Ok(client) => Ok((client, None)),
                    // The connect attempt itself just proved the token bad,
                    // which the store's own recorded `expires` may not know
                    // yet — a forced refresh, not the due-check
                    // `bearer_header`'s proactive one already ran, and one
                    // retry: see this module's "OAuth" doc section.
                    Err(error) if remote.oauth.is_some() && error.is_authorization_required() => {
                        headers.insert(
                            reqwest::header::AUTHORIZATION,
                            self.forced_refresh_header(name).await?,
                        );
                        let client = handler
                            .serve(transport(headers))
                            .await
                            .map_err(|error| error.to_string())?;

                        Ok((client, None))
                    }
                    Err(error) => Err(error.to_string()),
                }
            }
        }
    }

    /// The static headers a remote entry's own `headers` config asks for —
    /// everything [`Servers::dial`]'s Remote branch sent before **D466**, now
    /// a helper so the OAuth bearer above can be layered onto the same map.
    fn static_headers(
        remote: &crate::config::McpRemote,
    ) -> Result<HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>, String> {
        let mut headers = HashMap::new();
        for (key, value) in &remote.headers {
            let key = reqwest::header::HeaderName::try_from(key.as_str())
                .map_err(|error| format!("header \"{key}\" is not a header name: {error}"))?;
            // The *value* never reaches the message: a header is where a
            // token goes, and this string is one somebody wrote down next to
            // their API key.
            let value = reqwest::header::HeaderValue::try_from(value.as_str())
                .map_err(|_| format!("the value of header \"{key}\" is not sendable"))?;
            headers.insert(key, value);
        }

        Ok(headers)
    }

    /// `name`'s bearer, refreshed first if the stored credential is due —
    /// [`ganja_provider::auth::Refresher::usable`] over
    /// [`ganja_provider::auth::mcp_oauth::Refresher`], keyed by the reserved
    /// `mcp:<name>` storage prefix.
    async fn bearer_header(&self, name: &str) -> Result<reqwest::header::HeaderValue, String> {
        let key = format!("mcp:{name}");
        let refresher: std::sync::Arc<dyn ganja_provider::auth::RefreshOauth> =
            std::sync::Arc::new(ganja_provider::auth::mcp_oauth::Refresher);

        match ganja_provider::auth::Refresher::shared()
            .usable(&key, refresher)
            .await
        {
            Ok(credential) => bearer_value(&credential),
            Err(error) if error.kind() == ganja_provider::auth::AuthErrorKind::NotOauth => Err(
                format!("mcp server \"{name}\" needs a login: run `ganja mcp login {name}`"),
            ),
            Err(error) => Err(error.to_string()),
        }
    }

    /// `name`'s bearer, forced through a fresh refresh regardless of the
    /// stored deadline — what a 401 at connect time asks for, because the
    /// server, not [`ganja_provider::auth::OauthCredential::needs_refresh_for`],
    /// is what said the token in hand was already bad.
    async fn forced_refresh_header(
        &self,
        name: &str,
    ) -> Result<reqwest::header::HeaderValue, String> {
        let key = format!("mcp:{name}");
        let Some(current) =
            ganja_provider::auth::oauth_for(&key).map_err(|error| error.to_string())?
        else {
            return Err(format!(
                "mcp server \"{name}\" needs a login: run `ganja mcp login {name}`"
            ));
        };

        let renewed = ganja_provider::auth::mcp_oauth::Refresher
            .refresh(&key, &current)
            .await
            .map_err(|error| error.to_string())?;
        ganja_provider::auth::renew_oauth(&key, &renewed).map_err(|error| error.to_string())?;

        bearer_value(&renewed)
    }

    /// Spawns a local server's command with its stderr piped and its own
    /// process group.
    fn spawn(
        &self,
        name: &str,
        local: &crate::config::McpLocal,
    ) -> Result<
        (
            TokioChildProcess,
            Option<tokio::process::ChildStderr>,
            Option<u32>,
        ),
        String,
    > {
        let (program, args) = local
            .command
            .split_first()
            .ok_or_else(|| "the command is empty".to_owned())?;

        let mut command = tokio::process::Command::new(program);
        command.args(args);
        // Layered over the environment this process already has, which is
        // upstream's `{...process.env, ...mcp.environment}` (`index.ts:347`).
        command.envs(&local.environment);
        if let Some(cwd) = &local.cwd {
            command.current_dir(self.root.join(cwd));
        }
        // The backstop under an exit that never reaches `shutdown` — a panic, a
        // `?` on some future path out of startup, a frontend that is killed.
        //
        // Two other things already end a child in the ordinary case, and this
        // is here because neither covers the last moment of the process.
        // Closing stdin ends a stdio server that respects EOF, but only once
        // something drops the transport, and a server that ignores EOF is
        // unmoved. `rmcp`'s own `ChildWithCleanup::drop` kills the child — but
        // it does so from a `tokio::spawn`, so it needs a runtime that is still
        // scheduling; a runtime being torn down may never poll that task.
        // `kill_on_drop` is the tokio-level guarantee that does not: the kill
        // rides the child handle's own destructor.
        //
        // It ends the spawned process, not the group below: killing a group is
        // `shutdown`'s job, because it needs the pid and a `killpg` that no
        // destructor can make. Belt and braces, in that order.
        command.kill_on_drop(true);
        #[cfg(unix)]
        {
            // The same call `tool/shell.rs` makes, for the same reason: a
            // server that spawns helpers of its own is a tree, and only a
            // group can be ended as one.
            command.process_group(0);
        }

        let (process, stderr) = TokioChildProcess::builder(command)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| format!("{program} could not be started: {error}"))?;
        let group = process.id();
        tracing::debug!(server = name, program, ?group, "an MCP server was spawned");

        Ok((process, stderr, group))
    }

    /// Every tool `client` advertises, following `nextCursor` to the end.
    async fn list_tools(
        &self,
        name: &str,
        server: &McpServer,
        client: &Client,
    ) -> Result<Vec<rmcp::model::Tool>, String> {
        let budget = Duration::from_millis(server.timeout(MCP_LIST_TIMEOUT));
        let mut defs = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen: Vec<String> = Vec::new();

        for _ in 0..MAX_PAGES {
            let params = PaginatedRequestParams::default().with_cursor(cursor.clone());
            let page = tokio::time::timeout(budget, client.list_tools(Some(params)))
                .await
                .map_err(|_| format!("tools/list timed out after {}ms", budget.as_millis()))?
                .map_err(|error| error.to_string())?;

            defs.extend(page.tools);
            let Some(next) = page.next_cursor else {
                return Ok(defs);
            };
            // A cursor already followed means the listing is a loop, not a
            // page (`catalog.ts:18-36`).
            if seen.contains(&next) {
                return Err(format!("tools/list repeated the cursor {next:?}"));
            }
            seen.push(next.clone());
            cursor = Some(next);
        }

        tracing::warn!(
            server = name,
            "an MCP server's tool listing exceeded {MAX_PAGES} pages"
        );

        Err(format!("tools/list exceeded {MAX_PAGES} pages"))
    }

    /// Where every configured server stands, connected or not.
    ///
    /// A server the config named but nothing has reached yet reports
    /// [`Status::Failed`] with no error text only after a connect attempt; up
    /// to then it is absent from the map, which is what "still connecting"
    /// looks like without a fourth variant to mean it.
    #[must_use]
    pub fn status(&self) -> BTreeMap<String, Status> {
        self.state()
            .iter()
            .filter_map(|(name, server)| Some((name.clone(), server.status.clone()?)))
            .collect()
    }

    /// The process group of every local server currently running.
    ///
    /// Exists for the test that pins the no-orphan rule; nothing in the engine
    /// asks, and a frontend has no use for a pid it must not signal.
    #[must_use]
    pub fn process_groups(&self) -> Vec<u32> {
        self.state()
            .values()
            .filter_map(|server| server.group)
            .collect()
    }

    /// How many times the tool surface has changed.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Marks every connected server whose transport has gone away as failed,
    /// dropping its tools.
    ///
    /// Nothing here reconnects it — the next registry rebuild simply does not
    /// carry those tools until [`Servers::reconnect`] or [`Servers::retry_once`]
    /// revives the connection (**D463**).
    pub fn reap(&self) {
        let mut lost = Vec::new();
        {
            let mut state = self.state();
            for (name, server) in state.iter_mut() {
                let closed = server
                    .client
                    .as_ref()
                    .is_some_and(|client| client.is_transport_closed());
                if !closed {
                    continue;
                }

                server.status = Some(Status::Failed {
                    error: CLOSED.to_owned(),
                });
                server.client = None;
                server.defs.clear();
                server.instructions = None;
                lost.push(name.clone());
            }
        }

        if lost.is_empty() {
            return;
        }
        for name in lost {
            tracing::warn!(server = %name, "an MCP connection closed; its tools are withdrawn");
        }
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Re-dials `name` through the same `Servers::connect` a startup uses,
    /// naming why when reconnect does not mean anything for this server
    /// (**D463**): not configured at all, `enabled: false`, already
    /// [`Status::Connected`], or still on its very first dial (absent from
    /// [`Servers::status`]'s map). The one status this proceeds for is
    /// [`Status::Failed`] — reached by a dial that never succeeded, or by
    /// [`Servers::reap`] noticing a transport that closed.
    ///
    /// `Ok(())` means the attempt ran, not that it succeeded: `connect`
    /// itself never fails outward, so the outcome — connected again, or
    /// failed again with a fresh reason — is read back through
    /// [`Servers::status`], exactly as a first connect is.
    ///
    pub async fn reconnect(self: &Arc<Self>, name: &str) -> Result<(), String> {
        let Some(server) = self.config.get(name) else {
            return Err(format!("mcp server \"{name}\" is not configured"));
        };

        let status = self.state().get(name).and_then(|held| held.status.clone());
        match status {
            Some(Status::Failed { .. }) => {}
            Some(Status::Connected) => {
                return Err(format!("mcp server \"{name}\" is already connected"));
            }
            Some(Status::Disabled) => {
                return Err(format!("mcp server \"{name}\" is disabled"));
            }
            None => {
                return Err(format!(
                    "mcp server \"{name}\" has not finished its first connection attempt yet"
                ));
            }
        }

        self.connect(name, server).await;

        Ok(())
    }

    /// Spends each still-[`Status::Failed`] server's one automatic re-dial, for
    /// a server whose first-ever dial never succeeded (**D463**). Spawned and
    /// never awaited, so a call from the synchronous `refresh_mcp` turn-start
    /// seam returns immediately regardless of how the retry goes — the whole
    /// point being that a dead server cannot add a connect timeout to every
    /// turn.
    ///
    /// A server that *did* connect once and later closed is not "the first
    /// dial" any more (`Server::ever_connected`) and is left for
    /// [`Servers::reconnect`] to revive on request. Bookkept per server, not
    /// per call: once spent, a server's one automatic retry never fires again
    /// this session, however many more times its status reads
    /// [`Status::Failed`].
    pub fn retry_once(self: &Arc<Self>) {
        let candidates: Vec<String> = {
            let state = self.state();
            let mut retried = self
                .retried
                .lock()
                .expect("the MCP retry set is never poisoned");

            state
                .iter()
                .filter(|(name, server)| {
                    matches!(server.status, Some(Status::Failed { .. }))
                        && !server.ever_connected
                        && retried.insert((*name).clone())
                })
                .map(|(name, _)| name.clone())
                .collect()
        };

        for name in candidates {
            let Some(server) = self.config.get(&name).cloned() else {
                continue;
            };
            let this = Arc::clone(self);
            tokio::spawn(async move {
                this.connect(&name, &server).await;
            });
        }
    }

    /// The tools every connected server contributes, in the one order they are
    /// ever built in.
    ///
    /// Servers contribute in sorted-name order and each server's tools in the
    /// order it listed them, so two rebuilds of the same connections produce
    /// the same list. A name that survives sanitization as one already taken
    /// **refuses the later tool**, naming both sides — upstream's record
    /// assignment silently overwrites, which loses a tool without saying so
    /// (deviation: mcp-collision-refuses-the-later-tool).
    #[must_use]
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let state = self.state();
        let listings = connected_listings(&state);

        catalog(&listings)
            .into_iter()
            .filter_map(|(id, server, def)| {
                let client = state.get(server)?.client.clone()?;

                Some(Arc::new(McpTool {
                    id,
                    remote: def.name.to_string(),
                    description: def.description.clone().unwrap_or_default().into_owned(),
                    schema: force_object(&def.input_schema),
                    client,
                    timeout: self.call_timeout(server),
                    output_limit: self.output_limit(server),
                }) as Arc<dyn Tool>)
            })
            .collect()
    }

    /// How many tools each connected server lends, by name — after the same
    /// sanitizing and collision resolution [`Servers::tools`] applies, so a
    /// `/mcp` row or `ganja mcp`'s listing never disagrees with what the
    /// model is actually offered. A server that is not connected — disabled,
    /// failed, still dialling — has no entry, which is what zero looks like
    /// without every caller having to spell out the default.
    #[must_use]
    pub fn tool_counts(&self) -> BTreeMap<String, usize> {
        let state = self.state();
        let listings = connected_listings(&state);

        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for (_, server, _) in catalog(&listings) {
            *counts.entry(server.clone()).or_insert(0) += 1;
        }

        counts
    }

    /// How long one call to `server`'s tools may take.
    fn call_timeout(&self, server: &str) -> Duration {
        let millis = self
            .config
            .get(server)
            .map_or(MCP_CALL_TIMEOUT, |entry| entry.timeout(MCP_CALL_TIMEOUT));

        Duration::from_millis(millis)
    }

    /// Bytes one call to `server`'s tools may return before the result is
    /// clamped; see [`crate::config::McpServer::output_limit`].
    fn output_limit(&self, server: &str) -> usize {
        let bytes = self
            .config
            .get(server)
            .map_or(crate::tool::truncate::MAX_CHARS as u64, |entry| {
                entry.output_limit(crate::tool::truncate::MAX_CHARS as u64)
            });

        usize::try_from(bytes).unwrap_or(crate::tool::truncate::MAX_CHARS)
    }

    /// The `<mcp_instructions>` block for the system prompt, or [`None`] when
    /// no connected server said anything.
    ///
    /// Ported from upstream's `session/system.ts:112-128`, including the
    /// indentation, which is part of what the model reads. Upstream also
    /// admits a connected server that contributed no tools at all, and drops
    /// one whose every tool a rule has denied; the gate here is simply
    /// "connected, and lent at least one tool" (deviation:
    /// mcp-instructions-gated-on-registered-tools).
    #[must_use]
    pub fn instructions(&self) -> Option<String> {
        let mut lines = vec!["<mcp_instructions>".to_owned()];
        let mut said_anything = false;

        for (name, server) in self.state().iter() {
            let Some(instructions) = &server.instructions else {
                continue;
            };
            if server.client.is_none() || server.defs.is_empty() {
                continue;
            }

            said_anything = true;
            lines.push(format!("  <server name=\"{name}\">"));
            lines.extend(instructions.lines().map(|line| format!("    {line}")));
            lines.push("  </server>".to_owned());
        }

        if !said_anything {
            return None;
        }
        lines.push("</mcp_instructions>".to_owned());

        Some(lines.join("\n"))
    }

    /// Closes every connection and ends every local server's process group.
    pub async fn shutdown(&self) {
        // Set before the lock below is taken, so a `connect` that checks
        // this after it takes the same lock — the one place a connection
        // lands in `state` — always sees a session that is over, whether it
        // gets there before this drain or after it.
        self.closed.store(true, Ordering::Release);

        let (clients, groups): (Vec<_>, Vec<_>) = {
            let mut state = self.state();
            let mut clients = Vec::new();
            let mut groups = Vec::new();
            for server in state.values_mut() {
                clients.extend(server.client.take());
                groups.extend(server.group.take());
                server.defs.clear();
            }

            (clients, groups)
        };

        for client in clients {
            // Cancelling the token is what `RunningService::close` does with
            // the handle this does not own: the service task stops, the
            // transport closes, and rmcp's own child cleanup runs.
            client.cancellation_token().cancel();
        }
        #[cfg(unix)]
        {
            // `SIGTERM` first, a shared grace, then `SIGKILL` for whichever
            // groups ignored it — upstream's `killTree` sequence, the same
            // one `tool/shell.rs` runs for a shell command's own tree. One
            // `SIGTERM` and nothing after it, which is what this replaced,
            // left a server's helpers running for as long as the server
            // itself chose to ignore the signal.
            for &group in &groups {
                ganja_tool::shell::signal_group(group, libc::SIGTERM);
            }
            if !groups.is_empty() {
                tokio::time::sleep(KILL_GRACE).await;
                for group in groups {
                    ganja_tool::shell::signal_group(group, libc::SIGKILL);
                }
            }
        }
        #[cfg(not(unix))]
        drop(groups);
    }

    /// Re-lists `name`'s tools after it said its list changed.
    async fn refresh(self: &Arc<Self>, name: &str) {
        let (Some(client), Some(server)) = (
            self.state().get(name).and_then(|held| held.client.clone()),
            self.config.get(name),
        ) else {
            return;
        };

        match self.list_tools(name, server, &client).await {
            Ok(defs) => {
                let mut state = self.state();
                if let Some(held) = state.get_mut(name) {
                    // Only if this is still the same connection: a re-list that
                    // finished after the server was replaced would install a
                    // dead server's tools.
                    if held
                        .client
                        .as_ref()
                        .is_some_and(|held| Arc::ptr_eq(held, &client))
                    {
                        held.defs = defs;
                    }
                }
                drop(state);
                self.generation.fetch_add(1, Ordering::Release);
            }
            Err(error) => {
                tracing::warn!(
                    server = name,
                    %error,
                    "an MCP server announced new tools and then could not list them"
                );
            }
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Server>> {
        self.state.lock().expect("the MCP state is never poisoned")
    }

    fn mark(&self, name: &str, status: Status) {
        self.state().entry(name.to_owned()).or_default().status = Some(status);
    }

    /// Whether `name` is a remote server configured with `oauth` — what gates
    /// the `/mcp` dialog's Login action and `ganja mcp login`, whatever the
    /// server's current [`Status`], unlike [`Servers::reconnect`]'s
    /// `Failed`-only gate.
    #[must_use]
    pub fn has_oauth(&self, name: &str) -> bool {
        matches!(
            self.config.get(name),
            Some(McpServer::Remote(remote)) if remote.oauth.is_some()
        )
    }

    /// The URL a login for `name` wants opened, while one is in flight —
    /// cleared the moment [`Servers::start_login`]'s background wait ends,
    /// success or failure.
    #[must_use]
    pub fn login_url(&self, name: &str) -> Option<String> {
        self.logins
            .lock()
            .expect("the MCP login map is never poisoned")
            .get(name)
            .cloned()
    }

    /// Starts a login for `name`: discovery and registration run here, so
    /// this returns once the URL is ready rather than once the login
    /// finishes — [`Servers::login_url`] is how a caller shows it, and
    /// [`Servers::status`] together with it is how a caller learns the
    /// outcome, the way [`Servers::retry_once`]'s spawned re-dial is read
    /// back.
    ///
    /// An in-flight login is not cancelled by [`Servers::shutdown`], which
    /// only ever drains connected clients — the spawned wait above is bounded
    /// by its own five-minute
    /// [`ganja_provider::auth::mcp_oauth::CALLBACK_DEADLINE`] regardless, and
    /// holds nothing more than a loopback socket and a spawned task, the same
    /// spawned-never-joined shape [`Servers::retry_once`]'s re-dial already
    /// has. A real gap only if `shutdown` is ever expected to mean "nothing
    /// of mine still runs".
    ///
    /// # Errors
    ///
    /// Named refusals: not configured, not a remote server, `oauth` not
    /// configured, a login for this server already in flight, or discovery
    /// and registration's own failures.
    pub async fn start_login(self: &Arc<Self>, name: &str) -> Result<(), String> {
        let Some(McpServer::Remote(remote)) = self.config.get(name) else {
            return Err(format!("mcp server \"{name}\" is not a remote server"));
        };
        if remote.oauth.is_none() {
            return Err(format!("mcp server \"{name}\" has no oauth configured"));
        }
        {
            let mut logins = self
                .logins
                .lock()
                .expect("the MCP login map is never poisoned");
            if logins.contains_key(name) {
                return Err(format!("a login for \"{name}\" is already in progress"));
            }
            // A placeholder until the URL below is ready — the window is
            // exactly the discovery-and-registration round trip this
            // function itself awaits before returning, the same shape
            // `announce()`-before-a-long-wait already has in `ganja-cli`.
            logins.insert(name.to_owned(), String::new());
        }
        let forget = |servers: &Self| {
            servers
                .logins
                .lock()
                .expect("the MCP login map is never poisoned")
                .remove(name);
        };

        let browser = match ganja_provider::auth::mcp_oauth::Login::new(&remote.url)
            .map_err(|error| error.to_string())
        {
            Ok(login) => match login.browser().await {
                Ok(browser) => browser,
                Err(error) => {
                    forget(self);
                    return Err(error.to_string());
                }
            },
            Err(error) => {
                forget(self);
                return Err(error);
            }
        };
        self.logins
            .lock()
            .expect("the MCP login map is never poisoned")
            .insert(name.to_owned(), browser.url().to_owned());

        let this = Arc::clone(self);
        let name = name.to_owned();
        tokio::spawn(async move {
            let cancel = tokio_util::sync::CancellationToken::new();
            let result = browser
                .wait(ganja_provider::auth::mcp_oauth::CALLBACK_DEADLINE, &cancel)
                .await;
            this.logins
                .lock()
                .expect("the MCP login map is never poisoned")
                .remove(&name);

            match result {
                Ok(credential) => {
                    let key = format!("mcp:{name}");
                    if let Err(error) = ganja_provider::auth::set_oauth(&key, &credential) {
                        tracing::warn!(server = %name, %error, "an MCP login could not be stored");
                        this.mark(
                            &name,
                            Status::Failed {
                                error: error.to_string(),
                            },
                        );
                        return;
                    }
                    if let Some(server) = this.config.get(&name).cloned() {
                        this.connect(&name, &server).await;
                    }
                }
                Err(error) => {
                    tracing::warn!(server = %name, %error, "an MCP login did not complete");
                    this.mark(
                        &name,
                        Status::Failed {
                            error: error.to_string(),
                        },
                    );
                }
            }
        });

        Ok(())
    }
}

/// `credential`'s access token as the `Authorization` header value a request
/// carries — the one place either helper above turns a token into bytes on
/// the wire.
fn bearer_value(
    credential: &ganja_provider::auth::OauthCredential,
) -> Result<reqwest::header::HeaderValue, String> {
    let mut value = reqwest::header::HeaderValue::try_from(format!(
        "Bearer {}",
        credential.access.expose_secret()
    ))
    .map_err(|_| "the stored mcp bearer token is not a sendable header value".to_owned())?;
    value.set_sensitive(true);

    Ok(value)
}

/// Reads a local server's stderr and writes it to the trace log.
///
/// A server is entitled to chatter on stderr; upstream pipes it for the same
/// reason (`stderr: "pipe"`, `index.ts:347-357`). It is never shown and never
/// reaches the model — it is diagnostics about somebody else's process.
fn drain(name: String, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(server = %name, "{line}");
        }
    });
}

/// The client half of the protocol: what a server may ask of ganja, and what
/// it may tell it.
///
/// Almost nothing. Upstream declares `roots` and comments the rest out
/// (`index.ts:39-50`); this declares none of it, so a server is told plainly
/// that sampling, elicitation and roots are not on offer rather than being
/// offered a root it then cannot use.
#[derive(Clone)]
struct Handler {
    /// Weak, because the servers own the connection that owns this.
    servers: Weak<Servers>,
    name: String,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let Some(servers) = self.servers.upgrade() else {
            return;
        };

        servers.refresh(&self.name).await;
    }
}

/// One tool a server lends the agent loop.
struct McpTool {
    /// The namespaced name the model calls and the permission engine gates.
    id: String,
    /// What the server calls it, which is what goes back over the wire.
    remote: String,
    description: String,
    /// Already forced to the object shape a provider will take; see
    /// [`force_object`].
    schema: rmcp::model::JsonObject,
    client: Arc<Client>,
    timeout: Duration,
    /// Bytes a result from this tool may carry before [`render`] clamps it;
    /// see [`crate::config::McpServer::output_limit`].
    output_limit: usize,
}

#[async_trait]
impl Tool for McpTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> schemars::Schema {
        schemars::Schema::from(self.schema.clone())
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let mut params = CallToolRequestParams::new(self.remote.clone());
        // The schema says the arguments are an object, so anything else is a
        // call the model got wrong; sending no arguments lets the server say
        // which one it wanted, in its own words.
        if let serde_json::Value::Object(map) = args {
            params = params.with_arguments(map);
        }

        let call = tokio::time::timeout(self.timeout, self.client.call_tool(params));
        let result = tokio::select! {
            () = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            result = call => result,
        };

        let result = match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Err(ToolError::Failed(error.to_string())),
            Err(_) => {
                return Err(ToolError::Failed(format!(
                    "the MCP server did not answer within {}ms",
                    self.timeout.as_millis()
                )));
            }
        };

        render(&self.id, result, self.output_limit)
    }
}

/// A tool result as the model reads it.
///
/// `isError` becomes a [`ToolError::Failed`] carrying the server's own text,
/// which the agent loop hands the model as the call's result: an error here is
/// something to read, never something that ends a turn. `output_limit` clamps
/// only the successful case — see this module's "Output caps" doc section.
fn render(id: &str, result: CallToolResult, output_limit: usize) -> Result<ToolOutput, ToolError> {
    let text = result
        .content
        .iter()
        .map(block)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    if result.is_error.unwrap_or(false) {
        return Err(ToolError::Failed(if text.is_empty() {
            UNSPOKEN_ERROR.to_owned()
        } else {
            text
        }));
    }

    // A server that answered with structure and no content blocks has still
    // answered; upstream synthesizes the one text block this does
    // (`catalog.ts:75-80`).
    let output = match (&text.is_empty(), &result.structured_content) {
        (true, Some(structured)) => structured.to_string(),
        _ => text,
    };
    let clamped = crate::tool::truncate::clamp_bytes(&output, output_limit);

    Ok(ToolOutput {
        title: id.to_owned(),
        output: clamped.text,
        metadata: serde_json::json!({ "truncated": clamped.truncated }),
    })
}

/// One content block as a line of text.
fn block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(image) => omitted(&image.mime_type, &image.data),
        ContentBlock::Audio(audio) => omitted(&audio.mime_type, &audio.data),
        ContentBlock::Resource(resource) => match &resource.resource {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            ResourceContents::BlobResourceContents {
                blob, mime_type, ..
            } => omitted(
                mime_type.as_deref().unwrap_or("application/octet-stream"),
                blob,
            ),
            _ => String::new(),
        },
        // Not a payload, a pointer — and there is no `resources/read` here to
        // follow it with (deviation: mcp-resource-link-rendered-as-a-uri-line).
        ContentBlock::ResourceLink(link) => format!("[MCP resource link: {}]", link.uri),
        _ => String::new(),
    }
}

/// What stands in for a content block this build cannot carry.
///
/// There is no image part on this wire until a provider carries one, so an
/// image or a blob is described rather than dropped: the model is told
/// something came back and what shape it was (deviation:
/// mcp-binary-content-described-not-carried).
fn omitted(mime: &str, base64: &str) -> String {
    format!(
        "[binary MCP content omitted: {mime}, {} bytes]",
        decoded_len(base64)
    )
}

/// How many bytes a base64 payload holds, without decoding it.
///
/// Four characters carry three bytes, less one per `=` of padding. Counted
/// rather than decoded because the number is all that is wanted and the bytes
/// may be megabytes.
///
/// Padding is optional on the wire — some servers emit the unpadded form
/// RFC 4648 §3.2 allows — so a length that is not a multiple of four is not
/// malformed, just a final partial group: two leftover characters carry one
/// more byte, three carry two more. A padded string never leaves a
/// remainder (padding always brings the total to a multiple of four), so
/// this falls back to the plain `quads * 3 - padding` count for one exactly
/// as before.
fn decoded_len(base64: &str) -> usize {
    let length = base64.len();
    let padding = base64
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    let extra = match length % 4 {
        2 => 1,
        3 => 2,
        _ => 0,
    };

    (length / 4 * 3 + extra).saturating_sub(padding)
}

/// The connected servers' tool listings, in `state`'s own sorted order — the
/// shared first step behind [`Servers::tools`] and [`Servers::tool_counts`],
/// so the two can never disagree about which servers are even in the running.
fn connected_listings(state: &BTreeMap<String, Server>) -> Vec<(&String, &[rmcp::model::Tool])> {
    state
        .iter()
        .filter(|(_, held)| held.client.is_some())
        .map(|(name, held)| (name, held.defs.as_slice()))
        .collect()
}

/// Which tools a set of listings contributes, and under which names.
///
/// Split out from [`Servers::tools`] because it is the whole of the ordering
/// and collision rules and none of the connection state: `listings` is
/// expected in sorted-server order, each server's definitions in the order it
/// listed them, and the result is what a registry rebuild carries.
///
/// A name already taken **refuses the later tool**, naming both sides.
/// Upstream assigns into a record and so silently overwrites
/// (`index.ts:683-685`), which loses a tool without saying which (deviation:
/// mcp-collision-refuses-the-later-tool).
fn catalog<'a>(
    listings: &[(&'a String, &'a [rmcp::model::Tool])],
) -> Vec<(String, &'a String, &'a rmcp::model::Tool)> {
    let mut taken: BTreeMap<String, String> = BTreeMap::new();
    let mut catalog = Vec::new();

    for (server, defs) in listings {
        for def in *defs {
            let id = tool_name(server, &def.name);
            if let Some(held) = taken.get(&id) {
                tracing::warn!(
                    name = %id,
                    first = %held,
                    second = %format!("{server}/{}", def.name),
                    "two MCP tools sanitize to one name; the later one is not registered"
                );
                continue;
            }
            taken.insert(id.clone(), format!("{server}/{}", def.name));
            catalog.push((id, *server, def));
        }
    }

    catalog
}

/// A server's tool as this build names it.
#[must_use]
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{MCP_PREFIX}{}__{}", sanitize(server), sanitize(tool))
}

/// A name with everything a tool name may not contain replaced.
///
/// Upstream's sanitizer verbatim (`catalog.ts:117-119`): `[^a-zA-Z0-9_-]` →
/// `_`. Hyphens survive; dots, spaces, slashes and everything else do not.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// A server's input schema as the model is shown it.
///
/// Forced to an object with properties and nothing else allowed
/// (`catalog.ts:42-52`), because that is the shape every provider's tool-call
/// schema has to be and a server is free to send something else.
fn force_object(schema: &rmcp::model::JsonObject) -> rmcp::model::JsonObject {
    let mut schema = schema.clone();
    schema.insert(
        "type".to_owned(),
        serde_json::Value::String("object".to_owned()),
    );
    schema
        .entry("properties")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    schema.insert(
        "additionalProperties".to_owned(),
        serde_json::Value::Bool(false),
    );

    schema
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use serde_json::json;

    use super::{
        Server, Servers, Status, catalog, decoded_len, force_object, render, sanitize, tool_name,
    };
    use crate::{
        config::{MCP_CALL_TIMEOUT, MCP_LIST_TIMEOUT, McpServer},
        tool::ToolError,
    };

    /// A tool definition as a server would have listed it.
    fn listed(name: &str) -> rmcp::model::Tool {
        rmcp::model::Tool::new(
            name.to_owned(),
            "does a thing".to_owned(),
            Arc::new(serde_json::Map::new()),
        )
    }

    #[test]
    fn a_tool_is_named_for_its_server_and_itself() {
        let cases = [
            ("github", "create_issue", "mcp__github__create_issue"),
            // Hyphens survive the sanitizer; dots and spaces do not.
            (
                "my.special-server",
                "tool-a",
                "mcp__my_special-server__tool-a",
            ),
            (
                "my.special-server",
                "tool.b",
                "mcp__my_special-server__tool_b",
            ),
            ("a b", "c/d", "mcp__a_b__c_d"),
            // Not ASCII, so not kept: one replacement per character.
            ("héllo", "wörld", "mcp__h_llo__w_rld"),
        ];

        for (server, tool, expected) in cases {
            assert_eq!(tool_name(server, tool), expected, "{server}/{tool}");
        }
    }

    #[test]
    fn sanitizing_touches_only_what_a_name_may_not_hold() {
        assert_eq!(sanitize("Abc_09-xyz"), "Abc_09-xyz");
        assert_eq!(sanitize("a.b:c d/e"), "a_b_c_d_e");
    }

    #[test]
    fn a_schema_is_forced_to_an_object_a_provider_will_take() {
        let forced = force_object(
            json!({ "type": "string", "description": "kept" })
                .as_object()
                .expect("the fixture is an object"),
        );

        assert_eq!(
            serde_json::Value::Object(forced),
            json!({
                "type": "object",
                "description": "kept",
                "properties": {},
                "additionalProperties": false,
            })
        );

        // An object that already had properties keeps them.
        let forced = force_object(
            json!({ "type": "object", "properties": { "path": { "type": "string" } } })
                .as_object()
                .expect("the fixture is an object"),
        );
        assert_eq!(forced["properties"]["path"]["type"], json!("string"));
        assert_eq!(forced["additionalProperties"], json!(false));
    }

    /// Two tools whose sanitized names collide: the first one listed keeps the
    /// name and the second is not registered at all.
    #[test]
    fn two_tools_that_sanitize_to_one_name_leave_only_the_first() {
        let one = "a.b".to_owned();
        let defs = [listed("tool.x"), listed("tool_x"), listed("other")];

        let names: Vec<String> = catalog(&[(&one, defs.as_slice())])
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();

        assert_eq!(
            names,
            vec!["mcp__a_b__tool_x".to_owned(), "mcp__a_b__other".to_owned(),]
        );
    }

    /// A collision across two servers is decided the same way, and the order
    /// servers contribute in is the sorted one a rebuild always sees.
    #[test]
    fn servers_contribute_in_sorted_order_and_the_earlier_one_keeps_the_name() {
        let first = "alpha.one".to_owned();
        let second = "beta".to_owned();
        let shared = [listed("run")];
        let alias = [listed("run"), listed("stop")];

        let catalog = catalog(&[(&first, shared.as_slice()), (&second, alias.as_slice())]);
        let names: Vec<String> = catalog.iter().map(|(id, _, _)| id.clone()).collect();

        assert_eq!(
            names,
            vec![
                "mcp__alpha_one__run".to_owned(),
                "mcp__beta__run".to_owned(),
                "mcp__beta__stop".to_owned(),
            ]
        );

        // `alpha.one` and `alpha_one` are one name after sanitization, so now
        // the two `run` tools really do collide and only the first survives.
        let clash = "alpha_one".to_owned();
        let names = catalog_names(&[(&first, shared.as_slice()), (&clash, alias.as_slice())]);
        assert_eq!(
            names,
            vec![
                "mcp__alpha_one__run".to_owned(),
                "mcp__alpha_one__stop".to_owned(),
            ]
        );
    }

    /// The names [`catalog`] would register, for a test that only cares about
    /// those.
    fn catalog_names(listings: &[(&String, &[rmcp::model::Tool])]) -> Vec<String> {
        catalog(listings).into_iter().map(|(id, _, _)| id).collect()
    }

    #[test]
    fn a_configured_timeout_governs_calls_and_listings_but_not_the_connect() {
        let entry: McpServer = serde_json::from_value(json!({
            "type": "local",
            "command": ["echo"],
            "timeout": 1234,
        }))
        .expect("the fixture entry parses");

        assert_eq!(entry.timeout(MCP_CALL_TIMEOUT), 1234);
        assert_eq!(entry.timeout(MCP_LIST_TIMEOUT), 1234);

        let silent: McpServer = serde_json::from_value(json!({
            "type": "local",
            "command": ["echo"],
        }))
        .expect("the fixture entry parses");

        assert_eq!(silent.timeout(MCP_CALL_TIMEOUT), MCP_CALL_TIMEOUT);
        assert_eq!(silent.timeout(MCP_LIST_TIMEOUT), MCP_LIST_TIMEOUT);
    }

    #[test]
    fn an_error_result_carries_the_servers_own_words() {
        let mut result = rmcp::model::CallToolResult::success(vec![
            rmcp::model::ContentBlock::text("   "),
            rmcp::model::ContentBlock::text("the repository is archived"),
        ]);
        result.is_error = Some(true);

        let error = render(
            "mcp__github__create_issue",
            result,
            crate::tool::truncate::MAX_CHARS,
        )
        .expect_err("isError is an error");
        assert!(
            matches!(&error, ToolError::Failed(text) if text == "the repository is archived"),
            "{error}"
        );
    }

    #[test]
    fn an_error_result_with_nothing_to_say_still_says_something() {
        let mut result = rmcp::model::CallToolResult::success(Vec::new());
        result.is_error = Some(true);

        let error = render("mcp__x__y", result, crate::tool::truncate::MAX_CHARS)
            .expect_err("isError is an error");
        assert!(
            matches!(&error, ToolError::Failed(text) if text == super::UNSPOKEN_ERROR),
            "{error}"
        );
    }

    #[test]
    fn a_structured_only_result_becomes_one_json_block() {
        let mut result = rmcp::model::CallToolResult::success(Vec::new());
        result.structured_content = Some(json!({ "count": 2 }));

        let output = render("mcp__x__y", result, crate::tool::truncate::MAX_CHARS)
            .expect("a structured answer is an answer");
        assert_eq!(output.output, r#"{"count":2}"#);
    }

    #[test]
    fn binary_content_is_described_rather_than_carried() {
        let result = rmcp::model::CallToolResult::success(vec![
            rmcp::model::ContentBlock::text("here it is"),
            // Nine bytes, base64-encoded.
            rmcp::model::ContentBlock::image("MTIzNDU2Nzg5", "image/png"),
        ]);

        let output = render("mcp__x__y", result, crate::tool::truncate::MAX_CHARS)
            .expect("an image answer is an answer");
        assert_eq!(
            output.output,
            "here it is\n[binary MCP content omitted: image/png, 9 bytes]"
        );
    }

    #[test]
    fn a_base64_length_is_counted_rather_than_decoded() {
        let cases = [
            // Padded, the common wire form.
            ("", 0),
            ("MTIz", 3),
            ("MTI=", 2),
            ("MQ==", 1),
            // Unpadded (RFC 4648 §3.2), the same "f"/"fo"/"foob"/"fooba"
            // vectors as above with their `=` stripped: a length that is not
            // a multiple of four is a partial final group, not a malformed
            // string.
            ("Zg", 1),
            ("Zm8", 2),
            ("Zm9vYg", 4),
            ("Zm9vYmE", 5),
        ];
        for (encoded, expected) in cases {
            assert_eq!(decoded_len(encoded), expected, "{encoded:?}");
        }
    }

    #[test]
    fn a_disabled_server_is_disabled_before_anything_is_dialled() {
        let config = BTreeMap::from([
            (
                "off".to_owned(),
                serde_json::from_value(json!({
                    "type": "local",
                    "command": ["never-run"],
                    "enabled": false,
                }))
                .expect("the fixture entry parses"),
            ),
            (
                "on".to_owned(),
                serde_json::from_value(json!({ "type": "local", "command": ["also-never"] }))
                    .expect("the fixture entry parses"),
            ),
        ]);
        let servers = Servers::new(config, std::path::Path::new("/"));

        assert_eq!(
            servers.status(),
            BTreeMap::from([("off".to_owned(), Status::Disabled)]),
            "a server nothing has tried yet has no status to report"
        );
    }

    #[test]
    fn instructions_come_only_from_a_server_that_lent_a_tool() {
        let servers = Servers::new(BTreeMap::new(), std::path::Path::new("/"));
        {
            let mut state = servers.state();
            // Connected, but lent nothing: nothing to instruct about.
            state.insert(
                "quiet".to_owned(),
                Server {
                    status: Some(Status::Connected),
                    client: None,
                    defs: Vec::new(),
                    instructions: Some("ignored".to_owned()),
                    group: None,
                    ever_connected: true,
                },
            );
        }

        assert_eq!(servers.instructions(), None);
    }
}
