#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -euo pipefail

results="$PWD/target/handoff-reset-results"
for tool in bun tmux cargo-nextest rust-analyzer; do
  command -v "$tool" >/dev/null || { printf 'Missing load prerequisite: %s\n' "$tool" >&2; exit 1; }
done
{
  cargo nextest --version
  tmux -V
  bun --version
} > "$results/load-versions.txt"
timeout --signal=TERM --kill-after=15s 600s env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest run --locked --workspace --profile ci --no-run > "$results/workspace-build.txt" 2>&1
observer_pids=()
load_pid=''
cleanup() {
  if [[ -n "$load_pid" ]]; then
    kill -TERM -- "-$load_pid" 2>/dev/null || true
    wait "$load_pid" 2>/dev/null || true
    load_pid=''
  fi
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
  sudo -n bpftrace -B line -q "scripts/handoff-reset-trace/$program.bt" > "$results/observers/kernel.txt" 2>&1 &
  observer_pids+=("$!")
  # BPF compilation happens before BEGIN; do not count a test before attach.
  for ((attempt=0; attempt<150; attempt++)); do
    if grep -q '^READY' "$results/observers/kernel.txt"; then
      break
    fi
    sleep 0.1
  done
  if ! grep -q '^READY' "$results/observers/kernel.txt"; then
    printf 'Kernel observer did not become ready; other observers remain active.\n' | tee -a "$results/observers/availability.txt"
  fi
fi
setsid bash scripts/handoff-reset-trace/suite.sh &
load_pid=$!
status=0
LOAD_PID="$load_pid" MEASURE_SECONDS=1800 bash scripts/handoff-reset-trace/measure.sh 5000 loaded || status=$?
touch "$results/load/stop"
wait "$load_pid" || status=1
load_pid=''
if [[ -e "$results/load/failed" ]]; then
  printf 'An unexpected real-suite failure occurred; stop without rerunning it.\n' >&2
  status=1
fi
if [[ -e "$results/load/target-failed" ]]; then
  status=1
fi
cleanup
ps -eo pid,ppid,comm > "$results/observers/processes-after.txt"
if (( status == 0 )); then
  for log in "$results/observers/kernel.txt" "$results/observers/capture.txt"; do
    if [[ -f "$log" ]]; then
      wc -l "$log" >> "$results/observers/counts.txt"
    fi
  done
  rm -f "$results/observers/control.pcap" "$results/observers/kernel.txt"
fi
exit "$status"
