# ganja-code

Glossary for ganja's domain vocabulary — the canonical term for each concept and
the words to avoid. Definitions only; implementation detail lives in `AGENTS.md`
and decisions live there as D-numbers. Seeded from the multi-agent surface
(teams, teammates, orchestration); grows lazily as terms are resolved.

## Language

### Multi-agent

**Teammate**:
A persistent named agent that outlives the call that started it, registered as
a member of the session's team and addressed through mailboxes.
_Avoid_: worker, peer agent

**Subagent**:
A child agent bound to a single `task` tool call; its result returns to the
caller and its life ends with the call.
_Avoid_: worker, teammate (for call-scoped children)

**Lead**:
The session that owns a team: it spawns teammates and is the only address a
teammate's `send_message` may target.
_Avoid_: orchestrator, coordinator, team-lead (Claude Code's literal)

**Member**:
An entry in a team's roster — the lead or a teammate.

**Team**:
The set of members one lead session owns, named `session-<8 hex>`. A team
exists once a teammate is spawned; a session whose registry holds nobody is
teamless.

**Backend**:
The mechanism a teammate runs on: `in-process`, `ganja`, `claude`, `codex`,
`grok`, or `agy`. Also called the teammate's *surface*.
_Avoid_: provider (that is a model vendor), worker type, execution mode

**Agent** (persona):
A named prompt-and-rules identity a session or teammate runs as — `build`,
`plan`, `general`, `explore`, the six roles a team run routes its stages to
(`analyst`, `executor`, `verifier`, `critic`, `debugger`, `reviewer`), or one
the config adds.
_Avoid_: role, agent-type (OMC's word, which mixes personas and providers)

**task tool**:
The delegation door: one call either runs a subagent to completion or — given
`name` — spawns a teammate. Always spelled "the `task` tool", never bare
"task", which is reserved for shared task-list entries.

**Task**:
An entry in a team's shared task list: one piece of work a member will do,
carrying an owner, dependencies, and a lifecycle from pending to completed.
_Avoid_: todo (the private per-session list), ticket, work item

**Task list**:
The shared list of Tasks that the lead and teammates coordinate through, kept
in the team's own directory. It belongs to a team but does not wait for one:
it is created with its first Task, which a lead files before spawning whoever
will do the work.
_Avoid_: todo list

**Stage**:
One of the five phases of a team run: `team-plan`, `team-prd`, `team-exec`,
`team-verify`, `team-fix`. A stage is not ganja's plan mode; the two are
unrelated.
_Avoid_: phase

**Pipeline**:
The staged flow of a team run — `team-plan` through `team-fix`, with the
verify/fix loop at its tail.

**Handoff**:
A document the lead writes at a stage transition recording what was decided,
rejected, risked, touched, and left remaining, so a later session resumes
from record rather than memory.
