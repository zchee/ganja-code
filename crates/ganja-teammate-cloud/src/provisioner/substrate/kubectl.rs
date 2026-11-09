//! Explicit fallback to the released kubectl plugin, never an implicit retry.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use kube::Client;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{ChildStderr, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;

use super::ateapi_v1::ObjectRef;
use super::kube::{control_channel, serving_trust};
use super::tunnel::{Tunnel, TunnelIo, failure, parse_actor};
use crate::provisioner::{ProvisionError, ProvisionErrorKind};

/// Operator-selected command proxy with bounded startup and owned child lifetime.
pub struct KubectlTunnel {
    channel: Channel,
    executable: PathBuf,
    kubeconfig: PathBuf,
    context: Option<String>,
    proxies: Mutex<BTreeMap<String, ProxyChild>>,
}

impl fmt::Debug for KubectlTunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KubectlTunnel(<redacted>)")
    }
}

impl KubectlTunnel {
    /// Connects the control plane; actor proxies start lazily, without a wake probe.
    ///
    /// # Errors
    /// Refuses relative binary/config paths and unverified control serving trust.
    pub async fn connect(
        client: Client,
        executable: PathBuf,
        kubeconfig: PathBuf,
        context: Option<String>,
        api_service: &str,
    ) -> Result<Self, ProvisionError> {
        if !executable.is_absolute() || !kubeconfig.is_absolute() {
            return Err(failure("proxy configuration", ProvisionErrorKind::Refused));
        }
        let trust = serving_trust(&client).await?;
        let channel = control_channel(client, &trust, api_service).await?;
        Ok(Self { channel, executable, kubeconfig, context, proxies: Mutex::new(BTreeMap::new()) })
    }
}

#[async_trait]
impl Tunnel for KubectlTunnel {
    async fn control(&self) -> Result<Channel, ProvisionError> {
        Ok(self.channel.clone())
    }

    async fn actor(&self, actor: &ObjectRef) -> Result<TunnelIo, ProvisionError> {
        let target = format!("{}/{}", actor.atespace, actor.name);
        parse_actor(&target)?;
        let mut proxies = self.proxies.lock().await;
        if proxies.get(&target).is_none_or(|proxy| proxy.task.is_finished()) {
            let proxy = start_proxy(
                &self.executable,
                &self.kubeconfig,
                self.context.as_deref(),
                actor,
                Duration::from_secs(120),
            )
            .await?;
            proxies.insert(target.clone(), proxy);
        }
        let address = proxies
            .get(&target)
            .ok_or_else(|| failure("proxy startup", ProvisionErrorKind::Transient))?
            .address;
        drop(proxies);
        let stream = TcpStream::connect(address)
            .await
            .map_err(|_| failure("proxy connect", ProvisionErrorKind::Transient))?;
        Ok(Box::new(stream))
    }
}

struct ProxyChild {
    address: SocketAddr,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Drop for ProxyChild {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn start_proxy(
    executable: &std::path::Path,
    kubeconfig: &std::path::Path,
    context: Option<&str>,
    actor: &ObjectRef,
    budget: Duration,
) -> Result<ProxyChild, ProvisionError> {
    let failed = || failure("proxy startup", ProvisionErrorKind::Transient);
    let mut command = Command::new(executable);
    command.arg("--kubeconfig").arg(kubeconfig);
    if let Some(context) = context {
        command.arg("--context").arg(context);
    }
    command.args([
        "ate",
        "proxy",
        "actor",
        &actor.name,
        "0:80",
        "-a",
        &actor.atespace,
        "--address",
        "127.0.0.1",
    ]);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| failed())?;
    let stderr = child.stderr.take().ok_or_else(failed)?;
    let mut stderr = BufReader::new(stderr);
    let result = tokio::time::timeout(budget, async {
        tokio::select! {
            result = ready_address(&mut stderr, actor) => result,
            _ = child.wait() => Err(failed()),
        }
    })
    .await;
    let address = match result {
        Ok(Ok(address)) => address,
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(failed());
        }
    };
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let task = tokio::spawn(async move {
        // Drain diagnostics without retaining or forwarding any credential body.
        let drain = async {
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        };
        let monitor = async {
            tokio::select! {
                _ = child.wait() => {},
                () = stop.cancelled() => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
            }
        };
        tokio::join!(drain, monitor);
    });
    Ok(ProxyChild { address, cancel, task })
}

async fn ready_address(
    stderr: &mut BufReader<ChildStderr>,
    actor: &ObjectRef,
) -> Result<SocketAddr, ProvisionError> {
    let failed = || failure("proxy readiness", ProvisionErrorKind::Transient);
    let suffix = format!(" -> {}/{}:80", actor.atespace, actor.name);
    let mut line = Vec::new();
    for _ in 0..65536 {
        let byte = stderr.read_u8().await.map_err(|_| failed())?;
        if byte == b'\n' {
            if let Ok(text) = std::str::from_utf8(&line)
                && let Some(address) =
                    text.strip_prefix("Proxying ").and_then(|text| text.strip_suffix(&suffix))
                && let Ok(address) = address.parse::<SocketAddr>()
                && address.ip() == std::net::Ipv4Addr::LOCALHOST
                && address.port() != 0
            {
                return Ok(address);
            }
            line.clear();
        } else {
            if line.len() >= 8192 {
                return Err(failed());
            }
            line.push(byte);
        }
    }
    Err(failed())
}

#[cfg(test)]
#[path = "kubectl_tests.rs"]
mod tests;
