<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-18 | Updated: 2026-08-18 -->

# ganja-team

## Purpose

Claude Code's teams directory: the member records a team file carries, and the file-backed mailboxes teammates are addressed through. **Upstream opencode has no counterpart at all** — it has no teams, no mailbox and no second agent to address — so unlike almost everything else in this workspace there is no TypeScript to port behavior from. The specification is Claude Code's, read out of the Claude Code teammates reference (kept outside this repository): §1.1 for the name grammar, §2 for the on-disk data model, §3 for the mailbox surface. The divergences and the reasons for them are **D497**.

**Why this is a crate.** Because the documents are *somebody else's format*. A real `claude` process can be sharing the very directory this crate writes into (D-1), which makes two things interop contracts rather than implementation details: the bytes of a document, and the protocol by which a writer holds an inbox. Both are served by shapes that keep what they do not understand — a `#[serde(flatten)]` passthrough over an `IndexMap`, so an unknown key survives a rewrite in the position it arrived in. That is the exact opposite of `ganja-protocol`'s posture, which declares an exhaustive vocabulary and refuses a peer that grew a field rather than guessing at it; putting a passthrough shape there would contradict the doctrine at the point it is stated. So the split is Claude's documents here, beside the file I/O, and ganja's own `TeamView`/`MemberView` projection in `ganja-protocol` for anything that merely *renders* a team. A frontend therefore needs no dependency on this crate at all.

**What it does not know.** Where ganja keeps its homes, what a session is, and what a permission decides — every one of those would put an engine's answers underneath the engine. The teams root arrives as a `TeamsRoot` value, the way `skill::Roots` arrives in `ganja-tool`; a message timestamp arrives as a string. CI asserts the whole of it: this crate's internal dependency list is exactly `ganja-protocol `, asserted closed from its first commit, before there was a convenient edge to add.

**It is synchronous, deliberately.** A mailbox write is a sub-second read-modify-write on a small file, and the lock schedule it may sleep through is measured in milliseconds; putting a runtime under that would buy nothing. Whoever calls it from inside a turn wraps it in `spawn_blocking`.

**D545 — the shared task list a team claims work from** (2026-09-02, `.omc/plans/2026-09-02-team-orchestration.md`, W2; `src/task.rs`). The `/team` pipeline hands work to teammates that are separate processes, so the list they coordinate through has to survive two of them reaching for the same item at the same instant. That one requirement decides the whole shape, and everything below follows from it rather than from taste. **A document per task** (`<team dir>/tasks/<id>.json`) rather than one array: a claim then contends only the task being claimed, instead of queueing every create, every status change and every claim in the team behind a single lock, and a create touches nothing that already exists — the cost is that a listing reads a directory instead of a file, which is the cheaper half of the trade at any size a team's list reaches. **A counter document** (`tasks/counter`, deliberately extension-less so no listing mistakes it for a task) issues the ids, read-and-bumped under its own hold, because a directory listing cannot issue the next one without two creates both deciding on `4` and one of them silently losing its task. **An id is never reused** — deleting `3` leaves a gap where 3 was and the next create still gets `4` — and a counter that has gone missing is rebuilt from the highest id on disk rather than restarted at 1, so losing that file costs a gap instead of a collision. The lock is this crate's own, unchanged: `lock::acquire_unseeded` for both, because a task document has no empty state to seed and a create writes a path that is not there yet. **A claim reads, tests the owner and writes it under one hold**, which is the property the file-per-task layout exists for; a delete removes the document under the same hold; a read takes no lock at all, because a write lands through a rename.

The record is **Claude Code's Task\* shape** and ganja's own bytes: `subject`, `description`, `active_form`, `status` (`pending` → `in_progress` → `completed`, with `deleted` a permanent removal rather than a fourth state), `owner` (empty until somebody claims it), `blocks`/`blockedBy`, a free `metadata` map that merges on update and drops a key given `null`, and `comments` that only ever grow (`{from, at, text}`). What is borrowed is the *semantics a model is already trained on*, never a file: a `claude` teammate keeps its own list inside its own process and cannot see this one, which is why this is ganja's format to define. The crate's passthrough posture is kept all the same — every shape here carries the `#[serde(flatten)]` extra — and `tasks/` is the **one liberty taken with a directory a peer may share**: a ganja-only *subdirectory* beside Claude's documents, never a ganja-only key inside one of them, which the crate's standing rule still forbids.

