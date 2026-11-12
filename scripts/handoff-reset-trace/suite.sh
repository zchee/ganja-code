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
command=(env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest run --locked --workspace --profile ci)
if [[ "$condition" == neighbourhood ]]; then command+=(-E 'binary(worker_init)'); fi
iteration=0
while (( $(date +%s) < deadline )); do
  iteration=$((iteration + 1))
  directory="$results/$condition/iteration-$iteration"
  mkdir -p "$directory"
  record_disk "$condition-$iteration-before"
  if ! require_space; then
    printf 'Diagnostic-output floor reached; no test started.\n' > "$directory/environment-stop.txt"
    exit 78
  fi
  before_free="$(free_bytes /)"
  started_ns="$(date +%s%N)"
  printf '%s\n' "$started_ns" > "$directory/started-epoch-ns.txt"
  nstat -az > "$directory/counters-before.txt"
  if [[ "$condition" == neighbourhood && "$iteration" == 1 ]]; then
    binary="$(< "$results/test-binary.txt")"
    {
      du -sb --exclude=handoff-reset-results target
      stat -c '%s %n' target/debug/ganja "$binary"
      sha256sum target/debug/ganja "$binary"
    } > "$directory/build-before.txt"
  fi
  status=0
  "${command[@]}" 2>&1 | bash scripts/handoff-reset-trace/record.sh "$directory" || status=$?
  if [[ "$condition" == neighbourhood && "$iteration" == 1 ]]; then
    {
      du -sb --exclude=handoff-reset-results target
      stat -c '%s %n' target/debug/ganja "$binary"
      sha256sum target/debug/ganja "$binary"
    } > "$directory/build-after.txt"
    if ! cmp -s "$directory/build-before.txt" "$directory/build-after.txt"; then
      diff -u "$directory/build-before.txt" "$directory/build-after.txt" > "$directory/build-difference.txt" || true
      touch "$results/unexpected-build-change"
      printf 'Workspace build-artifact size or binary identity changed; stop before another iteration.\n' >&2
      exit 1
    fi
  fi
  finished_ns="$(date +%s%N)"
  after_free="$(free_bytes /)"
  printf '%s\n' "$finished_ns" > "$directory/finished-epoch-ns.txt"
  nstat -az > "$directory/counters-after.txt"
  record_disk "$condition-$iteration-after"
  awk -F '\t' -v start="$started_ns" -v end="$finished_ns" 'NR == 1 || ($1 >= start && $1 <= end)' "$results/disk-samples.tsv" > "$directory/disk-window.tsv"
  minimum="$(awk -F '\t' -v before="$before_free" -v after="$after_free" 'BEGIN { low = before < after ? before : after } NR > 1 && $3 < low { low = $3 } END { printf "%.0f", low }' "$directory/disk-window.tsv")"
  printf 'minimum_free_bytes=%s binary_copy_bytes=%s\n' "$minimum" "$(binary_bytes)" > "$directory/disk-minimum.txt"
  awk 'NR == FNR { before[$1] = $2; next } /^[A-Za-z]/ && $2 != before[$1] { printf "%s\t%.0f\n", $1, $2 - before[$1] }' "$directory/counters-before.txt" "$directory/counters-after.txt" > "$directory/counter-deltas.tsv"
  awk '/^[[:space:]]*Summary / { exit } $1 ~ /^(PASS|FAIL|TIMEOUT|XPASS|LEAK|ABORT|CRASH)$/ { for (i = 2; i < NF; i++) if ($i ~ /^ganja-/ || $i ~ /^tmux($|::)/) { print $1 "\t" $i "\t" $(i+1); break } }' "$directory/tests.txt" > "$directory/test-counts.tsv"
  printf 'iteration=%s exit=%s minimum_free_bytes=%s finished=%s\n' "$iteration" "$status" "$minimum" "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" >> "$results/$condition/counts.txt"
  grep 'Summary' "$directory/tests.txt" >> "$results/$condition/counts.txt" || true
  if (( status == 0 )); then
    rm "$directory/tests.txt" "$directory/counters-before.txt" "$directory/counters-after.txt"
    continue
  fi
  touch "$results/iteration-failed"
  record_disk "$condition-$iteration-failure"
  sudo -n ss -ntap > "$directory/sockets-after.txt" 2>&1 || true
  awk '$1 != "PASS"' "$directory/test-counts.tsv" > "$directory/failures.tsv"
  target_hit=0
  if grep -q 'same client reaches replacement server:.*ConnectionReset' "$directory/tests.txt" && grep -q 'ganja-cli::worker_init[[:space:]]the_initializer_and_replacement_server_announce_the_same_ephemeral_port' "$directory/failures.tsv"; then
    touch "$results/target-reset-recorded"
    grep -o 'ganja server listening on http://0\.0\.0\.0:[0-9]*' "$directory/tests.txt" | grep -o '[0-9]*$' | sort -u > "$directory/worker-ports.txt"
    while IFS= read -r port; do
      for capture in "$results/observers"/control.pcap*; do
        if [[ ! -f "$capture" ]]; then continue; fi
        tcpdump -nn -tt -r "$capture" "tcp port $port" 2>> "$directory/capture-decode.txt" | awk -v start="$started_ns" -v end="$finished_ns" '$1 >= start / 1000000000 && $1 <= end / 1000000000' >> "$directory/port-$port-packets.txt" || true
      done
    done < "$directory/worker-ports.txt"
    target_hit=1
  fi
  if grep -q 'No space left on device\|StorageFull' "$directory/tests.txt" || (( minimum < $(binary_bytes) )); then
    printf 'Disk classification under the approved rule: explicit ENOSPC or measured free bytes below one binary copy. This does not establish the health-reset mechanism.\n' > "$directory/disk-classification.txt"
    continue
  fi
  if (( target_hit == 1 )) && ! grep -v 'ganja-cli::worker_init[[:space:]]the_initializer_and_replacement_server_announce_the_same_ephemeral_port' "$directory/failures.tsv"; then continue; fi
  touch "$results/unexpected-failure"
  printf 'Unexpected failure outside the approved disk classification; stop before rerunning.\n' >&2
  exit 1
done
