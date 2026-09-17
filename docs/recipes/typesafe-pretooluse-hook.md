# Asking TypeSafe about a shell command before ganja runs it

This recipe wires `ganja evaluate` into a `PreToolUse` hook so that TypeSafe
System One is asked about every `bash` call. It ships in two variants, and only
one of them protects anything. **`annotate`, the default, guards nothing**: what
it prints lands in `hookSpecificOutput.additionalContext`, which the engine
appends to the tool's result *after the call has already run*, and only when the
call succeeded (`crates/ganja-core/src/session.rs:5056`, inside the `Ok` arm).
On a call the person rejected, or one that failed, the text is dropped and
nobody ever reads it. It is commentary addressed to the model's next step, not a
gate. **`deny` is the preventive variant**: it prints
`hookSpecificOutput.permissionDecision: "deny"` with a reason
(`crates/ganja-core/src/hook.rs:833-849`), the call is refused before it runs,
and the reason is what the model reads instead of an output. `annotate` ships as
the default because it cannot break a session; switch to `deny` once you have
measured your thresholds.

## Files

| File | What it is |
|---|---|
| `typesafe-pretooluse-hook.sh` | The hook. POSIX `sh` and `awk`, no `jq`. |
| `questions.json` | Two `noul` questions about the command, passed to `ganja evaluate --questions`. |

