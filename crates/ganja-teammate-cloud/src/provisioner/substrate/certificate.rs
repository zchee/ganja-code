//! Router certificates stay local and are never used as ateapi credentials.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kube::api::{DeleteParams, DynamicObject, ObjectMeta, PostParams};
use kube::{Api, Client, ResourceExt};
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore};
use tokio::sync::Mutex;
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer as _, X509Certificate};

use super::kube::certificate_api;
use super::tunnel::failure;
use crate::provisioner::{ProvisionError, ProvisionErrorKind};

const IDENTITY: &str = "spiffe://cluster.local/ns/ate-system/sa/atenet-client";
const ISSUE_BUDGET: Duration = Duration::from_secs(120);

pub(super) struct RouterIdentity {
    client: Client,
    trust: Vec<u8>,
    current: Mutex<Option<Issued>>,
}

struct Issued {
    config: Arc<ClientConfig>,
    refresh_at: SystemTime,
    expires_at: SystemTime,
}

impl RouterIdentity {
    pub(super) async fn new(client: Client, trust: Vec<u8>) -> Result<Self, ProvisionError> {
        let identity = Self { client, trust, current: Mutex::new(None) };
        identity.config().await?;
        Ok(identity)
    }

    pub(super) async fn config(&self) -> Result<Arc<ClientConfig>, ProvisionError> {
        let mut current = self.current.lock().await;
        let now = SystemTime::now();
        if let Some(issued) =
            current.as_ref().filter(|issued| now < issued.refresh_at && now < issued.expires_at)
        {
            return Ok(Arc::clone(&issued.config));
        }
        let budget = if current.as_ref().is_some_and(|issued| now < issued.expires_at) {
            Duration::from_secs(15)
        } else {
            ISSUE_BUDGET
        };
        match tokio::time::timeout(budget, request(&self.client, &self.trust)).await {
            Ok(Ok(issued)) => {
                let config = Arc::clone(&issued.config);
                *current = Some(issued);
                Ok(config)
            }
            _ => {
                let now = SystemTime::now();
                if let Some(issued) = current.as_mut().filter(|issued| now < issued.expires_at) {
                    // Failed renewal never extends certificate validity.
                    issued.refresh_at = (now + Duration::from_secs(60)).min(issued.expires_at);
                    tracing::warn!(
                        "router certificate renewal failed; cached certificate remains valid"
                    );
                    return Ok(Arc::clone(&issued.config));
                }
                Err(failure("router certificate", ProvisionErrorKind::Refused))
            }
        }
    }
}

async fn request(client: &Client, trust: &[u8]) -> Result<Issued, ProvisionError> {
    let refused = || failure("router certificate", ProvisionErrorKind::Refused);
    let key = KeyPair::generate().map_err(|_| refused())?;
    let mut parameters = CertificateParams::default();
    parameters.distinguished_name = DistinguishedName::new();
    let csr = parameters.serialize_request(&key).map_err(|_| refused())?;
    let (api, resource) = certificate_api(
        client,
        "podcertificaterequests",
        "PodCertificateRequest",
        Some("ate-system"),
    )
    .await?;
    let name = format!("ganja-{}", uuid::Uuid::now_v7());
    let mut object = DynamicObject::new(&name, &resource);
    object.metadata = ObjectMeta {
        name: Some(name.clone()),
        namespace: Some("ate-system".to_owned()),
        ..ObjectMeta::default()
    };
    object.data = serde_json::json!({"spec": {
        "signerName": "podidentity.podcert.ate.dev/identity",
        "podName": "ganja", "podUID": uuid::Uuid::now_v7().to_string(),
        "serviceAccountName": "atenet-client", "serviceAccountUID": uuid::Uuid::now_v7().to_string(),
        "nodeName": "ganja", "nodeUID": uuid::Uuid::now_v7().to_string(),
        "maxExpirationSeconds": 86400,
        "stubPKCS10Request": STANDARD.encode(csr.der())
    }});
    // The name is known before create, so a lost create response is cleaned up too.
    let cleanup = RequestCleanup { api: api.clone(), name };
    let object = api.create(&PostParams::default(), &object).await.map_err(|_| refused())?;
    if object.name_any() != cleanup.name {
        return Err(refused());
    }
    let chain = loop {
        let object = api.get(&cleanup.name).await.map_err(|_| refused())?;
        let status = &object.data["status"];
        if let Some(conditions) = status["conditions"].as_array() {
            if conditions.iter().any(|condition| {
                condition["status"] == "True"
                    && (condition["type"] == "Denied" || condition["type"] == "Failed")
            }) {
                return Err(refused());
            }
            if conditions
                .iter()
                .any(|condition| condition["status"] == "True" && condition["type"] == "Issued")
            {
                break status["certificateChain"].as_str().ok_or_else(refused)?.to_owned();
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    // Drop always schedules deletion, including cancellation during polling.
    drop(cleanup);
    validate(&chain, &key, trust, SystemTime::now())
}

fn validate(
    chain: &str,
    key: &KeyPair,
    trust: &[u8],
    now: SystemTime,
) -> Result<Issued, ProvisionError> {
    let refused = || failure("router certificate", ProvisionErrorKind::Refused);
    let certificates = CertificateDer::pem_slice_iter(chain.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| refused())?;
    let first = certificates.first().ok_or_else(refused)?;
    let (_, certificate) = X509Certificate::from_der(first.as_ref()).map_err(|_| refused())?;
    if certificate.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
        return Err(refused());
    }
    let names =
        certificate.subject_alternative_name().map_err(|_| refused())?.ok_or_else(refused)?;
    if !names
        .value
        .general_names
        .iter()
        .any(|name| matches!(name, GeneralName::URI(uri) if *uri == IDENTITY))
    {
        return Err(refused());
    }
    let starts_at =
        u64::try_from(certificate.validity().not_before.timestamp()).map_err(|_| refused())?;
    let expires_at =
        u64::try_from(certificate.validity().not_after.timestamp()).map_err(|_| refused())?;
    let starts_at = UNIX_EPOCH + Duration::from_secs(starts_at);
    let expires_at = UNIX_EPOCH + Duration::from_secs(expires_at);
    if now < starts_at || now >= expires_at {
        return Err(refused());
    }
    let refresh_at = starts_at + expires_at.duration_since(starts_at).map_err(|_| refused())? / 2;
    let mut roots = RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(trust) {
        roots.add(certificate.map_err(|_| refused())?).map_err(|_| refused())?;
    }
    if roots.is_empty() {
        return Err(refused());
    }
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| refused())?
            .with_root_certificates(roots)
            .with_client_auth_cert(
                certificates,
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .map_err(|_| refused())?;
    Ok(Issued { config: Arc::new(config), refresh_at, expires_at })
}

struct RequestCleanup {
    api: Api<DynamicObject>,
    name: String,
}

impl Drop for RequestCleanup {
    fn drop(&mut self) {
        let api = self.api.clone();
        let name = self.name.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                match tokio::time::timeout(
                    Duration::from_secs(30),
                    api.delete(&name, &DeleteParams::default()),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    _ => tracing::warn!("router certificate request cleanup failed"),
                }
            });
        }
    }
}

#[cfg(test)]
#[path = "certificate_tests.rs"]
mod tests;
