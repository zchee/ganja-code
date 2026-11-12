#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -uo pipefail

results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
directory="$results/ci-initial/round-1"
mkdir -p "$directory" || note 'Invocation directory unavailable.'
started_ns="$(date +%s%N)"
printf '%s\n' "$started_ns" > "$directory/started-epoch-ns.txt" || note 'Invocation start timestamp unavailable.'
nstat -az > "$directory/counters-before.txt" 2>&1 || note 'Initial counters unavailable.'
env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest run --locked --workspace --profile ci 2>&1 | bash scripts/handoff-reset-trace/record.sh "$directory"
status="${PIPESTATUS[0]}"
finished_ns="$(date +%s%N)"
printf '%s\n' "$finished_ns" > "$directory/finished-epoch-ns.txt" || note 'Invocation finish timestamp unavailable.'
nstat -az > "$directory/counters-after.txt" 2>&1 || note 'Final counters unavailable.'
record_disk after-invocation
awk -F '\t' -v start="$started_ns" -v end="$finished_ns" 'NR == 1 || ($1 >= start && $1 <= end)' "$results/system-samples.tsv" > "$directory/system-window.tsv" || note 'Invocation system sample window unavailable.'
awk '/^[[:space:]]*Summary / {exit} $1 ~ /^(PASS|FAIL|TIMEOUT|XPASS|LEAK|ABORT|CRASH)$/ {for(i=2;i<NF;i++) if($i ~ /^ganja-/ || $i ~ /^tmux($|::)/) {print $1 "\t" $i "\t" $(i+1);break}}' "$directory/tests.txt" > "$directory/test-counts.tsv" || note 'Per-test counts unavailable.'
awk '/ganja-cli::worker_init[[:space:]]+the_initializer_and_replacement_server_announce_the_same_ephemeral_port([[:space:]]|$)/ {print "position=" NR "\treceived_epoch_ns=" $0}' "$directory/status-received.tsv" > "$directory/target-result.tsv" || note 'Target result unavailable.'
classification=pass
if (( status != 0 )); then
  classification=other
  touch "$results/iteration-failed" || note 'Failure marker unavailable.'
  sudo -n ss -ntap > "$directory/sockets-after.txt" 2>&1 || note 'Socket owners unavailable.'
  awk '$1 != "PASS"' "$directory/test-counts.tsv" > "$directory/failures.tsv" || note 'Failing test names unavailable.'
  if grep -q 'same client reaches replacement server:.*ConnectionReset' "$directory/tests.txt" && grep -q 'ganja-cli::worker_init[[:space:]]the_initializer_and_replacement_server_announce_the_same_ephemeral_port' "$directory/failures.tsv"; then
    classification=HIT
    touch "$results/target-reset-recorded" || note 'Reset marker unavailable.'
    grep -o 'ganja server listening on http://0\.0\.0\.0:[0-9]*' "$directory/tests.txt" | grep -o '[0-9]*$' | sort -u > "$directory/worker-ports.txt" || note 'Worker ports unavailable.'
  elif grep -q 'No space left on device' "$directory/tests.txt"; then
    classification=disk
  fi
fi
printf 'classification=%s exit=%s finished=%s\n' "$classification" "$status" "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" > "$results/ci-initial/counts.txt" || note 'Invocation summary record unavailable.'
grep 'Summary' "$directory/tests.txt" | tee -a "$results/ci-initial/counts.txt" || note 'Nextest summary unavailable.'
if (( status == 0 )); then
  rm -f "$directory/tests.txt" "$directory/counters-before.txt" "$directory/counters-after.txt" "$directory/status-received.tsv" "$directory/command-prelude.txt" || note 'Passing invocation diagnostic cleanup failed.'
fi
exit "$status"
