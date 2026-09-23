<!-- Parent: ../AGENTS.md -->
# ganja-serve

The engine over HTTP: REST routes and an SSE event stream over `ganja-core`, on TCP and on a per-session Unix socket. It holds no transcript state; handlers translate requests onto engine commands and engine events onto frames. `ganja-cli` links it for `ganja serve` and for binding each session's socket. `ganja-client` is its consumer and must not link it.

## Boundary

- `depgate.toml` `[rules."ganja-serve"]`: `deny = ["ratatui*", "ganja-teammate-local"]`. The `ganja serve` dependency closure never contains the terminal or the pane backends.
- `ganja-core`'s rule denies `axum*`, and so do the `ganja-tui` and `ganja-client` rules; this crate is the only member manifest that names `axum`.
- The socket scheme (directory, names, modes) belongs to `ganja_core::tool::socket`; `src/socket.rs` re-exports it and owns only the binding.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | `serve(Arc<Engine>, ServeConfig) -> Result<Handle, ServeError>`, `Listen`, `Address`, `DEFAULT_HOSTNAME`, `DEFAULT_PORT`, `HEARTBEAT`, the permission tracker. |
| `src/routes.rs` | `tcp_routes`, `socket_routes`, the `guard` middleware (log, credential, directory), every handler. |
| `src/sse.rs` | `GET /event`: the frame pump. |
| `src/auth.rs` | `PASSWORD_ENV`, `USERNAME_ENV`, Basic parsing, the `auth_token` query, the `401` challenge. |
| `src/error.rs` | `ApiError` and the `EngineError` to status mapping. |
| `src/socket.rs` | `NameLock`, candidate-name walk, `0700` directory check, peer-uid check on accept. |
| `src/state.rs` | `AppState`: engine, served directory, storage, config projection, pending permissions, transport. |
| `tests/support/` | Shared fixtures (a directory module, not a binary). |

## Routes

TCP (`tcp_routes`), behind the credential when one is configured:

| Method | Path |
|---|---|
| GET | `/global/health`, `/config`, `/path`, `/agent`, `/command`, `/event`, `/session`, `/session/{id}`, `/session/{id}/message`, `/permission`, `/team` |
| POST | `/session`, `/session/{id}/message`, `/session/{id}/prompt_async`, `/session/{id}/abort`, `/session/{id}/summarize`, `/session/{id}/command`, `/session/{id}/shell`, `/session/{id}/revert`, `/session/{id}/unrevert`, `/session/{id}/agent`, `/session/{id}/model`, `/permission/{id}/reply` |

Unix socket (`socket_routes`), no credential, exactly four: `GET /global/health`, `GET /team`, `POST /team/{name}/message`, `POST /peer/receipt`. Every other path answers `404` on the socket.

The socket takes no password, so it must not serve any route that changes what the session does next. Adding a route here is a deliberate change: document it in the `socket_routes` doc comment and pin it in `tests/team.rs`.

## Commands

```sh
cargo nextest run -p ganja-serve
cargo nextest run -p ganja-serve -E 'binary(replay_identity)'
cargo nextest run -p ganja-serve --test no_secrets_in_logs
cargo nextest run -p ganja-cli --test frames      # frame vocabulary pinned against ganja-client
cargo depgate check --config depgate.toml         # the boundary rules above
```

## Conventions

- A new TCP route gets a pin in `tests/surface.rs`, plus a `400` case if it takes a body.
- Handlers take the body as `Bytes` and parse it with `serde_json` through `parse` in `src/routes.rs`, so a bad payload is `400`, not axum's `415`/`422`. Request bodies use `#[serde(deny_unknown_fields)]`.
- The engine holds one current session. A route naming another session resumes it first; it answers `404` when the session does not exist and `409` while a turn is running (`src/routes.rs`, pinned by `tests/surface.rs`).
- The request log writes method and path only, never the query string, because `?auth_token=` carries the credential (pinned by `tests/no_secrets_in_logs.rs`).

## Gotchas

- Bind rules (`serve` in `src/lib.rs`): hostname is an IP or `localhost`, anything else is `UnknownHostname`. An explicit port is taken or refused; no port tries `DEFAULT_PORT` (4096) and falls back to an OS-assigned port. A non-loopback TCP bind with no credential fails with `UnsecuredNonLoopback`, naming `GANJA_SERVER_PASSWORD`.
- Password: `GANJA_SERVER_PASSWORD`, with `GANJA_SERVER_USERNAME` defaulting to `ganja` (`src/auth.rs`). With a password set, every TCP route answers `401` with `Basic realm="Secure Area"` unless the request carries the Basic header or `?auth_token=`. The socket never asks for the password, even when one is configured.
- `ganja serve` (`ganja-cli/src/serve.rs`, flags `--port`, `--hostname`) warns on stderr when the password is unset.
- Directory rule: a `?directory=` query or `x-ganja-directory` header naming anything but the launch directory is `400` (`wrong_directory` in `src/routes.rs`).
- SSE frames on `GET /event` (`src/sse.rs`): `event: connected` (`{}`) first, then each engine event as `event: message`, `event: heartbeat` (`{}`) every `HEARTBEAT` (10 s), and a final `event: evicted` with `{"type": "evicted", "message": ...}` when the subscriber falls behind. The subscription is registered before the response starts.
- Error mapping (`src/error.rs`), body `{"type", "message"}`: `SessionNotFound` is `404 not_found`; `Busy` is `409 conflict`; `HookRefused`, `MisdirectedCommand`, `TeamSpec`, `ProviderToolReach` and unparseable payloads are `400 invalid_request`; everything else is `500 unknown`.
- `POST /team/{name}/message` accepts only the lead's name. Past the engine's shape checks it answers `200` with identical bytes for accept, refuse and drop; only a hold is announced (pinned by `tests/inbound_admission.rs`). `POST /peer/receipt` answers the same bytes whether or not the id is outstanding.
- Socket files: directory `/tmp/ganja-<uid>/` at `0700` (refused otherwise, `UnsafeSocketDirectory`), socket `0600`, a live name is `SocketInUse`, a stale socket file is unlinked and reused. The `.lock` sibling is never removed; `NameLock::unlink_stale` requires the lock to be held (`src/socket.rs`).
- `GET /permission` is kept by a tracker task on a lossless engine subscription taken before the router exists (`spawn_permission_tracker` in `src/lib.rs`).

## Tests

Unit tests live in sibling `*_tests.rs` files through `#[path]`. One inline exception: the private `unix` module in `src/socket.rs` has its own `mod tests` for the peer-uid refusal.

Every integration binary binds `127.0.0.1:0` or a private temp directory, so they run in parallel; `ports.rs` touches 4096 and tolerates it being taken. `no_secrets_in_logs.rs` installs a global tracing subscriber and holds one test. `uds.rs` is `#![cfg(unix)]`. Each binary's `//!` header states what it covers.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-serve.md`, frozen from commit 35d1720. New decisions are recorded there, not here.
