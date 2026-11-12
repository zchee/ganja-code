#!/usr/bin/env bash

free_bytes() {
  local -a rows
  mapfile -t rows < <(df -B1 --output=avail "$1")
  printf '%s\n' "${rows[1]//[[:space:]]/}"
}

binary_bytes() {
  if [[ -f target/debug/ganja ]]; then stat -c %s target/debug/ganja; else printf '0\n'; fi
}

space_floor() {
  # Protect diagnostic output only; leave CI's test-copy pressure unchanged.
  printf '209715200\n'
}

record_disk() {
  local label="$1" results="$PWD/target/handoff-reset-results"
  local path bytes target_bytes=0 source=not-built
  if [[ -f target/debug/ganja ]]; then source=measured; fi
  if [[ -d target ]]; then read -r target_bytes _ < <(du -sb target); fi
  for path in / "${TMPDIR:-/tmp}" "$PWD/target"; do
    bytes="$(free_bytes "$path")"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" "$label" "$path" "$bytes" "$(binary_bytes)" "$source" "$target_bytes" "$(space_floor)" >> "$results/disk.tsv"
  done
}

require_space() {
  local bytes
  bytes="$(free_bytes "$PWD/target")"
  if (( bytes < $(space_floor) )); then
    printf 'Diagnostic-output floor reached: available=%s minimum=%s.\n' "$bytes" "$(space_floor)" >&2
    return 78
  fi
}
