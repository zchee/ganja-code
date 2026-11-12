#!/usr/bin/env bash
# Output files belong to the runner; elevation applies only to observers.
# shellcheck disable=SC2024
set -euo pipefail

missing=()
for tool in bash sudo apt-get rustup cargo rustc git uname nproc lscpu taskset sysctl ip timeout jq wc sha256sum tee date rm mkdir grep setsid touch env pgrep ps sleep /home/linuxbrew/.linuxbrew/bin/brew; do
  command -v "$tool" >/dev/null || missing+=("$tool")
done
if (( ${#missing[@]} != 0 )); then
  printf 'Missing prerequisites: %s\n' "${missing[*]}" >&2
  exit 1
fi
results="$PWD/target/handoff-reset-results"
mkdir -p "$results/observers"
sudo apt-get update
sudo apt-get install -y iproute2
for tool in nstat ip ss; do
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
if command -v bpftrace >/dev/null; then
  for program in sockets resets; do
    status=0
    sudo -n timeout --signal=INT 5s bpftrace -q "scripts/handoff-reset-trace/$program.bt" > "$results/observers/$program-preflight.txt" 2>&1 || status=$?
    if (( status == 0 || status == 124 )) && grep -q '^READY' "$results/observers/$program-preflight.txt"; then
      printf '%s\n' "$program" > "$results/observers/kernel.enabled"
      break
    fi
    printf 'Kernel observer %s unavailable (exit %s).\n' "$program" "$status" | tee -a "$results/observers/availability.txt"
  done
fi
{
  date -u '+%Y-%m-%d %H:%M:%S UTC'
  printf 'epoch_ns=%s\n' "$(date +%s%N)"
  read -r uptime _ < /proc/uptime
  printf 'uptime_seconds=%s\n' "$uptime"
  command -v tcpdump >/dev/null && tcpdump --version || true
  command -v bpftrace >/dev/null && bpftrace --version || true
} > "$results/observers/versions.txt"
