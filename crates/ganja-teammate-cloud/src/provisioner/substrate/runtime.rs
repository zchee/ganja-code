//! Substrate lifecycle operations and their read-after-write reconciliation.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue};
use secrecy::SecretString;
use tonic::transport::Channel;
use tonic::{Code, Status};

use super::ateapi_v1::control_client::ControlClient;
use super::ateapi_v1::{
    Actor, ActorState, ActorTemplate, Atespace, CreateActorEgressPolicyRequest, CreateActorRequest,
    CreateActorTemplateRequest, CredentialHeader, DeleteActorRequest, DeleteOptions,
    DurabilityMode, EgressPolicy, EgressRule, GetActorEgressPolicyRequest, GetActorRequest,
    GetActorTemplateRequest, GetAtespaceRequest, HttpRuleEffects, HttpsRule,
    ListActorTemplatesRequest, ListActorTemplatesResponse, ListOptions, ObjectRef,
    ResourceMetadata, ResumeActorRequest, RevertActorRequest, Selector, SuspendActorRequest,
    SuspendOptions, UpdateActorEgressPolicyRequest,
};
use super::identity::AteapiIdentity;
use super::kube::{KubeTunnel, client_from_config};
use super::kubectl::KubectlTunnel;
use super::settings::{Settings, TunnelMode};
use super::tunnel::{ActorProxy, Tunnel, failure, parse_actor};
use crate::provisioner::{
    Endpoint, ProvisionError, ProvisionErrorKind, Provisioner, Resumed, SuspendOutcome, Worker,
    WorkerHandle, WorkerSpec, WorkerState,
};

/// Provisioner for the released Agent Substrate control plane.
///
/// Router admission and control authorization are deliberately distinct members;
/// the worker endpoint contains neither kind of control-plane credential.
pub struct SubstrateProvisioner {
    settings: Settings,
    control: ControlClient<Channel>,
    identity: AteapiIdentity,
    proxy: ActorProxy,
    username: String,
    password: SecretString,
}

impl fmt::Debug for SubstrateProvisioner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SubstrateProvisioner(<redacted>)")
    }
}

impl SubstrateProvisioner {
    /// Loads operator settings and the template password, without creating an actor.
    ///
    /// # Errors
    /// Refuses invalid settings, missing `GANJA_CLOUD_PASSWORD`, or unavailable identities.
    pub async fn connect(config_home: &Path, username: &str) -> Result<Self, ProvisionError> {
        let settings = Settings::load(config_home)?;
        let password = std::env::var("GANJA_CLOUD_PASSWORD")
            .ok()
            .filter(|value| !value.is_empty())
            .map(SecretString::from)
            .ok_or_else(|| failure("GANJA_CLOUD_PASSWORD", ProvisionErrorKind::Refused))?;
        let kubeconfig = ::kube::config::Kubeconfig::read_from(&settings.kubeconfig)
            .map_err(|_| failure("kubeconfig", ProvisionErrorKind::Refused))?;
        let options = ::kube::config::KubeConfigOptions {
            context: settings.context.clone(),
            ..Default::default()
        };
        let config = ::kube::Config::from_custom_kubeconfig(kubeconfig, &options)
            .await
            .map_err(|_| failure("kubeconfig", ProvisionErrorKind::Refused))?;
        let client = client_from_config(config)?;
        let identity = AteapiIdentity::new(client.clone()).await?;
        let tunnel: Arc<dyn Tunnel> = match settings.tunnel {
            TunnelMode::InProcess => Arc::new(
                KubeTunnel::with_services(client, &settings.api_service, &settings.router_service)
                    .await?,
            ),
            TunnelMode::Kubectl => {
                let executable = settings
                    .kubectl
                    .clone()
                    .ok_or_else(|| failure("kubectl", ProvisionErrorKind::Refused))?;
                Arc::new(
                    KubectlTunnel::connect(
                        client,
                        executable,
                        settings.kubeconfig.clone(),
                        settings.context.clone(),
                        &settings.api_service,
                    )
                    .await?,
                )
            }
        };
        Self::with_tunnel(settings, username.to_owned(), password, tunnel, identity).await
    }

