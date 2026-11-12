#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -euo pipefail

missing=()
for tool in bash sudo apt-get rustup cargo rustc git uname nproc lscpu taskset sysctl ip timeout jq wc sha256sum tee date rm mkdir grep setsid touch env ps sleep df du stat cat awk xargs sort cmp diff /home/linuxbrew/.linuxbrew/bin/brew; do
  command -v "$tool" >/dev/null || missing+=("$tool")
done
if (( ${#missing[@]} != 0 )); then
  printf 'Missing prerequisites: %s\n' "${missing[*]}" >&2
  exit 1
fi
results="$PWD/target/handoff-reset-results"
mkdir -p "$results/observers"
printf '%s\n' "$(date +%s)" > "$results/job-start-epoch.txt"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
printf 'utc\tlabel\tpath\tfree_bytes\tbinary_bytes\tbinary_size_source\ttarget_bytes\trecords_floor_bytes\n' > "$results/disk.tsv"
record_disk job-start
require_space
df -B1 > "$results/filesystems.txt"
sudo apt-get update
sudo apt-get install -y iproute2 fd-find
for tool in nstat ip ss fdfind; do
  command -v "$tool" >/dev/null || { printf 'Missing installed prerequisite: %s\n' "$tool" >&2; exit 1; }
done
sudo apt-get install -y tcpdump bpftrace > "$results/observers/install.txt" 2>&1 || printf 'Optional observer installation failed; inspect the install log.\n'
for tool in tcpdump bpftrace; do
  if ! command -v "$tool" >/dev/null; then
    printf 'Optional observer missing: %s\n' "$tool" | tee -a "$results/observers/availability.txt"
  fi
done
filter='ip and tcp and (tcp[tcpflags] & (tcp-syn|tcp-rst|tcp-fin) != 0)'
if command -v tcpdump >/dev/null; then
  status=0
  sudo -n timeout --signal=INT 3s tcpdump -i lo -nn -s 54 -c 1 -w "$results/observers/preflight.pcap" "$filter" > "$results/observers/capture-preflight.txt" 2>&1 || status=$?
  if (( status == 0 || status == 124 )) && grep -q 'listening on lo' "$results/observers/capture-preflight.txt"; then
    touch "$results/observers/capture.enabled"
  else
    printf 'Packet observer unavailable (exit %s); continuing without it.\n' "$status" | tee -a "$results/observers/availability.txt"
  fi
fi
for event in tcp/tcp_send_reset tcp/tcp_receive_reset sock/inet_sock_set_state; do
  name="${event//\//-}"
  sudo -n cat "/sys/kernel/tracing/events/$event/format" > "$results/observers/$name-format.txt" 2>&1 || true
done
if command -v bpftrace >/dev/null; then
  program="$results/observers/kernel-generated.bt"
  printf 'BEGIN { printf("READY mono_ns=%%llu\\n", nsecs); }\n' > "$program"
  sudo -n cat /sys/kernel/tracing/available_filter_functions | grep -E '^(tcp_close|tcp_send_active_reset|inet_csk_listen_stop|inet_csk_accept)( |$)' > "$results/observers/socket-functions.txt" || true
  full=1
  for symbol in tcp_close tcp_send_active_reset inet_csk_listen_stop inet_csk_accept; do
    if ! grep -q "^$symbol\( \|$\)" "$results/observers/socket-functions.txt"; then full=0; fi
  done
  if (( full == 1 )); then
    printf '%s\n' "$(< scripts/handoff-reset-trace/sockets.bt)" >> "$program"
  fi
  if grep -q 'saddr\[28\]' "$results/observers/tcp-tcp_send_reset-format.txt" && grep -q 'field:.*reason;' "$results/observers/tcp-tcp_send_reset-format.txt"; then
    # The runner's sockaddr arrays contain family, network-order port and IPv4 bytes.
    printf '%s\n' "$(< scripts/handoff-reset-trace/resets.bt)" >> "$program"
  else
    printf 'Sent-reset tracepoint layout unrecognized; socket and packet observers remain available.\n' | tee -a "$results/observers/availability.txt"
  fi
  if grep -q 'field:.*sport;' "$results/observers/tcp-tcp_receive_reset-format.txt" && grep -q 'field:.*oldstate;' "$results/observers/sock-inet_sock_set_state-format.txt"; then
    printf '%s\n' "$(< scripts/handoff-reset-trace/states.bt)" >> "$program"
  fi
  status=0
  sudo -n timeout --signal=INT --kill-after=5s 45s bpftrace -B line -q "$program" > "$results/observers/kernel-preflight.txt" 2>&1 || status=$?
  if grep -q '^READY' "$results/observers/kernel-preflight.txt"; then
    printf '%s\n' "$program" > "$results/observers/kernel.enabled"
    printf 'Kernel attach verified READY; bounded preflight shutdown exit=%s.\n' "$status" | tee -a "$results/observers/availability.txt"
  else
    printf 'Single kernel preflight unavailable (exit %s); continuing with packet capture and counters.\n' "$status" | tee -a "$results/observers/availability.txt"
  fi
fi
{
  date -u '+%Y-%m-%d %H:%M:%S UTC'
  uname -a
  printf 'epoch_ns=%s\n' "$(date +%s%N)"
  read -r uptime _ < /proc/uptime
  printf 'uptime_seconds=%s\n' "$uptime"
  command -v tcpdump >/dev/null && tcpdump --version || true
  command -v bpftrace >/dev/null && bpftrace --version || true
} > "$results/observers/versions.txt"
record_disk after-preflight
