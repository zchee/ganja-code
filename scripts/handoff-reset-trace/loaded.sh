#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -euo pipefail

results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
for tool in bun tmux cargo-nextest rust-analyzer; do
  command -v "$tool" >/dev/null || { printf 'Missing measurement prerequisite: %s\n' "$tool" >&2; exit 1; }
done
{
  cargo nextest --version
  tmux -V
  bun --version
} > "$results/load-versions.txt"
require_space
observer_wrappers=()
observer_pidfiles=()
sampler_pid=''
cleanup() {
  if [[ -n "$sampler_pid" ]]; then
    kill -TERM -- "-$sampler_pid" 2>/dev/null || true
    wait "$sampler_pid" 2>/dev/null || true
    sampler_pid=''
  fi
  for index in "${!observer_wrappers[@]}"; do
    wrapper="${observer_wrappers[$index]}"
    pidfile="${observer_pidfiles[$index]}"
    if [[ -s "$pidfile" ]]; then
      observer="$(< "$pidfile")"
      sudo -n kill -INT "$observer" 2>/dev/null || true
      sudo -n timeout 15s bash -c 'while kill -0 "$1" 2>/dev/null; do sleep 0.1; done' _ "$observer" || sudo -n kill -KILL "$observer" 2>/dev/null || true
    else
      sudo -n kill -INT "$wrapper" 2>/dev/null || true
    fi
    wait "$wrapper" 2>/dev/null || true
  done
  observer_wrappers=()
  observer_pidfiles=()
}
trap 'cleanup' EXIT INT TERM
setsid bash scripts/handoff-reset-trace/sample.sh &
sampler_pid=$!
if [[ -e "$results/observers/capture.enabled" ]]; then
  pidfile="$results/observers/capture.pid"
  sudo -n bash -c 'file="$1"; shift; printf "%s\n" "$$" > "$file"; exec "$@"' _ "$pidfile" tcpdump -i lo -nn -s 54 -U -C 8 -W 4 -w "$results/observers/control.pcap" 'ip and tcp and (tcp[tcpflags] & (tcp-syn|tcp-rst|tcp-fin) != 0)' > "$results/observers/capture.txt" 2>&1 &
  observer_wrappers+=("$!")
  observer_pidfiles+=("$pidfile")
fi
if [[ -e "$results/observers/kernel.enabled" ]]; then
  program="$(< "$results/observers/kernel.enabled")"
  pidfile="$results/observers/kernel.pid"
  sudo -n bash -c 'file="$1"; shift; printf "%s\n" "$$" > "$file"; ulimit -c 0; ulimit -f 65536; exec "$@"' _ "$pidfile" bpftrace -B line -q "$program" > "$results/observers/kernel.txt" 2>&1 &
  observer_wrappers+=("$!")
  observer_pidfiles+=("$pidfile")
  for ((attempt=0; attempt<450; attempt++)); do
    if grep -q '^READY' "$results/observers/kernel.txt"; then break; fi
    sleep 0.1
  done
  if ! grep -q '^READY' "$results/observers/kernel.txt"; then
    printf 'Kernel observer did not become ready; packet and counter observers remain active.\n' | tee -a "$results/observers/availability.txt"
  fi
fi
status=0
neighbourhood_deadline=$(( $(date +%s) + 900 ))
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
  rm -f "$results/observers"/control.pcap* "$results/observers/kernel.txt"
fi
exit "$status"
