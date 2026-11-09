//! The bounded initialization request, separate from patient engine mutations.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Client, ClientError};

/// Repository and branch prepared before the served engine starts.
#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    /// Repository URL to clone.
    pub repository: String,
    /// Commit or reference checked out before creating the branch.
    pub reference: String,
    /// New working branch.
    pub branch: String,
    /// Identity written to the clone's local git configuration.
    pub author: BootstrapAuthor,
}

/// Git author for the initialized checkout.
#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapAuthor {
    /// Display name.
    pub name: String,
    /// Commit email.
    pub email: String,
}

/// Operation that prevented initialization.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapStage {
    /// Request refused before git ran.
    Dispatch,
    /// Repository clone failed.
    Clone,
    /// Branch checkout failed.
    Switch,
    /// Local author configuration failed.
    Config,
    /// The server's initialization budget expired.
    Deadline,
    /// Partial checkout could not be removed.
    Cleanup,
}

/// Current state of the initialization listener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootstrapStatus {
    /// No initialization has been admitted.
    Idle,
    /// Initialization is still running.
    InProgress,
    /// Checkout is ready for the served engine.
    Cloned {
        /// Checked-out commit.
        head: String,
        /// Working branch.
        branch: String,
    },
    /// Initialization failed.
    Failed {
        /// Failing operation.
        stage: BootstrapStage,
        /// Bounded diagnostic from the initializer.
        detail: String,
    },
    /// The initializer has handed off to the served engine.
    AlreadyServing,
}

impl<'de> Deserialize<'de> for BootstrapStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Struct variants are required even when empty: serde's internally
        // tagged unit variants otherwise discard unknown payload fields.
        #[derive(Deserialize)]
        #[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Idle {},
            InProgress {},
            Cloned { head: String, branch: String },
            Failed { stage: BootstrapStage, detail: String },
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Idle {} => Self::Idle,
            Wire::InProgress {} => Self::InProgress,
            Wire::Cloned { head, branch } => Self::Cloned { head, branch },
            Wire::Failed { stage, detail } => Self::Failed { stage, detail },
        })
    }
}

/// Result of one initialization attempt and its bounded reconciliation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// Checkout is ready.
    Cloned {
        /// Checked-out commit.
        head: String,
        /// Working branch.
        branch: String,
    },
    /// Initialization failed.
    Failed {
        /// Failing operation.
        stage: BootstrapStage,
        /// Bounded diagnostic from the initializer.
        detail: String,
    },
    /// Another request is still initializing.
    InProgress,
    /// The engine is already serving.
    AlreadyServing,
    /// The budget expired while initialization was known to be running.
    TimedOut,
    /// The request may still arrive; it must not be resent.
    AmbiguousStartup,
}

impl BootstrapStatus {
    fn outcome(self) -> Option<BootstrapOutcome> {
        match self {
            Self::Idle => None,
            Self::InProgress => Some(BootstrapOutcome::InProgress),
            Self::Cloned { head, branch } => Some(BootstrapOutcome::Cloned { head, branch }),
            Self::Failed { stage, detail } => Some(BootstrapOutcome::Failed { stage, detail }),
            Self::AlreadyServing => Some(BootstrapOutcome::AlreadyServing),
        }
    }
}

impl Client {
    /// Reads initialization status without starting or repeating a mutation.
    ///
    /// # Errors
    ///
    /// Transport, authentication, refusal and strict JSON-shape errors.
    pub async fn bootstrap_status(&self) -> Result<BootstrapStatus, ClientError> {
        self.bootstrap_request(reqwest::Method::GET, None).await
    }

    /// Sends exactly one initialization POST, then reconciles an ambiguous answer.
    ///
    /// `deadline` bounds the whole operation. The POST read uses the smaller of
    /// that budget and this client's read bound; later reads never resend it.
    /// The server receives its own relative budget, starting on admission.
    ///
    /// # Errors
    ///
    /// A budget outside 5–600 seconds is refused, as are authentication errors,
    /// explicit HTTP refusals and bodies this version cannot understand.
    pub async fn bootstrap(
        &self,
        bootstrap: &Bootstrap,
        deadline: Duration,
    ) -> Result<BootstrapOutcome, ClientError> {
        if !(Duration::from_secs(5)..=Duration::from_secs(600)).contains(&deadline) {
            return Err(ClientError::BootstrapDeadline { given: deadline });
        }
        let end = tokio::time::Instant::now() + deadline;
        let mut body = serde_json::to_value(bootstrap).map_err(|error| ClientError::Skew {
            detail: format!("a bootstrap request does not serialize: {error}"),
        })?;
        body["deadline_secs"] = deadline.as_secs().into();
        let post = tokio::time::timeout(
            deadline.min(self.bounds.read),
            self.bootstrap_request(reqwest::Method::POST, Some(body)),
        )
        .await;
        match post {
            Ok(Ok(status)) => {
                return status.outcome().ok_or_else(|| ClientError::Skew {
                    detail: "bootstrap POST answered idle".to_owned(),
                });
            }
            Ok(Err(ClientError::Transport { .. })) | Err(_) => {}
            Ok(Err(error)) => return Err(error),
        }

        let mut running = false;
        loop {
            match tokio::time::timeout_at(end, self.bootstrap_status()).await {
                Ok(Ok(BootstrapStatus::Idle)) => {}
                Ok(Ok(BootstrapStatus::InProgress)) => running = true,
                Ok(Ok(status)) => {
                    return Ok(status.outcome().expect("terminal bootstrap status has an outcome"));
                }
                Ok(Err(ClientError::Transport { .. })) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => break,
            }
            if tokio::time::timeout_at(end, tokio::time::sleep(Duration::from_millis(100)))
                .await
                .is_err()
            {
                break;
            }
        }
        Ok(if running { BootstrapOutcome::TimedOut } else { BootstrapOutcome::AmbiguousStartup })
    }

    async fn bootstrap_request(
        &self,
        method: reqwest::Method,
        body: Option<serde_json::Value>,
    ) -> Result<BootstrapStatus, ClientError> {
        let mut request = self.request_on(&self.http, method.clone(), "/bootstrap");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|source| self.transport(source))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(BootstrapStatus::AlreadyServing);
        }
        let method = if method == reqwest::Method::GET { "GET" } else { "POST" };
        let response = if response.status() == reqwest::StatusCode::CONFLICT {
            response
        } else {
            self.checked(method, "/bootstrap", response).await?
        };
        let text = self.bounded(method, "/bootstrap", response).await?;
        serde_json::from_str(&text).map_err(|error| ClientError::Skew {
            detail: format!("{method} /bootstrap answered a body this build cannot read: {error}"),
        })
    }
}
