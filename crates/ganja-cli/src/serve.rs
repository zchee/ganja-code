//! `ganja serve` — the whole engine behind a socket, headless, until a
//! signal ends it.
//!
//! Spec: upstream `packages/opencode/src/cli/cmd/serve.ts` and
//! `cli/network.ts`. What is ported: the two flags a headless server needs
//! (`--port`, `--hostname`), the unsecured-server warning when no password is
//! configured (`serve.ts:15-17`), and the address line (`serve.ts:20`) — on
//! stdout, because it is the one thing a script consumes, where the warning
//! and every other diagnostic go to stderr the way every subcommand here
//! writes (deviation: serve-warning-is-stderr). Upstream's mDNS and CORS
//! options name features this build does not have and are not carried
//! (deviation: serve-carries-the-flags-ganja-can-honor).
//!
//! The engine is assembled exactly the way `run` assembles one — same
//! config, provider, tools, permissions, agents, instructions, storage — with
//! two differences that follow from being a server rather than a one-shot
//! turn: the file watcher runs, because later turns must distrust files that
//! moved between them, and `run`'s auto-refuse permission rules are **not**
//! installed, because a serve client is a person at a distance — dialogs
//! travel out on `/event` and answers come back on
//! `POST /permission/{id}/reply` (deviation:
//! serve-keeps-interactive-permissions).

use std::{io::Write as _, sync::Arc};

use anyhow::{Context as _, Result};
use clap::Args;
use ganja_core::config::Overrides;

use crate::assemble::assemble;

/// `ganja serve`'s flags — upstream's network options, minus what this build
/// has no feature behind (see the module docs).
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Port to listen on: taken exactly, or refused. Absent tries 4096
    /// first, then any free port.
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,
    /// Hostname to listen on: an IP address, or "localhost". Anything that
    /// is not loopback requires GANJA_SERVER_PASSWORD to be set.
    #[arg(long, value_name = "HOST", default_value = ganja_serve::DEFAULT_HOSTNAME)]
    hostname: String,
}

/// Serves the engine until SIGINT or SIGTERM, then shuts everything down in
/// the order `run` does: the server first, then the MCP server processes,
/// then the language servers.
///
/// # Errors
///
/// Exit 1 when the engine cannot be assembled, and for the serve layer's
/// startup refusals — an unresolvable hostname, a taken explicit port, and
/// the deliberate one: a non-loopback bind with no password configured.
pub async fn serve(args: ServeArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let assembled = assemble(&cwd, &Overrides::default())?;
    // Dialled in the background, exactly as the UI dials them: a server that
    // never answers costs its tools rather than the listener.
    assembled.engine.connect_mcp();
    // A listener is a session too, and its hooks open it the same way.
    assembled.engine.session_start().await;
    // And it starts under the configured effort the same way, for the session
    // this process minted; a route that resumes another one restores that
    // row's own effort and this yields to it.
    assembled
        .engine
        .seed_effort(assembled.config.effort.clone())
        .await;

    let credentials = ganja_serve::Credentials::from_env();
    if credentials.is_none() {
        // Upstream's sentence (`serve.ts:16`) naming this build's variable.
        eprintln!(
            "Warning: {} is not set; server is unsecured.",
            ganja_serve::PASSWORD_ENV
        );
    }

    let engine = Arc::new(assembled.engine);
    // A server outlives many turns, so a file that moves between them must be
    // flagged before the next turn trusts it — the same reason the UI
    // watches and one-shot `run` does not.
    engine.watch_files();

    let handle = ganja_serve::serve(
        Arc::clone(&engine),
        ganja_serve::ServeConfig {
            hostname: args.hostname.clone(),
            port: args.port,
            credentials,
            directory: cwd,
            root: assembled.root,
            data: Some(assembled.data),
            storage: Some(assembled.storage),
            config: Some(assembled.config),
            heartbeat: ganja_serve::HEARTBEAT,
        },
    )
    .await?;

    // Upstream's line (`serve.ts:20`) under this build's name: the payload a
    // script parses, so stdout, flushed before anything waits.
    println!(
        "ganja server listening on http://{}:{}",
        args.hostname,
        handle.address().port()
    );
    std::io::stdout()
        .flush()
        .context("failed to write the address line")?;

    wait_for_shutdown().await;

    handle
        .shutdown()
        .await
        .context("the server did not stop cleanly")?;
    engine.session_end(ganja_core::hook::EXIT_REASON).await;
    assembled.servers.shutdown().await;
    engine.shutdown_lsp();
    engine.shutdown_jobs().await;

    Ok(())
}

/// The first of SIGINT or SIGTERM, which are the two ways a supervisor or a
/// terminal ends a server it started.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(sigterm) => sigterm,
                // A process that cannot register SIGTERM still stops on ^C.
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                    return;
                }
            };

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
