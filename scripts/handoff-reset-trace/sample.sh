#!/usr/bin/env bash
set -uo pipefail

results="$PWD/target/handoff-reset-results"
# shellcheck source=scripts/handoff-reset-trace/disk.sh
source scripts/handoff-reset-trace/disk.sh
printf 'epoch_ns\tutc\tfree_bytes\n' > "$results/disk-samples.tsv" || note 'Disk sampler header failed.'
printf 'epoch_ns\tutc\tfree_disk_bytes\tmem_total_kib\tmem_free_kib\tmem_available_kib\tcached_kib\tdirty_kib\twriteback_kib\tdirty_pages\twriteback_pages\tpage_size_bytes\n' > "$results/system-samples.tsv" || note 'System sampler header failed.'
page_size="$(getconf PAGESIZE 2>/dev/null)"
if [[ ! "$page_size" =~ ^[1-9][0-9]*$ ]]; then page_size=unknown; note 'Page size unavailable; raw KiB memory fields still recorded.'; fi
while true; do
  epoch="$(date +%s%N)"
  utc="$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')"
  available_disk="$(free_bytes /)"
  values="$(awk '/^(MemTotal|MemFree|MemAvailable|Cached|Dirty|Writeback):/ { value[$1]=$2 } END { keys[1]="MemTotal:"; keys[2]="MemFree:"; keys[3]="MemAvailable:"; keys[4]="Cached:"; keys[5]="Dirty:"; keys[6]="Writeback:"; for(i=1;i<=6;i++) printf "%s%s", (i==1 ? "" : "\t"), (value[keys[i]] == "" ? "unknown" : value[keys[i]]); print "" }' /proc/meminfo 2>/dev/null)" || note 'Memory sample unavailable.'
  IFS=$'\t' read -r total free available cached dirty writeback <<< "$values"
  dirty_pages=unknown
  writeback_pages=unknown
  if [[ "$page_size" =~ ^[1-9][0-9]*$ && "${dirty:-unknown}" =~ ^[0-9]+$ ]]; then dirty_pages=$(( dirty * 1024 / page_size )); fi
  if [[ "$page_size" =~ ^[1-9][0-9]*$ && "${writeback:-unknown}" =~ ^[0-9]+$ ]]; then writeback_pages=$(( writeback * 1024 / page_size )); fi
  printf '%s\t%s\t%s\n' "$epoch" "$utc" "$available_disk" >> "$results/disk-samples.tsv" || note 'Disk sampler row failed.'
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$epoch" "$utc" "$available_disk" "${total:-unknown}" "${free:-unknown}" "${available:-unknown}" "${cached:-unknown}" "${dirty:-unknown}" "${writeback:-unknown}" "$dirty_pages" "$writeback_pages" "$page_size" >> "$results/system-samples.tsv" || note 'System sampler row failed.'
  for name in capture kernel; do
    pidfile="$results/observers/$name.pid"
    if [[ -s "$pidfile" && ! -e "$results/observers/$name.not-running" ]]; then
      observer="$(< "$pidfile")"
      if [[ ! -d "/proc/$observer" ]]; then
        printf 'Observer process absent at %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%S.%NZ')" > "$results/observers/$name.not-running" || note 'Observer-loss record unavailable.'
      fi
    fi
  done
  sleep 1 || { note 'Sampler wait failed; sampler stops without blocking tests.'; break; }
done
exit 0
