//! Explicit fallback to the released kubectl plugin, never an implicit retry.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
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
    proxies: Mutex<BTreeMap<String, Arc<ProxyEntry>>>,
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
        tracing::warn!("kubectl proxy local ports cannot be token-protected by ganja");
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
        let entry = {
            let mut proxies = self.proxies.lock().await;
            Arc::clone(proxies.entry(target).or_insert_with(|| Arc::new(ProxyEntry::default())))
        };
        // Serialize one actor's startup without blocking routes to other actors.
        let mut proxy = entry.child.lock().await;
        if entry.cancel.is_cancelled() {
            return Err(failure("proxy retired", ProvisionErrorKind::Gone));
        }
        if proxy.as_ref().is_none_or(|proxy| proxy.task.is_finished()) {
            *proxy = Some(tokio::select! {
                biased;
                () = entry.cancel.cancelled() => {
                    return Err(failure("proxy retired", ProvisionErrorKind::Gone));
                },
                result = start_proxy(
                    &self.executable,
                    &self.kubeconfig,
                    self.context.as_deref(),
                    actor,
                    Duration::from_secs(120),
                ) => result?,
            });
        }
        let address = proxy
            .as_ref()
            .ok_or_else(|| failure("proxy startup", ProvisionErrorKind::Transient))?
            .address;
        drop(proxy);
        let stream = TcpStream::connect(address)
            .await
            .map_err(|_| failure("proxy connect", ProvisionErrorKind::Transient))?;
        Ok(Box::new(stream))
    }

    async fn retire(&self, actor: &ObjectRef) {
        let target = format!("{}/{}", actor.atespace, actor.name);
        let Some(entry) = self.proxies.lock().await.remove(&target) else { return };
        entry.cancel.cancel();
        if let Some(mut proxy) = entry.child.lock().await.take() {
            proxy.cancel.cancel();
            if tokio::time::timeout(Duration::from_secs(2), &mut proxy.task).await.is_err() {
                proxy.task.abort();
                let _ = (&mut proxy.task).await;
            }
        }
    }
}

#[derive(Default)]
struct ProxyEntry {
    cancel: CancellationToken,
    child: Mutex<Option<ProxyChild>>,
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

fn proxy_command(
    executable: &std::path::Path,
    kubeconfig: &std::path::Path,
    context: Option<&str>,
    actor: &ObjectRef,
) -> Result<Command, ProvisionError> {
    let path =
        std::env::var_os("PATH").ok_or_else(|| failure("PATH", ProvisionErrorKind::Refused))?;
    let directories =
        std::env::split_paths(&path).filter(|entry| entry.is_absolute()).collect::<Vec<_>>();
    if directories.is_empty() {
        return Err(failure("PATH", ProvisionErrorKind::Refused));
    }
    let path = std::env::join_paths(directories)
        .map_err(|_| failure("PATH", ProvisionErrorKind::Refused))?;
    let home =
        std::env::var_os("HOME").ok_or_else(|| failure("HOME", ProvisionErrorKind::Refused))?;
    let mut command = Command::new(executable);
    // Credential and kubectl plugins resolve through the operator's PATH and
    // HOME. Pinning kubectl does not pin these plugins; that trust is accepted.
    // Absolute PATH entries and a neutral cwd keep the project out of discovery.
    command
        .current_dir("/")
        .env_clear()
        .env("PATH", path)
        .env("HOME", home)
        .env("KUBECONFIG", kubeconfig);
    command.args(["ate", "proxy", "actor", &actor.name, "0:8080"]);
    command.arg(format!("--atespace={}", actor.atespace));
    let mut config_flag = std::ffi::OsString::from("--kubeconfig=");
    config_flag.push(kubeconfig);
    command.arg(config_flag);
    if let Some(context) = context {
        command.arg(format!("--context={context}"));
    }
    command.arg("--address=127.0.0.1");
    Ok(command)
}

async fn start_proxy(
    executable: &std::path::Path,
    kubeconfig: &std::path::Path,
    context: Option<&str>,
    actor: &ObjectRef,
    budget: Duration,
) -> Result<ProxyChild, ProvisionError> {
    let failed = || failure("proxy startup", ProvisionErrorKind::Transient);
    let mut command = proxy_command(executable, kubeconfig, context, actor)?;
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
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
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
                    if tokio::time::timeout(Duration::from_millis(100), child.wait()).await.is_err() {
                        let _ = child.start_kill();
                        let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
                    }
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
    let suffix = format!(" -> {}/{}:8080", actor.atespace, actor.name);
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
