#!/usr/bin/env bash

note() {
  local results="$PWD/target/handoff-reset-results"
  printf '%s\t%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" "$*" >> "$results/diagnostic-notes.txt" 2>/dev/null || true
}

diag() {
  local description="$1" status
  shift
  if "$@"; then return 0; else status=$?; fi
  note "$description failed (exit $status); measurement continues."
  return 0
}

free_bytes() {
  local output value
  local -a rows
  if output="$(df -B1 --output=avail "$1" 2>/dev/null)"; then
    mapfile -t rows <<< "$output"
    value="${rows[1]:-unknown}"
    value="${value//[[:space:]]/}"
    if [[ "$value" =~ ^[0-9]+$ ]]; then printf '%s\n' "$value"; return 0; fi
  fi
  note "Free-space measurement unavailable for $1."
  printf 'unknown\n'
}

binary_bytes() {
  local value
  if [[ -f target/debug/ganja ]] && value="$(stat -c %s target/debug/ganja 2>/dev/null)"; then
    printf '%s\n' "$value"
  else
    printf 'unknown\n'
  fi
}

space_floor() {
  printf '209715200\n'
}

record_disk() {
  local label="$1" results="$PWD/target/handoff-reset-results"
  local path bytes target_bytes=unknown source=not-built output
  if [[ -f target/debug/ganja ]]; then source=measured; fi
  if output="$(du -sb target 2>/dev/null)"; then
    read -r target_bytes _ <<< "$output"
  else
    note "Target directory size unavailable at $label."
  fi
  for path in / "${TMPDIR:-/tmp}" "$PWD/target"; do
    bytes="$(free_bytes "$path")"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" "$label" "$path" "$bytes" "$(binary_bytes)" "$source" "$target_bytes" "$(space_floor)" >> "$results/disk.tsv" || note "Disk table write failed at $label."
  done
  return 0
}

require_space() {
  local bytes
  bytes="$(free_bytes "$PWD/target")"
  if [[ "$bytes" =~ ^[0-9]+$ ]] && (( bytes < $(space_floor) )); then
    note "Diagnostic-output floor reached: available=$bytes minimum=$(space_floor)."
    return 78
  fi
  return 0
}
