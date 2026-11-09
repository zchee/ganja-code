//! The worker listens before assembling an engine. A completed checkout is the
//! process handoff boundary, so project discovery and snapshots see a real repo.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use futures::future::BoxFuture;
use ganja_serve::bootstrap::{Bootstrap, Cloned, Executor, Failure, Service, Stage};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::worker_git;

#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// Listen for a bounded checkout request, then replace this process with serve.
    Init(InitArgs),
}

#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    /// Port shared by the init listener and the served engine after handoff.
    #[arg(long, default_value_t = 80)]
    port: u16,
}

pub(crate) async fn run(action: Action) -> Result<()> {
    let Action::Init(args) = action;
    let credentials = ganja_serve::Credentials::from_env()
        .context("ganja worker init requires GANJA_SERVER_PASSWORD")?;
    for variable in ["GIT_CONFIG_GLOBAL", "GIT_SSL_CAINFO"] {
        let Some(path) = std::env::var_os(variable).filter(|path| !path.is_empty()) else {
            bail!("ganja worker init requires {variable}");
        };
        if !tokio::fs::metadata(path).await.is_ok_and(|metadata| metadata.is_file()) {
            bail!("ganja worker init requires {variable} to name a readable file");
        }
    }
    let directory = std::env::current_dir().context("failed to read the worker directory")?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, args.port))
        .await
        .context("failed to bind the worker init listener")?;
    let executor = Arc::new(GitExecutor {
        directory: directory.clone(),
        cancel: CancellationToken::new(),
        active: Arc::new(Semaphore::new(1)),
    });
    let handle = Service::new(credentials, directory.clone(), executor.clone())
        .serve(listener)
        .context("failed to serve worker initialization")?;
    let port = handle.address().port();
    tokio::select! {
        result = handle.handoff() => { result.context("worker initialization failed before handoff")?; }
        result = shutdown_signal() => {
            result.context("failed to listen for worker shutdown")?;
            executor.cancel.cancel();
            // The reservation is taken synchronously before a git task starts.
            // Acquiring it proves every admitted task has killed/reaped its group.
            let _finished = executor.active.acquire().await.context("checkout tracking stopped")?;
            return Ok(());
        }
    }

    use std::os::unix::process::CommandExt as _;
    let error = std::process::Command::new(std::env::current_exe().context("cannot locate ganja")?)
        .env_clear()
        .envs(
            worker_git::CHILD_ENVIRONMENT
                .into_iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (name, value))),
        )
        .args(["serve", "--hostname", "0.0.0.0", "--port", &port.to_string()])
        .current_dir(directory)
        .exec();
    Err(error).context("failed to replace worker init with ganja serve")
}

async fn shutdown_signal() -> std::io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

struct GitExecutor {
    directory: PathBuf,
    cancel: CancellationToken,
    active: Arc<Semaphore>,
}

impl Executor for GitExecutor {
    fn execute(
        &self,
        request: Bootstrap,
        deadline: Instant,
    ) -> BoxFuture<'static, Result<Cloned, Failure>> {
        if self.cancel.is_cancelled() {
            return Box::pin(async {
                Err(Failure {
                    stage: Stage::Dispatch,
                    detail: "worker is shutting down".to_owned(),
                })
            });
        }
        let Ok(active) = Arc::clone(&self.active).try_acquire_owned() else {
            return Box::pin(async {
                Err(Failure {
                    stage: Stage::Dispatch,
                    detail: "a checkout is already active".to_owned(),
                })
            });
        };
        let root = self.directory.clone();
        let cancel = self.cancel.clone();
        // A disconnected HTTP caller cannot drop the task which must finish
        // process-group cleanup before the listener admits another checkout.
        let task = tokio::spawn(async move {
            let _active = active;
            let branch = request.branch.clone();
            let checkout = worker_git::Checkout {
                repository: request.repository,
                reference: request.reference,
                branch: request.branch,
                author_name: request.author.name,
                author_email: request.author.email,
                git: worker_git::GitOptions {
                    cancel,
                    global_config: std::env::var_os("GIT_CONFIG_GLOBAL").map(PathBuf::from),
                    ssl_ca: std::env::var_os("GIT_SSL_CAINFO").map(PathBuf::from),
                },
            };
            worker_git::prepare(root, checkout, deadline)
                .await
                .map(|head| Cloned { head, branch })
                .map_err(|failure| Failure {
                    stage: match failure.stage {
                        worker_git::Stage::Dispatch => Stage::Dispatch,
                        worker_git::Stage::Clone => Stage::Clone,
                        worker_git::Stage::Switch => Stage::Switch,
                        worker_git::Stage::Config => Stage::Config,
                        worker_git::Stage::Deadline => Stage::Deadline,
                        worker_git::Stage::Cleanup => Stage::Cleanup,
                    },
                    // Raw git diagnostics can echo credentials and helper output.
                    detail: failure.to_string(),
                })
        });
        Box::pin(async move {
            task.await.unwrap_or_else(|_| {
                Err(Failure {
                    stage: Stage::Cleanup,
                    detail: "the checkout task stopped before cleanup was confirmed".to_owned(),
                })
            })
        })
    }
}
