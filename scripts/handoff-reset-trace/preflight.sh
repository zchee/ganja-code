#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -uo pipefail

mkdir -p target/handoff-reset-results/observers || true
results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
for tool in bash sudo apt-get rustup cargo rustc git uname nproc lscpu taskset sysctl ip timeout jq wc sha256sum tee date rm mkdir grep setsid touch env ps sleep df du stat cat awk xargs sort cmp diff /home/linuxbrew/.linuxbrew/bin/brew; do
  command -v "$tool" >/dev/null || note "Prerequisite missing: $tool; diagnostics will not block measurement."
done
printf '%s\n' "$(date +%s)" > "$results/job-start-epoch.txt" || note 'Job epoch record failed.'
printf 'utc\tlabel\tpath\tfree_bytes\tbinary_bytes\tbinary_size_source\ttarget_bytes\trecords_floor_bytes\n' > "$results/disk.tsv" || note 'Disk table header failed.'
record_disk job-start
require_space || exit $?
df -B1 > "$results/filesystems.txt" 2>&1 || note 'Filesystem listing unavailable.'
diag 'Package index refresh' sudo apt-get update
diag 'Counter and inventory utilities install' sudo apt-get install -y iproute2 fd-find
for tool in nstat ip ss fdfind; do
  command -v "$tool" >/dev/null || note "Installed diagnostic prerequisite missing: $tool."
done
sudo apt-get install -y tcpdump bpftrace > "$results/observers/install.txt" 2>&1 || note 'Optional observer installation failed.'
filter='ip and tcp and (tcp[tcpflags] & (tcp-syn|tcp-rst|tcp-fin) != 0)'
if command -v tcpdump >/dev/null; then
  status=0
  sudo -n timeout --signal=INT 3s tcpdump -i lo -nn -s 54 -c 1 -w "$results/observers/preflight.pcap" "$filter" > "$results/observers/capture-preflight.txt" 2>&1 || status=$?
  if (( status == 0 || status == 124 )) && grep -q 'listening on lo' "$results/observers/capture-preflight.txt"; then
    diag 'Capture enabled marker' touch "$results/observers/capture.enabled"
  else
    note "Packet preflight unavailable (exit $status); measurement will continue."
  fi
else
  note 'Packet observer command missing.'
fi
for event in tcp/tcp_send_reset tcp/tcp_receive_reset sock/inet_sock_set_state; do
  name="${event//\//-}"
  sudo -n cat "/sys/kernel/tracing/events/$event/format" > "$results/observers/$name-format.txt" 2>&1 || note "Tracepoint format unavailable: $event."
done
if command -v bpftrace >/dev/null; then
  program="$results/observers/kernel-generated.bt"
  printf 'BEGIN { printf("READY mono_ns=%%llu\\n", nsecs); }\n' > "$program" || note 'Kernel program header unavailable.'
  sudo -n cat /sys/kernel/tracing/available_filter_functions | grep -E '^(tcp_close|tcp_send_active_reset|inet_csk_listen_stop|inet_csk_accept)( |$)' > "$results/observers/socket-functions.txt" || note 'Socket-function inventory unavailable.'
  full=1
  for symbol in tcp_close tcp_send_active_reset inet_csk_listen_stop inet_csk_accept; do
    if ! grep -q "^$symbol\( \|$\)" "$results/observers/socket-functions.txt"; then full=0; fi
  done
  if (( full == 1 )); then printf '%s\n' "$(< scripts/handoff-reset-trace/sockets.bt)" >> "$program" || note 'Socket probe snippet unavailable.'; fi
  if grep -q 'saddr\[28\]' "$results/observers/tcp-tcp_send_reset-format.txt" && grep -q 'field:.*reason;' "$results/observers/tcp-tcp_send_reset-format.txt"; then
    printf '%s\n' "$(< scripts/handoff-reset-trace/resets.bt)" >> "$program" || note 'Sent-reset probe snippet unavailable.'
  fi
  if grep -q 'field:.*sport;' "$results/observers/tcp-tcp_receive_reset-format.txt" && grep -q 'field:.*oldstate;' "$results/observers/sock-inet_sock_set_state-format.txt"; then
    printf '%s\n' "$(< scripts/handoff-reset-trace/states.bt)" >> "$program" || note 'State probe snippet unavailable.'
  fi
  if grep -q 'tracepoint:\|kprobe:\|kretprobe:' "$program"; then
    status=0
    sudo -n timeout --signal=INT --kill-after=5s "${TRACE_PREFLIGHT_SECONDS:-45}s" bpftrace -B line -q "$program" > "$results/observers/kernel-preflight.txt" 2>&1 || status=$?
    if grep -q '^READY' "$results/observers/kernel-preflight.txt"; then
      printf '%s\n' "$program" > "$results/observers/kernel.enabled" || note 'Kernel enabled marker failed.'
      note "Kernel attach verified READY; bounded preflight shutdown exit=$status."
    else
      note "Single kernel preflight unavailable (exit $status); no retry."
    fi
  else
    note 'No schema-matched kernel probe could be generated; no attach attempted.'
  fi
else
  note 'Kernel observer command missing.'
fi
{
  date -u '+%Y-%m-%d %H:%M:%S UTC' || note 'Observer version timestamp failed.'
  uname -a || note 'Kernel version failed.'
  printf 'epoch_ns=%s\n' "$(date +%s%N)" || note 'Observer clock anchor failed.'
  read -r uptime _ < /proc/uptime && printf 'uptime_seconds=%s\n' "$uptime" || note 'Boot clock anchor unavailable.'
  command -v tcpdump >/dev/null && tcpdump --version || note 'Packet observer version unavailable.'
  command -v bpftrace >/dev/null && bpftrace --version || note 'Kernel observer version unavailable.'
} > "$results/observers/versions.txt" 2>&1 || note 'Observer version record failed.'
record_disk after-preflight
exit 0
