<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-04 | Updated: 2026-08-04 -->

# ganja-core/src

## Purpose

The engine's modules. The shape to hold in mind: `engine.rs` accepts commands and publishes events, `session.rs` runs one turn's agent loop against a `provider/`, executing `tool/` calls that `permission.rs` gates, with `protocol.rs` supplying every type that crosses the boundary.

## Key Files

| File | Description |
|------|-------------|
| `lib.rs` | Module list and the public re-export surface. States the no-terminal-dependency rule that CI enforces. |
| `engine.rs` | `Engine`: commands in, an ordered event stream out. Owns the turn lifecycle and the transcript. |
| `session.rs` | The agent loop — one turn, as many model requests as its tool calls demand. Also `TurnHandle`/`PendingReply`, the plumbing a permission reply is routed through. |
| `protocol.rs` | Wire protocol v1: `Command`, `Event`, `Message`, `Part`/`PartBody`, `ToolState`, `Usage`, and the ascending id types. Every type is serde-serializable. |
| `provider/` | Sources of assistant text (see `provider/AGENTS.md`). |
| `tool/` | What the model can do besides talk (see `tool/AGENTS.md`). |
| `permission.rs` | Decides which tool calls run unasked and which wait for the user; persists "always" answers per project. |
| `auth.rs` | Provider credentials: environment first, then `auth.json` under the XDG data directory. |
| `project.rs` | Which project a working directory belongs to (walk up for `.git`), and where its state lives. |
| `catalog.rs` | Compiled-in models.dev snapshot: context windows, max output, pricing, per-provider default model. |
| `storage.rs` | **P4, in progress.** Versioned JSON session storage under the project data directory. Currently uncommitted with `todo!()` stubs — owned by another lane; do not edit. |

## For AI Agents

### Working In This Directory

The invariants below are what the tests actually pin. Breaking one is a behavioral regression even if everything compiles.

- **Delivery is lossless.** Events travel a bounded `mpsc`, so a producer that outruns its consumer waits instead of dropping fragments; backpressure lands on the turn task and never on the render loop. Do not swap in `broadcast` — lag-drops would silently tear a transcript.
- **One subscriber, one turn.** `subscribe()` after the first returns `AlreadySubscribed`, because splitting one lossless queue between two readers hands each an arbitrary half of the transcript. A prompt sent while a turn is streaming — or waiting on a permission — returns `Busy`. Fanout arrives in P7.
- **The event stream is the whole story.** A frontend that applies every event holds exactly what the next `ChatRequest` will carry. Anything the engine knows and does not report is a bug in the protocol, not something a frontend should reconstruct.
- **Tool results are information, never control flow.** A refused permission, an unknown tool, unparseable arguments, a tool that failed — each becomes error text the model reads on the next request, and the loop continues. (This is a deliberate divergence: upstream stops the turn on a refusal unless `experimental.continue_loop_on_deny` is set.) The only two early exits are the ones that mean it: the user cancelled, or the provider died. There is no step cap — `Command::CancelTurn` is the escape hatch, as upstream.
- **A turn always ends with a terminal event.** The turn task is deliberately never joined and never aborted from outside: cancellation reaches the provider and every running tool, and aborting the task instead would skip the cleanup that releases the busy slot and guarantees the finish event.
- **Failed turns do not enter the history.** An empty reply is dropped rather than carried into the next request.

### Testing Requirements

Unit tests sit in `#[cfg(test)] mod tests` at the bottom of each module and cover the module's own logic. Cross-module behavior — the loop, cancellation, delivery, permissions round-tripping to disk — belongs in `../tests/`. When adding an invariant to this directory, add the test that would notice if someone removed it.

### Common Patterns

- **Ids sort in creation order.** `MessageId`/`PartId`/`PermissionId` are a millisecond timestamp plus a per-process counter, both fixed-width hex, so ids sort lexicographically by creation and cannot collide in one process. Storage reassembly (P4) leans on this.
- **Ids adopted from disk are taken verbatim.** The prefix is a convention, not an invariant.
- New `PartBody` variants are additive — the tag travels as a `type` field beside the part's id — so adding one changes nothing already on the wire.
- Time is milliseconds since the Unix epoch via `protocol::now()`, saturating rather than failing when the clock is set before 1970.

## Dependencies

### Internal

Nothing outside this crate. Within it, the flow is `engine` → `session` → {`provider`, `tool`, `permission`}, with `protocol` beneath all of them and `project` supplying the data directory that `permission` (and P4's `storage`) write into.

### External

See `../AGENTS.md`.

<!-- MANUAL: -->
