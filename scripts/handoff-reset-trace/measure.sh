#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -euo pipefail

rounds="${1:?execution count}"
condition="${2:?condition name}"
results="$PWD/target/handoff-reset-results"
binary="$(< "$results/test-binary.txt")"
mkdir -p "$results/$condition"
printf 'execution\texit\ttimeout_pid\tstarted_utc\tfinished_utc\n' > "$results/$condition/counts.tsv"
failures=0
started="$(date +%s)"
for ((execution=1; execution<=rounds; execution++)); do
  # Leave room for artifact upload within the job's explicit budget.
  if (( $(date +%s) - started >= ${MEASURE_SECONDS:-420} )); then
    break
  fi
  if [[ -n "${LOAD_PID:-}" ]] && ! kill -0 "$LOAD_PID" 2>/dev/null; then
    printf 'Real-suite load stopped; end measurement rather than continuing unloaded.\n' >&2
    break
  fi
  directory="$results/$condition/execution-$execution"
  mkdir -p "$directory"
  before="$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')"
  nstat -az > "$directory/counters-before.txt"
  status=0
  timeout --signal=TERM --kill-after=10s 45s "$binary" --exact the_initializer_and_replacement_server_announce_the_same_ephemeral_port --nocapture > "$directory/test.txt" 2>&1 &
  timeout_pid=$!
  wait "$timeout_pid" || status=$?
  nstat -az > "$directory/counters-after.txt"
  after="$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')"
  printf '%s\t%s\t%s\t%s\t%s\n' "$execution" "$status" "$timeout_pid" "$before" "$after" >> "$results/$condition/counts.tsv"
  if (( status == 0 )); then
    rm -r "$directory"
  else
    failures=$((failures + 1))
    if command -v ss >/dev/null; then
      sudo -n ss -ntap > "$directory/sockets-after.txt" 2>&1 || true
    fi
    printf 'FAIL condition=%s execution=%s exit=%s\n' "$condition" "$execution" "$status"
    if [[ -n "${LOAD_PID:-}" ]] && ! grep -q 'same client reaches replacement server:.*ConnectionReset' "$directory/test.txt"; then
      printf 'Unexpected target failure; stop before another execution.\n' >&2
      touch "$results/unexpected-failure"
      break
    fi
  fi
  if (( execution % 50 == 0 )); then
    printf 'condition=%s executions=%s failures=%s\n' "$condition" "$execution" "$failures"
  fi
done
completed=$(( $(wc -l < "$results/$condition/counts.tsv") - 1 ))
printf 'condition=%s executions=%s failures=%s\n' "$condition" "$completed" "$failures" | tee "$results/$condition/summary.txt"
# Measurement failures are records, not a request to rerun or hide a red result.
(( failures == 0 ))
