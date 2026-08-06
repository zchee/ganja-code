//! Talking to a model vendor: the wires, the credentials they present, and the
//! table that sizes and prices what they serve.
//!
//! Its own crate because none of it is the agent loop. A wire turns a
//! [`ChatRequest`](provider::ChatRequest) into HTTP and an HTTP response back
//! into a stream of [`ProviderEvent`](provider::ProviderEvent)s; it knows
//! nothing about sessions, tools, snapshots or storage, and with the engine
//! outside this crate's dependency graph that is the compiler's rule rather
//! than a convention. What is left in `ganja-core` is the half that reads a
//! `Config` — which provider a session runs as, and which model it asks for.
//!
//! **One crate, not three.** Auth and the catalog fold in here rather than
//! standing on their own, and the reason is the direction of the traffic
//! between them. The auth→provider edge is a single function
//! ([`provider::reachable_in_the_clear`], consumed by the OpenAI login's
//! redirect check), while the provider→auth edge is some forty-odd references
//! reaching per-provider submodule internals — every wire resolves its
//! credential through [`auth::Refresher`], and three of them implement
//! [`auth::RefreshOauth`] against their vendor's token endpoint. The catalog
//! is tangled with auth in the other direction again: it names providers by
//! the same ids [`auth::storage_key`] maps to disk. A boundary drawn between
//! any two of these would carry no invariant anyone would gate, and a boundary
//! nobody would defend is worse than no boundary — it invites the traffic and
//! then fails to describe it.
//!
//! Three modules, each documented where it lives:
//!
//! - [`provider`] — the [`Provider`](provider::Provider) trait, the four wires
//!   behind it (Anthropic Messages, OpenAI Responses, the chat-completions
//!   endpoint grok and Copilot ride, and the fake one), the SSE splitter they
//!   share, the retry driver, and the credential seam
//!   ([`Presented`](provider::Presented),
//!   [`CredentialSource`](provider::CredentialSource), and the `Resolved` a
//!   request carries, which stays inside).
//! - [`auth`] — where a credential comes from and how it is kept: the
//!   environment, `auth.json`, and the OAuth logins that fill it.
//! - [`catalog`] — what each model costs and how much it can hold, fetched,
//!   cached, and falling back to a compiled-in snapshot that never fails.
//!
//! The types a request and a reply are made of are [`ganja_protocol`]'s and the
//! definitions a model is offered are [`ganja_tool`]'s; both are re-exported
//! here under the module names the moved code already wrote them as.

/// Replacing a file atomically. Not public: it is how the catalog cache is
/// written, and it is one of three copies of the same thirty lines in this
/// tree — see the module's own note before touching any of them.
pub(crate) mod atomic;
pub mod auth;
pub mod catalog;
pub mod provider;

pub use ganja_protocol as protocol;
pub use ganja_tool as tool;
