<!-- Parent: ../AGENTS.md -->
# ganja-client

A typed client for `ganja-serve`'s REST routes and SSE event stream. `ganja run --attach <URL>` drives a served engine through it, and `ganja sessions --live` probes every session socket through it. It never contains engine logic and never links `ganja-core` or `ganja-serve`.

## Boundary

- `depgate.toml` `[rules."ganja-client"]`: `internal = ["ganja-protocol"]`, `deny = ["axum*"]`. Linking `ganja-core` would make this a second frontend; linking `ganja-serve` would pull `axum` into every build that only talks to a server. CI runs `cargo depgate check --config depgate.toml`.
- Five external dependencies: `reqwest`, `serde`, `serde_json`, `futures`, `thiserror`; `tokio` is dev-only. Each carries its reason in `Cargo.toml`.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | `Client` (`health`, `create_session`, `sessions`, `prompt`, `events`, `permissions`, `reply_permission`), `Client::new`, `Client::on_socket`, `Credentials`, `ClientError`, `BODY_CAP`, the declared bodies `Health`, `SessionRow`, `PendingPermission`, `Prompt`, and the `Events` stream. |
| `src/sse.rs` | Frame vocabulary: `CONNECTED`, `MESSAGE`, `HEARTBEAT`, `EVICTED`, `FRAMES`, `EvictedNotice`, `Frame`, the `Frames` splitter. |
| `tests/wire.rs` | Every surface against a stub answering real bytes, including malformed ones. |
| `tests/socket.rs` | The Unix-socket form: health with no credential, a dead socket, an oversized answer. |
| `tests/support/` | Hand-rolled loopback HTTP stub on a port or a Unix socket (a directory module, not a binary). |

## Commands

```sh
cargo nextest run -p ganja-client
cargo nextest run -p ganja-cli --test frames   # the frame vocabulary against a real ganja-serve
cargo nextest run -p ganja-cli --test attach   # one turn in-process and attached, compared
cargo depgate check --config depgate.toml
```

The `frames` and `attach` tests live in `ganja-cli/tests/` because `ganja-cli` is the only crate that links both this client and the server.

## Conventions

- Two address forms. `Client::new(address, credentials)` takes an absolute `http` or `https` URL; a bare `host:port` is `ClientError::Address`. `Client::on_socket(path)` binds one client to one session socket, takes no credential, and names the address `uds:<path>` in every error; an empty path or one with a NUL is `ClientError::SocketPath` (`src/lib.rs`).
- Never share a socket-bound `Client` across paths. `reqwest`'s `unix_socket` routes every request of that client through one path, so each socket path gets its own `Client::on_socket` (`src/lib.rs`).
- The frame vocabulary serve writes is declared in `src/sse.rs` as `FRAMES` = `connected`, `message`, `heartbeat`, `evicted`. Changing it on either side needs the pin in `ganja-cli/tests/frames.rs` updated.
- Every shape this crate cannot read (unknown event `type`, undeclared body field, frame outside `FRAMES`) becomes one `ClientError::Skew`, and a stream that hits one ends. Do not add unknown-variant tolerance (pinned by `tests/wire.rs`).
- `PendingPermission` and `Health` are declared whole with `deny_unknown_fields`. `SessionRow` is deliberately partial and must not copy `ganja-core`'s `SessionInfo`.
- Error messages name the address, route or variable that would fix the problem.
- `Credentials` and `Client` implement `Debug` by hand and never render the password (pinned by `no_rendering_of_a_client_or_its_credential_shows_the_password` in `src/lib_tests.rs`).

## Gotchas

- Credentials: this crate reads no environment. `ganja run --attach` builds `Credentials` from `GANJA_SERVER_PASSWORD` and `GANJA_SERVER_USERNAME` (`ganja-cli/src/run.rs`); a `401` is `ClientError::Unauthorized`, whose message names both variables.
- `events()` returns only after the `connected` frame, so a caller subscribes first and prompts second without losing events. A stream that opens with anything else is `Skew`; an `evicted` frame is `ClientError::Evicted`.
- Every body is read under `BODY_CAP` (8 MiB); a longer one is `ClientError::Oversized`, refused unread.
- A socket client times out a connect after 2 s and a stalled read after 30 s (`SOCKET_CONNECT_DEADLINE`, `SOCKET_READ_DEADLINE`).
- Of the four socket routes, this crate declares only `health`. The team and receipt routes are called from the engine side, which may not link this crate.

## Tests

Unit tests live in sibling `*_tests.rs` files through `#[path]`. The integration tests use `tests/support/`, not `ganja-serve`, so `axum` stays out of this crate's graph; no binary needs setup beyond a loopback port or a temp socket path.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-client.md`, frozen from commit 35d1720. New decisions are recorded there, not here.