Two properties are held by tests rather than by review. **The claim is proved across processes** (`tests/task_race.rs`): eight tasks, two real processes, a per-round barrier, exactly one owner each and the loser told who holds it — threads would prove *less* than nothing, since the lock's in-process half serializes them before the on-disk protocol ever runs and a threaded version passes with the `mkdir` deleted; four processes creating at once ride along, and the ids they are handed are the first twenty numbers, once each. **What a task says is content** (`tests/no_task_content_in_logs.rs`), which is `no_bodies_in_logs.rs`'s rule reaching a second document family: a description, a comment, a metadata value and a document that will not decode may none of them reach a log line, so a listing that drops a damaged document reports which *kind* of failure it was rather than the decoder's own sentence, which would quote the value it choked on.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest, `publish = false`. One internal entry and seven external, each with the reason it is there. |
| `src/lib.rs` | Crate doc — the D497 statement, the passthrough/refuse split against `ganja-protocol`, and the synchronous ruling — plus the module declarations and the headline re-exports. A runnable doc example that seeds, writes and drains one inbox. |
| `src/team.rs` | Where a team's files live and what a member may be called: `TeamsRoot` (the root as a *value*), `TeamName`/`MemberName` and their `parse`, the path builders that take those types rather than `&str`, `LEAD`, and the collision counter. **A name is refused, never repaired** — the agent half of a path derives from a name a model chose, and a sanitizer that quietly rewrote `../../etc/passwd` into something joinable would answer a question nobody asked. |
| `src/record.rs` | Claude's file formats: `TeamFile`, the two `MemberRecord` emit orders, and `MailboxMessage` at rest. Every shape carries a `#[serde(flatten)] extra: IndexMap`. Also `now_iso8601`, whose exact spelling is a compatibility surface because message identity is composed from it. |
| `src/mailbox.rs` | An inbox: seed, append, drain, `prune_delivered`. A mailbox is a **queue, not a history** — delivered messages are pruned and `read` is a tombstone nothing ever writes `true`, so inbox depth is genuine backlog. Corruption is survivable and never silent: a bad entry is dropped and reported by field, expectation and JSON type, never by value, deduplicated and capped. |
| `src/lock.rs` | npm **proper-lockfile**'s protocol, reproduced rather than chosen: `realpath` the target, acquire by `mkdir` (never a file), release by `rmdir` in a `Drop`, staleness by the lock directory's mtime past ten seconds, and §2.5's literal `{retries: 10, minTimeout: 5, maxTimeout: 100}` ladder. |
| `src/task.rs` | The shared list a team coordinates through: one document per task under `tasks/`, a counter document issuing sequential ids, and the claim those two exist for. **Ganja's own format rather than Claude's** — a `claude` teammate keeps its task list inside itself and cannot see this one — so what is borrowed is the Task* semantics a model is already trained on. The passthrough posture is kept all the same, and an id is never reused: deleting 3 leaves a gap where 3 was. |
| `tests/claude_format.rs` | AC-1a — the shape round-trip over documents this repository writes. Holds the shape, not interop. |
| `tests/claude_format_interop.rs` | AC-1b — byte identity against the **captured** documents under `tests/fixtures/`. The only test in the workspace that can tell whether this crate reads and rewrites a foreign document byte-for-byte, and therefore Driver 2's falsifier. |
| `tests/contention.rs`, `tests/lock_release.rs`, `tests/lock_break.rs` | The lock, from three sides: N processes writing one inbox lose no message; every lock is released even when the write fails; a stale lock directory is broken on mtime while a fresh one held by a peer is waited for. |
| `tests/task_race.rs` | The claim, from two real processes: eight tasks, a per-round barrier, exactly one owner each and the loser told who holds it. Threads would prove *less* than nothing here — the lock's in-process half serializes them before the on-disk protocol runs, so a threaded version passes with the `mkdir` deleted. Four processes creating at once ride along, and the ids they are handed are the first twenty numbers, once each. |
| `tests/no_task_content_in_logs.rs` | `no_bodies_in_logs.rs`'s rule for what a task *says*: a description, a comment, a metadata value and a document that will not decode, none of which may reach a log line. |
| `tests/no_bodies_in_logs.rs` | The canary: nothing here may log a message body. |
| `tests/fixtures/` | **Bytes Claude Code wrote**, captured verbatim and committed as interop test data. `PROVENANCE.md` records where each came from, what three spans were redacted and under what law, and what the capture found about key order. Attributed in `THIRD_PARTY_NOTICES.md`. |

