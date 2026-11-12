#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -uo pipefail

results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
for tool in bun tmux cargo-nextest rust-analyzer; do command -v "$tool" >/dev/null || note "Measurement prerequisite missing: $tool."; done
require_space || exit $?
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
job_start="$(< "$results/job-start-epoch.txt")"
if [[ ! "$job_start" =~ ^[0-9]+$ ]]; then note 'Job clock anchor unavailable; using current clock for loop deadline.'; job_start="$(date +%s)"; fi
job_deadline=$(( job_start + ${TRACE_TOTAL_SECONDS:-2100} ))
bash scripts/handoff-reset-trace/suite.sh "$job_deadline" || status=$?
cleanup
record_disk after-measurement
ps -eo pid,ppid,comm > "$results/observers/processes-after.txt" || note 'Process cleanup listing unavailable.'
if [[ -e "$results/target-reset-recorded" ]]; then status=1; fi
if (( status == 0 )) && [[ ! -e "$results/iteration-failed" ]]; then
  for log in "$results/observers/kernel-live.txt" "$results/observers/capture.txt"; do
    if [[ -f "$log" ]]; then wc -l "$log" >> "$results/observers/counts.txt" || note 'Observer line count unavailable.'; fi
  done
  rm -f "$results/observers"/control.pcap* "$results/observers/kernel-live.txt" || note 'Passing observer cleanup failed.'
fi
if [[ ! -e "$results/retain-kernel-live" ]]; then rm -f "$results/observers/kernel-live.txt" || note 'Live kernel cleanup unavailable.'; fi
exit "$status"
