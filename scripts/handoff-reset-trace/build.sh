#!/usr/bin/env bash
set -euo pipefail

mkdir -p target/handoff-reset-results
results="$PWD/target/handoff-reset-results"
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
timeout --signal=TERM --kill-after=15s 600s env -u RUSTFLAGS -u RUSTC_WRAPPER CARGO_INCREMENTAL=0 cargo test --locked -p ganja-cli --test worker_init --no-run --message-format=json > "$results/build.jsonl" 2> "$results/build.log"
jq -r 'select(.reason == "compiler-artifact" and .target.name == "worker_init" and .executable != null) | .executable' "$results/build.jsonl" > "$results/test-binary.txt"
test "$(wc -l < "$results/test-binary.txt")" -eq 1
binary="$(< "$results/test-binary.txt")"
sha256sum "$binary" target/debug/ganja > "$results/binary-sha256.txt"
"$binary" --list > "$results/test-list.txt"
grep -q '^the_initializer_and_replacement_server_announce_the_same_ephemeral_port: test$' "$results/test-list.txt"
