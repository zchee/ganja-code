#!/bin/sh
# A PreToolUse hook that asks TypeSafe System One about a shell command before
# ganja runs it. See typesafe-pretooluse-hook.md in this directory for what the
# two variants can and cannot do, and for what leaves the machine.
#
# Usage: typesafe-pretooluse-hook.sh [annotate|deny]
#
# POSIX sh and awk only, no jq. It reads the hook envelope on its standard
# input and always exits 0: whatever goes wrong here, the session must end up
# exactly where it would have been with no hook at all.

# Every number below is parsed and printed by `awk`, whose string-to-number
# conversion honours LC_NUMERIC in mawk and in `gawk --posix`. Under a
# comma-decimal locale `"0.92" + 0` stops at the `.` and yields 0, so nothing
# ever clears a threshold and this hook goes permanently, silently inert --
# exit 0 and empty stdout, indistinguishable from "no question crossed". That
# is a failure OPEN, which is the direction that matters here, so the locale
# is pinned rather than trusted.
export LC_ALL=C

# Thresholds, as `<question id>=<probability>` pairs separated by spaces. A
# question whose id is not listed here is read out of the answer and ignored.
#
# THESE TWO NUMBERS ARE PLACEHOLDERS AND MUST BE MEASURED. They are not a
# default anybody calibrated: Jev answers a probability about the question as
# written, and how that probability lands across the commands your agent
# actually proposes is a property of your repository, your instructions and
# your questions file, not of this script. Run the `annotate` variant over a
# few days of real work first, read the numbers it reports, and only then pick
# a `deny` threshold -- which should be the higher of the two, because that
# variant refuses the call.
THRESHOLDS="destructive=0.90 secret_exposure=0.90"

# `annotate` (the default) or `deny`. The first argument wins, then
# GANJA_TYPESAFE_HOOK_VARIANT, then the default. An unknown word prints
# nothing, which leaves the call to the normal permission rules.
variant=${1:-${GANJA_TYPESAFE_HOOK_VARIANT:-annotate}}

# The questions file ships beside this script. Point the variable somewhere
# else to ask your own questions; the ids must match THRESHOLDS above.
questions=${GANJA_TYPESAFE_HOOK_QUESTIONS:-"$(dirname "$0")/questions.json"}

case $variant in
annotate | deny) ;;
*) exit 0 ;;
esac

# One attempt, and the status decides everything. `set -e` is deliberately
# absent, and so is `exec`: this script must reach its own `exit 0` even when
# `ganja evaluate` is missing, unconfigured, refused or unreachable.
#
# stdin is the hook envelope, which `ganja evaluate` reads as the state because
# `--state` defaults to `-`. Dropping the `2>/dev/null` while you are setting
# this up is how you see why a call failed; ganja itself never reads the stderr
# of a hook that exits 0.
answers=$(ganja evaluate --format text --questions "@$questions" 2>/dev/null)
status=$?

# Exit 3 (not configured), 4 (the vendor refused), 5 (unavailable) and 64 (a
# bad argument) all mean the same thing here: no judgement, so say nothing and
# let the call be decided the way it would have been anyway.
[ "$status" -eq 0 ] || exit 0

# `ganja evaluate --format text` prints one line per question, in id order:
#
#     destructive: noul=0.92
#     kind: choice=build p=0.85 confidence=0.82 (test 0.08, deploy 0.07)
#     risk: score=1.60 of 0..2 confidence=0.78
#
# Only the `noul` lines are read below. A line in any other shape, and a line
# whose id is not in THRESHOLDS, falls through and contributes nothing, so an
# answer this script does not understand is silently the same as no answer.
printf '%s\n' "$answers" | awk -v variant="$variant" -v thresholds="$THRESHOLDS" '
# Any text to a JSON string literal. The reason below is assembled from output
# this script did not write, and a hook that prints invalid JSON is read as
# plain text and dropped, so escaping it properly is what keeps the two
# variants working on arbitrary command text.
function jstring(s,   out, i, n, c, code) {
    out = "\""
    n = length(s)
    for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\"") { out = out "\\\""; continue }
        if (c == "\\") { out = out "\\\\"; continue }
        # A character outside the ASCII table is a byte of something
        # multi-byte, which is legal in a JSON string exactly as it stands.
        code = (c in ORD) ? ORD[c] : 128
        if (code < 32 || code == 127) { out = out sprintf("\\u%04x", code); continue }
        out = out c
    }
    return out "\""
}

BEGIN {
    for (i = 1; i < 128; i++) ORD[sprintf("%c", i)] = i

    n = split(thresholds, pairs, " ")
    for (i = 1; i <= n; i++) {
        eq = index(pairs[i], "=")
        if (eq < 2) continue
        limit[substr(pairs[i], 1, eq - 1)] = substr(pairs[i], eq + 1) + 0
    }
    flagged = 0
    reason = ""
}

NF >= 2 {
    id = $1
    if (substr(id, length(id), 1) != ":") next
    id = substr(id, 1, length(id) - 1)
    if (!(id in limit)) next
    if (substr($2, 1, 5) != "noul=") next
    value = substr($2, 6) + 0
    if (value < limit[id]) next
    if (flagged > 0) reason = reason "; "
    reason = reason sprintf("%s %.2f, over a threshold of %.2f", id, value, limit[id])
    flagged++
}

END {
    if (flagged == 0) exit
    if (variant == "deny") {
        text = "Refused before it ran by a PreToolUse hook. TypeSafe System One answered " reason ". That is a calibrated probability about the command as written, not a finding about what it would have done. Rewrite the command so it no longer reads that way, or explain why it is safe and let the person decide."
        printf "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":%s}}\n", jstring(text)
    } else {
        text = "TypeSafe System One was asked about this command before it ran and answered " reason ". That is a calibrated probability about the command as written, not a finding, and the command has already run. Check what it actually did before building on it."
        printf "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"additionalContext\":%s}}\n", jstring(text)
    }
}
'

exit 0
