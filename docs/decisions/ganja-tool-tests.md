<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-06 -->

# ganja-tool/tests

## Purpose

The tools' integration suites: the handful of behaviours that cannot be tested beside the code they belong to. Every tool's own tests live in its module (`#[cfg(test)] mod tests`), and that is where a new test belongs unless it needs something a unit test may not have — process-wide state, or the crate seen from outside its own walls.

## Key Files

| File | Description |
|------|-------------|
| `evaluate_keys.rs` | What `evaluate` does about the credential it reads from the environment: no key at all, a key exported blank, and a key with a base URL that would carry it in the clear each mean `EvaluateTool::configured()` is `None` — **no tool**, not a tool that refuses. The opposite of `websearch_keys.rs` below, and deliberately: a tool whose dialog must name the host project content would travel to has to be built from its settings, and builtins are never deferred, so an unconfigured `evaluate` would spend prompt on every request of every session. Removes `TYPESAFE_API_KEY`, `TYPESAFE_BASE_URL` and `TYPESAFE_DEFAULT_MODEL` — **one test, one binary**. No socket is opened on any of these paths. |
| `evaluate_live.rs` | One real `noul` against `https://api.typesafe.ai`, asserting the *shape* of the answer — a probability in 0..=1 and a non-zero input-token count — never its value, which a model is free to change its mind about. Two locks: `#[ignore]` **and** a `GANJA_LIVE_TEST` check, because either alone is eventually defeated and this one costs a third party a request. Sends a literal from the vendor's own documentation, so a live run carries nothing belonging to whoever ran it. |
| `evaluate_log.rs` | What one `evaluate` call puts in the log, and what it must never put there: the `status`/`latency_ms`/`input_tokens` debug event is asserted **first**, as the control, and only then that no captured event carries the API key or the state. The call goes the whole shipped road, `EvaluateTool::configured()` and `Tool::run`, since the environment is what this binary exists to be allowed to touch. **One test, one binary** for two reasons at once — it sets `TYPESAFE_API_KEY`/`TYPESAFE_BASE_URL`, and its subscriber is the process's **global** default. The global is not a convenience: a thread-local `set_default` does not re-register a callsite another thread already reached, so in the unit-test binary a sibling reaches `Client::evaluate`'s event first with no subscriber installed, caches that callsite as never, and the capture comes back empty. Filtered to `ganja_tool` targets, because the claim is about what *this client* logs and an unfiltered TRACE would be recording `hyper`'s view of every other test's wire. |
| `websearch_keys.rs` | What `websearch` does about the credentials it reads from the environment: no key at all names both variables, a service named without its key names that one, and a variable exported blank is no key rather than a key that fails at the service. Mutates `EXA_API_KEY`, `PARALLEL_API_KEY` and `GANJA_WEBSEARCH_PROVIDER` — **one test, one binary**. No socket is opened on any of these paths, which is half the claim: a search that cannot be paid for should be refused before a third party hears about it. |

## For AI Agents

### Working In This Directory

- **A test lands here only when it has to.** The crate's suites are in-module by default: they can reach a private helper, and they sit beside the behaviour they describe. What earns a file here is process-wide state — the environment, chiefly — because `cargo test` runs a binary's tests on parallel threads and one test's `set_var` is every other test's surprise. (`nextest` gives each test its own process, but the separation has to hold under both.)
- **One test per environment-mutating binary**, with the `// SAFETY:` comment saying why the mutation is sound: that this binary holds exactly one test. A second test in such a file silently invalidates the first one's comment.
- **Public API only.** An integration test links the crate the way a frontend does, so anything it needs must already be public — which is a design signal, not an obstacle. If a test here wants a private seam, the test probably belongs in the module.

### Testing Requirements

```sh
cargo test -p ganja-tool                       # the in-module suites and these
cargo nextest run -E 'binary(websearch_keys)'  # one of these binaries
cargo nextest run -E 'binary(evaluate_log)'    # one `evaluate` call's log
cargo nextest run -E 'binary(evaluate_keys)'   # and what it does without a key

GANJA_LIVE_TEST=1 cargo test -p ganja-tool --test evaluate_live -- --ignored --nocapture
```

### Common Patterns

Test names are sentences about behaviour, as everywhere else in the workspace. A fixture that reaches the network reaches loopback and nowhere else; nothing in this crate's tests may depend on a service being up.
