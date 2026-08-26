//! Logging in to GitHub Copilot, on github.com or on an enterprise
//! deployment.
//!
//! Spec: upstream `packages/opencode/src/plugin/github-copilot/copilot.ts`.
//!
//! Two things about this credential are unlike every other OAuth provider, and
//! both are load-bearing rather than curiosities:
//!
//! 1. **The GitHub OAuth token *is* the Copilot credential.** There is no
//!    second exchange — no `copilot_internal/v2/token` step — anywhere in the
//!    pin. The token the device flow returns is stored as both `refresh` and
//!    `access` (`copilot.ts:296-298`) and is what every request carries
//!    (`:74`, `:164`, which read `refresh`).
//! 2. **`expires: 0` means it never expires** (`copilot.ts:299`), not that it
//!    expired in 1970. There is no refresh endpoint and no refresh code
//!    anywhere in the Copilot plugin, so **this module implements no
//!    [`RefreshOauth`](super::RefreshOauth) at all** — there is nothing for one
//!    to call, and a stub that returned an error would be a worse answer than
//!    its absence, because a caller could reasonably conclude from its
//!    existence that renewal is a thing that happens here. A port that copies
//!    the xAI or Codex shape and writes `if expires < now { refresh() }` will
//!    loop against a credential that has no renewal path; [`super::
//!    OauthCredential::needs_refresh`] already exempts zero, and the test
//!    beneath this module pins that as a property of *this* credential so a
//!    change to the shared helper reddens here.
//!
//! Nothing here writes a credential either: a login hands back an
//! [`OauthCredential`] and the caller stores it.

use super::{
    OauthCredential,
    device::{BodyEncoding, DeviceError, DeviceFlow, Tokens, UPSTREAM_USER_AGENT},
};

/// What this provider is called, on the command line and in the credential
/// file alike — upstream uses the same string (`copilot.ts:61`, `:95`), so
/// there is no alias to map.
pub const PROVIDER_ID: &str = "github-copilot";

/// The OAuth client the login presents itself as (`copilot.ts:9`).
///
/// **Upstream's verbatim, and it has to be**: this is opencode's registered
/// application, and GitHub refuses a client id it has not seen. The
/// `User-Agent` sent beside it is upstream's too, for a related but weaker
/// reason — see
/// [`UPSTREAM_USER_AGENT`](super::device::UPSTREAM_USER_AGENT), which records
/// both why and what it costs.
const CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";

/// The API version every Copilot request declares (`copilot.ts:10`, sent as
/// `X-GitHub-Api-Version` at `:76` and `:363`).
///
/// Public because the lane that puts a bearer on a chat request needs the same
/// value, and two copies of a date are two things to forget to update.
pub const API_VERSION: &str = "2026-06-01";

/// What the login asks for (`copilot.ts:243`). It is a read of the account's
/// identity and nothing more; the Copilot entitlement rides on the account.
const SCOPE: &str = "read:user";

/// The public deployment (`copilot.ts:225`).
pub const DEFAULT_DOMAIN: &str = "github.com";

/// Where a request to the public deployment goes (`copilot.ts:27`).
pub const DEFAULT_API_BASE: &str = "https://api.githubcopilot.com";

/// A hostname with any scheme and trailing slash taken off it.
///
/// Spec: `copilot.ts:15-17`, whose two regexes this is. A person asked for
/// "your GitHub Enterprise URL or domain" (`:207-208`) will type any of
/// `company.ghe.com`, `https://company.ghe.com` or a pasted address with the
/// slash still on the end, and all three name the same deployment.
///
/// One deliberate difference: upstream's `/\/$/` removes a single trailing
/// slash and this removes every one of them, so a pasted `…com//` normalises
/// rather than keeping a slash that would land in the middle of a URL.
#[must_use]
pub fn normalize_domain(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .trim_end_matches('/')
        .to_owned()
}

