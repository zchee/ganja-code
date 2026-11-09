<!-- Parent: ../AGENTS.md -->
# ganja-teammate-cloud

Hosted teammate lifecycle above `ganja-core`. The engine never imports this crate,
and the server must never include a provisioner in its dependency closure.

## Boundary

- `Provisioner` is vendor-neutral. Its `spawn` returns a suspended worker with
  policy installed and endpoint known; the member owns resume and bootstrap.
- Vendor-specific resource names, credentials and routing stay in a provisioner.
- `depgate.toml` permits exactly `ganja-client`, `ganja-core`, and the six crates
  below core; neither `axum*` nor `ratatui*` may enter the production closure.
- `ganja-testkit` holds the fake provisioner. The reverse dependency is dev-only.

## Lifecycle

Every worker request needs a `WorkerGate` permit. Stream and REST have separate
admission doors. Suspend closes both, holds the reconnect loop with an ack,
actively closes and joins the stream, drains REST, and checks that every attempt
received a typed answer from the worker before calling the control plane.

A drain timeout defers suspend. An unclassified routed attempt permanently disables
automatic suspend for that worker identity: a later health answer, including through
another router, cannot fence an earlier dispatch. Only a reconciled deletion and a
distinctly named replacement recover that identity boundary.

Launch and cleanup have finite independent deadlines. A failed or ambiguous
mutation is reconciled, never resent blindly. One terminal decision and one mail
per assistant message id apply across storage reads and the fresh stream.

Owner-reported reverts and contradictory lifecycle observations invalidate the
held cursor before reconnect. An unannounced revert with no such observation is
unsupported; snapshot fields and cursor ordering cannot identify one.

## Environment

`CloudBackend::new` reads `GANJA_CLOUD_PASSWORD` once as the worker template's
Basic-auth password. If it is unset, empty, or not Unicode, cloud spawn is refused;
it does not fall back to `GANJA_SERVER_PASSWORD`. The backend installs this value
as the client's password for the worker endpoint; provisioner routing headers
cannot override that authentication.

The served engine binds all interfaces so the platform router can reach its pod
address; this relies on one tenant per worker sandbox and mandatory Basic auth.
The worker image supplies `GIT_CONFIG_GLOBAL` and the platform projects
`GIT_SSL_CAINFO`; these trusted paths are followed, including projection symlinks.

## Code and tests

- `src/provisioner.rs`: lifecycle types and trait.
- `src/gate.rs`: request admission and completion accounting.
- Unit tests are sibling `*_tests.rs` files; socket/process fixtures live in `tests/`.
- Credentials never enter `Debug`, `Display`, ring rows, traces or mail.
- Commands run from the repository root; never create operational directories in
  this member.
