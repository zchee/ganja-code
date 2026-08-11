<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-05 -->

# ganja-serve

## Purpose

The engine over a socket: REST routes and an SSE event stream over `ganja-core`, so a remote client can drive the same sessions a terminal does. Spec: upstream `packages/opencode/src/server/server.ts` and `server/routes/instance/httpapi/*`, on the legacy `/session/…` path spellings. Its own crate for the same reason the engine carries no terminal dependency: a build that only wants the terminal must never pull an HTTP server, and CI asserts it the same inverted way (`! cargo tree -p ganja-core -e normal | grep -q axum`).

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest: `axum` for the routed REST-plus-SSE shape, `secrecy` for the configured password, `base64` for the Basic credential, `tokio-stream` for the SSE body. No `tower`/`tower-http`: the one middleware this surface needs is an `axum::middleware::from_fn` away. |
| `src/lib.rs` | `serve(Arc<Engine>, ServeConfig) -> Handle`: hostname/port policy (explicit port or fail; none means 4096 then OS-assigned), the startup refusal of a passwordless non-loopback bind, the permission tracker's lossless subscription, graceful shutdown through the `Handle`. |
| `src/routes.rs` | Every route and the guard in front of them: request log (method and path, **never** the query), auth, the served-directory check, and the session-routing policy — a route naming a session that is not current resumes it first, `404`/`409` when it cannot. |
| `src/sse.rs` | `GET /event`: `event: connected` first, engine events as `event: message`, ten-second `event: heartbeat`, and a terminal `event: evicted` frame when the subscriber fell behind. Registration happens before the response body exists. |
| `src/auth.rs` | `GANJA_SERVER_PASSWORD`/`GANJA_SERVER_USERNAME` (upstream's `OPENCODE_`-spelled pair), the `Basic realm="Secure Area"` challenge, the `?auth_token=` escape hatch an `EventSource` needs, and the whole-fold credential compare. |
| `src/error.rs` | The refusal table: `SessionNotFound`→404, `Busy`→409, `HookRefused`→400 (**P13** — nothing went wrong on the server when the operator's own hook refused a prompt; was falling into the `_ => 500` arm), unparseable payload→400, everything else→500, each as `{"type": …, "message": …}`. |
| `src/state.rs` | What handlers share: the engine, the served directory (given and canonical), the read-only storage handle, the config projection, the pending-permission map. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `tests/` | Socket-driving suites, each a self-contained binary on an OS-assigned loopback port: `replay_identity.rs` (one turn, two readers — a direct subscriber and an SSE client hold the same transcript frame for frame), `surface.rs` (the REST pins and the 404/409/400 table over real routes), `permissions.rs` (a dialog listed, answered over HTTP, gone), `posture.rs` (the startup refusal, the directory `400`, the auth trio), `ports.rs` (4096-first-then-fallback on real sockets), `no_secrets_in_logs.rs` (the canary: a configured password reaches no log line, `Debug`, or error body — one test, one binary, global subscriber), `support/` (shared fixtures; a directory module, not a binary). |

## For AI Agents

### Working In This Directory

The three postures are invariants, not defaults to soften:

- **A non-loopback bind with no password is refused at startup.** Upstream warns and serves anyway; this build does not, and the refusal names `GANJA_SERVER_PASSWORD`. Weakening this to a warning is a spec change, not a cleanup.
- **The launch directory is the only directory served.** A request whose `?directory=` or `x-ganja-directory` header names anywhere else is `400`, never silently answered about the wrong worktree — upstream would load an instance per directory; this engine cannot.
- **No query string ever reaches a log line.** The request log writes method and path; `?auth_token=` is a credential in a URL, which is exactly why. The canary suite fails if any log line carries `auth_token`, the password, or its base64.

Two shapes worth knowing before editing:

- **The engine is the truth and the stream is complete.** Handlers translate onto `Command` and off `Event`; nothing here invents transcript state. `GET /permission` is the one derived view, kept by a tracker task on a lossless subscription — it only moves map entries, so it always drains and the turn task never waits on it.
- **One session at a time.** The engine holds one current session; a route naming another resumes it first. `409` while a turn streams is engine law surfacing, not a serve-layer choice.

### Testing Requirements

```sh
cargo nextest run -p ganja-serve                       # everything
cargo nextest run -p ganja-serve -E 'binary(replay_identity)'
cargo test -p ganja-serve --test no_secrets_in_logs   # the canary, its own binary
```

Every socket suite binds `127.0.0.1:0` so parallel runs cannot collide; `ports.rs` is the one that touches 4096 and it tolerates an environment that already holds it. Adding a route means adding its pin in `surface.rs` and, if it takes a payload, its `400` case.

### Common Patterns

Handlers take the body as `Bytes` and parse with `serde_json` directly rather than through axum's `Json` extractor: the refusal table says an unparseable payload is `400`, and the extractor's rejection is a 415/422. Bodies use `#[serde(deny_unknown_fields)]` so a client's typo is a refusal, not a silent drop.

## Dependencies

### Internal

`ganja-core` (`Engine`, `Storage`, `Config`, `EngineError`, the `permission` re-export in tests), `ganja-protocol` (`Command`, `Event`, the ids). Dev: `ganja-testkit` (scripted provider, recorder/blocking tools, drain, storage seeding).

### External

`axum` (routes, middleware, SSE body), `secrecy`, `base64`, `tokio`/`tokio-stream`/`futures`, `serde`/`serde_json`, `thiserror`, `tracing`. Dev: `reqwest` (the suites' client; `stream` reads SSE frames as they arrive), `tempfile`, `tracing-subscriber` (the canary's capture).

<!-- MANUAL: -->
