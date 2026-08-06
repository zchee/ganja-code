<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-05 -->

# ganja-cli/src

## Purpose

The binary's whole surface: clap parsing, the credential subcommands, the config importer, the model listing, the headless turn, and the terminal prompt that reads an API key without echoing it.

## Key Files

| File | Description |
|------|-------------|
| `main.rs` | `Cli`/`Command`/`Auth`/`Config` clap types, `login`/`list`/`logout`, `models`, `mcp`, and the raw-mode key prompt. No subcommand delegates straight to `ganja_tui::run()`. |
| `login.rs` | Which login a provider gets and running the ones that are not a key: `--method` selection, the Copilot deployment question (`--deployment`/`--enterprise-url`, else the prompts), the device and browser flows, and the interrupt that ends a wait. Spec: upstream `packages/opencode/src/cli/cmd/providers.ts:39-205`. Stores nothing — `main.rs` does. |
| `import.rs` | `ganja config import-opencode`: discovery of opencode's config tiers, the key mapping, the mapped/skipped table, and the JSON writer that produces a `ganja.json`. The `--global` destination resolves through `ganja_core::config::config_home`, so what this writes is what the next launch reads wherever `GANJA_CONFIG_HOME` or a `~/.ganja` has moved the home; discovery deliberately does not — `~/.config/opencode` is opencode's home, not ganja's. |
| `run.rs` | `ganja run`: assembles the same `Engine` the TUI drives, takes one turn, writes an account of it, and exits. Spec: upstream `packages/opencode/src/cli/cmd/run.ts`, its non-interactive branch. |
| `serve.rs` | `ganja serve`: the same assembly as `run`, handed to `ganja-serve` and served until SIGINT/SIGTERM. Spec: upstream `packages/opencode/src/cli/cmd/serve.ts` + `cli/network.ts`. The address line is stdout's one payload; the unsecured warning and every other diagnostic go to stderr. |

## For AI Agents

### Working In This Directory

Every rule here exists because a secret passed through this code, and each is pinned by a test in `../tests/`.

- **A key is wrapped the moment it is whole, and the buffer it was assembled in is wiped.** `secret()` trims (a key pasted from a password manager arrives with whitespace that would corrupt the request header), wraps in `SecretString`, and zeroizes the `String` — nothing else will clear it.
- **The prompt does not echo.** An echoed key survives the exchange: it sits in scrollback that may be shared, recorded or logged. Raw mode is the only way crossterm offers to suppress echo, which is why that loop handles Enter, Backspace and Ctrl-C itself — in raw mode the driver does none of that and Ctrl-C raises no signal.
- **Raw mode is left on every path, panic included**, via a `Drop` guard. A terminal left in raw mode is unusable and the shell that owns it will not fix it.
- **What was typed is wiped even when the read is abandoned** — a half-typed key is still a key. The guard's limitation is documented honestly in the source: `zeroize` clears the buffer's capacity, but a `String` that grew as it was typed reallocated, and the prefixes it left behind are not reachable.
- **Only the redacted tail is ever printed** (`auth::RedactedTail`), including in success messages.
- **Say so when an environment variable shadows a stored key** — in the warning `login` and `logout` print, *and* in the listing, which gives the outranked credential a row of its own marked `(shadowed by <VAR>)`. Otherwise a `login` that appears to have worked changes nothing and leaves no trace anywhere a person would look for one.
- A key given via `--key` was already in shell history and the process table before this ran; the flag's help says so, and wrapping it is all that is left to do.

Prompts and diagnostics go to stderr so stdout stays a clean channel for whatever a caller is capturing. A piped key is read whole so `pass show … | ganja auth login` works.

`login.rs` adds the logins that are not a key, and every rule it has is about what a person can see or get out of:

