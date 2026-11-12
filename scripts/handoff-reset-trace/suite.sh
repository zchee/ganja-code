#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -euo pipefail

condition="${1:?condition name}"
deadline="${2:?deadline epoch seconds}"
results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
mkdir -p "$results/$condition"
command=(env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest run --locked --profile ci)
if [[ "$condition" == neighbourhood ]]; then
  command+=(-p ganja-cli --test worker_init)
else
  command+=(--workspace)
fi
iteration=0
while (( $(date +%s) < deadline )); do
  iteration=$((iteration + 1))
  directory="$results/$condition/iteration-$iteration"
  mkdir -p "$directory"
  record_disk "$condition-$iteration-before"
  if ! require_space; then
    printf 'Disk floor reached before an iteration; no test started.\n' > "$directory/environment-stop.txt"
    exit 78
  fi
  date -u '+%Y-%m-%dT%H:%M:%S.%NZ' > "$directory/started.txt"
  nstat -az > "$directory/counters-before.txt"
  status=0
  "${command[@]}" > "$directory/tests.txt" 2>&1 || status=$?
  nstat -az > "$directory/counters-after.txt"
  date -u '+%Y-%m-%dT%H:%M:%S.%NZ' > "$directory/finished.txt"
  record_disk "$condition-$iteration-after"
  awk 'NR == FNR { before[$1] = $2; next } /^[A-Za-z]/ && $2 != before[$1] { printf "%s\t%.0f\n", $1, $2 - before[$1] }' "$directory/counters-before.txt" "$directory/counters-after.txt" > "$directory/counter-deltas.tsv"
  awk '/^[[:space:]]*Summary / { exit } $1 ~ /^(PASS|FAIL|TIMEOUT|XPASS|LEAK|ABORT|CRASH)$/ { for (i = 2; i < NF; i++) if ($i ~ /^ganja-/ || $i ~ /^tmux($|::)/) { print $1 "\t" $i "\t" $(i+1); break } }' "$directory/tests.txt" > "$directory/test-counts.tsv"
  printf 'iteration=%s exit=%s finished=%s\n' "$iteration" "$status" "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" >> "$results/$condition/counts.txt"
  grep 'Summary' "$directory/tests.txt" >> "$results/$condition/counts.txt" || true
  if (( status == 0 )); then
    rm "$directory/tests.txt" "$directory/counters-before.txt" "$directory/counters-after.txt"
    continue
  fi
  touch "$results/iteration-failed"
  record_disk "$condition-$iteration-failure"
  sudo -n ss -ntap > "$directory/sockets-after.txt" 2>&1 || true
  awk '$1 != "PASS"' "$directory/test-counts.tsv" > "$directory/failures.tsv"
  if [[ -s "$directory/failures.tsv" ]] && grep -q 'same client reaches replacement server:.*ConnectionReset' "$directory/tests.txt" && ! grep -v 'ganja-cli::worker_init[[:space:]]the_initializer_and_replacement_server_announce_the_same_ephemeral_port' "$directory/failures.tsv"; then
    touch "$results/target-reset-recorded"
    continue
  fi
  if grep -q 'No space left on device\|StorageFull' "$directory/tests.txt" && ! require_space; then
    printf 'Measured storage exhaustion; any adjacent unexplained assertion is only inferred to share that cause.\n' > "$directory/environment-cause.txt"
    choose_test_temp
    record_disk "$condition-$iteration-temp-reselected"
    if require_space; then continue; fi
  fi
  touch "$results/unexpected-failure"
  printf 'Unexpected failure without a recoverable measured harness cause; stop before rerunning.\n' >&2
  exit 1
done
