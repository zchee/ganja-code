# Cloud support for ganja-code: the task inventory

Research, not a plan. Written 2026-11-09 from the repository at main `317e511`.

The README describes ganja as a cloud-native coding agent whose subagents,
teammates and parallel workstreams run in the cloud with ganja as their
control plane, and says in the same breath that none of it is implemented
yet (`README.md`, Design). This document lists what that would take, what
already exists to build on, and what has to be decided before the first line
of code. A companion note covers the other half of "cloud": running the
agent loop against a model served from cloud hardware.

Sources: `README.md` (the design and the "not implemented" note); the ideas
index on GitHub, theme #51 (Cloud fleet) with ideas #63, #64 and #65 (verdict
*do*, size XL; #64 and #65 blocked by #63), and theme #52 (Serve, attach and
remote access) with #66 (*do*, M), #67 (*park*), #68 (*drop*) and #101;
`crates/ganja-core/src/teammate.rs` (`TeammateBackend`, `Spawned`, decisions
D538 and D539); `crates/ganja-serve/AGENTS.md`; `crates/ganja-client/AGENTS.md`;
`crates/ganja-team/AGENTS.md`; `crates/ganja-teammate-local/AGENTS.md`;
`docs/decisions/crates.md`; `depgate.toml`. Two standing decisions: the
`-local` in `ganja-teammate-local` reserves the name of a cloud sibling crate,
and reusing Anthropic's Remote Control protocol as the transport was ruled out
on 2026-09-15, so the transport is ganja's own wire.

## What exists that cloud work stands on

- **The teammate backend seam.** A backend is a factory for one surface and
  turns a `SpawnSpec` into a `Spawned`; the registry does not know which kind
  it started (`teammate.rs`, the `TeammateBackend` doc). A cloud backend is one
  more implementor.
- **`ganja serve` and `ganja-client`.** The engine over HTTP and SSE with Basic
  auth, loopback by default and a non-loopback bind only with a password; a
  per-session Unix socket with a peer-uid check; a typed client behind
  `ganja run --attach <URL>`. The serve crate never links the pane backends
  (a `depgate.toml` rule).
- **`ganja-team`.** File-backed mailboxes and the shared task list under one
  local directory, with a mkdir-lock protocol that a real `claude` process
  shares.
- **Isolation today.** The permission engine plus each backend's own sandbox
  floor. Tools run in the engine's process against the local working tree;
  sessions live in a local SQLite database per project.

## Task table

| # | Task | Exists today | Missing | Size | Depends on | Decision needed first |
|---|---|---|---|---|---|---|
| T1 | Decide the hosting model: a hosted control-plane service, or "bring your own" (the user runs `ganja serve` on a cloud VM or container) | Only the second is possible today (serve plus attach) | The README names ganja as the control plane; who hosts it is unstated (#63: "needs parts that do not exist: a control-plane service") | S (decision) | none | Yes: hosting, tenancy, billing |
| T2 | Transport between the terminal and a remote worker | `ganja-serve` REST and SSE, `ganja-client`, Basic auth, a 10 s SSE heartbeat | Reconnect and resume of an SSE stream, auth beyond Basic (tokens, expiry), a TLS story (serve binds plain HTTP), a protocol version | M | T1 | Token format, TLS termination |
| T3 | A cloud teammate backend crate (the sibling the `-local` name reserves) implementing `TeammateBackend` and `Spawned` over T2 | The seam; the local backends as the reference shape | Spawn as "create a remote session"; liveness; retire; the `Delivery` kind; the rules for which environment and credentials travel (the local `CARRIED_ENV` has no remote equivalent) | L | T2, T5 | none beyond T1 |
| T4 | The mailbox and task list off the local filesystem | `ganja-team` is file I/O with mkdir locks, shared with `claude` | A transport-backed postbox (the socket routes `/team/{name}/message` and `/peer/receipt` exist only locally; #66, name registration for served sessions, is the first step of addressing them); keep the file form for local `claude` teammates | L | T2 | Whether cloud teammates keep Claude's teams-directory compatibility |
| T5 | Where the working tree is | Tools run in-process against the lead's cwd; `/undo` snapshots are local | Sync or check out the repository on the worker; results back as patches or commits; the read log and the permission worktree identity across machines | L | T1 | Push/pull model or a shared volume |
| T6 | Isolation by default (#64) | Permission rules plus each backend's sandbox floor; the README promises no reach into credentials, the SSH agent or the home directory | A cloud-side sandbox definition: what is mounted, which environment is carried, the network egress policy; an audit of every tool for host reach | XL | T3, T5 | Blocked by #63 by the ideas index's own note |
| T7 | A scheduler and the fleet (#65) | None; parallelism is bounded by panes and CPU | Queueing, concurrency caps, placement, retries, cost accounting ("a budget question rather than a hardware one") | XL | T3, T1 (billing) | Blocked by #63 |
| T8 | Credentials on the worker | Provider keys are environment variables or `auth.json` on the lead | Short-lived delegated credentials, or a proxy through the control plane so the worker never holds the user's long-lived key (the README's own isolation claim) | M | T1, T2 | Proxy or delegated tokens |
| T9 | Permission dialogs across the wire | The permission engine and the TUI dialogs locally; served sessions answer over REST | A remote tool call that needs consent must reach the cockpit and wait; timeout and `--auto` semantics for remote workers | M | T2, T3 | none |
| T10 | Observability for a fleet | `/usage` counters, live status in the TUI, SSE events | Per-worker status, log retrieval, cost per worker in the cockpit | M | T3, T7 | none |
| T11 | Tests and CI | The pane suites need a real tmux; live tests are gated by environment variables | A fake cloud backend in `ganja-testkit`; an end-to-end suite against a local `ganja serve` standing in for the cloud; CI without real cloud credentials | M | T3 | none |
| T12 | Docs and decisions | The README's design paragraph; ideas #63 to #65 | A decision entry per boundary above, the new crate's `AGENTS.md`, the `depgate.toml` rules (serve must still never link the pane backends; the cloud crate's own allowlist) | S per decision | each task | none |

## The order the evidence suggests

T1 (a decision) → T2 → T3, T4 and T5 in parallel → T6 and T7 once a first
worker runs → T8 and T9 cut across T2 and T3. T11 and T12 ride with every
task.

## Unknown or unverifiable from the repository

- Whether the intent is a hosted service or self-hosted workers (T1). The shape
  of everything else depends on it.
- Whether Claude teams-directory compatibility must hold for cloud teammates
  (T4).
- Billing, tenancy and quotas: described nowhere in the tree.
