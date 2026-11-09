//! Provisioner-owned TOML settings; the serve password is environment-only.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::tunnel::{failure, parse_actor};
use crate::provisioner::{ProvisionError, ProvisionErrorKind};

/// How the operator chooses to reach actor ingress.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TunnelMode {
    /// Request router admission and forward connections within this process.
    #[default]
    InProcess,
    /// Use the explicitly configured released kubectl plugin.
    Kubectl,
}

/// Strict contents of `<config home>/cloud/substrate.toml`.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Absolute operator kubeconfig path; credentials are never copied to an actor.
    pub kubeconfig: PathBuf,
    /// Optional context selected from that operator kubeconfig.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Atespace in which the operator granted the ganja identity editor access.
    #[serde(default = "default_atespace")]
    pub atespace: String,
    /// Existing actor template, which must have a ready golden snapshot.
    pub template: String,
    /// Optional pool labels narrowing placement beyond the template's selector.
    #[serde(default)]
    pub worker_selector: BTreeMap<String, String>,
    /// The installed API identity; the namespace remains `ate-users`.
    #[serde(default = "default_atespace")]
    pub service_account: String,
    /// Control-plane Service name in `ate-system`.
    #[serde(default = "default_api")]
    pub api_service: String,
    /// Router Service name in `ate-system`.
    #[serde(default = "default_router")]
    pub router_service: String,
    /// Reference resolved by the gateway's credential provider, not a secret value.
    pub git_credential_uri: String,
    /// Prefix placed before the injected git credential, including its separator.
    #[serde(default)]
    pub git_credential_prefix: String,
    /// Explicit transport choice; connection failures never change this setting.
    #[serde(default)]
    pub tunnel: TunnelMode,
    /// Absolute released kubectl executable; required only for the command tunnel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubectl: Option<PathBuf>,
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Settings(<redacted>)")
    }
}

impl Settings {
    /// Reads only this provisioner's file beneath the configured application home.
    ///
    /// # Errors
    /// Refuses unreadable, malformed or unsupported settings without quoting content.
    pub fn load(config_home: &Path) -> Result<Self, ProvisionError> {
        let text = std::fs::read_to_string(config_home.join("cloud/substrate.toml"))
            .map_err(|_| failure("Substrate configuration file", ProvisionErrorKind::Refused))?;
        Self::parse(&text)
    }

    /// Parses the strict TOML document and validates its runtime constraints.
    ///
    /// # Errors
    /// Refuses unknown fields, invalid names and unsupported credential references.
    pub fn parse(text: &str) -> Result<Self, ProvisionError> {
        let settings: Self = toml::from_str(text)
            .map_err(|_| failure("Substrate configuration", ProvisionErrorKind::Refused))?;
        settings.validate()?;
        Ok(settings)
    }

    /// Validates settings constructed directly as well as settings read from TOML.
    ///
    /// # Errors
    /// Refuses invalid names, paths, selectors, transport choices and references.
    pub fn validate(&self) -> Result<(), ProvisionError> {
        let refused = |field| failure(field, ProvisionErrorKind::Refused);
        if !self.kubeconfig.is_absolute() {
            return Err(refused("kubeconfig"));
        }
        if self.context.as_ref().is_some_and(|context| context.trim().is_empty()) {
            return Err(refused("context"));
        }
        parse_actor(&format!("{}/{}", self.atespace, self.template))?;
        parse_actor(&format!("{}/{}", self.api_service, self.router_service))?;
        if self.service_account != "ganja" {
            return Err(refused("service_account"));
        }
        if self.worker_selector.len() > 10
            || self.worker_selector.iter().any(|(key, value)| {
                key.is_empty()
                    || key.chars().any(char::is_whitespace)
                    || value.chars().any(char::is_whitespace)
            })
        {
            return Err(refused("worker_selector"));
        }
        let uri =
            url::Url::parse(&self.git_credential_uri).map_err(|_| refused("git_credential_uri"))?;
        if !self.git_credential_uri.starts_with("ate-secret://")
            || self.git_credential_uri.chars().any(char::is_whitespace)
            || self.git_credential_uri["ate-secret://".len()..]
                .split('/')
                .next()
                .is_some_and(|authority| authority.contains('@'))
            || uri.scheme() != "ate-secret"
            || uri.host_str().is_none()
            || uri.port().is_some()
            || !uri.username().is_empty()
            || uri.password().is_some()
            || uri.query().is_some()
            || uri.fragment().is_some()
            || uri.path().split('/').skip(1).count() < 2
            || uri.path().split('/').skip(1).any(str::is_empty)
        {
            return Err(refused("git_credential_uri"));
        }
        if !self.git_credential_prefix.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
            return Err(refused("git_credential_prefix"));
        }
        if self.kubectl.as_ref().is_some_and(|path| !path.is_absolute())
            || (self.tunnel == TunnelMode::Kubectl
                && (self.kubectl.is_none() || self.router_service != "atenet-router"))
        {
            return Err(refused("kubectl"));
        }
        Ok(())
    }
}

fn default_atespace() -> String {
    "ganja".to_owned()
}
fn default_api() -> String {
    "api".to_owned()
}
fn default_router() -> String {
    "atenet-router".to_owned()
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
