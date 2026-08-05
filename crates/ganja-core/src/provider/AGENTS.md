<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# provider

## Purpose

Sources of assistant text. A provider turns a `ChatRequest` into a stream of `ProviderEvent`s; the engine maps those onto the protocol frontends see. Six ship, but only four are wires: a fake one for demos and tests, Anthropic Messages, OpenAI-compatible chat completions, and the Responses API a ChatGPT subscription answers on, which really is a second wire. The other two are chat completions under another name — grok is xAI's endpoint and copilot is GitHub's, each a base URL and a credential source over `openai.rs`, with a header set as well for Copilot. Every HTTP provider shares the same shape — build a request, retry it while it has not started answering, split the `text/event-stream` body into frames, map those onto events — so everything except the mapping lives in `mod.rs`.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | `Provider` trait, `ChatRequest`, `ProviderEvent`, `ProviderError`, the shared HTTP client, endpoint checks, `from_env()` provider selection, and the credential seam — `Presented` (a key or an access token, opaque and self-redacting), `Credential` (where one comes from, consulted per request) and `Resolved` (what one request carries: the secret, plus the ChatGPT account id that travels beside it). `openai_provider()` is the one dispatch that reads a *kind* of credential rather than a provider name. |
| `anthropic.rs` | `POST {base}/v1/messages` with `stream: true`, `x-api-key` auth, pinned API version `2023-06-01`. Frames are *named*, so the mapping matches on `Frame::event`. |
| `openai.rs` | `POST {base}/chat/completions` with `stream: true`, bearer auth. The base URL is configurable because the shape is a de-facto standard — the same code drives OpenAI, a local llama.cpp server, OpenRouter, or xAI. Three things a wrapper may declare about itself and nothing else may: `with_credential` (where the secret comes from), `with_base_url`, and `with_headers` — the last empty for every endpoint that asks for nothing but the token, which is all of them except Copilot. |
| `grok.rs` | xAI, via `openai.rs`: `https://api.x.ai/v1` plus an OAuth access token, which is *already* how that provider authenticates. A newtype rather than a flag, because `Provider::id` is what the session layer prices a turn by. `ID` is `auth::grok::PROVIDER_ID` — ganja calls it `grok`, the credential file calls it `xai`, and a second literal would be a login writing where the provider does not read. **A second request/response mapping appearing here means the endpoint stopped being compatible, which is a new provider rather than a bigger version of this one.** |
| `copilot.rs` | GitHub Copilot, via `openai.rs`: `https://api.githubcopilot.com` — or `https://copilot-api.{domain}` for an enterprise login — plus the stored `gho_` GitHub token as the bearer and **four headers** (`x-github-api-version`, `openai-intent: conversation-edits`, `x-initiator: user`, and the one `User-Agent` this build sends). All four were confirmed against the live endpoint *together*, so none of them is a value to drop on a hunch. **There is no token exchange**: the GitHub OAuth token *is* the Copilot credential and the raw token is what the endpoint accepts, so anything that transformed it before presenting it is a regression. The credential never expires, so this is the one OAuth provider with no `RefreshOauth` behind it — `NeverRenews` refuses, because only another login could fix a Copilot credential the endpoint stopped taking. Unlike grok, `from_stored` does read the store, for one thing that is not the token: which deployment the login was against, which decides the endpoint and cannot change without another login. `Provider::id` is upstream's own `github-copilot`, so unlike grok there is no storage alias to look through. |
| `responses.rs` | `POST {base}/responses`, the wire a ChatGPT subscription answers on. The **only** second request/response mapping in this directory, and it earns it: the API carries *items* rather than chat messages (a call and its output are entries of their own, paired by `call_id`), names every frame, and reports one call across three event types. `DEFAULT_BASE_URL` is `https://chatgpt.com/backend-api/codex` — `codex.ts:12`, and `codex.ts:410-418` rewrites every Responses URL to it for an OAuth credential, because a ChatGPT token is minted for that backend and `api.openai.com` refuses it. `Provider::id` is `openai`: same vendor, same catalog rows, different wire. |
| `sse.rs` | Hand-rolled Server-Sent Events frame splitter. `reqwest` hands over whatever the socket produced, which splits fields, lines and multi-byte characters wherever it likes; the decoder buffers until a frame is whole. |
| `retry.rs` | Retry policy for the request that *opens* a turn. |
| `fake.rs` | The provider every demo and end-to-end test runs against: one canned answer unscripted, or a JSON script (`GANJA_FAKE_SCRIPT`) played one entry per model request. |

## For AI Agents

### Working In This Directory

