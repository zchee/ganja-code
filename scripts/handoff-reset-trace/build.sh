#!/usr/bin/env bash
set -uo pipefail

mkdir -p target/handoff-reset-results || true
results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
record_disk before-workspace-build
require_space || exit $?
{
  diag 'Build timestamp' date -u '+%Y-%m-%d %H:%M:%S UTC'
  diag 'Build revision' git rev-parse HEAD
  diag 'Kernel identity' uname -a
  diag 'Processor count' nproc
  diag 'Processor inventory' lscpu
  diag 'Processor affinity' taskset -pc $$
  diag 'Compiler version' rustc --version --verbose
  diag 'Cargo version' cargo --version
  diag 'TCP configuration' sysctl net.ipv4.tcp_fin_timeout net.ipv4.tcp_tw_reuse net.ipv4.tcp_abort_on_overflow net.ipv4.tcp_syncookies net.core.somaxconn net.ipv4.tcp_max_syn_backlog
  diag 'Loopback configuration' ip -details link show lo
} > "$results/environment.txt" 2>&1 || note 'Environment record unavailable.'
if timeout --signal=TERM --kill-after=15s 900s env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest run --locked --workspace --profile ci --no-run > "$results/workspace-build.txt" 2>&1; then
  :
else
  status=$?
  note "Workspace build failed (exit $status)."
  exit "$status"
fi
record_disk after-workspace-build-idle
require_space || exit $?
if ! env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo nextest list --locked --workspace --profile ci --message-format json > "$results/workspace-metadata.json" 2> "$results/metadata.log"; then
  note 'Binary identity discovery failed: workspace metadata unavailable.'
  exit 1
fi
if ! jq -r '."rust-suites"[] | select(."binary-id" == "ganja-cli::worker_init") | ."binary-path"' "$results/workspace-metadata.json" > "$results/test-binary.txt"; then
  note 'Binary identity discovery failed: worker path unavailable.'
  exit 1
fi
mapfile -t binaries < "$results/test-binary.txt"
if (( ${#binaries[@]} != 1 )) || [[ ! -x "${binaries[0]:-}" ]]; then
  note 'Binary identity discovery failed: expected one executable worker_init.'
  exit 1
fi
if ! sha256sum "${binaries[0]}" target/debug/ganja > "$results/binary-sha256.txt"; then
  note 'Initial binary identity checksum failed.'
  exit 1
fi
record_disk after-workspace-discovery
exit 0
