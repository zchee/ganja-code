#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -uo pipefail

condition="${1:?condition name}"
deadline="${2:?deadline epoch seconds}"
results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
mkdir -p "$results/$condition" || note "Condition directory unavailable: $condition."
command=(env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest run --locked --workspace --profile ci)
if [[ "$condition" == neighbourhood ]]; then command+=(-E 'binary(worker_init)'); fi
iteration=0
while (( $(date +%s) < deadline )); do
  iteration=$((iteration + 1))
  directory="$results/$condition/iteration-$iteration"
  mkdir -p "$directory" || note "Iteration directory unavailable: $condition/$iteration."
  record_disk "$condition-$iteration-before"
  require_space || exit $?
  before_free="$(free_bytes /)"
  started_ns="$(date +%s%N)"
  printf '%s\n' "$started_ns" > "$directory/started-epoch-ns.txt" || note 'Iteration start timestamp unavailable.'
  nstat -az > "$directory/counters-before.txt" 2>&1 || note 'Initial counters unavailable.'
  target_before=unknown
  if [[ "$condition" == neighbourhood && "$iteration" == 1 ]]; then
    binary="$(< "$results/test-binary.txt")"
    if output="$(du -sb --exclude=handoff-reset-results target 2>/dev/null)"; then read -r target_before _ <<< "$output"; else note 'Initial build-directory size unavailable; binary identity will still be checked.'; fi
    printf '%s\n' "$target_before" > "$directory/build-target-before.txt" || note 'Initial artifact-size record unavailable.'
    if ! { stat -c '%s %n' target/debug/ganja "$binary" && sha256sum target/debug/ganja "$binary"; } > "$directory/build-before.txt"; then
      note 'Initial binary identity check failed.'
      exit 1
    fi
  fi
  "${command[@]}" 2>&1 | bash scripts/handoff-reset-trace/record.sh "$directory"
  status="${PIPESTATUS[0]}"
  if [[ "$condition" == neighbourhood && "$iteration" == 1 ]]; then
    target_after=unknown
    if output="$(du -sb --exclude=handoff-reset-results target 2>/dev/null)"; then read -r target_after _ <<< "$output"; else note 'Final build-directory size unavailable; binary identity will still be checked.'; fi
    printf '%s\n' "$target_after" > "$directory/build-target-after.txt" || note 'Final artifact-size record unavailable.'
    if ! { stat -c '%s %n' target/debug/ganja "$binary" && sha256sum target/debug/ganja "$binary"; } > "$directory/build-after.txt"; then
      note 'Final binary identity check failed.'
      exit 1
    fi
    changed=0
    cmp -s "$directory/build-before.txt" "$directory/build-after.txt" || changed=1
    if [[ "$target_before" =~ ^[0-9]+$ && "$target_after" =~ ^[0-9]+$ ]] && [[ "$target_before" != "$target_after" ]]; then changed=1; fi
    if (( changed == 1 )); then
      diff -u "$directory/build-before.txt" "$directory/build-after.txt" > "$directory/build-difference.txt" || note 'Binary identity difference recorded or diff unavailable.'
      touch "$results/unexpected-build-change" || note 'Identity-change marker unavailable.'
      note "Build identity changed; target bytes before=$target_before after=$target_after."
      exit 1
    fi
  fi
  finished_ns="$(date +%s%N)"
  after_free="$(free_bytes /)"
  printf '%s\n' "$finished_ns" > "$directory/finished-epoch-ns.txt" || note 'Iteration finish timestamp unavailable.'
  nstat -az > "$directory/counters-after.txt" 2>&1 || note 'Final counters unavailable.'
  record_disk "$condition-$iteration-after"
  awk -F '\t' -v start="$started_ns" -v end="$finished_ns" 'NR == 1 || ($1 >= start && $1 <= end)' "$results/disk-samples.tsv" > "$directory/disk-window.tsv" || note 'Iteration disk window unavailable.'
  minimum="$(awk -F '\t' -v before="$before_free" -v after="$after_free" 'BEGIN { low = -1; if (before ~ /^[0-9]+$/) low = before; if (after ~ /^[0-9]+$/ && (low < 0 || after < low)) low = after } NR > 1 && $3 ~ /^[0-9]+$/ && (low < 0 || $3 < low) { low = $3 } END { if (low < 0) print "unknown"; else printf "%.0f", low }' "$directory/disk-window.tsv")" || note 'Iteration minimum calculation unavailable.'
  minimum="${minimum:-unknown}"
  copy_bytes="$(binary_bytes)"
  printf 'minimum_free_bytes=%s binary_copy_bytes=%s\n' "$minimum" "$copy_bytes" > "$directory/disk-minimum.txt" || note 'Iteration disk minimum unavailable.'
  awk 'NR == FNR { before[$1] = $2; next } /^[A-Za-z]/ && $2 != before[$1] { printf "%s\t%.0f\n", $1, $2 - before[$1] }' "$directory/counters-before.txt" "$directory/counters-after.txt" > "$directory/counter-deltas.tsv" || note 'Counter delta unavailable.'
  awk '/^[[:space:]]*Summary / { exit } $1 ~ /^(PASS|FAIL|TIMEOUT|XPASS|LEAK|ABORT|CRASH)$/ { for (i = 2; i < NF; i++) if ($i ~ /^ganja-/ || $i ~ /^tmux($|::)/) { print $1 "\t" $i "\t" $(i+1); break } }' "$directory/tests.txt" > "$directory/test-counts.tsv" || note 'Per-test counts unavailable.'
  printf 'iteration=%s exit=%s minimum_free_bytes=%s finished=%s\n' "$iteration" "$status" "$minimum" "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" >> "$results/$condition/counts.txt" || note 'Iteration count record unavailable.'
  grep 'Summary' "$directory/tests.txt" | tee -a "$results/$condition/counts.txt" || note 'Iteration summary unavailable.'
  if (( status == 0 )); then
    rm -f "$directory/tests.txt" "$directory/counters-before.txt" "$directory/counters-after.txt" || note 'Passing diagnostic cleanup failed.'
    continue
  fi
  touch "$results/iteration-failed" || note 'Failure marker unavailable.'
  record_disk "$condition-$iteration-failure"
  sudo -n ss -ntap > "$directory/sockets-after.txt" 2>&1 || note 'Socket owners unavailable.'
  awk '$1 != "PASS"' "$directory/test-counts.tsv" > "$directory/failures.tsv" || note 'Failing test names unavailable.'
  target_hit=0
  if grep -q 'same client reaches replacement server:.*ConnectionReset' "$directory/tests.txt" && grep -q 'ganja-cli::worker_init[[:space:]]the_initializer_and_replacement_server_announce_the_same_ephemeral_port' "$directory/failures.tsv"; then
    touch "$results/target-reset-recorded" || note 'Reset marker unavailable.'
    grep -o 'ganja server listening on http://0\.0\.0\.0:[0-9]*' "$directory/tests.txt" | grep -o '[0-9]*$' | sort -u > "$directory/worker-ports.txt" || note 'Worker announcement ports unavailable.'
    while IFS= read -r port; do
      for capture in "$results/observers"/control.pcap*; do
        if [[ ! -f "$capture" ]]; then continue; fi
        tcpdump -nn -tt -r "$capture" "tcp port $port" 2>> "$directory/capture-decode.txt" | awk -v start="$started_ns" -v end="$finished_ns" '$1 >= start / 1000000000 && $1 <= end / 1000000000' >> "$directory/port-$port-packets.txt" || note "Port $port capture slice unavailable."
      done
    done < "$directory/worker-ports.txt"
    target_hit=1
  fi
  disk_classified=0
  if grep -q 'No space left on device\|StorageFull' "$directory/tests.txt"; then disk_classified=1; fi
  if [[ "$minimum" =~ ^[0-9]+$ && "$copy_bytes" =~ ^[0-9]+$ ]] && (( minimum < copy_bytes )); then disk_classified=1; fi
  if (( disk_classified == 1 )); then
    printf 'Disk classification: explicit ENOSPC or measured free bytes below one binary copy; not a reset mechanism verdict.\n' > "$directory/disk-classification.txt" || note 'Disk classification record unavailable.'
    continue
  fi
  if (( target_hit == 1 )) && ! grep -v 'ganja-cli::worker_init[[:space:]]the_initializer_and_replacement_server_announce_the_same_ephemeral_port' "$directory/failures.tsv"; then continue; fi
  touch "$results/unexpected-failure" || note 'Unexpected-failure marker unavailable.'
  note 'Unexpected test failure outside approved disk classification; stop before rerunning.'
  exit 1
done
exit 0
