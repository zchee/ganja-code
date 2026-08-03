<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# golden

## Purpose

The canned tasks the differential harness drives **both** agents through — this port and real upstream opencode v1.18.11 — so the two can be compared on the one thing a user would notice if the port drifted: the ordered list of tool calls each actually executed, and the arguments each ran with.

## Key Files

| File | Description |
|------|-------------|
| `task-read-edit.json` | Read a seeded file, then edit a string in it. |
| `task-search-read.json` | Search the seeded tree, then read what was found. |
| `task-write-run.json` | Write a file, then run it through the shell tool. |

## For AI Agents

### Working In This Directory

Each task is one JSON document:

```json
{
  "prompt":  "what the user asks; both legs send it verbatim",
  "seed":    { "relative/path": "file contents the directory starts with" },
  "steps":   [ { "text": "streamed before the calls",
                 "calls": [ { "name": "read", "arguments": { "filePath": "notes.txt" } } ] } ]
}
```

- **`steps` are answers, not assertions.** They are served by a loopback endpoint speaking OpenAI chat completions; a step with no `calls` ends the turn. What is under test is everything downstream of the wire — frame decoding, argument assembly across chunk boundaries, the permission gate, tool dispatch, and the order all of it happens in.
- **Tool names and argument keys are upstream's spelling** (`filePath`, `oldString`, `newString`). Both legs receive the identical script, so a name only this port understands makes the upstream leg do nothing and the comparison meaningless.
- **Seeded paths are relative.** Each leg runs in its own temp directory, so absolute paths in arguments are normalized to a `<CWD>` placeholder before comparison — an argument is equal only up to its root.
- A new task should exercise a tool interaction the existing three do not, and must be answerable by a fixed script: no step may depend on what the previous step's tool actually returned.

### Testing Requirements

```sh
cargo test -p ganja-core --test golden
```

Requires `bun` and an installed upstream checkout (`GANJA_OPENCODE_DIR`, else `.omc/reference/opencode-v1.18.11`). Missing prerequisites fail the suite rather than skipping it — see `../AGENTS.md`.

<!-- MANUAL: -->
