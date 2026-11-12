#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -uo pipefail

deadline="${1:?deadline epoch seconds}"
results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
round=0
while (( round == 0 || $(date +%s) < deadline )); do
  round=$((round + 1))
  condition=ci-initial
  if (( round > 1 )); then condition=ci-cold; fi
  directory="$results/$condition/round-$round"
  mkdir -p "$directory" || note "Round directory unavailable: $condition/$round."
  record_disk "$condition-$round-before"
  require_space || exit $?
  if (( round > 1 )); then
    sync_status=0
    sync > "$directory/cache-drop.txt" 2>&1 || sync_status=$?
    drop_status=0
    sudo -n bash -c 'printf "3\n" > /proc/sys/vm/drop_caches' >> "$directory/cache-drop.txt" 2>&1 || drop_status=$?
    printf 'sync_exit=%s drop_caches_exit=%s finished=%s\n' "$sync_status" "$drop_status" "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" >> "$directory/cache-drop.txt" || note 'Cache-drop status unavailable.'
    if (( sync_status != 0 || drop_status != 0 )); then note "Cold-cache preparation not confirmed for round $round: sync=$sync_status drop=$drop_status; continuing."; fi
  fi
  before_free="$(free_bytes /)"
  started_ns="$(date +%s%N)"
  printf '%s\n' "$started_ns" > "$directory/started-epoch-ns.txt" || note 'Round start timestamp unavailable.'
  nstat -az > "$directory/counters-before.txt" 2>&1 || note 'Initial counters unavailable.'
  env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest run --locked --workspace --profile ci 2>&1 | bash scripts/handoff-reset-trace/record.sh "$directory"
  status="${PIPESTATUS[0]}"
  finished_ns="$(date +%s%N)"
  after_free="$(free_bytes /)"
  printf '%s\n' "$finished_ns" > "$directory/finished-epoch-ns.txt" || note 'Round finish timestamp unavailable.'
  nstat -az > "$directory/counters-after.txt" 2>&1 || note 'Final counters unavailable.'
  record_disk "$condition-$round-after"
  awk -F '\t' -v start="$started_ns" -v end="$finished_ns" 'NR == 1 || ($1 >= start && $1 <= end)' "$results/system-samples.tsv" > "$directory/system-window.tsv" || note 'Round system sample window unavailable.'
  minimum="$(awk -F '\t' -v before="$before_free" -v after="$after_free" 'BEGIN { low=-1; if(before ~ /^[0-9]+$/) low=before; if(after ~ /^[0-9]+$/ && (low<0 || after<low)) low=after } NR>1 && $3 ~ /^[0-9]+$/ && (low<0 || $3<low) {low=$3} END{if(low<0) print "unknown"; else printf "%.0f",low}' "$directory/system-window.tsv")" || note 'Round disk minimum unavailable.'
  minimum="${minimum:-unknown}"
  copy_bytes="$(binary_bytes)"
  printf 'minimum_free_bytes=%s binary_copy_bytes=%s\n' "$minimum" "$copy_bytes" > "$directory/disk-minimum.txt" || note 'Round minimum record unavailable.'
  awk 'NR==FNR {before[$1]=$2;next} /^[A-Za-z]/ && $2 ~ /^[0-9]+$/ && $2!=before[$1] {printf "%s\t%.0f\n",$1,$2-before[$1]}' "$directory/counters-before.txt" "$directory/counters-after.txt" > "$directory/counter-deltas.tsv" || note 'Counter delta unavailable.'
  awk '/^[[:space:]]*Summary / {exit} $1 ~ /^(PASS|FAIL|TIMEOUT|XPASS|LEAK|ABORT|CRASH)$/ {for(i=2;i<NF;i++) if($i ~ /^ganja-/ || $i ~ /^tmux($|::)/) {print $1 "\t" $i "\t" $(i+1);break}}' "$directory/tests.txt" > "$directory/test-counts.tsv" || note 'Per-test counts unavailable.'
  printf 'round=%s exit=%s minimum_free_bytes=%s finished=%s\n' "$round" "$status" "$minimum" "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" >> "$results/$condition/counts.txt" || note 'Round summary record unavailable.'
  grep 'Summary' "$directory/tests.txt" | tee -a "$results/$condition/counts.txt" || note 'Nextest summary unavailable.'
  if [[ -f "$results/observers/kernel-live.txt" ]]; then
    wc -l "$results/observers/kernel-live.txt" > "$directory/kernel-line-count.txt" || note 'Kernel round line count unavailable.'
    reset_live=1
    if [[ -e "$results/retain-kernel-live" ]]; then reset_live=0; fi
    if (( status != 0 )); then
      if ! cp "$results/observers/kernel-live.txt" "$directory/kernel.txt"; then
        note 'Failing-round kernel copy unavailable; preserve live output instead.'
        touch "$results/retain-kernel-live" || note 'Kernel preservation marker unavailable.'
        reset_live=0
      fi
    fi
    if (( reset_live == 1 )); then truncate -s 0 "$results/observers/kernel-live.txt" || note 'Kernel live-log reset unavailable; observer remains optional.'; fi
  fi
  if (( status == 0 )); then
    rm -f "$directory/tests.txt" "$directory/counters-before.txt" "$directory/counters-after.txt" || note 'Passing round diagnostic cleanup failed.'
    continue
  fi
  touch "$results/iteration-failed" || note 'Failure marker unavailable.'
  record_disk "$condition-$round-failure"
  sudo -n ss -ntap > "$directory/sockets-after.txt" 2>&1 || note 'Socket owners unavailable.'
  awk '$1 != "PASS"' "$directory/test-counts.tsv" > "$directory/failures.tsv" || note 'Failing test names unavailable.'
  target_hit=0
  if grep -q 'same client reaches replacement server:.*ConnectionReset' "$directory/tests.txt" && grep -q 'ganja-cli::worker_init[[:space:]]the_initializer_and_replacement_server_announce_the_same_ephemeral_port' "$directory/failures.tsv"; then
    touch "$results/target-reset-recorded" || note 'Reset marker unavailable.'
    grep -o 'ganja server listening on http://0\.0\.0\.0:[0-9]*' "$directory/tests.txt" | grep -o '[0-9]*$' | sort -u > "$directory/worker-ports.txt" || note 'Worker ports unavailable.'
    while IFS= read -r port; do
      for capture in "$results/observers"/control.pcap*; do
        if [[ ! -f "$capture" ]]; then continue; fi
        tcpdump -nn -tt -r "$capture" "tcp port $port" 2>> "$directory/capture-decode.txt" | awk -v start="$started_ns" -v end="$finished_ns" '$1>=start/1000000000 && $1<=end/1000000000' >> "$directory/port-$port-packets.txt" || note "Port $port packet slice unavailable."
      done
    done < "$directory/worker-ports.txt"
    target_hit=1
  fi
  disk_classified=0
  if grep -q 'No space left on device\|StorageFull' "$directory/tests.txt"; then disk_classified=1; fi
  if [[ "$minimum" =~ ^[0-9]+$ && "$copy_bytes" =~ ^[0-9]+$ ]] && (( minimum < copy_bytes )); then disk_classified=1; fi
  if (( disk_classified == 1 )); then
    printf 'Approved disk classification: explicit ENOSPC or sampled free space below one binary copy; not a reset mechanism verdict.\n' > "$directory/disk-classification.txt" || note 'Disk classification record unavailable.'
    continue
  fi
  if (( target_hit == 1 )) && ! grep -v 'ganja-cli::worker_init[[:space:]]the_initializer_and_replacement_server_announce_the_same_ephemeral_port' "$directory/failures.tsv"; then continue; fi
  touch "$results/unexpected-failure" || note 'Unexpected failure marker unavailable.'
  note 'Unexpected test/build failure outside the approved classification; final runner work stops.'
  exit 1
done
exit 0
