#!/usr/bin/env bash
set -uo pipefail

results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
shopt -s nullglob dotglob
{
  printf 'Immediate target children (bytes, path):\n'
  for entry in target/*; do du -sb "$entry" || note "Inventory size unavailable: $entry."; done
  printf 'Immediate target/debug children (bytes, path):\n'
  if [[ -d target/debug ]]; then
    for entry in target/debug/*; do du -sb "$entry" || note "Inventory size unavailable: $entry."; done
  else
    note 'Inventory target/debug absent; no directory layout assumed.'
  fi
  printf 'Workspace test executable inventory:\n'
  jq -r '."rust-suites"[]."binary-path"' "$results/workspace-metadata.json" | sort -u | while IFS= read -r executable; do stat -c '%s %n' "$executable" || note "Executable inventory unavailable: $executable."; done | awk '{ count++; bytes += $1 } END { printf "test_executable_count=%d test_executable_total_bytes=%.0f\n", count, bytes }' || note 'Executable count inventory incomplete.'
  printf 'Twenty largest target files (bytes, path):\n'
  fdfind -H -I -t f -0 . target | xargs -0 stat -c '%s %n' | sort -nr | awk 'NR <= 20' || note 'Largest-file inventory unavailable.'
} > "$results/inventory.txt" 2>&1 || note 'Inventory output unavailable.'
exit 0
