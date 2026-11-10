//! Hosted startup assembly and its interrupt handoff to the terminal loop.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::future::BoxFuture;
use ganja_core::Backends;
use ganja_core::config::{CloudConfig, Config};
use ganja_core::teammate::TeammateBackend;
use ganja_teammate_local::pane::{PaneShare, PaneShell};

/// Covers the provisioner's 30-second token and 120-second first-certificate
/// budgets without cutting either short; the remainder allows discovery and bind.
pub(crate) const CLOUD_CONNECT_BUDGET: Duration = Duration::from_secs(180);

pub(crate) async fn backends<F>(
    config: &Config,
    is_member: bool,
    home: impl FnOnce() -> Option<PathBuf>,
    budget: Duration,
    connect: impl FnOnce(CloudConfig, PathBuf) -> F,
) -> Result<Backends>
where
    F: Future<Output = Result<Arc<dyn TeammateBackend>>>,
{
    let shell = config.teammates.pane_shell().map(PaneShell::configured).unwrap_or_default();
    let share = config.teammates.pane_share().map(PaneShare::configured).unwrap_or_default();
    let backends = ganja_teammate_local::backends(shell, share);
    if is_member {
        return Ok(backends);
    }
    let Some(cloud) = config.cloud.as_ref() else {
        return Ok(backends);
    };
    if cloud.provisioner != "substrate" {
        anyhow::bail!("unsupported cloud provisioner; use substrate");
    }
    let home = home().ok_or_else(|| {
        anyhow::anyhow!("cloud configuration home not found; check GANJA_CONFIG_HOME and HOME")
    })?;
    let backend = tokio::time::timeout(budget, connect(cloud.clone(), home))
        .await
        .map_err(|_| anyhow::anyhow!("cloud connection timed out; check Substrate connectivity"))?
        .map_err(|_| {
            anyhow::anyhow!("cloud connection failed; check the log and Substrate settings")
        })?;
    Ok(backends.with(backend))
}

pub(crate) type Interrupt = BoxFuture<'static, io::Result<()>>;

pub(crate) async fn during_startup<T>(
    interrupt: &mut Option<Interrupt>,
    work: impl Future<Output = T>,
) -> Result<T> {
    let Some(interrupt) = interrupt.as_mut() else {
        return Ok(work.await);
    };
    tokio::select! {
        biased;
        _ = interrupt => anyhow::bail!("cloud connection interrupted; retry when ready"),
        value = work => Ok(value),
    }
}

#[cfg(test)]
#[path = "cloud_assembly_tests.rs"]
mod tests;
