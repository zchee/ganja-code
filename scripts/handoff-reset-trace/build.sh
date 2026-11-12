#!/usr/bin/env bash
set -euo pipefail

mkdir -p target/handoff-reset-results
results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
record_disk before-workspace-build
require_space
{
  date -u '+%Y-%m-%d %H:%M:%S UTC'
  git rev-parse HEAD
  uname -a
  nproc
  lscpu
  taskset -pc $$
  rustc --version --verbose
  cargo --version
  sysctl net.ipv4.tcp_fin_timeout net.ipv4.tcp_tw_reuse net.ipv4.tcp_abort_on_overflow net.ipv4.tcp_syncookies net.core.somaxconn net.ipv4.tcp_max_syn_backlog
  ip -details link show lo
} > "$results/environment.txt"
timeout --signal=TERM --kill-after=15s 900s env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest run --locked --workspace --profile ci --no-run > "$results/workspace-build.txt" 2>&1
record_disk after-workspace-build-idle
require_space
# This lists the same workspace build, not a differently featured single package.
env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest list --locked --workspace --profile ci --message-format json > "$results/workspace-metadata.json" 2> "$results/metadata.log"
jq -r '."rust-suites"[] | select(."binary-id" == "ganja-cli::worker_init") | ."binary-path"' "$results/workspace-metadata.json" > "$results/test-binary.txt"
test "$(wc -l < "$results/test-binary.txt")" -eq 1
binary="$(< "$results/test-binary.txt")"
test -x "$binary"
sha256sum "$binary" target/debug/ganja > "$results/binary-sha256.txt"
{
  du -sb target/debug/deps
  jq -r '."rust-suites"[]."binary-path"' "$results/workspace-metadata.json" | sort -u | while IFS= read -r executable; do stat -c '%s %n' "$executable"; done | awk '{ count++; bytes += $1 } END { printf "test_executable_count=%d test_executable_total_bytes=%.0f\n", count, bytes }'
  printf 'Ten largest target files (bytes, path):\n'
  fdfind -H -I -t f -0 . target | xargs -0 stat -c '%s %n' | sort -nr | awk 'NR <= 10'
} > "$results/inventory.txt"
record_disk after-metadata-and-inventory
