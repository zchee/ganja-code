# Decision ledger (after the 2026-09-23 freeze)

Every decision made after commit `35d1720` is recorded here, in full, in the
order it landed. The frozen files beside this one are never edited; this file
is append-only. `AGENTS.md` files carry no decision history and point here.

Rules:

- Numbers continue the `D` series. Take the next unused number across this
  file and the frozen files (`rg -o 'D[0-9]{3}' docs/decisions | sort -u`),
  and use the same number in the plan, the commit and the code comment that
  cite it. Execution-time deviations keep the `Dv-<n>` form, numbered per
  landing and named with the decision they deviate from.
- One entry per decision, appended at the end. Amending a decision is a new
  entry that names the one it amends; the old entry is not rewritten.
- State the decision as behaviour the code exhibits, and name the test or
  file that pins it. Rationale stays short; the plan holds the long form.
- No secret, no token, no account name.

Entry template:

```markdown
## D<nnn> — <title> (<YYYY-MM-DD from `date`>)

Crates: <crate>, <crate>
Amends: D<mmm> (or "none")

<What was decided, as the behaviour the code now has.>

Pinned by: `crates/<crate>/src/<file>_tests.rs` (`<test name>`)
Commit: <short sha>
```

Entries follow.
