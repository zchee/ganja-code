//! Building an [`AgentRegistry`] from a fixture [`Config`], for suites that
//! need one to construct an engine but are not testing config resolution
//! itself.

use std::sync::Arc;

use ganja_core::{AgentRegistry, Config};

/// The registry `config` resolves, or a panic naming why it could not —
/// every caller here hands over a fixture it wrote itself and expects to
/// resolve.
///
/// ```
/// use ganja_core::Config;
///
/// let registry = ganja_testkit::agent_registry(&Config::default());
/// assert!(registry.get("build").is_some(), "build is a builtin agent");
/// ```
pub fn agent_registry(config: &Config) -> Arc<AgentRegistry> {
    Arc::new(AgentRegistry::from_config(config).expect("the fixture config resolves an agent"))
}