- **Failures have exactly two channels, and neither is a completed turn.** A request that never starts streaming fails the call to `Provider::stream`; one that dies mid-stream yields `ProviderEvent::Failed`, which is terminal — nothing follows it. That variant is what keeps a body that stopped arriving from reading as a model that finished talking.
- **Retry only before the first byte.** Once a provider has started streaming, replaying the request would either duplicate text already rendered or silently restart the model. Failures after that point are reported, not retried.
- **Where a credential can travel is bounded here, not in the individual providers**, because all three bounds are shared:
  - *No redirects.* `reqwest` strips `Authorization` across hosts but knows nothing about Anthropic's `x-api-key`, so a 3xx from a hijacked endpoint would hand the key to whatever it names. These are one-shot `POST`s that never legitimately redirect.
  - *https, or loopback.* The base URL is environment-controlled, and plain HTTP anywhere else puts the key on the wire in the clear. The check compares a `url::Host`, not a string.
  - *`system-proxy` is on deliberately.* `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` redirect provider traffic, which is frequently the only way a corporate network is reachable — but it is a trust boundary, and it is documented as one.
- **The SSE decoder must tolerate anything.** Unknown event types are logged and skipped, never panicked on.
- Adding a provider means adding it to `PROVIDERS`, to `from_env()`, and to the catalog's default-model table **and its snapshot**, or a session naming it has no model to ask for and no cost to report. `catalog.rs`'s own table test fails the build if you do one and not the other. A provider that shares an existing id — `responses.rs` shares `openai`'s — adds none of those, and instead joins the dispatch that reads a credential *kind*.
- **A row with no price can be the honest row.** A subscription seat is billed by the month, so a Copilot row carries zeros and the status bar shows no cost while the token counters — which is what a quota is actually spent against — still show. `catalog.rs`'s table test excuses that by *provider name* rather than by `price == 0.0`, so a row anybody merely forgot to fill in still fails, and a seat row that grew half a price fails too.
- **One vendor can have two wires, and the credential picks.** `openai_provider()` gives a session with an API key the chat-completions provider and a session with only a stored ChatGPT login the Responses one, which is upstream's arrangement too (`plugin/openai/codex.ts:331`, `:356`, both branching on `auth.type !== "oauth"`). The key path is still built by `OpenAiProvider::from_env()`, so nothing about it — including which error a bad base URL produces — is this dispatch's to change.
- **A credential is resolved per request, not captured at construction.** `Credential` is the seam: a key resolves to itself, and an OAuth credential goes through `auth::Refresher::usable` — which reads the store, renews only when the credential is due, and holds concurrent callers to one exchange because xAI's and ChatGPT's refresh tokens both rotate. What comes back is a `Resolved`, not just a secret: a ChatGPT credential also names the account the request is billed to, and that has to come from the same read the token did or a renewal can leave the two disagreeing. That is the position upstream's `fetch` override occupies, and it is why a renewal cannot live in `session.rs`: nothing there knows when a token died.
- **How a failed renewal is classified is load-bearing, not cosmetic.** A refusal (`AuthErrorKind::ReauthRequired`, and the rest of the taxonomy) is `ProviderError::Auth`, which is *not* retryable; a token endpoint that could not be reached (`RefreshUnavailable`) is `ProviderError::Transport`, which *is*. Retry applies exactly where a refresh sits, so folding the first into the second turns one expired grant into a storm against an identity provider, and folding the second into the first sends a dropped network through a browser login it did not need.
- **`expires: 0` means never expires, not expired in 1970.** The question a credential is asked is whether it has a deadline, never whether a zero is in the past; `OauthCredential::needs_refresh` owns that and nothing here re-decides it.

### Testing Requirements

Mapping is proved twice: unit tests here, and a suite serving recorded transcripts over a real loopback socket — `../../tests/http.rs` for the two key-authenticated wires, `../../tests/responses_wire.rs` and `../../tests/copilot_wire.rs` for the two subscription ones. Fixtures live in `../../tests/fixtures/*.sse` and cover the cases that matter — happy path, truncated stream, mid-stream error, malformed frame, interleaved tool calls, a tool call cut short. A new provider or a new frame shape needs proving over a socket as well as in a unit test, whether the transcript lives in a fixture file or beside the suite that serves it.

Live vendor checks are opt-in (`GANJA_LIVE_TEST=1`, `-- --ignored`) and prove only what a socket cannot: that the header names, API version and model identifiers are still current.

### Common Patterns

- Environment variables are read through one helper so an unset variable, an empty variable and a non-UTF-8 variable behave the same everywhere.
- `ProviderError` is transport-agnostic on purpose: the same taxonomy has to fit a provider that never leaves the process and one that speaks HTTP.
- Reasoning deltas are a distinct `ProviderEvent`, not text — providers that report thinking apart keep it apart.

## Dependencies

### Internal

`crate::protocol` (`Message`, `Usage`, `FinishReason`) and `crate::tool::ToolDefinition` (what the model is offered) — both re-exports, of `ganja-protocol` and `ganja-tool`; `crate::auth` (credential lookup) and `crate::catalog` (default models), which are this crate's own.

### External

`reqwest` (rustls, `stream`, `json`, `system-proxy`), `futures`, `secrecy`, `serde_json`, `tokio-util` (`CancellationToken`), `url`, `tracing`.

<!-- MANUAL: -->
