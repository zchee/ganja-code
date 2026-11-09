//! Kubernetes service forwarding and serving trust for both tunnel modes.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use std::{fmt, io};

use async_trait::async_trait;
use futures::future::BoxFuture;
use hyper_util::rt::TokioIo;
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{ApiResource, DynamicObject, ListParams, Portforwarder};
use kube::{Api, Client, ResourceExt};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tower_service::Service as TowerService;

use super::ateapi_v1::ObjectRef;
use super::certificate::RouterIdentity;
use super::tunnel::{Tunnel, TunnelIo, connect_actor, failure};
use crate::provisioner::{ProvisionError, ProvisionErrorKind};

/// Builds the operator client without ambiguous process-wide crypto selection.
///
/// # Errors
/// Refuses invalid Kubernetes client configuration without retaining its contents.
pub fn client_from_config(config: ::kube::Config) -> Result<Client, ProvisionError> {
    // Other workspace transports may enable aws-lc as well as ring. kube's
    // default builder needs one process choice; preserve any existing choice.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    Client::try_from(config).map_err(|_| failure("Kubernetes client", ProvisionErrorKind::Refused))
}

const SYSTEM_NAMESPACE: &str = "ate-system";
const API_NAME: &str = "api.ate-system.svc";
const ROUTER_NAME: &str = "atenet-router.ate-system.svc";

/// In-process service port-forwards, with separate router mutual TLS.
pub struct KubeTunnel {
    client: Client,
    channel: Channel,
    identity: RouterIdentity,
}

impl fmt::Debug for KubeTunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KubeTunnel(<redacted>)")
    }
}

impl KubeTunnel {
    /// Establishes control serving trust and the allowed router client identity.
    ///
    /// # Errors
    /// Refuses missing trust or router identity; never falls back to plaintext.
    pub async fn connect(client: Client) -> Result<Self, ProvisionError> {
        let trust = serving_trust(&client).await?;
        let channel = control_channel(client.clone(), &trust).await?;
        let identity = RouterIdentity::new(client.clone(), trust).await?;
        Ok(Self { client, channel, identity })
    }
}

#[async_trait]
impl Tunnel for KubeTunnel {
    async fn control(&self) -> Result<Channel, ProvisionError> {
        Ok(self.channel.clone())
    }

    async fn actor(&self, actor: &ObjectRef) -> Result<TunnelIo, ProvisionError> {
        let config = self.identity.config().await?;
        let stream = forward_service(self.client.clone(), "atenet-router", 8444).await?;
        let tls = tokio_rustls::TlsConnector::from(config)
            .connect(
                ServerName::try_from(ROUTER_NAME)
                    .map_err(|_| failure("router name", ProvisionErrorKind::Refused))?,
                stream,
            )
            .await
            .map_err(|_| failure("router TLS", ProvisionErrorKind::Refused))?;
        connect_actor(Box::new(tls), actor).await
    }
}

pub(super) async fn control_channel(
    client: Client,
    trust: &[u8],
) -> Result<Channel, ProvisionError> {
    let endpoint = Endpoint::from_static("https://api.ate-system.svc")
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(120))
        .tls_config(
            ClientTlsConfig::new()
                .domain_name(API_NAME)
                .ca_certificate(Certificate::from_pem(trust)),
        )
        .map_err(|_| failure("control TLS", ProvisionErrorKind::Refused))?;
    Ok(endpoint.connect_with_connector_lazy(KubeConnector(client)))
}

#[derive(Clone)]
struct KubeConnector(Client);

impl TowerService<http::Uri> for KubeConnector {
    type Response = TokioIo<TunnelIo>;
    type Error = ProvisionError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: http::Uri) -> Self::Future {
        let client = self.0.clone();
        Box::pin(async move { forward_service(client, "api", 443).await.map(TokioIo::new) })
    }
}

/// Discover only the requested certificate API, preferring stable over beta.
pub(super) async fn certificate_api(
    client: &Client,
    plural: &str,
    kind: &str,
    namespace: Option<&str>,
) -> Result<(Api<DynamicObject>, ApiResource), ProvisionError> {
    for version in ["v1", "v1beta1"] {
        let resources = match client
            .list_api_group_resources(&format!("certificates.k8s.io/{version}"))
            .await
        {
            Ok(resources) => resources,
            Err(kube::Error::Api(response)) if response.code == 404 => continue,
            Err(_) => return Err(failure("certificate discovery", ProvisionErrorKind::Refused)),
        };
        if !resources.resources.iter().any(|resource| resource.name == plural) {
            continue;
        }
        let resource = ApiResource {
            group: "certificates.k8s.io".to_owned(),
            version: version.to_owned(),
            api_version: format!("certificates.k8s.io/{version}"),
            kind: kind.to_owned(),
            plural: plural.to_owned(),
        };
        let api = match namespace {
            Some(namespace) => Api::namespaced_with(client.clone(), namespace, &resource),
            None => Api::all_with(client.clone(), &resource),
        };
        return Ok((api, resource));
    }
    Err(failure("certificate discovery", ProvisionErrorKind::Refused))
}

