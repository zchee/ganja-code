#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -uo pipefail

results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
for tool in bun tmux cargo-nextest rust-analyzer; do command -v "$tool" >/dev/null || note "Measurement prerequisite missing: $tool."; done
observer_wrappers=()
observer_pidfiles=()
sampler_pid=''
cleanup() {
  if [[ -n "$sampler_pid" ]]; then
    kill -TERM -- "-$sampler_pid" 2>/dev/null || note 'Sampler was already stopped or could not be signalled.'
    wait "$sampler_pid" 2>/dev/null || true
    sampler_pid=''
  fi
  for index in "${!observer_wrappers[@]}"; do
    wrapper="${observer_wrappers[$index]}"
    pidfile="${observer_pidfiles[$index]}"
    if [[ -s "$pidfile" ]]; then
      observer="$(< "$pidfile")"
      if [[ "$observer" =~ ^[0-9]+$ ]]; then
        sudo -n kill -INT "$observer" 2>/dev/null || note "Observer $observer already stopped or signal failed."
        sudo -n timeout 15s bash -c 'while kill -0 "$1" 2>/dev/null; do sleep 0.1; done' _ "$observer" || { note "Observer $observer shutdown bound reached; attempting final stop."; sudo -n kill -KILL "$observer" 2>/dev/null || note "Observer $observer final signal failed or process absent."; }
      else
        note 'Observer pid file was not a numeric identity.'
      fi
    else
      sudo -n kill -INT "$wrapper" 2>/dev/null || note 'Optional observer wrapper was already stopped or signal failed.'
    fi
    wait "$wrapper" 2>/dev/null || note 'Optional observer exited nonzero; measurement result unchanged.'
  done
  observer_wrappers=()
  observer_pidfiles=()
  return 0
}
trap 'cleanup' EXIT INT TERM
setsid bash scripts/handoff-reset-trace/sample.sh &
sampler_pid=$!
if [[ -e "$results/observers/capture.enabled" ]]; then
  pidfile="$results/observers/capture.pid"
  capture_user="$(< "$results/observers/capture-user.txt")"
  sudo -n bash -c 'file="$1"; shift; printf "%s\n" "$$" > "$file"; exec "$@"' _ "$pidfile" tcpdump -Z "$capture_user" -i lo -nn -s 54 -U -C 8 -W 4 -w "$results/observers/control.pcap" 'ip and tcp and (tcp[tcpflags] & (tcp-syn|tcp-rst|tcp-fin) != 0)' > "$results/observers/capture.txt" 2>&1 &
  observer_wrappers+=("$!")
  observer_pidfiles+=("$pidfile")
fi
if [[ -e "$results/observers/kernel.enabled" ]]; then
  program="$(< "$results/observers/kernel.enabled")"
  pidfile="$results/observers/kernel.pid"
  truncate -s 0 "$results/observers/kernel-live.txt" || note 'Kernel live-log initialization unavailable.'
  sudo -n bash -c 'file="$1"; shift; printf "%s\n" "$$" > "$file"; ulimit -c 0; ulimit -f 65536; exec "$@"' _ "$pidfile" bpftrace -B line -q "$program" >> "$results/observers/kernel-live.txt" 2>&1 &
  kernel_wrapper=$!
  observer_wrappers+=("$kernel_wrapper")
  observer_pidfiles+=("$pidfile")
  for ((attempt=0; attempt<${TRACE_OBSERVER_WAIT_TICKS:-450}; attempt++)); do
    if grep -q '^READY' "$results/observers/kernel-live.txt"; then break; fi
    if ! kill -0 "$kernel_wrapper" 2>/dev/null; then note 'Optional kernel observer exited before READY.'; break; fi
    sleep 0.1 || note 'Observer readiness wait unavailable.'
  done
  grep -q '^READY' "$results/observers/kernel-live.txt" || note 'Kernel observer unavailable; continue with remaining diagnostics.'
fi
status=0
bash scripts/handoff-reset-trace/suite.sh || status=$?
cleanup
record_disk after-measurement
ps -eo pid,ppid,comm > "$results/observers/processes-after.txt" || note 'Process cleanup listing unavailable.'
if (( status == 0 )); then
  for name in capture kernel; do
    enabled=0
    lost=0
    if [[ -e "$results/observers/$name.enabled" ]]; then enabled=1; fi
    if [[ -e "$results/observers/$name.not-running" ]]; then lost=1; fi
    printf '%s_enabled=%s %s_process_absent=%s\n' "$name" "$enabled" "$name" "$lost" >> "$results/observers/counts.txt" || note 'Observer availability count unavailable.'
  done
  for log in "$results/observers/kernel-live.txt" "$results/observers/capture.txt"; do
    if [[ -f "$log" ]]; then wc -l "$log" >> "$results/observers/counts.txt" || note 'Observer line count unavailable.'; fi
  done
  if [[ -f "$results/observers/kernel-live.txt" ]]; then
    awk '/^READY/ {print} END {print "kernel_lines=" NR}' "$results/observers/kernel-live.txt" >> "$results/observers/counts.txt" || note 'Kernel readiness count unavailable.'
  fi
  if [[ -f "$results/observers/capture.txt" ]]; then
    awk '/packets (captured|received by filter|dropped by kernel)/ {print}' "$results/observers/capture.txt" >> "$results/observers/counts.txt" || note 'Capture counts unavailable.'
  fi
  for file in "$results/observers"/*; do
    if [[ "$file" != "$results/observers/counts.txt" ]]; then rm -f "$file" || note 'Passing observer cleanup failed.'; fi
  done
  rm -f "$results/filesystems.txt" "$results/diagnostic-notes.txt" || note 'Passing diagnostic cleanup failed.'
fi
exit "$status"
