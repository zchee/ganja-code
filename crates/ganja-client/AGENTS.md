<!-- Parent: ../AGENTS.md -->
# ganja-client

A typed client for `ganja-serve`'s REST routes and SSE event stream. `ganja run --attach <URL>` drives a served engine through it, and `ganja sessions --live` probes every session socket through it. It never contains engine logic and never links `ganja-core` or `ganja-serve`.

## Boundary

- `depgate.toml` `[rules."ganja-client"]`: `internal = ["ganja-protocol"]`, `deny = ["axum*"]`. Linking `ganja-core` would make this a second frontend; linking `ganja-serve` would pull `axum` into every build that only talks to a server. CI runs `cargo depgate check --config depgate.toml`.
- Six external dependencies: `reqwest`, `serde`, `serde_json`, `futures`, `thiserror`, and `tokio` for the event stream's per-read timer (already in the graph under `reqwest`: an edge, not a crate). Each carries its reason in `Cargo.toml`.

## Layout

| Path | Holds |
|---|---|
| `src/lib.rs` | `Client` (`health`, `create_session`, `sessions`, `prompt`, `events`, `permissions`, `reply_permission`), `Client::new`, `Client::with_bounds`, `Client::on_socket`, `Bounds`, `Credentials`, `ClientError`, `BODY_CAP`, `READ_DEADLINE`, `LONGEST_BOUND`, the declared bodies `Health`, `SessionRow`, `PendingPermission`, `Prompt`, and the `Events` stream. |
| `src/sse.rs` | Frame vocabulary: `CONNECTED`, `MESSAGE`, `HEARTBEAT`, `EVICTED`, `FRAMES`, `EvictedNotice`, `Frame`, the `Frames` splitter. |
| `tests/wire.rs` | Every surface against a stub answering real bytes, including malformed and silent ones. |
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

- Two address forms. `Client::new(address, credentials)` takes an absolute `http` or `https` URL; a bare `host:port` is `ClientError::Address`, and so is one carrying a user name or password (`reqwest` would send it as a credential and every error would print it), a query or a fragment (every route is spelled after the address, so none would be reached), or a control character anywhere or a space at either end or before a trailing slash (the parser does not read those as written, so the text routes and errors are spelled from would differ from what it read; a space inside is accepted). A refusal repeats the address only as the URL parser read it, user name, password, query and fragment cleared through `Url`'s setters, and not at all when it does not parse as a URL with a host, because then nothing says which part is the password. `Client::on_socket(path)` binds one client to one session socket, takes no credential, and names the address `uds:<path>` in every error; an empty path or one with a NUL is `ClientError::SocketPath` (`src/lib.rs`).
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
- Bounds. A socket client times out a connect after 2 s and any read silent for 30 s (`SOCKET_CONNECT_DEADLINE`, `READ_DEADLINE`). A TCP client (`Bounds::default`) times out a connect after 10 s and a read `GET` not answered in full within 30 s (a whole-answer bound, so a server trickling bytes cannot hold one), each a `ClientError::Transport`; its `POST` routes have no read bound, because a prompt runs the session's `UserPromptSubmit` hooks before it answers. `Client::with_bounds` refuses a bound of zero or longer than `LONGEST_BOUND` (an hour) as `ClientError::Bound`, and its read bound must stay well above serve's 10 s heartbeat.
- The event stream is opened on the unbounded client and bounded per read by `Events` itself (`ClientError::Silent`, naming `GET /event`), counted from when each read starts: `reqwest`'s read bound counts from the last bytes it delivered, so a caller away longer than the bound (on a slow `POST`, or a blocked stdout) came back to a dead stream. The head and the `connected` frame share one bound, which serve meets at once. The bound is safe only while serve heartbeats well inside it (10 s); `ganja-cli/tests/frames.rs` pins that.
- The bounds catch a silent server, not a stalled engine. `POST /session` and `prompt_async` wait as long as their handler does, and an engine that stops after accepting a prompt leaves serve heartbeating, so a reader waits as long as the server lives.
- Of the four socket routes, this crate declares only `health`. The team and receipt routes are called from the engine side, which may not link this crate.

## Tests

Unit tests live in sibling `*_tests.rs` files through `#[path]`. The integration tests use `tests/support/`, not `ganja-serve`, so `axum` stays out of this crate's graph; no binary needs setup beyond a loopback port or a temp socket path. The bound tests in `tests/wire.rs` use `Client::with_bounds` so none waits out 30 s: a 3 s read bound where the bound must end the wait, and a 5 s one beside a 1 s stub heartbeat where it must not; `Client::new`'s own bounds are pinned in `src/lib_tests.rs`.

## History

Decisions before 2026-09-23 (D-numbers, phase ledgers): `docs/decisions/ganja-client.md`, frozen from commit 35d1720. New decisions go in `docs/decisions/ledger.md`, not here.
