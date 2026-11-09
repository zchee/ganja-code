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

## The Substrate provisioner

`src/provisioner/substrate/` talks to the installed Agent Substrate release.

- The wire is checked in: a verbatim v1.0.0 `ateapi.proto` plus the Rust that
  `buf.gen.yaml`'s pinned plugins generate from it. Hash and regenerate-and-
  compare tests refuse drift; `buf generate` rewrites, hands never do.
- Separate credential boundaries: the `ate-users/ganja` TokenRequest bearer
  (audience `api.ate-system.svc`) goes only to ateapi; a locally keyed
  PodCertificateRequest certificate for `atenet-client` admits the router
  mTLS CONNECT hop. Neither the control bearer nor the router client key
  enters an Actor. The worker Basic password is deliberately installed by
  its template in the Actor and travels inside the established tunnel.
  Credentials stay out of ganja's own logs, mail and errors under the shipped
  `info` and `info,ganja=debug` filters. Enabling debug for third-party HTTP or
  Kubernetes client targets is unsupported: those targets can print remote
  response bodies containing credentials.
- Settings are `<config home>/cloud/substrate.toml`, strict against
  `schema/ganja-cloud-substrate.schema.json` (one shared fixture corpus keeps
  loader and schema classifying identically). The serve password is
  `GANJA_CLOUD_PASSWORD` only, never a file key.
  Like the main configuration, location-only settings and the selected kubectl
  executable have no extra owner/mode check; the operator owns their permissions.
- Both tunnels sit behind one seam: in-process kube port-forwards, or the
  released `kubectl ate proxy actor` child, with `--kubeconfig=...` and optional
  `--context=...` after the plugin name; readiness is its own stderr line,
  never a probe connection, which would wake a suspended actor.
  The child receives only operator-owned `PATH` and `HOME` plus the explicit
  settings `KUBECONFIG`: credential and kubectl plugins resolve through those
  trusted operator paths, so pinning kubectl does not pin plugin resolution.
  The shared ingress requires a sensitive process-local token and admits only
  issued, unretired actors in the configured atespace. It consumes the token
  before forwarding. The plugin owns its separate unauthenticated socket and
  has no header-authentication hook; it is bound explicitly to `127.0.0.1`,
  targets only its fixed actor, and still requires the worker's Basic auth.
  Other local processes can reach that plugin socket; use in-process mode
  when local users or processes are outside the trust boundary.
- State and policy mutations are reconciled by reads (`GetActor`,
  `GetActorEgressPolicy`), never blindly resent; deletion carries the known
  Actor UID and succeeds only on a confirmed `NOT_FOUND`. Creation ownership
  requires a matching successful create reply: an unknown UID leaves the name
  logged for owner reconciliation and is never adopted from a later read.
  A cancelled policy installation schedules bounded, UID-guarded cleanup
  independently of the cancelled caller.
- Live cases in `tests/substrate_live.rs` are `#[ignore]` and inert without
  `GANJA_LIVE_TEST=1`; with it set, missing `GANJA_SUBSTRATE_CONFIG_HOME`
  (or `GANJA_CLOUD_PASSWORD` for `actor_lifecycle`) fails naming the variable.
  `control_plane_authorized_reads` requires the complete strict settings file
  but never looks up its named template or reads a worker password; it proves
  control reads and trust, not a spawned-actor proxy connection.

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
- Credentials never enter ganja's `Debug`, `Display`, ring rows, mail or own
  tracing fields; the shipped filters keep third-party debug bodies disabled.
- Commands run from the repository root; never create operational directories in
  this member.
