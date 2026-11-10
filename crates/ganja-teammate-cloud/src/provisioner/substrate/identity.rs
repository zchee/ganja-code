//! Renewable ateapi-only bearer identity, never router admission.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use k8s_openapi::api::authentication::v1::{TokenRequest, TokenRequestSpec};
use k8s_openapi::api::core::v1::ServiceAccount;
use kube::api::PostParams;
use kube::{Api, Client};
use secrecy::{ExposeSecret as _, SecretString};
use tokio::sync::Mutex;
use tonic::metadata::{Ascii, MetadataValue};

use super::tunnel::failure;
use crate::provisioner::{ProvisionError, ProvisionErrorKind};

/// Kubernetes-issued bearer usable only by the Substrate control-plane client.
pub struct AteapiIdentity {
    accounts: Api<ServiceAccount>,
    current: Mutex<Option<Credential>>,
}

struct Credential {
    authorization: SecretString,
    renew_at: SystemTime,
    expires_at: SystemTime,
}

impl fmt::Debug for AteapiIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AteapiIdentity(<redacted>)")
    }
}

impl AteapiIdentity {
    /// Obtains the `ate-users/ganja` identity for `api.ate-system.svc`.
    ///
    /// # Errors
    /// Refuses failed issuance, malformed tokens or already expired credentials.
    pub async fn new(client: Client) -> Result<Self, ProvisionError> {
        let identity =
            Self { accounts: Api::namespaced(client, "ate-users"), current: Mutex::new(None) };
        identity.authorization().await?;
        Ok(identity)
    }

    /// Attaches a freshly checked, redacting bearer header to one control request.
    ///
    /// # Errors
    /// Fails closed on renewal failure, even if an older credential remains cached.
    pub async fn authorize<T>(&self, message: T) -> Result<tonic::Request<T>, ProvisionError> {
        let authorization = self.authorization().await?;
        let mut request = tonic::Request::new(message);
        request.metadata_mut().insert("authorization", authorization);
        Ok(request)
    }

    async fn authorization(&self) -> Result<MetadataValue<Ascii>, ProvisionError> {
        let refused = || failure("ateapi identity", ProvisionErrorKind::Refused);
        let mut current = self.current.lock().await;
        let now = SystemTime::now();
        if current
            .as_ref()
            .is_none_or(|credential| now >= credential.renew_at || now >= credential.expires_at)
        {
            let request = TokenRequest {
                spec: Some(TokenRequestSpec {
                    audiences: Some(vec!["api.ate-system.svc".to_owned()]),
                    expiration_seconds: Some(3600),
                    ..TokenRequestSpec::default()
                }),
                ..TokenRequest::default()
            };
            let response: TokenRequest = tokio::time::timeout(
                Duration::from_secs(30),
                self.accounts.create_subresource(
                    "token",
                    "ganja",
                    &PostParams::default(),
                    &request,
                ),
            )
            .await
            .map_err(|_| refused())?
            .map_err(|_| refused())?;
            let status = response.status.ok_or_else(refused)?;
            let expiry = status.expiration_timestamp.ok_or_else(refused)?.0;
            let seconds = u64::try_from(expiry.as_second()).map_err(|_| refused())?;
            let expires_at =
                UNIX_EPOCH.checked_add(Duration::from_secs(seconds)).ok_or_else(refused)?;
            let now = SystemTime::now();
            let remaining = expires_at.duration_since(now).map_err(|_| refused())?;
            if remaining.is_zero() {
                return Err(refused());
            }
            let token = SecretString::from(
                status.token.filter(|token| !token.is_empty()).ok_or_else(refused)?,
            );
            let authorization = SecretString::from(format!("Bearer {}", token.expose_secret()));
            // Build once in wiped storage; requests copy only into sensitive metadata.
            let mut header =
                MetadataValue::try_from(authorization.expose_secret()).map_err(|_| refused())?;
            header.set_sensitive(true);
            *current =
                Some(Credential { authorization, renew_at: now + remaining / 2, expires_at });
            return Ok(header);
        }
        let credential = current.as_ref().ok_or_else(refused)?;
        let mut header = MetadataValue::try_from(credential.authorization.expose_secret())
            .map_err(|_| refused())?;
        header.set_sensitive(true);
        Ok(header)
    }
}