/// Where requests for `domain` go (`copilot.ts:26-28`).
///
/// Upstream branches on whether an `enterpriseUrl` was stored rather than on
/// the domain string, which comes to the same thing for every input a caller
/// can produce: the public deployment is the only one whose domain is
/// [`DEFAULT_DOMAIN`], and it is the only one with no `enterpriseUrl`. Taking
/// the domain is what lets a caller holding one string get the answer without
/// having to model the deployment first.
#[must_use]
pub fn api_base_for(domain: &str) -> String {
    let domain = normalize_domain(domain);

    if domain == DEFAULT_DOMAIN {
        DEFAULT_API_BASE.to_owned()
    } else {
        format!("https://copilot-api.{domain}")
    }
}

/// Which GitHub this login is against.
///
/// The two branches upstream's `deploymentType` prompt produces
/// (`copilot.ts:186-221`, `:223-230`), as one value instead of a string and a
/// flag that have to be kept agreeing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deployment {
    /// github.com.
    Public,
    /// A GitHub Enterprise deployment, by its normalized domain.
    Enterprise(String),
}

impl Deployment {
    /// An enterprise deployment at `url`, however the person spelled it.
    #[must_use]
    pub fn enterprise(url: &str) -> Self {
        Self::Enterprise(normalize_domain(url))
    }

    /// The host the login endpoints hang off (`copilot.ts:19-24`).
    #[must_use]
    pub fn domain(&self) -> &str {
        match self {
            Self::Public => DEFAULT_DOMAIN,
            Self::Enterprise(domain) => domain,
        }
    }

    /// What gets stored as `enterpriseUrl`, which is nothing for the public
    /// deployment (`copilot.ts:301-303`).
    #[must_use]
    pub fn enterprise_url(&self) -> Option<&str> {
        match self {
            Self::Public => None,
            Self::Enterprise(domain) => Some(domain),
        }
    }
}

/// The device login against `deployment`'s real endpoints.
///
/// # Errors
///
/// Returns [`DeviceError::Unreachable`] when no HTTP client can be built.
pub fn device_flow(deployment: &Deployment) -> Result<DeviceFlow, DeviceError> {
    let domain = deployment.domain();

    device_flow_at(
        format!("https://{domain}/login/device/code"),
        format!("https://{domain}/login/oauth/access_token"),
    )
}

/// The same login against endpoints of the caller's choosing, which is how a
/// test drives it against a loopback socket.
///
/// # Errors
///
/// Returns [`DeviceError::Unreachable`] when no HTTP client can be built.
pub fn device_flow_at(
    device_code_url: impl Into<String>,
    token_url: impl Into<String>,
) -> Result<DeviceFlow, DeviceError> {
    DeviceFlow::new(
        device_code_url,
        token_url,
        CLIENT_ID,
        SCOPE,
        // Upstream's own product name, against upstream's own registered
        // client id — the combination the live spikes measured, and the one
        // host this build deliberately did not move to its own name. See
        // `UPSTREAM_USER_AGENT`, which records what that costs and why this
        // host in particular keeps paying it.
        UPSTREAM_USER_AGENT,
        // GitHub's device endpoints take a **JSON** body
        // (`copilot.ts:236-245`, `:271-275`), which is a divergence from RFC
        // 8628's own examples and from what xAI wants. Sending the form
        // encoding here gets an error that says nothing about why.
        BodyEncoding::Json,
    )
}

/// The credential a completed login stores.
///
/// Spec: `copilot.ts:286-305`. The same token lands in both fields because
/// there is only one token, and `expires` is **zero, meaning never** — see the
/// module documentation for why that is not a typo and why nothing here
/// renews.
#[must_use]
pub fn credential_from(tokens: &Tokens, deployment: &Deployment) -> OauthCredential {
    let mut credential = OauthCredential::new(
        tokens.access.clone(),
        tokens.access.clone(),
        // Not `now_ms() + …`: this credential has no expiry to compute.
        0,
    );
    credential.enterprise_url = deployment.enterprise_url().map(str::to_owned);

    credential
}

#[cfg(test)]
#[path = "copilot_tests.rs"]
mod tests;