- **A device grant is two calls with a print between them.** `start` returns the code and the address, `poll` blocks until somebody has typed the one into the other; a build that printed afterwards would leave a person watching a terminal that had told them nothing. `tests/auth_login.rs` holds the token exchange open until it has read the code, so that ordering is asserted rather than assumed.
- **The wait ends on the interrupt keystroke**, which is wired to the `CancellationToken` the flows take. A cancelled login says so *and* says nothing was stored — which is structural, not reassurance: storing is the step after the flow returns and an error never reaches it.
- **Which login runs is decided in one place, in this order**: `--key`, then `--method`, then **a provider whose one login is not a key** (Copilot's device grant), then "standard input that is not a terminal is a key" (`pass show … | ganja auth login` predates every flow here), then a provider's only login, then a menu. ChatGPT and grok reach the menu — each has a browser login and a device login, and nothing here can tell whether there is a browser on *this* machine; Anthropic and Copilot have one login worth offering and never ask. The menu's words are upstream's own labels, because they are what somebody recognises from having read its documentation. The third step is the same question as the fifth asked on the other side of the pipe rule, and the split is the whole of it: a device grant *is* a provider's headless login — a code typed into a browser on some other machine — so a pipe deciding it was a key put the only unattended login out of reach of the only unattended invocation. Anthropic's one login is a key, so it answers the fifth step and its piped-key case is untouched.
- **Both Copilot deployments are nameable up front.** `--deployment public` is github.com outright and `--enterprise-url` remains the enterprise branch *and* its address; `--deployment enterprise` alone still needs an address from somewhere. A flag existed only for enterprise, so the common login — github.com — was the one that could not run unattended, and the obvious workaround was worse than the gap: a piped menu answer was a non-terminal standard input, which the ordering above used to read as an API key and store `1` as a credential. The two flags contradicting each other is refused rather than resolved by precedence, and a question that has to be asked with nobody to ask names the flag that would have answered it.
- **A method a provider does not have is refused with the ones it does have named.** The flow dispatch refuses an impossible pairing a second time as the shape of its match, so the "it has …" clause is the only part of the message that distinguishes the front-door check from the fallback — and the test asserts on it for that reason.
- **A login says what it is about to replace.** A ChatGPT login and an OpenAI API key are stored under the same provider key, so each silently overwrites the other; `ganja-core` is handed a credential and cannot know somebody is watching, so this is the only place that can warn. Nothing is refused — replacing is what `login` is for, and the point is that it not be silent.
- **`GANJA_AUTH_ISSUER` redirects every login endpoint, and only to loopback.** It exists so a test can complete a login against endpoints it owns. The value decides where a device code and then a pair of tokens are sent, so it is checked by *shape* — the whole value has to be `http://<loopback host>:<port>`, which leaves nowhere for userinfo, a path or a query to hide. A prefix match alone would accept `http://127.0.0.1:80@elsewhere.example`. A value that is set and not loopback is refused rather than ignored: quietly using the real issuer instead is the one outcome whoever set it cannot have wanted.
- **Storing a credential is all a login does.** Whether a model then runs on it is a separate question per provider, and nothing printed here may imply an answer to it.

The importer inherits the same posture, for the same reason — it reads a file that may hold a credential:

- **`provider.<id>.options.apiKey` is never written**, only reported, with a warning naming `ganja auth login`. Neither the table nor the warning repeats the key.
- **`{env:VAR}`/`{file:path}` is never expanded**, in either direction. A value that *is* a token is left out (carrying it would name a model or a path that does not exist); a value that merely contains one is carried verbatim and warned about, because ganja will then read it literally.
- **Every key is either mapped or reported.** A key that vanished without a row would be a setting its author still believes is in force — the table is the command's output and the file is a side effect of it. A container (`agent`, `mcp`, an `lsp` map) is covered by the rows its entries carry rather than one of its own.
- **An `mcp` or `lsp` entry is written whole or not at all**, so its fields are read into a report of their own and only adopted if it survives: a `mapped` row under an entry that was then refused would name a setting that was never written. Only what such a pass had to *say* outlives the refusal.
- **An `lsp` entry is judged by what it leans on, not by its name.** opencode ships definitions for thirty-eight language servers and ganja ships two, but an entry naming one of the other thirty-six is only a problem when it relied on that definition: upstream lets an entry give just a `command` (or just `disabled`) and inherit the extensions and the root. An entry naming both its `command` and its `extensions` is a whole server description already, so it imports as a custom server under its own name and does here what it did there; anything less is named, skipped and explained.
- **Nothing is completed on an entry's behalf.** A language server with no `command` is left out rather than given one — a fabricated command starts a program nobody chose — and the two shapes `ganja_core::config` refuses at load (a command-less server that is not disabled, a custom one that does not name its `extensions`) are refused here instead, because a file this wrote that the next launch will not read is the failure the round trip exists to prevent. The same holds for a remote MCP endpoint that is neither `https` nor loopback; the check here is deliberately the conservative half of core's, which parses the URL properly and is the authority.
- **A command line and an extension list travel whole or not at all**, where `instructions` drops entries one by one: a command missing an argument runs a different program, and an extension list emptied of what could be carried is `[]`, which ganja reads as *every* file.
- **An existing `ganja.json` *or* `ganja.jsonc` at the destination is refused by name** rather than overwritten; the write itself uses `create_new`, because the check and the write are not the same moment.
- **What is written is decoded back into `ganja_core::config::Config` before it lands**, so a mapping bug is an error at import time rather than a broken file discovered at the next launch. Decoding is not the whole of what `Config::load` does — its `mcp` and `lsp` checks run after it — which is why `../tests/import_round_trip.rs` loads the imported file for real and is the assertion those two mappings answer to.
- Object keys keep the order they were written in throughout: `permission` is evaluated last-match-wins, so a reader or writer that sorted them would change which rule decides a call.

The two listings each have one rule that is not obvious from their code:

- **`models` calls `catalog::load_cached()` before it reads the table.** The disk tier is a layer somebody installs, not one a lookup reaches for, so a listing that skipped installing it would answer from the compiled-in snapshot however recently a session had fetched something newer. `--refresh` fetches on top of that and is never fatal — fetching switched off and an endpoint that refuses the connection both leave the table standing and say so on stderr. Only a named provider the table does not carry is a failure, because a header over no rows is indistinguishable from the typo it usually is.
- **`mcp` dials.** A standing nothing has tried is not a standing, so the listing connects every enabled server and reports what came of it — and reads the statuses and the tools it lends *before* it shuts them down again, because closing a connection takes its tools with it. The rows are driven by the config rather than by the statuses, which deliberately omit a server nothing has finished trying: a row that could silently vanish is worse than one reporting it has no standing. Nothing here wants a credential, so no engine is built, for the reason `sessions` reads the store directly.
- **`sessions` lists roots.** A session carrying a `parent` belongs to the `task` call that spawned it; the picker in `ganja-tui` filters the same way, and filtering before the count is what makes a project whose every session is delegated read as one with none.

`run` is the one subcommand that takes a turn, and every rule it has exists because nobody is watching it:

- **It drives the concrete `Engine`, not a transport.** Upstream reaches its own engine through a loopback HTTP client and therefore has an `--attach` that reaches somebody else's; ganja has neither a server nor a second transport, so `run` calls the engine the TUI calls, assembled in the same order and for the same reasons (deviation: `run-drives-the-engine-directly`).
- **Subscribe before prompting.** Upstream's ordering, kept for a different reason: ganja's queue is created with the engine and is lossless, so a late subscriber does not lose the head of the turn — it *wedges* once the turn fills a queue nobody drains.
- **The session id is a local**, captured before a single event is read, and stamped on every emitted object — deliberately not read off the events, though every event now carries `session_id`: the stamp is the run's contract with its consumer, and the test fixtures put a *different* id on the events so a stamp read off the wrong place would show. Upstream additionally filters other sessions out per event; this build still has nothing to filter, because a subagent's events never reach the subscribed stream at all, and the permission dialogs that do cross arrive re-addressed to the parent's session (`ganja-core/tests/task.rs` pins both, and `../tests/run.rs` pins the corollary here) — a documented divergence, not an omission.
- **Nothing waits on a person.** Two mechanisms, both ported: the session refuses `question`/`plan_enter`/`plan_exit` at every pattern — tools this build does not have yet, which is the point, since it makes a later `question` safe in `run` by construction — and a live request is answered the moment it arrives, `once` under `--auto`/`--yolo`/`--dangerously-skip-permissions` and otherwise a warning plus `reject`.
- **Those refusal rules are installed after `with_agents`, never before.** `Engine::with_agents` installs the default agent's ruleset as the baseline *wholesale*, so rules written earlier are thrown away; they are appended to the agent's own, which is where last-match-wins needs them.
- **`--format json` carries exactly six `type` names** — `tool_use`, `step_start`, `step_finish`, `text`, `reasoning`, `error` — and this build has five sources for them: ganja's protocol has no reasoning part, so `reasoning` is a name a consumer must still handle and nothing here emits (deviation: `run-emits-no-reasoning`). Text has no completion event of its own; the step's `step_finish` marker is what closes it, so text is accumulated and written when the step ends.
- **A flag ganja cannot honor is absent, not stubbed.** `--attach`, `--port`, `--mini`, `--share`, `--file`, `--title`, `--variant`, `--thinking` and the rest name features this build has no surface for. `--fork` is the exception, and deliberately so: upstream's *validation* of it is worth keeping whole, so the flag parses, `--fork` without `--continue`/`--session` is refused exactly as upstream refuses it, and a `--fork` that survives that is refused loudly because nothing in `ganja-core` copies a session.
- **Payload on stdout, diagnostics on stderr.** Upstream mixes its warnings into stdout; here a warning inside `--format json`'s stream would corrupt it, so the account of the turn is the only thing on stdout. A failure is emitted as an `error` object *and* returned, and the caller prints it once on its way to exit 1 — never the same sentence twice.

`serve` reuses `run`'s assembly shape whole — duplicated deliberately rather than shared through core, because the seam between the binary and the engine is frozen — with the two differences a server earns: the file watcher runs (later turns must distrust files that moved between them), and `run`'s auto-refuse permission rules are **not** installed, because a serve client is a person at a distance — dialogs travel out on `/event` and answers come back on `POST /permission/{id}/reply`. The bind posture, the auth, and the port policy all live in `ganja-serve`; this file only assembles, prints, and waits for the signal.

### Testing Requirements

```sh
cargo test -p ganja-cli --test cli
cargo test -p ganja-cli --bin ganja            # the mapping table lives beside the mapping
cargo test -p ganja-cli --test import_opencode
cargo test -p ganja-cli --test run             # the exit-code table and the nd-JSON shape
```

Adding a subcommand means adding its assertion there; adding anything that handles key material means proving the key does not reach stdout, stderr, or a stored file in the clear.

### Common Patterns

`ProviderId` is a clap `ValueEnum` that maps to the string ids `ganja-core` knows (`anthropic`, `openai`, `grok`, `github-copilot`), so the CLI's spelling and the engine's cannot drift — through each login module's own `PROVIDER_ID` constant rather than a literal, because a login that wrote under one spelling while a request read another would read as a storage bug rather than the naming one it is. `grok` is deliberately not `xai`: `xai` is what the credential is filed as so a shared `auth.json` keeps working, and `auth::storage_key` is the single place that translation happens. Prices render with trailing zeros trimmed rather than padded, so a fraction of a cent shows as itself instead of rounding to a different number.

## Dependencies

### Internal

`ganja_provider::auth`, named directly rather than through the engine's re-export (store, list, remove, redaction, and the `grok`/`copilot`/`openai` login flows with `device`'s two-call shape) — `login.rs` has no engine and no reason to build one; `ganja_core::catalog` (the `models` table), `ganja_core::config::Config` (what the importer's output has to decode as, and what `run` assembles an engine from), `ganja_core::lsp::server::BUILTIN_IDS` (which language servers this build ships, so the CLI's answer and the engine's cannot drift), `ganja_core::Project` (the import's project walk and destination, and where `run`'s session store lives), `ganja_core::Engine` + `provider::select` + `instruction` + `permission` + `tool::Registry` (everything `run` needs to take a turn), `ganja_tui::run`.

### External

`clap`, `anyhow`, `tokio` (+ `tokio-util`'s `CancellationToken`, which is what ends a login's wait), `futures` (`run` consumes the engine's `BoxStream`), `serde_json` (`run --format json`), `secrecy` (+ `zeroize`), `ratatui`'s crossterm re-export for raw mode, `jsonc-parser` (the importer reads someone else's config, in document order).

<!-- MANUAL: -->
