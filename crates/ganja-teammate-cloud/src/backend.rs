//! The backend allocates the worker; its member owns every request and lifecycle
//! transition after allocation. No hosting-vendor type crosses this boundary.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use ganja_core::config::CloudConfig;
use ganja_core::teammate::{
    Delivery, Lent, SpawnSpec, Spawned, TeammateBackend, Unsupported, preamble,
};
use ganja_protocol::team::MemberBackend;
use secrecy::{ExposeSecret as _, SecretString};

use crate::member::{CloudMember, register};
use crate::provisioner::{Author, Bootstrap, Provisioner, WorkerSpec};
use crate::state::Deadlines;

/// The environment variable carrying the worker template's Basic-auth password.
pub const PASSWORD_ENV: &str = "GANJA_CLOUD_PASSWORD";

pub(crate) struct Runtime {
    pub config: CloudConfig,
    pub provisioner: Arc<dyn Provisioner>,
    pub password: Option<SecretString>,
    pub deadlines: Deadlines,
}

/// Allocates hosted teammates without importing a hosting vendor into the engine.
pub struct CloudBackend {
    runtime: Arc<Runtime>,
}

impl fmt::Debug for CloudBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CloudBackend(<redacted configuration>)")
    }
}

impl CloudBackend {
    /// Uses the configured provisioner and reads the template password once.
    /// Missing or empty credentials are refused at spawn rather than falling back.
    pub fn new(config: CloudConfig, provisioner: Arc<dyn Provisioner>) -> Self {
        let password = std::env::var(PASSWORD_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .map(SecretString::from);
        Self::configured(config, provisioner, password, Deadlines::default())
    }

    /// Supplies already-resolved credentials and lifecycle budgets explicitly.
    pub fn configured(
        config: CloudConfig,
        provisioner: Arc<dyn Provisioner>,
        password: Option<SecretString>,
        deadlines: Deadlines,
    ) -> Self {
        Self { runtime: Arc::new(Runtime { config, provisioner, password, deadlines }) }
    }
}

#[async_trait]
impl TeammateBackend for CloudBackend {
    fn backend(&self) -> MemberBackend {
        MemberBackend::Cloud
    }

    async fn spawn(&self, spec: &SpawnSpec, lent: Lent) -> Result<Arc<dyn Spawned>, Unsupported> {
        let refused = |reason: String| Unsupported { backend: MemberBackend::Cloud, reason };
        let password = self
            .runtime
            .password
            .as_ref()
            .filter(|password| !password.expose_secret().is_empty())
            .ok_or_else(|| refused(format!("cloud teammates require {PASSWORD_ENV}")))?;
        let config = &self.runtime.config;
        let repository = url::Url::parse(&config.bootstrap.repository).map_err(|_| {
            refused(
                "cloud.bootstrap.repository must be an HTTPS URL without credentials".to_owned(),
            )
        })?;
        if repository.scheme() != "https"
            || !repository.username().is_empty()
            || repository.password().is_some()
            || repository.query().is_some()
            || repository.fragment().is_some()
            || repository.host_str().is_none_or(|host| !host.eq_ignore_ascii_case(&config.git_host))
        {
            return Err(refused(
                "cloud.bootstrap.repository must use HTTPS on cloud.git_host without credentials"
                    .to_owned(),
            ));
        }
        let bootstrap = Bootstrap {
            repository: config.bootstrap.repository.clone(),
            reference: config.bootstrap.reference.clone(),
            branch: format!(
                "{}/{}-{}",
                config.bootstrap.branch_prefix.trim_end_matches('/'),
                spec.name.as_str(),
                ganja_team::record::now_millis()
            ),
            author: Author {
                name: format!("ganja-cloud-{}", spec.name.as_str()),
                email: config.no_reply_email.clone(),
            },
            deadline_secs: self.runtime.deadlines.bootstrap.as_secs().clamp(5, 600),
        };
        let worker_spec = WorkerSpec {
            name: spec.name.clone(),
            lead: spec.lead.clone(),
            bootstrap: bootstrap.clone(),
            git_host: config.git_host.clone(),
        };
        let mut worker = self
            .runtime
            .provisioner
            .spawn(&worker_spec)
            .await
            .map_err(|error| refused(error.to_string()))?;
        worker.endpoint.username = config.username.clone();
        worker.endpoint.password = password.clone();
        let member =
            CloudMember::new(Arc::clone(&self.runtime), worker, spec.clone(), lent, bootstrap);
        register(&member);
        Ok(member)
    }

    fn preamble(&self, spec: &SpawnSpec) -> String {
        preamble::frame(
            preamble::Names::of(spec),
            "You are working in your own hosted checkout. Each message is one turn; your final response is delivered to the lead automatically. Do not try to address another local mailbox.",
            &spec.prompt,
        )
    }

    fn delivery(&self) -> Delivery {
        Delivery::Acknowledged
    }

    fn surface_line(&self) -> Option<String> {
        Some("hosted checkout; the image's tool rules apply; an unexpected permission request ends the turn".to_owned())
    }
}