    /// Assembles a provisioner over an explicit tunnel and scoped API identity.
    ///
    /// # Errors
    /// Refuses invalid settings or endpoint credentials, or a failed local bind.
    pub async fn with_tunnel(
        settings: Settings,
        username: String,
        password: SecretString,
        tunnel: Arc<dyn Tunnel>,
        identity: AteapiIdentity,
    ) -> Result<Self, ProvisionError> {
        use secrecy::ExposeSecret as _;
        settings.validate()?;
        if username.is_empty() || password.expose_secret().is_empty() {
            return Err(failure("worker credentials", ProvisionErrorKind::Refused));
        }
        let control = ControlClient::new(tunnel.control().await?);
        let proxy = ActorProxy::bind(tunnel).await?;
        Ok(Self { settings, control, identity, proxy, username, password })
    }

    /// Reads the provisioner's atespace without requiring global-owner permission.
    ///
    /// # Errors
    /// Returns a sanitized control-plane status.
    pub async fn get_atespace(&self) -> Result<Atespace, ProvisionError> {
        let request = GetAtespaceRequest {
            atespace: Some(ObjectRef {
                atespace: String::new(),
                name: self.settings.atespace.clone(),
            }),
        };
        self.control
            .clone()
            .get_atespace(self.identity.authorize(request).await?)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| rpc_error("get atespace", &status, false))
    }

    /// Creates a template in the configured atespace; actor creation remains separate.
    ///
    /// # Errors
    /// Refuses a template for another atespace or an unconfirmed control mutation.
    pub async fn create_actor_template(
        &self,
        template: ActorTemplate,
    ) -> Result<ActorTemplate, ProvisionError> {
        if template
            .metadata
            .as_ref()
            .is_none_or(|metadata| metadata.atespace != self.settings.atespace)
        {
            return Err(failure("create template", ProvisionErrorKind::Refused));
        }
        self.control
            .clone()
            .create_actor_template(
                self.identity
                    .authorize(CreateActorTemplateRequest { actor_template: Some(template) })
                    .await?,
            )
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| rpc_error("create template", &status, true))
    }

    /// Reads the configured template and its golden-snapshot status.
    ///
    /// # Errors
    /// Returns a sanitized unavailable, refused or missing-template result.
    pub async fn get_actor_template(&self) -> Result<ActorTemplate, ProvisionError> {
        self.control
            .clone()
            .get_actor_template(
                self.identity
                    .authorize(GetActorTemplateRequest {
                        actor_template: Some(self.template_ref()),
                    })
                    .await?,
            )
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| rpc_error("get template", &status, false))
    }

    /// Lists one page of templates only in the configured atespace.
    ///
    /// # Errors
    /// Returns a sanitized control-plane status; the caller owns page traversal.
    pub async fn list_actor_templates(
        &self,
        page_token: String,
    ) -> Result<ListActorTemplatesResponse, ProvisionError> {
        let request = ListActorTemplatesRequest {
            atespace: self.settings.atespace.clone(),
            options: Some(ListOptions { page_size: 100, page_token }),
        };
        self.control
            .clone()
            .list_actor_templates(self.identity.authorize(request).await?)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| rpc_error("list templates", &status, false))
    }

    /// Reverts an actor only when the owner explicitly requests recovery.
    ///
    /// # Errors
    /// Returns an unconfirmed mutation rather than retrying a revert.
    pub async fn revert(&self, worker: &Worker) -> Result<(), ProvisionError> {
        let (actor, uid) = self.worker_ref(worker)?;
        self.checked_actor(&actor, &uid).await?;
        let response = self
            .control
            .clone()
            .revert_actor(
                self.identity.authorize(RevertActorRequest { actor: Some(actor.clone()) }).await?,
            )
            .await;
        let confirmed = response
            .ok()
            .and_then(|response| response.into_inner().actor)
            .is_some_and(|value| actor_state(&value) == WorkerState::Suspended);
        if confirmed
            || self
                .checked_actor(&actor, &uid)
                .await
                .is_ok_and(|value| actor_state(&value) == WorkerState::Suspended)
        {
            Ok(())
        } else {
            Err(failure("revert", ProvisionErrorKind::Ambiguous))
        }
    }

    /// Replaces a worker policy using the server's current UID and version guards.
    ///
    /// # Errors
    /// Refuses a missing worker or unconfirmed policy mutation, without retrying it.
    pub async fn update_egress_policy(
        &self,
        worker: &Worker,
        rules: Vec<EgressRule>,
    ) -> Result<(), ProvisionError> {
        let (actor, uid) = self.worker_ref(worker)?;
        self.checked_actor(&actor, &uid).await?;
        let mut policy = self.read_policy(&actor).await?;
        policy.rules = rules;
        self.control
            .clone()
            .update_actor_egress_policy(
                self.identity
                    .authorize(UpdateActorEgressPolicyRequest {
                        actor: Some(actor),
                        egress_policy: Some(policy),
                    })
                    .await?,
            )
            .await
            .map(|_| ())
            .map_err(|status| rpc_error("update policy", &status, true))
    }

    fn template_ref(&self) -> ObjectRef {
        ObjectRef { atespace: self.settings.atespace.clone(), name: self.settings.template.clone() }
    }

    fn worker_ref(&self, worker: &Worker) -> Result<(ObjectRef, String), ProvisionError> {
        let (reference, uid) = worker
            .handle
            .as_str()
            .rsplit_once('/')
            .ok_or_else(|| failure("worker identity", ProvisionErrorKind::Refused))?;
        let actor = parse_actor(reference)?;
        if actor.atespace != self.settings.atespace || uid.is_empty() {
            return Err(failure("worker identity", ProvisionErrorKind::Refused));
        }
        Ok((actor, uid.to_owned()))
    }

    fn worker(&self, actor: &Actor) -> Result<Worker, ProvisionError> {
        let metadata = actor
            .metadata
            .as_ref()
            .filter(|metadata| !metadata.uid.is_empty())
            .ok_or_else(|| failure("created identity", ProvisionErrorKind::Ambiguous))?;
        parse_actor(&format!("{}/{}", metadata.atespace, metadata.name))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "ate-target-actor",
            HeaderValue::try_from(format!("{}/{}", metadata.atespace, metadata.name))
                .map_err(|_| failure("worker route", ProvisionErrorKind::Refused))?,
        );
        Ok(Worker {
            handle: WorkerHandle::new(format!(
                "{}/{}/{}",
                metadata.atespace, metadata.name, metadata.uid
            )),
            endpoint: Endpoint {
                base_url: self.proxy.base_url(),
                headers,
                username: self.username.clone(),
                password: self.password.clone(),
            },
        })
    }

    async fn read_actor(&self, actor: &ObjectRef) -> Result<Actor, ProvisionError> {
        self.control
            .clone()
            .get_actor(
                self.identity.authorize(GetActorRequest { actor: Some(actor.clone()) }).await?,
            )
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| rpc_error("state", &status, false))
    }

    async fn checked_actor(&self, actor: &ObjectRef, uid: &str) -> Result<Actor, ProvisionError> {
        let value = self.read_actor(actor).await?;
        if value.metadata.as_ref().is_none_or(|metadata| metadata.uid != uid) {
            return Err(failure("state", ProvisionErrorKind::Gone));
        }
        Ok(value)
    }

    async fn read_policy(&self, actor: &ObjectRef) -> Result<EgressPolicy, ProvisionError> {
        self.control
            .clone()
            .get_actor_egress_policy(
                self.identity
                    .authorize(GetActorEgressPolicyRequest {
                        actor: Some(actor.clone()),
                        name: "default".to_owned(),
                    })
                    .await?,
            )
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| rpc_error("get policy", &status, false))
    }

    async fn delete_actor(&self, actor: &ObjectRef, uid: &str) -> Result<(), ProvisionError> {
        let request = DeleteActorRequest {
            actor: Some(actor.clone()),
            options: Some(DeleteOptions { uid: uid.to_owned(), version: 0 }),
        };
        let response =
            self.control.clone().delete_actor(self.identity.authorize(request).await?).await;
        match self.read_actor(actor).await {
            Err(error) if error.kind == ProvisionErrorKind::Gone => Ok(()),
            _ => Err(response.err().map_or_else(
                || failure("delete", ProvisionErrorKind::Ambiguous),
                |status| rpc_error("delete", &status, true),
            )),
        }
    }

    fn initial_policy(&self, git_host: &str) -> Result<EgressPolicy, ProvisionError> {
        let url = url::Url::parse(&format!("https://{git_host}"))
            .map_err(|_| failure("git host", ProvisionErrorKind::Refused))?;
        if url.host_str() != Some(git_host)
            || url.port().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(failure("git host", ProvisionErrorKind::Refused));
        }
        Ok(EgressPolicy {
            metadata: Some(ResourceMetadata {
                atespace: self.settings.atespace.clone(),
                name: "default".to_owned(),
                ..Default::default()
            }),
            rules: vec![EgressRule {
                https: Some(HttpsRule {
                    hostnames: vec![git_host.to_owned()],
                    effects: Some(HttpRuleEffects {
                        replace_headers: vec![CredentialHeader {
                            header: "Authorization".to_owned(),
                            prefix: self.settings.git_credential_prefix.clone(),
                            credential_uri: self.settings.git_credential_uri.clone(),
                            actor_jwt: None,
                        }],
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
        })
    }
}

#[async_trait]
impl Provisioner for SubstrateProvisioner {
    async fn spawn(&self, spec: &WorkerSpec) -> Result<Worker, ProvisionError> {
        let policy = self.initial_policy(&spec.git_host)?;
        let template = self.get_actor_template().await?;
        let golden = template
            .status
            .and_then(|status| status.golden_snapshot_status)
            .filter(|status| status.error_message.is_empty())
            .and_then(|status| status.golden_tag)
            .ok_or_else(|| failure("template readiness", ProvisionErrorKind::Refused))?;
        parse_actor(&format!("{}/{}", golden.atespace, golden.name))?;
        let actor_ref = ObjectRef {
            atespace: self.settings.atespace.clone(),
            name: format!("ganja-{}", uuid::Uuid::now_v7()),
        };
        let actor = Actor {
            metadata: Some(ResourceMetadata {
                atespace: actor_ref.atespace.clone(),
                name: actor_ref.name.clone(),
                ..Default::default()
            }),
            actor_template: Some(self.template_ref()),
            source_tag: Some(golden),
            worker_selector: (!self.settings.worker_selector.is_empty()).then(|| Selector {
                match_labels: self.settings.worker_selector.clone().into_iter().collect(),
            }),
            ..Default::default()
        };
        let created = self
            .control
            .clone()
            .create_actor(self.identity.authorize(CreateActorRequest { actor: Some(actor) }).await?)
            .await;
        let mut created = match created {
            Ok(response) => response.into_inner(),
            Err(status) => match self.read_actor(&actor_ref).await {
                Ok(actor) => actor,
                Err(_) => return Err(rpc_error("create", &status, true)),
            },
        };
        if created.metadata.as_ref().is_none_or(|metadata| {
            metadata.atespace != actor_ref.atespace
                || metadata.name != actor_ref.name
                || metadata.uid.is_empty()
        }) {
            created = self
                .read_actor(&actor_ref)
                .await
                .map_err(|_| failure("created identity", ProvisionErrorKind::Ambiguous))?;
        }
        let uid =
            created.metadata.as_ref().map(|metadata| metadata.uid.clone()).unwrap_or_default();
        let worker = match self.worker(&created) {
            Ok(worker) => worker,
            Err(error) => {
                self.delete_actor(&actor_ref, &uid).await.map_err(|_| {
                    failure("spawn cleanup unconfirmed", ProvisionErrorKind::Ambiguous)
                })?;
                return Err(error);
            }
        };
        if actor_state(&created) != WorkerState::Suspended {
            self.delete_actor(&actor_ref, &uid).await?;
            return Err(failure("created state", ProvisionErrorKind::Ambiguous));
        }
        let request = match self
            .identity
            .authorize(CreateActorEgressPolicyRequest {
                actor: Some(actor_ref.clone()),
                egress_policy: Some(policy.clone()),
            })
            .await
        {
            Ok(request) => request,
            Err(error) => {
                self.delete_actor(&actor_ref, &uid).await.map_err(|_| {
                    failure("spawn cleanup unconfirmed", ProvisionErrorKind::Ambiguous)
                })?;
                return Err(error);
            }
        };
        let result = self.control.clone().create_actor_egress_policy(request).await;
        if let Err(status) = result {
            if self.read_policy(&actor_ref).await.is_ok_and(|value| value.rules == policy.rules) {
                return Ok(worker);
            }
            self.delete_actor(&actor_ref, &uid)
                .await
                .map_err(|_| failure("spawn cleanup unconfirmed", ProvisionErrorKind::Ambiguous))?;
            return Err(rpc_error("create policy", &status, true));
        }
        Ok(worker)
    }

    async fn resume(&self, worker: &Worker) -> Result<Resumed, ProvisionError> {
        let (actor, uid) = self.worker_ref(worker)?;
        let before = actor_state(&self.checked_actor(&actor, &uid).await?);
        if before == WorkerState::Crashed {
            return Err(failure("resume", ProvisionErrorKind::Crashed));
        }
        let result = self
            .control
            .clone()
            .resume_actor(
                self.identity.authorize(ResumeActorRequest { actor: Some(actor.clone()) }).await?,
            )
            .await;
        match result {
            Ok(response) => {
                let response = response.into_inner();
                if response.actor.as_ref().is_some_and(|value| {
                    actor_state(value) == WorkerState::Running
                        && value.metadata.as_ref().is_some_and(|metadata| metadata.uid == uid)
                }) || self
                    .checked_actor(&actor, &uid)
                    .await
                    .is_ok_and(|value| actor_state(&value) == WorkerState::Running)
                {
                    Ok(Resumed { was_suspended: response.resumed })
                } else {
                    Err(failure("resume", ProvisionErrorKind::Ambiguous))
                }
            }
            Err(status) => {
                if rpc_error("resume", &status, true).kind == ProvisionErrorKind::Ambiguous
                    && matches!(before, WorkerState::Suspended | WorkerState::Running)
                    && self
                        .checked_actor(&actor, &uid)
                        .await
                        .is_ok_and(|value| actor_state(&value) == WorkerState::Running)
                {
                    Ok(Resumed { was_suspended: before == WorkerState::Suspended })
                } else {
                    Err(rpc_error("resume", &status, true))
                }
            }
        }
    }

    async fn suspend(&self, worker: &Worker) -> Result<SuspendOutcome, ProvisionError> {
        let (actor, uid) = self.worker_ref(worker)?;
        match actor_state(&self.checked_actor(&actor, &uid).await?) {
            WorkerState::Suspended => return Ok(SuspendOutcome::AlreadySuspended),
            WorkerState::Crashed => return Err(failure("suspend", ProvisionErrorKind::Crashed)),
            _ => {}
        }
        let request = SuspendActorRequest {
            actor: Some(actor.clone()),
            options: Some(SuspendOptions { mode: DurabilityMode::Asynchronous.into() }),
        };
        let result =
            self.control.clone().suspend_actor(self.identity.authorize(request).await?).await;
        if self
            .checked_actor(&actor, &uid)
            .await
            .is_ok_and(|value| actor_state(&value) == WorkerState::Suspended)
        {
            return Ok(SuspendOutcome::Suspended);
        }
        Err(result.err().map_or_else(
            || failure("suspend", ProvisionErrorKind::Ambiguous),
            |status| rpc_error("suspend", &status, true),
        ))
    }

    async fn state(&self, worker: &Worker) -> Result<WorkerState, ProvisionError> {
        let (actor, uid) = self.worker_ref(worker)?;
        match self.checked_actor(&actor, &uid).await {
            Ok(actor) => Ok(actor_state(&actor)),
            Err(error) if error.kind == ProvisionErrorKind::Gone => Ok(WorkerState::Gone),
            Err(error) => Err(error),
        }
    }

    async fn kill(&self, worker: &Worker) -> Result<(), ProvisionError> {
        let (actor, uid) = self.worker_ref(worker)?;
        self.delete_actor(&actor, &uid).await
    }

    fn describe(&self, worker: &Worker) -> String {
        self.worker_ref(worker).map_or_else(
            |_| "Substrate Actor (<invalid identity>)".to_owned(),
            |(actor, _)| format!("Substrate Actor {}/{}", actor.atespace, actor.name),
        )
    }
}

fn actor_state(actor: &Actor) -> WorkerState {
    match actor.status.as_ref().and_then(|status| ActorState::try_from(status.state).ok()) {
        Some(ActorState::Running) => WorkerState::Running,
        Some(ActorState::Suspended) => WorkerState::Suspended,
        Some(ActorState::Crashed) => WorkerState::Crashed,
        Some(
            ActorState::Resuming
            | ActorState::Suspending
            | ActorState::Deleting
            | ActorState::Reverting
            | ActorState::Unspecified,
        )
        | None => WorkerState::Transitioning,
    }
}

fn rpc_error(operation: &'static str, status: &Status, mutation: bool) -> ProvisionError {
    let kind = match status.code() {
        Code::NotFound => ProvisionErrorKind::Gone,
        Code::PermissionDenied
        | Code::Unauthenticated
        | Code::InvalidArgument
        | Code::FailedPrecondition
        | Code::AlreadyExists
        | Code::OutOfRange
        | Code::Unimplemented => ProvisionErrorKind::Refused,
        _ if mutation => ProvisionErrorKind::Ambiguous,
        _ => ProvisionErrorKind::Transient,
    };
    failure(operation, kind)
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
