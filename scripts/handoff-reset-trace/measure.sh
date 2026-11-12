#!/usr/bin/env bash
set -euo pipefail

rounds="${1:?execution count}"
condition="${2:?condition name}"
results="$PWD/target/handoff-reset-results"
binary="$(< "$results/test-binary.txt")"
mkdir -p "$results/$condition"
printf 'execution\texit\tstarted_utc\tfinished_utc\n' > "$results/$condition/counts.tsv"
failures=0
started="$(date +%s)"
for ((execution=1; execution<=rounds; execution++)); do
  # Leave room for artifact upload within the job's explicit budget.
  if (( $(date +%s) - started >= 420 )); then
    break
  fi
  directory="$results/$condition/execution-$execution"
  mkdir -p "$directory"
  before="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  nstat -az > "$directory/counters-before.txt"
  status=0
  timeout --signal=TERM --kill-after=10s 45s "$binary" --exact the_initializer_and_replacement_server_announce_the_same_ephemeral_port --nocapture > "$directory/test.txt" 2>&1 || status=$?
  nstat -az > "$directory/counters-after.txt"
  after="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  printf '%s\t%s\t%s\t%s\n' "$execution" "$status" "$before" "$after" >> "$results/$condition/counts.tsv"
  if (( status == 0 )); then
    rm -r "$directory"
  else
    failures=$((failures + 1))
    printf 'FAIL condition=%s execution=%s exit=%s\n' "$condition" "$execution" "$status"
  fi
  if (( execution % 50 == 0 )); then
    printf 'condition=%s executions=%s failures=%s\n' "$condition" "$execution" "$failures"
  fi
done
completed=$(( $(wc -l < "$results/$condition/counts.tsv") - 1 ))
printf 'condition=%s executions=%s failures=%s\n' "$condition" "$completed" "$failures" | tee "$results/$condition/summary.txt"
# Measurement failures are records, not a request to rerun or hide a red result.
(( failures == 0 ))
