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
another router, cannot fence an earlier dispatch. This is an independent gate latch,
not a lifecycle phase. Only a reconciled deletion and a distinctly named
replacement recover that identity boundary.

Launch and cleanup have finite independent deadlines; ambiguous mutations are reconciled,
never resent blindly. Terminal decisions are deduplicated per assistant message id;
mail is best effort. Four native filesystem workers plus one supervisor serve two lane
identities per member: inbound own-inbox reads/prunes and outbound lead-inbox writes.
FIFO applies within each direction only, one executing job per lane; a late head cannot
be overtaken there. Limits: eight outstanding jobs per lane (eight plus eight per member),
64 globally including running/unconsumed finals, 1 MiB write/prune payloads, 1 s queued expiry.
Read/prune observation and standalone event interruption use 250 ms budgets; cancellation
interrupts observation, not a running syscall. After cleanup, Shutdown submits inbound
prune then outbound acknowledgement, with no execution order between those directions.
Both lanes seal, inbound then outbound, before the final-observation early return;
receipts from both share 250 ms total outside the transition lock.
Delivered/Failed mean success/error within observation; PendingQueued means observation
ended before execution. Work executing after observation is late (LateSucceeded/LateFailed);
Dropped means no execution, including queued expiry. Late writes can land after return.
Two members with both lanes stuck exhaust all workers and stop hosted mailbox delivery
for every member; external Retire stays bounded. Default launch/quiesce/cleanup: 420/60/60 s;
bootstrap/health 180/90 s are inside launch. Preceding-holder formulas: `CloudControl::admit`,
`docs/decisions/ledger.md`. These await budgets assume scheduler progress and returning
synchronous calls, not arbitrary-kernel-stall exit guarantees. Native payload work retains
no member/registry; delivery, runner return, ordinary Tokio shutdown and Gone are separate.

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
  explicitly selected `tunnel = "kubectl"` running the released `kubectl ate proxy actor`
  child. The latter requires an absolute executable path and `router_service = "atenet-router"`;
  it is not an automatic fallback. `--kubeconfig=...` and optional `--context=...`
  follow the plugin name; startup has a 120-second budget and bounded child reaping.
  Readiness is its own stderr line, never a probe connection, which would wake a suspended actor.
  The child receives only absolute entries from operator-owned `PATH`, `HOME`,
  and the explicit settings `KUBECONFIG`, with `/` as its neutral working
  directory; credential and kubectl plugins resolve through those trusted
  paths, so pinning kubectl does not pin plugin resolution. Selecting this
  mode warns that its local ports cannot be token-protected.
  The shared ingress requires a sensitive process-local token and admits only
  issued, unretired actors in the configured atespace. It consumes the token
  before forwarding one request per connection, bounded by `Content-Length`
  (or no body); upgrades and transfer-encoded request bodies are refused.
  Responses, including event streams, remain streaming. The plugin owns its separate unauthenticated socket and
  has no header-authentication hook; it is bound explicitly to `127.0.0.1`,
  targets only its fixed actor, and still requires the worker's Basic auth.
  Other local processes can reach that plugin socket; use in-process mode
  when local users or processes are outside the trust boundary.
- State and policy mutations are reconciled by reads (`GetActor`,
  `GetActorEgressPolicy`), never blindly resent; deletion carries the known
  Actor UID and succeeds only on a confirmed `NOT_FOUND`. Creation ownership
  requires a matching successful create reply: an unknown UID leaves the name
  logged for owner reconciliation and is never adopted from a later read.
  After that warning, the owner must look up the named actor in the logged
  atespace and delete it if it exists.
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
cannot override that authentication. `[cloud].username` is the Basic user the
control side sends; the worker's `GANJA_SERVER_USERNAME` must match it (serve's
default is `ganja`). Git author and committer use `ganja-cloud-<member>` with
`[cloud].no_reply_email`, not that username. The checkout branch is
`<branch_prefix>/<member>-<epoch milliseconds>`, not a formatted date.

The served engine binds all interfaces so the platform router can reach its pod
address; this relies on one tenant per worker sandbox and mandatory Basic auth.
The worker image supplies `GIT_CONFIG_GLOBAL` and the platform projects
`GIT_SSL_CAINFO`; these trusted paths are followed, including projection symlinks.

### Acceptance image

`worker/Dockerfile` defines a fake-provider acceptance image, not a real-provider worker.
First acceptance is one owner/team and one hosted fake worker in a reviewed disposable checkout.
The owner must build/push the image and prove the live run; no repository test does so.

