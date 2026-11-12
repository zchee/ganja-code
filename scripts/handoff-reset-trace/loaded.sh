#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -euo pipefail

results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
for tool in bun tmux cargo-nextest rust-analyzer; do
  command -v "$tool" >/dev/null || { printf 'Missing load prerequisite: %s\n' "$tool" >&2; exit 1; }
done
{
  cargo nextest --version
  tmux -V
  bun --version
} > "$results/load-versions.txt"
record_disk before-workspace-build
require_compile_space
timeout --signal=TERM --kill-after=15s 600s env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest run --locked --workspace --profile ci --no-run > "$results/workspace-build.txt" 2>&1
# No suite or continuous observer is running at this measurement point.
record_disk after-workspace-build-idle
require_space
observer_pids=()
cleanup() {
  for observer in "${observer_pids[@]}"; do
    sudo -n kill -INT "$observer" 2>/dev/null || true
    wait "$observer" 2>/dev/null || true
  done
  observer_pids=()
}
trap 'cleanup' EXIT INT TERM
if [[ -e "$results/observers/capture.enabled" ]]; then
  sudo -n tcpdump -i lo -nn -s 54 -U -w "$results/observers/control.pcap" 'ip and tcp and (tcp[tcpflags] & (tcp-syn|tcp-rst|tcp-fin) != 0)' > "$results/observers/capture.txt" 2>&1 &
  observer_pids+=("$!")
fi
if [[ -e "$results/observers/kernel.enabled" ]]; then
  program="$(< "$results/observers/kernel.enabled")"
  sudo -n bpftrace -B line -q "$program" > "$results/observers/kernel.txt" 2>&1 &
  observer_pids+=("$!")
  for ((attempt=0; attempt<450; attempt++)); do
    if grep -q '^READY' "$results/observers/kernel.txt"; then break; fi
    sleep 0.1
  done
  if ! grep -q '^READY' "$results/observers/kernel.txt"; then
    printf 'Kernel observer did not become ready; packet and counter observers remain active.\n' | tee -a "$results/observers/availability.txt"
  fi
fi
status=0
neighbourhood_deadline=$(( $(date +%s) + 1200 ))
job_deadline=$(( $(< "$results/job-start-epoch.txt") + 3000 ))
bash scripts/handoff-reset-trace/suite.sh neighbourhood "$neighbourhood_deadline" || status=$?
if (( status == 0 )); then
  bash scripts/handoff-reset-trace/suite.sh workspace "$job_deadline" || status=$?
fi
cleanup
record_disk after-measurement
ps -eo pid,ppid,comm > "$results/observers/processes-after.txt"
if [[ -e "$results/target-reset-recorded" ]]; then status=1; fi
if (( status == 0 )) && [[ ! -e "$results/iteration-failed" ]]; then
  for log in "$results/observers/kernel.txt" "$results/observers/capture.txt"; do
    if [[ -f "$log" ]]; then wc -l "$log" >> "$results/observers/counts.txt"; fi
  done
  rm -f "$results/observers/control.pcap" "$results/observers/kernel.txt"
fi
exit "$status"
