<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-06 -->

# ganja-tool/tests

## Purpose

The tools' integration suites: the handful of behaviours that cannot be tested beside the code they belong to. Every tool's own tests live in its module (`#[cfg(test)] mod tests`), and that is where a new test belongs unless it needs something a unit test may not have — process-wide state, or the crate seen from outside its own walls.

## Key Files

| File | Description |
|------|-------------|
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
```

### Common Patterns

Test names are sentences about behaviour, as everywhere else in the workspace. A fixture that reaches the network reaches loopback and nowhere else; nothing in this crate's tests may depend on a service being up.
