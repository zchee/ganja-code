//! The engine over a socket: the HTTP routes and the SSE event stream a
//! remote client drives a session through.
//!
//! Spec: upstream packages/opencode/src/server/server.ts
//!
//! Its own crate rather than a module in `ganja-core` for the same reason the
//! engine carries no terminal dependency: a build that only wants the
//! terminal must never pull an HTTP server, and CI asserts it the same
//! inverted way (`! cargo tree -p ganja-core -e normal | grep -q axum`).