pub(super) async fn serving_trust(client: &Client) -> Result<Vec<u8>, ProvisionError> {
    let (api, _) =
        certificate_api(client, "clustertrustbundles", "ClusterTrustBundle", None).await?;
    let bundles = api
        .list(&ListParams::default().labels("podcert.ate.dev/canarying=live"))
        .await
        .map_err(|_| failure("serving trust", ProvisionErrorKind::Refused))?;
    let mut trust = Vec::new();
    for bundle in bundles {
        if bundle.data.pointer("/spec/signerName").and_then(serde_json::Value::as_str)
            != Some("servicedns.podcert.ate.dev/identity")
        {
            continue;
        }
        let pem = bundle
            .data
            .pointer("/spec/trustBundle")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure("serving trust", ProvisionErrorKind::Refused))?;
        trust.extend_from_slice(pem.as_bytes());
        trust.push(b'\n');
    }
    if trust.is_empty() {
        return Err(failure("serving trust", ProvisionErrorKind::Refused));
    }
    Ok(trust)
}

async fn forward_service(
    client: Client,
    name: &str,
    port: i32,
) -> Result<TunnelIo, ProvisionError> {
    let services: Api<Service> = Api::namespaced(client.clone(), SYSTEM_NAMESPACE);
    let service = services
        .get(name)
        .await
        .map_err(|_| failure("service lookup", ProvisionErrorKind::Transient))?;
    let spec =
        service.spec.ok_or_else(|| failure("service lookup", ProvisionErrorKind::Refused))?;
    let selector = spec
        .selector
        .filter(|selector| !selector.is_empty())
        .ok_or_else(|| failure("service selector", ProvisionErrorKind::Refused))?
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    let service_port = spec
        .ports
        .unwrap_or_default()
        .into_iter()
        .find(|entry| entry.port == port)
        .ok_or_else(|| failure("service port", ProvisionErrorKind::Refused))?;
    let pods: Api<Pod> = Api::namespaced(client, SYSTEM_NAMESPACE);
    let pod = pods
        .list(&ListParams::default().labels(&selector))
        .await
        .map_err(|_| failure("pod lookup", ProvisionErrorKind::Transient))?
        .items
        .into_iter()
        .find(|pod| {
            pod.metadata.deletion_timestamp.is_none()
                && pod.status.as_ref().and_then(|status| status.conditions.as_ref()).is_some_and(
                    |conditions| {
                        conditions.iter().any(|condition| {
                            condition.type_ == "Ready" && condition.status == "True"
                        })
                    },
                )
        })
        .ok_or_else(|| failure("ready pod", ProvisionErrorKind::Transient))?;
    let target = match service_port.target_port {
        Some(IntOrString::Int(port)) => port,
        Some(IntOrString::String(name)) => pod
            .spec
            .as_ref()
            .into_iter()
            .flat_map(|spec| &spec.containers)
            .flat_map(|container| container.ports.as_deref().unwrap_or_default())
            .find(|port| port.name.as_deref() == Some(&name))
            .map(|port| port.container_port)
            .ok_or_else(|| failure("pod port", ProvisionErrorKind::Refused))?,
        None => port,
    };
    let port = u16::try_from(target)
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| failure("pod port", ProvisionErrorKind::Refused))?;
    let mut forward = pods
        .portforward(&pod.name_any(), &[port])
        .await
        .map_err(|_| failure("port forward", ProvisionErrorKind::Transient))?;
    let stream = forward
        .take_stream(port)
        .ok_or_else(|| failure("port forward", ProvisionErrorKind::Transient))?;
    Ok(Box::new(ForwardedIo { stream, forward }))
}

struct ForwardedIo {
    stream: DuplexStream,
    forward: Portforwarder,
}

impl Drop for ForwardedIo {
    fn drop(&mut self) {
        self.forward.abort();
    }
}
impl AsyncRead for ForwardedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}
impl AsyncWrite for ForwardedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