- Build context: Linux `ganja`, `ganja.toml`, `gitconfig`, `acceptance-script.json`;
  `.dockerignore` excludes unrelated files. Required inputs: digest-pinned `BASE_IMAGE`,
  matching binary `GANJA_SHA256`, and `GIT_HOST`. Apt packages remain unpinned.
- The image declares `USER 10001:10001`, starts `ganja worker init --port 8080`
  in empty `/work`, and leaves only `/work`, `/var/lib/ganja` and `/home/ganja`
  writable by that user. Binary/config are root-owned and read-only, shared temp
  is not writable, and the build clears/checks setuid/setgid files.
- Both transports target 8080. Templates commanding/probing port 80 are incompatible;
  migrate template/image/client together, or keep the old trio together for rollback.
- The Basic password is not in the image; supplied by the platform template as a
  disposable literal, it must match `GANJA_CLOUD_PASSWORD`. Retire it and the Git
  token after acceptance. Username defaults to `ganja` and must match `[cloud].username`.
  `GIT_HOST` scopes the placeholder HTTPS header; the platform supplies real Git auth.
  `gitconfig` disables signing. Child filtering is not isolation: the template and
  parent environments still hold plaintext readable by same-user processes.
- The platform must project its CA bundle at `/run/ate/egress-trust-bundle/trust-bundle.pem`
  for `SSL_CERT_FILE` and `GIT_SSL_CAINFO`. Owner proof must cover actual uid, all five
  zero capability sets (drop all/add none), mounts, CA, bind, work/data writes, failed
  binary/config writes, and no unintended service-account material or template overrides.
- The image policy grants only the fixed script's whole append/stage/unsigned-commit/push
  commands, not unrelated `git -c` or alias forms. Project config still merges permission
  rules, hooks, MCP entries and shell settings over it: the narrow grant requires an
  owner-reviewed checkout. The reviewed fake script is primary; grants are defense in depth,
  not isolation. First/resumed messages use two/one requests; policy and commit/push need owner proof.
- Init's `/readyz` proves only a listener; the backend separately awaits authenticated
  engine health. Authentication, actor/tag storage disposal and credential retirement remain owner proof.

### Child environments

`ganja worker init` clears inherited environment for both child types and copies
only the corresponding allowlist:

- Served engine: `PATH`, `HOME`, `GANJA_SERVER_PASSWORD`, `GANJA_SERVER_USERNAME`,
  `GANJA_CONFIG_HOME`, `XDG_DATA_HOME`, `SSL_CERT_FILE`, `GIT_CONFIG_GLOBAL`,
  `GIT_SSL_CAINFO`, `GANJA_PROVIDER`, `GANJA_FAKE_SCRIPT`,
  `GANJA_DISABLE_MODELS_FETCH`, `GANJA_DISABLE_TERM_PROBE`.
  These image/template inputs supply engine configuration, authentication and
  non-secret Git trust paths needed by commands run through the engine's tools.
- Git: `PATH`, `HOME`, `GIT_CONFIG_GLOBAL`, `GIT_SSL_CAINFO`.
  These image/platform inputs supply binary lookup, home, Git policy and the
  projected CA bundle; Git needs no server credentials or engine configuration.
  The child also sets `GIT_TERMINAL_PROMPT=0` and `GIT_CONFIG_NOSYSTEM=1` explicitly.

Every other inherited variable is cleared; the two Git overrides above are not
copied from the parent.

### Worker endpoints

A worker endpoint must use HTTPS with a host, or HTTP with a loopback IP address
or `localhost`. It must name only the origin: parsed path `/`, spelled with no
path or one trailing slash, and no userinfo (including empty userinfo), query or
fragment. Backslashes and original nonempty paths, including dot-segments that
URL parsing would normalize, are refused. Provisioner headers named
`Authorization` or `Proxy-Authorization` are refused case-insensitively.

## Code and tests

- `src/provisioner.rs`: lifecycle types and trait.
- `src/gate.rs`: request admission and completion accounting.
- Unit tests are sibling `*_tests.rs` files; socket/process fixtures live in `tests/`.
- Credentials never enter ganja's `Debug`, `Display`, ring rows, mail or own
  tracing fields; the shipped filters keep third-party debug bodies disabled.
- Commands run from the repository root; never create operational directories in
  this member.
