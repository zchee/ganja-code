#!/usr/bin/env bash
set -uo pipefail

directory="${1:?iteration directory}"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
exec 3> "$directory/tests.txt" || { note 'Test log could not open; continue draining reporter output.'; exec 3> /dev/null; }
exec 4> "$directory/status-received.tsv" || { note 'Result timing log could not open.'; exec 4> /dev/null; }
exec 5> "$directory/command-prelude.txt" || { note 'Command prelude log could not open.'; exec 5> /dev/null; }
prelude=1
while IFS= read -r line; do
  if (( prelude == 1 )); then
    printf '%s\n' "$line" >&5 || { note 'Command prelude write failed.'; exec 5> /dev/null; }
    if [[ "$line" =~ ^[[:space:]]*Starting[[:space:]] ]]; then
      printf '%s\t%s\n' "$(date +%s%N)" "$line" > "$directory/test-phase-start.tsv" || note 'Test phase timestamp write failed.'
      prelude=0
      exec 5>&-
    fi
  fi
  if ! printf '%s\n' "$line" >&3; then
    note 'Test log write failed; continue draining reporter output.'
    exec 3> /dev/null
  fi
  if [[ "$line" =~ ^[[:space:]]*(PASS|FAIL|TIMEOUT|XPASS|LEAK|ABORT|CRASH)[[:space:]] ]]; then
    if ! printf '%s\t%s\n' "$(date +%s%N)" "$line" >&4; then
      note 'Result receipt timestamp write failed.'
      exec 4> /dev/null
    fi
  fi
done
exit 0
