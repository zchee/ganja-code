<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-07 -->

# ganja-provider

## Purpose

Talking to a model vendor: the wires, the credentials they present, and the table that sizes and prices what they serve. A wire turns a `ChatRequest` into HTTP and an HTTP response back into a stream of `ProviderEvent`s. It knows nothing about sessions, tools, snapshots or storage — and with `ganja-core` outside this crate's dependency graph, that is the compiler's rule rather than a convention a reviewer has to keep holding.

**One crate, not three.** Auth and the catalog fold in here rather than standing on their own, and the reason is the direction of the traffic between them. The auth→provider edge is a single function (`provider::reachable_in_the_clear`, consumed by both browser logins' endpoint checks — `auth/openai.rs`'s redirect check and `auth/mcp_oauth.rs`'s two); the provider→auth edge is some forty-odd references reaching per-provider submodule internals — every wire resolves its credential through `auth::Refresher`, and three of them implement `auth::RefreshOauth` against their vendor's token endpoint. The catalog is tangled with auth in the other direction again: it reads a fetched payload's keys through `auth::provider_id_for_storage_key`, because a row filed under upstream's name for a vendor is a turn nothing can price. A boundary drawn between any two of these would carry no invariant anyone would gate, and a boundary nobody would defend is worse than none — it invites the traffic and then fails to describe it.

**What did not move: selection.** `ganja_core::provider::select` reads a `Config`, so it stayed where the config is, over a glob re-export of this crate's `provider` module. Everything here takes plain data instead — an id, a dialect, a base URL, a credential, a header map — which is exactly what made the split possible. `CompatProvider::new`'s doc comment has said so since before there were two crates.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest. Every dependency entry carries the reason it is there. No version is named here; the root manifest owns them all. |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/` | The wires, the credential store and the catalog (see `src/AGENTS.md`) |
| `tests/fixtures/` | Captured `text/event-stream` bodies the wire tests replay. Read by this crate's unit tests and by `ganja-core`'s HTTP integration suites, which reach across for them because a recorded vendor transcript belongs beside the wire that parses it. |

## For AI Agents

### Working In This Directory

- **Nothing here may name `ganja-core`.** CI asserts the internal dependency set is exactly `ganja-permission ganja-protocol ganja-tool` — an allowlist, not a `! grep ganja-core`, so a new internal crate cannot slip in unnoticed. `ganja-permission` is transitive, through `ganja-tool`; the only thing this crate wants of the tools is `ToolDefinition`, which is what a request offers the model.
- **No terminal, no clipboard.** CI asserts `ratatui`, `crossterm` and `arboard` are absent. A login that wants to ask a person something hands the question back to whoever called it — which is why `auth/device.rs` and `auth/loopback.rs` return what they got and store nothing.
- **A credential is resolved per request, never captured at construction.** `CredentialSource` is the seam: a key resolves to itself, an OAuth credential goes through `auth::Refresher::usable`. That is the position upstream's `fetch` override occupies, and it is why a renewal cannot live in the engine — nothing there knows when a token died.
- **Two failure channels, never a completed turn.** A request that never starts streaming fails the call; a body that dies mid-stream ends with `ProviderEvent::Failed`. Retry applies only before the first byte.
- **Secrets never reach a log.** `Presented` is the only type that holds key material and the only place `expose_secret` is called; every `Debug` renders a placeholder, and `Presented::redact` scrubs a credential out of anything a provider echoes back. `crates/ganja-core/tests/secrets_env.rs` pins that with a canary.

### Testing Requirements

```sh
cargo nextest run -p ganja-provider           # unit tests, each in its own process
cargo test -p ganja-provider --doc            # doctests, which nextest does not run
```

Unit tests live in `#[cfg(test)] mod tests` at the bottom of the module they cover. This crate has no `tests/*.rs` binaries: the suites that need a real socket, a real credential store or process-wide environment mutation live in `ganja-core/tests/` and drive these wires through the engine's facade, which is also where the behavior they assert is observable.

### Common Patterns

- Errors are `thiserror` enums whose messages say what the caller can do about it. `ProviderError::is_retryable` is the one classification that is load-bearing rather than cosmetic: it decides what `retry::send` does, and a misclassified refresh failure becomes either a retry storm against an identity provider or a browser login nobody needed.
- Every module doc cites the upstream file it ports, and every deliberate divergence is documented where it happens.

## Dependencies

### Internal

`ganja-protocol` (the types a request and a reply are made of, re-exported as `ganja_provider::protocol`) and `ganja-tool` (`ToolDefinition`, re-exported as `ganja_provider::tool`); `ganja-permission` arrives transitively under the latter. `ganja-core` and `ganja-cli` depend on this crate — core for the wires, the CLI directly for `auth login`'s flows.

### External

`reqwest`/rustls (every wire's HTTP), `tokio` + `tokio-util` (`net` for the loopback listener a browser login comes back to, `rt` for `CancellationToken`), `futures` (stream combinators), `secrecy` (key material wiped on drop), `base64` + `sha2` + `getrandom` (PKCE, and the `state`/`nonce` a callback is answerable by), `etcetera` (the XDG data home `auth.json` lives in and the cache home the catalog does), `serde`/`serde_json`, `regex` (the upstream effort-family pattern), `uuid` (Cursor pairing ids), `httpdate` (`Retry-After`), `percent-encoding` (Copilot quota snapshots), `jiff` (rate-limit reset instants), `thiserror`, `tracing`, `url` (`Host`, for deciding loopback without matching strings), `async-trait` (`Provider` behind `Arc<dyn _>`). Dev: `tempfile`, and `tokio/net` for the tests that serve canned HTTP over a real socket.

<!-- MANUAL: -->