## For AI Agents

### Working In This Directory

- **Do not reformat anything under `tests/fixtures/`.** Not with an editor's save hook, not with `jq`, not with a formatter that thinks a JSON file wants a trailing newline. Every byte is evidence, and re-indenting a fixture to make a test pass deletes the only thing the test is for. If a change is ever genuinely needed, recapture rather than edit.
- **A `MemberRecord` has two emit orders and they are not reconcilable in one declaration.** A lead puts `agentType` third and `cwd` before `subscriptions`; a teammate puts `subscriptions` before `agentType` and `cwd` after `planModeRequired`. Claude builds them at two creation sites; byte identity for both needs two orders here. This is recorded in `PROVENANCE.md` with the evidence.
- **Never add a ganja-only field to a document.** `MemberRecord` is Claude's file, and even a namespaced `ganja_*` key would be an unstated amendment to a format a real `claude` also reads and AC-1b compares byte for byte. Anything ganja wants to show about a member goes in `ganja-protocol`'s `MemberView`, which exists precisely so a ganja-only field has somewhere to live that is not somebody else's file.
- **The lock is a directory, and nothing may live inside it.** Claude's own stale cleanup is `rmdir`, which fails `ENOTEMPTY` on a directory holding anything and `ENOTDIR` on a file — so a pid file, or a lock file, would turn the peer's crash recovery into a permanent failure. There is no liveness probe for the same reason: mtime is the only signal both sides read.
- **A task document is ganja's, an inbox is Claude's, and the difference decides everything about a change.** `tasks/` is the one ganja-only thing in a directory a real `claude` may share, and it is a *subdirectory* — never a ganja-only key inside one of Claude's documents, which the bullet above still forbids. A task list is also not something a `claude` teammate can be pointed at: it keeps its own, inside itself.
- **Nothing here logs a message body.** Log lines carry counts, paths and ids. The partial `Debug` implementations on `Identity`, `MailboxMessage` and the record shapes are the same rule stated as a type, and `tests/no_bodies_in_logs.rs` is what keeps it true.
- **An append rewrites the whole file.** That is the peer's protocol, not a shortcut; a mailbox is small and a queue rather than a log, which is what makes the N² byte movement a non-issue rather than a debt.

### Testing Requirements

`cargo nextest run -p ganja-team`, and the workspace gates from the repository root. `tests/claude_format_interop.rs` reads the committed capture under `tests/fixtures/` and **hard-fails, never skips**, when it is missing or incomplete — a checkout without it is broken, not unsupported.

The internal-dependency allowlist is a gate rather than a convention: the root `depgate.toml` closes this crate's internal set at exactly `ganja-protocol` (its `ganja-team` rule, with the rationale in the comment above it), evaluated by CI's `cargo depgate check --config depgate.toml`.

### Common Patterns

- Test names are sentences about behavior: `a_real_claude_team_file_round_trips_byte_identical`, `a_fresh_lock_directory_held_by_a_peer_is_waited_for_not_broken`.
- A test that needs a real filesystem layout takes an explicit `TeamsRoot` over a `tempfile` directory; nothing here reads an environment variable, so nothing here needs one redirected.
- Divergences carry their D-number at the point they occur, and every module doc names the reference section (`§n.n`) it ports.

## Dependencies

### Internal

`ganja-protocol`, and nothing else — the frames a message body carries when it is not prose, and the UUIDv7 mint a written message is stamped with (D493). A mailbox writes what the protocol already declares rather than a second spelling of it.

### External

`indexmap` (insertion-ordered, behind the `#[serde(flatten)]` passthrough — the whole reason the record shapes live here), `serde` + `serde_json` with `raw_value` (§2.4's validation is field-level, so an entry is checked as a value and decoded from raw bytes in two passes over one parse), `tempfile` (an inbox rewrite is write-then-`persist`, so a reader sees the old array or the new one and never a half), `backon` (the lock's retry ladder, used by `lock.rs` alone), `thiserror`, and `tracing`. Versions live in the root manifest, with the reason each is here.

<!-- MANUAL: -->
