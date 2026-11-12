#!/usr/bin/env bash
set -euo pipefail

directory="${1:?iteration directory}"
exec 3> "$directory/tests.txt"
exec 4> "$directory/status-received.tsv"
while IFS= read -r line; do
  printf '%s\n' "$line" >&3
  if [[ "$line" =~ ^[[:space:]]*(PASS|FAIL|TIMEOUT|XPASS|LEAK|ABORT|CRASH)[[:space:]] ]]; then
    # These are observer receipt times; the original line retains test duration.
    printf '%s\t%s\n' "$(date +%s%N)" "$line" >&4
  fi
done