Both are examples to copy and edit, not a supported interface. The thresholds in
the script are placeholders (see [Thresholds](#thresholds)).

## What leaves the machine

Everything the hook is given. The script does not filter the envelope; it pipes
its whole standard input to `ganja evaluate`, which sends it as the `state`. For
a `PreToolUse` hook that envelope is (`crates/ganja-core/src/hook.rs:371-381`):

| Field | Value |
|---|---|
| `session_id` | The session's id. |
| `cwd` | The project root, as an absolute path on your machine. |
| `hook_event_name` | `"PreToolUse"`. |
| `tool_name` | `"bash"`, given the matcher below. |
| `tool_input` | The tool's arguments — for `bash`, the `command` itself, and its `timeout`, `workdir` and `run_in_background` when they were passed. |

There is no `transcript_path`: ganja omits it (D457), so no conversation history
travels this way. Everything else in the table does, including the absolute path
of your project and any content the model put in the command line.

It goes to `https://api.typesafe.ai` unless `TYPESAFE_BASE_URL` names another
host, which must be `https` or a loopback `http`. The questions file travels in
the same request body. Together they are capped at 256 KiB; a larger body is
refused locally and never sent.

Three environment variables configure the client, and the hook's process
inherits ganja's own environment, so they must be exported where `ganja` runs —
not in a shell profile the hook never sources:

| Variable | Effect |
|---|---|
| `TYPESAFE_API_KEY` | Required. Without it `ganja evaluate` exits 3 and the hook says nothing. |
| `TYPESAFE_BASE_URL` | Optional; default `https://api.typesafe.ai`. |
| `TYPESAFE_DEFAULT_MODEL` | Optional; default `jev-latest`. `jev-preview` is the other alias. |

## Configuring it

In `ganja.toml`:

```toml
[[hooks.PreToolUse]]
matcher = "^bash$"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "./docs/recipes/typesafe-pretooluse-hook.sh annotate"
timeout = 20
```

A hook runs with the project root as its working directory, so a relative
`command` resolves from there.

The `matcher` is a regular expression tested with `is_match`
(`crates/ganja-core/src/hook.rs:636`), which is **not** anchored for you. A bare
`"bash"` also matches `bash_output` and any MCP tool whose name contains the
word, and a matcher-less entry matches every tool — including `evaluate` itself,
which would send a second copy of the model's own evaluation arguments to
TypeSafe on every call. Write `^bash$`.

Hooks run with the user's own authority and pass no permission dialog: the
config file is the trust boundary. Read the module documentation at the top of
`crates/ganja-core/src/hook.rs` before adding one.

## Exit codes, and which of them block

`ganja evaluate` has its own exit-code taxonomy. The script reads the status and
prints nothing unless it is 0, so none of these ever reaches the engine — but
they are the codes you will see when you run it by hand:

| Code | Meaning | As a hook's own exit code |
|---|---|---|
| 0 | Answered; the answers are on stdout. | Stdout is read. |
| 2 | clap's parse failure. Never ganja's own choice. | **Blocks the call** (`hook.rs:744`); stderr becomes the refusal the model reads. |
| 3 | Not configured: no key, or a refused `TYPESAFE_BASE_URL`. | Non-blocking notice. |
| 4 | The vendor refused: 401, 403, 422. | Non-blocking notice. |
| 5 | Unavailable: 429, 529, 5xx, 3xx, timeout, transport, oversized, malformed. | Non-blocking notice. |
| 64 | A bad argument of ganja's own (`EX_USAGE`). | Non-blocking notice. |

Exit 2 is the one that matters, and it is why the script never `exec`s
`ganja evaluate` and never runs it under `set -e`. A hook whose command is
`ganja evaluate --questions @questions.json` with a flag typo would exit 2 and
block the call it was only meant to comment on. The script always reaches
`exit 0`, so a missing binary, an absent key, a rate limit or a broken questions
file all degrade to exactly the behaviour you would have had with no hook: the
normal permission rules decide.

## Budgets

A hook's default budget is 60 seconds (`crates/ganja-core/src/hook.rs:89`), and
the client's deadline over the whole exchange is 10 seconds with **one attempt**
— no retry. The default therefore leaves plenty of room, and the `timeout = 20`
above is the useful setting: it bounds the one case the client's own deadline
does not, a `ganja` process that never gets as far as opening a connection. Note
that every hook matching an event runs concurrently and all of them are awaited,
so the slowest one paces the tool call.

## What the script parses

`ganja evaluate --format text` prints one line per question, in id order, in one
of three shapes:

```
destructive: noul=0.92
kind: choice=build p=0.85 confidence=0.82 (test 0.08, deploy 0.07)
risk: score=1.60 of 0..2 confidence=0.78
```

The script reads only the `noul` lines, and only those whose id appears in
`THRESHOLDS`. Anything else falls through and contributes nothing, so a question
you add without adding a threshold is inert rather than an error.

Its own JSON is built in `awk`, with every character of the reason escaped,
because a hook that prints invalid JSON is read as plain text and discarded on
this event. The escaper is general — any byte, not just the text these two
questions can produce — so that a copy of this recipe which puts more into the
reason, the judged command itself included, stays correct without touching it.
The reason deliberately does not quote the command: on `deny` the model reads it
in place of that command's output, and on `annotate` it is appended to that
command's own result, so in both cases the command is already in front of
whoever is reading. The emitted `hookSpecificOutput` also carries
`"hookEventName": "PreToolUse"`, which ganja never reads
(`crates/ganja-core/src/hook.rs:793-855`); it is there for parity with the
documented envelope this module ports.

## Thresholds

The two numbers at the top of the script are placeholders. They are not a
calibrated default and must not be treated as one. Jev returns a probability
about the question exactly as written, and where that probability lands across
the commands your agent actually proposes depends on your repository, your
system prompt and the wording in `questions.json` — none of which this recipe
knows. Run `annotate` over real work, read the numbers, then pick a `deny`
threshold, which should be the higher of the two because that variant refuses
the call.

Editing `questions.json` is the more interesting lever. The ids are never sent,
so each `instructions` must stand on its own, and evidence is referenced by
backticked path into the state — `tool_input.command`, `cwd`. Both shipped
questions end by telling the judge to ignore any claim inside the command text
about whether it is safe, which brings us to the last section.

## There is no auto-allow variant, on purpose

A third variant that printed `permissionDecision: "allow"` when the
probabilities came back low would be the obviously useful one, and it is out of
scope.

`allow` skips the ask (`crates/ganja-core/src/session.rs:4794`; it does not
overturn a `deny` rule — D458). The text being judged is a command line the
model wrote, and it is sent to the judge as it stands. A shell command is not
only its verbs: nothing stops a model from writing one whose comments, heredoc
text, `echo` strings or file names address the judge and argue that the command
is safe. An `allow` variant would therefore hand a model a way to talk its way
past its own permission dialog, and the failure would be silent — a dialog that
never appeared looks exactly like a dialog that was never needed.

The two shipped variants do not have that property. `deny` can only take
permission away, and its worst failure is an unnecessary refusal the model reads
and can argue with. `annotate` changes no decision at all. Both directions stay
consistent with what the `evaluate` tool's own description tells the model: an
answer is a probability, never the sole ground for an irreversible action.

## Trying it without a session

The script reads an envelope on stdin and prints its answer on stdout, so it
runs anywhere:

```sh
printf '%s' '{"session_id":"local","cwd":"'"$PWD"'","hook_event_name":"PreToolUse","tool_name":"bash","tool_input":{"command":"git push --force origin main"}}' \
  | ./docs/recipes/typesafe-pretooluse-hook.sh deny
```

Empty output means no question crossed its threshold, or that `ganja evaluate`
did not answer. To tell those apart, drop the `2>/dev/null` from the script or
run `ganja evaluate --format text --questions @docs/recipes/questions.json`
against the same input directly and read its exit code against the table above.
