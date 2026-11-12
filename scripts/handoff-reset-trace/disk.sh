#!/usr/bin/env bash

free_bytes() {
  local path="$1"
  local -a rows
  mapfile -t rows < <(df -B1 --output=avail "$path")
  printf '%s\n' "${rows[1]//[[:space:]]/}"
}

binary_bytes() {
  if [[ -f target/debug/ganja ]]; then
    stat -c %s target/debug/ganja
  else
    # Conservative planning bound until the cached or built binary is present.
    printf '%s\n' 1073741824
  fi
}

space_floor() {
  printf '%s\n' "$(( $(binary_bytes) * $(nproc) + 2147483648 ))"
}

record_disk() {
  local label="$1" results="$PWD/target/handoff-reset-results"
  local path bytes target_bytes=0 source=planning-bound
  if [[ -f target/debug/ganja ]]; then source=measured; fi
  if [[ -d target ]]; then read -r target_bytes _ < <(du -sb target); fi
  for path in / "${TMPDIR:-/tmp}" "$PWD/target"; do
    bytes="$(free_bytes "$path")"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" "$label" "$path" "$bytes" "$(binary_bytes)" "$source" "$target_bytes" "$(space_floor)" >> "$results/disk.tsv"
  done
}

choose_test_temp() {
  local base best="${TMPDIR:-/tmp}" best_bytes bytes directory
  best_bytes="$(free_bytes "$best")"
  for base in /mnt "${RUNNER_TEMP:-/tmp}" /tmp; do
    if [[ ! -d "$base" ]]; then continue; fi
    bytes="$(free_bytes "$base")"
    if (( bytes > best_bytes )); then
      best="$base"
      best_bytes="$bytes"
    fi
  done
  if [[ ! -w "$best" ]]; then
    sudo -n install -d -m 1777 "$best/ganja-handoff-tests"
    best="$best/ganja-handoff-tests"
  fi
  directory="$(mktemp -d "$best/ganja-handoff.XXXXXX")"
  install -m 0755 /bin/true "$directory/execution-preflight"
  "$directory/execution-preflight"
  rm "$directory/execution-preflight"
  export TMPDIR="$directory"
  printf 'TMPDIR=%s\n' "$TMPDIR" >> "$GITHUB_ENV"
  printf 'Chosen test temp directory: %s (available bytes %s).\n' "$TMPDIR" "$(free_bytes "$TMPDIR")"
}

require_compile_space() {
  require_space || return $?
  local bytes floor
  bytes="$(free_bytes "$PWD/target")"
  floor="$(space_floor)"
  if (( bytes < floor )); then
    printf 'Insufficient pre-compilation headroom: target-volume=%s floor=%s.\n' "$bytes" "$floor" >&2
    return 78
  fi
}

require_space() {
  local floor bytes root_bytes
  floor="$(space_floor)"
  bytes="$(free_bytes "${TMPDIR:-/tmp}")"
  root_bytes="$(free_bytes "$PWD/target")"
  if (( bytes < floor || root_bytes < 536870912 )); then
    printf 'Insufficient harness disk space: test-temp=%s floor=%s target-volume=%s minimum=536870912.\n' "$bytes" "$floor" "$root_bytes" >&2
    return 78
  fi
}
