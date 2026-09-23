//! Which tool results a session sends to TypeSafe's host to be screened for
//! text that addresses an AI agent (**D567**) — an experimental feature that is
//! off by default.
//!
//! This module holds [`Screen`], the resolved answer
//! [`Config::evaluate_screen`](crate::Config::evaluate_screen) hands back.
//! Nothing reads a `Screen` yet: the judge that would act on one is not in this
//! build.

use std::collections::BTreeSet;

/// The sources a session screens, resolved from `[evaluate] screen`; see
/// [`EvaluateConfig::screen`](crate::config::EvaluateConfig::screen) for the
/// key and for how the tiers combine it.
///
/// **The default screens nothing**, and so does every config that names no
/// source: screening sends a result's text to a third party, so a session does
/// it only for a source somebody named in a trusted tier.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Screen {
    /// Whether `webfetch` results are screened.
    pub webfetch: bool,
    /// Whether `websearch` results are screened.
    pub websearch: bool,
    /// The MCP servers whose tools' results are screened, each by the name the
    /// config's `mcp` table gives it — one server per name, never a wildcard.
    /// A name may hold colons, which is how a plugin's server is named
    /// (`plugin:<plugin>:<server>`).
    pub mcp: BTreeSet<String>,
}

#[cfg(test)]
#[path = "judge_tests.rs"]
mod tests;
