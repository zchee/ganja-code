#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -euo pipefail

results="$PWD/target/handoff-reset-results"
mkdir -p "$results/load"
round=0
while [[ ! -e "$results/load/stop" ]]; do
  round=$((round + 1))
  log="$results/load/suite-$round.txt"
  date -u '+%Y-%m-%d %H:%M:%S UTC' > "$log"
  nstat -az > "$results/load/suite-$round-before.txt"
  status=0
  env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest run --locked --workspace --profile ci >> "$log" 2>&1 || status=$?
  nstat -az > "$results/load/suite-$round-after.txt"
  printf 'suite=%s exit=%s finished=%s\n' "$round" "$status" "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" >> "$results/load/counts.txt"
  grep 'Summary' "$log" >> "$results/load/counts.txt" || true
  if (( status != 0 )); then
    sudo -n ss -ntap > "$results/load/suite-$round-sockets.txt" 2>&1 || true
    grep -E '^[[:space:]]*(FAIL|TIMEOUT|XPASS|LEAK|ABORT|CRASH)[[:space:]]' "$log" > "$results/load/suite-$round-failures.txt" || true
    if [[ -s "$results/load/suite-$round-failures.txt" ]] && grep -q 'same client reaches replacement server:.*ConnectionReset' "$log" && ! grep -v 'ganja-cli::worker_init the_initializer_and_replacement_server_announce_the_same_ephemeral_port' "$results/load/suite-$round-failures.txt"; then
      touch "$results/load/target-failed"
      continue
    fi
    touch "$results/load/failed"
    exit "$status"
  fi
  rm "$log" "$results/load/suite-$round-before.txt" "$results/load/suite-$round-after.txt"
done
