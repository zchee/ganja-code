<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-06 | Updated: 2026-08-18 -->

# ganja-client

## Purpose

The consumer side of `ganja-serve`: a typed client for the served engine's REST routes and its SSE event stream, which is what `ganja run --attach` drives instead of an engine in its own process. Its internal dependency list is **exactly `ganja-protocol`**, asserted in CI as an allowlist — a client that linked `ganja-core` would quietly become a second frontend instead of a consumer of the served one, and one that linked `ganja-serve` would drag `axum` into every build that only wanted to talk to a server.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest, and deliberately six externals: `reqwest` (the routes and the stream's body), `serde`/`serde_json` (every body is a JSON document), `futures` (the event stream has to be nameable to be returned), `thiserror` (the error taxonomy), plus `ganja-protocol`. Dev: `tokio`, for the loopback server the suites answer from. |
| `src/lib.rs` | `Client`: `health`, `create_session`, `sessions`, `prompt`, `events`, `permissions`, `reply_permission`, with `Credentials` (Basic, `Debug` redacted by hand) and `ClientError`. Also the declared bodies — `Health` (since **D505** carrying the served `session_id`, required), `SessionRow` (partial on purpose), `PendingPermission` (whole and closed), `Prompt` — and `Events`, the typed stream. **P25 (D505)** added the second address form: `Client::on_socket(path)` binds one `reqwest` client to one session socket for its whole life — no credential, since the filesystem already said who may connect — shown under §5.6's `uds:` spelling in every error, and refused in words (`ClientError::SocketPath`) for a path that is empty or carries a NUL. Every answer is read under `BODY_CAP` (8 MiB) and a longer one is `ClientError::Oversized`, refused unread: the far end of a socket is another process's word, and `ganja sessions --live` walks every socket in the directory through this. Of the socket's three routes this crate declares `health` alone; the two team routes' one caller is the engine's deliver arm, whose crate may not link this one. |
| `src/sse.rs` | The frame vocabulary serve writes, **declared here**: `connected`, `message`, `heartbeat`, `evicted`, the `EvictedNotice` payload shape, the `Frame` parse and the `Frames` splitter. Pinned against a real server in `ganja-cli/tests/frames.rs`, because a declaration nobody checks is a comment. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `tests/` | `wire.rs` — every surface against a socket that answers real bytes, including the ones no real server would send: an unknown event `type`, an undeclared body field, a frame named outside the vocabulary, a stream that opens mid-conversation. `socket.rs` (**D505**) — the socket form: health crossing a real Unix socket with no credential, a dead socket as a transport error naming the `uds:` path, and an oversized answer refused unread. `support/` is a hand-rolled loopback HTTP stub (a directory module, not a binary) that listens on a port or on a Unix socket, rather than `ganja-serve`, because linking the server into these tests would put `axum` in the graph this crate exists to keep clean. |

## For AI Agents

### Working In This Directory

- **Version skew is unsupported and refused readably.** `Event` is internally tagged with no unknown-variant tolerance, so a server one version ahead sends frames this build cannot name. Every shape this crate cannot read — an unknown event `type`, a body field nobody declared, a frame outside the vocabulary — becomes one `ClientError::Skew` naming the mismatch, and a stream that hits one ends. A client that skipped what it did not recognize would render a transcript missing exactly the parts the two builds disagree about.
- **Do not add a dependency without the reason being load-bearing.** The internal allowlist is a CI gate; the external list is short on purpose and every entry carries its why in the manifest.
- **`events()` returns only after the `connected` frame.** That is the registration guarantee serve publishes: subscribe first, prompt second, and nothing the turn emits can be lost between.
- **What is declared here, and why each shape is the shape it is.** `PendingPermission` is serve's own projection and is declared whole with `deny_unknown_fields`, so the skew posture catches a drift. `SessionRow` is deliberately partial: the listing is `ganja-core`'s `SessionInfo`, a type this crate has no business duplicating.
- **One `Client` per socket path.** `reqwest`'s `unix_socket` routes every request of the client it is set on through that path, so a socket-bound `Client` is bound to that socket for life and is never shared across paths; `Client::on_socket` is where the rule is kept.

### Testing Requirements

```sh
cargo test -p ganja-client                 # the surfaces, against loopback
cargo nextest run -p ganja-cli --test frames   # the frame pin, against a real ganja-serve
cargo nextest run -p ganja-cli --test attach   # one turn, both ways, held against itself
```

The pin and the acceptance both live in `ganja-cli/tests/` because that is the one crate that links this client *and* the server.

### Common Patterns

Errors are written for a person to act on: every one of them names the address, the route, or the variable that would fix it. Nothing renders a password — `Credentials` and `Client` write their own `Debug`, and a named test is the canary.

## Dependencies

### Internal

`ganja-protocol`, and nothing else. CI asserts it.

### External

`reqwest` (rustls, no OpenSSL), `serde`/`serde_json`, `futures`, `thiserror`; `tokio` for tests.
