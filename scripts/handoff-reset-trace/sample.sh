#!/usr/bin/env bash
set -euo pipefail

results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
printf 'epoch_ns\tutc\tfree_bytes\n' > "$results/disk-samples.tsv"
while true; do
  printf '%s\t%s\t%s\n' "$(date +%s%N)" "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" "$(free_bytes /)" >> "$results/disk-samples.tsv"
  for name in capture kernel; do
    pidfile="$results/observers/$name.pid"
    if [[ -s "$pidfile" && ! -e "$results/observers/$name.not-running" ]]; then
      observer="$(< "$pidfile")"
      if [[ ! -d "/proc/$observer" ]]; then
        printf 'Observer process absent at %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" > "$results/observers/$name.not-running"
      fi
    fi
  done
  sleep 1
done
