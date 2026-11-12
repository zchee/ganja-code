<!-- Parent: ../AGENTS.md -->
# ganja-serve

The engine over HTTP: REST routes and an SSE event stream over `ganja-core`, on TCP and on a per-session Unix socket. It holds no transcript state; handlers translate requests onto engine commands and engine events onto frames. `ganja-cli` links it for `ganja serve` and for binding each session's socket. `ganja-client` is its consumer and must not link it.

## Boundary

- `depgate.toml` `[rules."ganja-serve"]`: `deny = ["ratatui*", "ganja-teammate-local", "ganja-teammate-cloud"]`. The `ganja serve` dependency closure never contains the terminal, pane backends or cloud provisioners.
- `ganja-core`'s rule denies `axum*`, and so do the `ganja-tui` and `ganja-client` rules; this crate is the only member manifest that names `axum`.
- The socket scheme (directory, names, modes) belongs to `ganja_core::tool::socket`; `src/socket.rs` re-exports it and owns only the binding.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | `serve(Arc<Engine>, ServeConfig)` binds a listener; `serve_on_tcp` consumes an already-bound Tokio TCP listener. Both return `Result<Handle, ServeError>` and share journal, routing and startup. Also `Listen`, `Address`, `DEFAULT_HOSTNAME`, `DEFAULT_PORT`, `HEARTBEAT`. |
| `src/journal.rs` | Caller-owned `EventJournal`: bounded replay and the ordered permission projection. |
| `src/bootstrap.rs` | Pre-engine init router, strict bootstrap bodies, one-operation admission and drained handoff. |
| `src/routes.rs` | `tcp_routes`, `socket_routes`, the `guard` middleware (log, credential, directory), every handler. |
| `src/sse.rs` | `GET /event`: the frame pump. |
| `src/auth.rs` | `PASSWORD_ENV`, `USERNAME_ENV`, Basic parsing, the `auth_token` query, the `401` challenge. |
| `src/error.rs` | `ApiError` and the `EngineError` to status mapping. |
| `src/socket.rs` | `NameLock`, candidate-name walk, `0700` directory check, peer-uid check on accept. |
| `src/state.rs` | `AppState`: engine, served directory, storage, config projection, pending permissions, transport. |
| `tests/support/` | Shared fixtures (a directory module, not a binary). |

## Routes

TCP (`tcp_routes`), behind the credential when one is configured, except exactly `GET /readyz` (empty 200; nonempty body is 400):

| Method | Path |
|---|---|
| GET | `/readyz`, `/global/health`, `/config`, `/path`, `/agent`, `/command`, `/event`, `/session`, `/session/{id}`, `/session/{id}/message`, `/permission`, `/team` |
| POST | `/session`, `/session/{id}/message`, `/session/{id}/prompt_async`, `/session/{id}/abort`, `/session/{id}/summarize`, `/session/{id}/command`, `/session/{id}/shell`, `/session/{id}/revert`, `/session/{id}/unrevert`, `/session/{id}/agent`, `/session/{id}/model`, `/permission/{id}/reply` |

Every TCP response, including refusals and SSE headers, carries `ganja-serve-epoch: <UUIDv7>`. It is the journal epoch, identical to the epoch in `connected.head`; the pre-engine init listener has its own epoch. A proxy-generated response has no such provenance header.

