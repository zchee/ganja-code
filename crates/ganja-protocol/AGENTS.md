<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-08-05 | Updated: 2026-08-05 -->

# ganja-protocol

## Purpose

The types every side of the app speaks: the `Command`s a frontend sends, the `Event`s the engine streams back, and the `Message`/`Part` model a session is stored as. One file, and a dependency list of exactly `serde` plus the value type a tool call's arguments arrive as — which is the whole reason it is a crate. Rendering a transcript, asserting on an event, or later driving a session from the far end of a socket takes none of the engine, and with the protocol on its own nothing has to build one to find that out.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Member manifest, `publish = false`. Two dependencies. Anything that would widen the list belongs on the other side of the boundary. |
| `src/lib.rs` | The whole crate: `Command`, `Event`, `Message`, `Part`, `PartBody` (including `PartBody::Reasoning` and its `REASONING_TAG`), `ToolState`, `Usage`, the id types and their ascending minting, `FinishReason`, `Mention`, `RevertInfo`, `PermissionReply`. Spec: upstream `session/message-v2.ts`. `PartBody::Reasoning` is the first variant whose absence changes what the *next request* carries rather than what a transcript looks like, so its tag prefix is a contract: a later variant of it keeps the `reasoning` prefix, and a reader that cannot decode such a record must keep the rest of the message and leave a stateless one of these in its place. `PartBody::ReasoningText` (tag `reasoning_text`) is the first variant to honor that contract — thinking a person can read, split out of what upstream fuses into one part. It is **display-only**: no wire sends it, no summary carries it, the context meter counts it as nothing, and it is outside `Part::as_text` so it can never title a checkpoint or answer a copy command. A caller that wants thinking matches the variant itself, and `Part::streamed_mut` is the one accessor spanning both kinds of text, for a frontend applying a `PartDelta` that names an id and not a kind. |

## For AI Agents

### Working In This Directory

- **Every type here is serde-serializable, and that constraint is load-bearing.** It is not a trait that preserves the path to serving the engine over a socket — it is this. A type that cannot round-trip through `serde` does not belong here, and one that can, but whose representation changes, is a wire break: the stored sessions on disk are these values written out verbatim.
- **Ids sort in creation order.** `ascending` mints `<prefix>_<millisecond timestamp><per-process counter>`, both fixed-width hex, mirroring upstream. Ordering across processes is only as good as the clock, which is the guarantee upstream makes too. `now` and `ascending` are public because the engine mints ids for messages it did not receive from here, and two implementations of "sorts after everything before it" is one too many.
- **This crate names no other crate in the workspace, and must not start.** If a doc comment here needs to talk about something on the engine's side of the line — the read log `edit` consults, say — it says so in prose rather than as an intra-doc link, because the link would require a dependency the boundary refuses.
- Adding a `PartBody` variant changes nothing already on the wire; changing or removing one changes what a stored session decodes to. Treat the two cases differently.

### Testing Requirements

```sh
cargo test -p ganja-protocol          # its unit tests
cargo tree -p ganja-protocol -e normal   # the boundary, visible: serde and serde_json
```

### Common Patterns

Types are plain data with derived `serde` impls; behavior belongs to whoever holds them. The exceptions are small and are about identity rather than meaning: `MessageId`/`PartId`/`PermissionId`/`SessionId` mint and compare themselves, `Part` carries the `as_text`/`as_text_mut` accessors that spare every caller a `match` on the body, and `Event::session_id` reads the one field every variant carries so a session-filtering consumer does not write the eight-arm match itself.

## Dependencies

### Internal

None, and that is the invariant.

### External

`serde` (every type derives it) and `serde_json` (a tool call's arguments and metadata are values the protocol carries rather than shapes it re-declares).

<!-- MANUAL: -->