Before an engine exists, `bootstrap::Service` serves `GET /readyz` and credential-gated `GET /bootstrap` / `POST /bootstrap`. Bodies are strict, with `deadline_secs` in 5..=600. The executor owns git and cleanup under the admission-based deadline; a disconnected POST keeps running and a concurrent POST gets 409 `in_progress`. At the deadline the router answers `failed` / `deadline` with `cleanup_in_progress: true`, retaining admission until the executor returns. Cleanup completion clears that flag; a late success is terminal `cleanup`, never a handoff. `Handle::handoff` drains the success response before returning the checkout for exec. Engine serve has no `/bootstrap` route. Failed POST and status responses carry a fixed detail sentence, never executor output; executor diagnostics are logged only after redacting the serve credential, request values, and both encoded and decoded repository userinfo.

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
- An already-bound TCP listener requires a TCP-classified `ServeConfig`; `serve_on_tcp` checks the password requirement against its actual local address, not the config's claimed hostname. It never rebinds. The worker CLI retains init's socket across exec: requests accepted by init finish with init's epoch; connections queued after init stops accepting are answered by serve with serve's epoch. The successful-handoff conditions and limits are in [the CLI instructions](../ganja-cli/AGENTS.md); TCP reachability during assembly is not HTTP readiness and does not remove the launch-time health exception.
- Password: `GANJA_SERVER_PASSWORD`, with `GANJA_SERVER_USERNAME` defaulting to `ganja` (`src/auth.rs`). With a password set, every TCP route except `GET /readyz` answers `401` with `Basic realm="Secure Area"` unless the request carries the Basic header or `?auth_token=`. The socket never asks for the password, even when one is configured.
- `ganja serve` (`ganja-cli/src/serve.rs`, flags `--port`, `--hostname`) warns on stderr when the password is unset.
- Directory rule: a `?directory=` query or `x-ganja-directory` header naming anything but the launch directory is `400` (`wrong_directory` in `src/routes.rs`). On the unauthenticated readiness route the refusal body is empty, so it never discloses the launch directory.
- SSE frames on `GET /event` (`src/sse.rs`) all carry `id: <epoch>-<seq>`; resume cursors use the 36-byte UUID prefix followed by a decimal `u64` sequence. `connected` carries `{head, replay}`: `fresh` starts at head, `from` replays strictly after `Last-Event-ID`, `gap` declares an unknown epoch or evicted/future cursor and streams after head. Control ids never advance: fresh connected uses head; from/gap connected use the requested cursor; heartbeat and terminal evicted repeat the last sent id. The caller supplies `ServeConfig.journal`; reusing it preserves replay across listener restarts. Default retention is 8192 events / 16 MiB of serialized JSON; either bound evicts the oldest entries. A slow reader falling outside retention gets `evicted`, never silent loss.
- Error mapping (`src/error.rs`), body `{"type", "message"}`: `SessionNotFound` is `404 not_found`; `Busy` is `409 conflict`; `HookRefused`, `MisdirectedCommand`, `TeamSpec`, `ProviderToolReach` and unparseable payloads are `400 invalid_request`; everything else is `500 unknown`.
- `POST /team/{name}/message` accepts only the lead's name. Past the engine's shape checks it answers `200` with identical bytes for accept, refuse and drop; only a hold is announced (pinned by `tests/inbound_admission.rs`). `POST /peer/receipt` answers the same bytes whether or not the id is outstanding.
- Socket files: directory `/tmp/ganja-<uid>/` at `0700` (refused otherwise, `UnsafeSocketDirectory`), socket `0600`, a live name is `SocketInUse`, a stale socket file is unlinked and reused. The `.lock` sibling is never removed; `NameLock::unlink_stale` requires the lock to be held (`src/socket.rs`).
- `GET /permission` is projected by the journal's single ordered lossless consumer before advancing head, under the same lock as fresh stream registration. There is no separate permission tracker. The stored transcript remains debounced and is not synchronized to that head.

## Tests

Unit tests live in sibling `*_tests.rs` files through `#[path]`. One inline exception: the private `unix` module in `src/socket.rs` has its own `mod tests` for the peer-uid refusal.

Every integration binary binds `127.0.0.1:0` or a private temp directory, so they run in parallel; `ports.rs` touches 4096 and tolerates it being taken. `no_secrets_in_logs.rs` installs a global tracing subscriber and holds one test. `uds.rs` is `#![cfg(unix)]`. Each binary's `//!` header states what it covers.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-serve.md`, frozen from commit 35d1720. New decisions go in `docs/decisions/ledger.md`, not here.
