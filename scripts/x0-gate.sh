#!/usr/bin/env bash
set -Eeuo pipefail

PROFILE_SECONDS=7200
CORPUS="benchmarks/x0-corpus.txt"
BROWSER_BIN="target/release/xanhtab-browser"
OUTPUT=""
DRY_RUN=0

usage() {
  printf '%s\n' \
    'Usage: scripts/x0-gate.sh [options]' \
    '  --profile-seconds N   Runtime for each profile; release gate requires 7200.' \
    '  --corpus FILE         Exactly 20 representative absolute URLs.' \
    '  --browser-bin FILE    Compiled xanhtab-browser binary.' \
    '  --output DIR          Result directory (default .x0-results/<UTC timestamp>).' \
    '  --dry-run             Validate hardware and print planned matrix.'
}

die() { printf 'x0-gate: error: %s\n' "$*" >&2; exit 1; }
log() { printf 'x0-gate: %s\n' "$*"; }

while (($#)); do
  case "$1" in
    --profile-seconds) [[ $# -ge 2 ]] || die "missing duration"; PROFILE_SECONDS="$2"; shift 2 ;;
    --corpus) [[ $# -ge 2 ]] || die "missing corpus"; CORPUS="$2"; shift 2 ;;
    --browser-bin) [[ $# -ge 2 ]] || die "missing browser binary"; BROWSER_BIN="$2"; shift 2 ;;
    --output) [[ $# -ge 2 ]] || die "missing output directory"; OUTPUT="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$PROFILE_SECONDS" =~ ^[0-9]+$ && "$PROFILE_SECONDS" -gt 0 ]] || die "duration must be a positive integer"
[[ -r "$CORPUS" ]] || die "corpus not found: $CORPUS"
mapfile -t URLS < <(sed '/^[[:space:]]*#/d; /^[[:space:]]*$/d' "$CORPUS")
[[ "${#URLS[@]}" == 20 ]] || die "X0 corpus must contain exactly 20 URLs"
for url in "${URLS[@]}"; do
  [[ "$url" =~ ^https?:// ]] || die "corpus contains a non-HTTP URL"
done

[[ "$(uname -m)" == "aarch64" ]] || die "X0 must run on aarch64 hardware"
grep -aq "Raspberry Pi Zero 2 W" /proc/device-tree/model 2>/dev/null || die "X0 must run on a real Raspberry Pi Zero 2 W"
[[ -r /proc/cmdline ]] || die "kernel command line is unavailable"
if grep -Eq '(^|[[:space:]])cgroup_disable=memory($|[[:space:]])' /proc/cmdline; then
  die "memory cgroup is disabled by the kernel command line"
fi
[[ -r /sys/fs/cgroup/cgroup.controllers ]] || die "cgroup v2 controllers are unavailable"
grep -qw memory /sys/fs/cgroup/cgroup.controllers || die "cgroup v2 memory controller is unavailable"
for element in wpesrc v4l2h264enc h264parse webrtcsink opusenc; do
  gst-inspect-1.0 "$element" >/dev/null || die "missing GStreamer element: $element"
done
[[ -e /dev/video11 ]] || die "Raspberry Pi H.264 encoder device /dev/video11 is unavailable"
[[ -x "$BROWSER_BIN" ]] || die "browser binary is not executable: $BROWSER_BIN"

if [[ -z "$OUTPUT" ]]; then
  OUTPUT=".x0-results/$(date -u +%Y%m%dT%H%M%SZ)"
fi
mkdir -p "$OUTPUT/logs"
scripts/x0-preflight-json.sh > "$OUTPUT/preflight.json"

cat > "$OUTPUT/environment.txt" <<EOF
started_at=$(date -u +%FT%TZ)
model=$(tr -d '\0' < /proc/device-tree/model)
kernel=$(uname -srvm)
profile_seconds=$PROFILE_SECONDS
corpus_sha256=$(sha256sum "$CORPUS" | awk '{print $1}')
browser_sha256=$(sha256sum "$BROWSER_BIN" | awk '{print $1}')
EOF
printf 'timestamp,profile,site_index,mem_available_mib,temperature_c,throttled,wifi_signal_dbm,browser_tree_rss_kib,browser_tree_cpu_percent\n' > "$OUTPUT/samples.csv"

if [[ "$PROFILE_SECONDS" -lt 7200 ]]; then
  log "WARNING: this is an exploratory run; X0 requires 7200 seconds for each profile"
fi
if [[ "$DRY_RUN" == 1 ]]; then
  printf 'profiles=1080p30,720p15,480p10 sites=%s total_seconds=%s\n' "${#URLS[@]}" "$((PROFILE_SECONDS * 3))"
  exit 0
fi

sample() {
  local profile="$1" site="$2" browser_pid="$3"
  local mem temp throttled wifi rss cpu
  mem="$(awk '/MemAvailable:/ {printf "%d", $2 / 1024}' /proc/meminfo)"
  temp="$(awk '{printf "%.1f", $1 / 1000}' /sys/class/thermal/thermal_zone0/temp)"
  throttled="$(vcgencmd get_throttled 2>/dev/null | cut -d= -f2 || printf 'unavailable')"
  wifi="$(awk 'NR==3 {gsub("\\.", "", $4); print $4}' /proc/net/wireless 2>/dev/null || true)"
  read -r rss cpu < <(ps -e -o pid=,ppid=,rss=,%cpu= | awk -v root="$browser_pid" '
    { parent[$1]=$2; memory[$1]=$3; usage[$1]=$4; ids[$1]=1 }
    function descendant(pid, next, depth) {
      if (pid == root) return 1
      next = parent[pid]
      if (next == "" || next == 0 || next == pid || depth > 64) return 0
      return descendant(next, parent[next], depth + 1)
    }
    END { for (pid in ids) if (descendant(pid, 0, 0)) { rss += memory[pid]; cpu += usage[pid] }; printf "%d %.1f\n", rss, cpu }
  ')
  printf '%s,%s,%s,%s,%s,%s,%s,%s,%s\n' "$(date -u +%FT%TZ)" "$profile" "$site" "${mem:-0}" "${temp:-0}" "${throttled:-unknown}" "${wifi:-unknown}" "${rss:-0}" "${cpu:-0}" >> "$OUTPUT/samples.csv"
}

run_site() {
  local profile="$1" site="$2" url="$3" seconds="$4"
  local runtime fifo browser_pid elapsed log_file
  runtime="$(mktemp -d -t xanhtab-x0.XXXXXXXX)"
  fifo="$runtime/control"
  mkfifo "$fifo"
  log_file="$OUTPUT/logs/${profile}-$(printf '%02d' "$site").log"
  GST_DEBUG="2,webrtcsink:4" "$BROWSER_BIN" --stdio < "$fifo" > "$log_file" 2>&1 &
  browser_pid=$!
  exec 3> "$fifo"
  printf '{"command":"start","session_id":"00000000-0000-4000-8000-%012d","url":"%s","stream_profile":"%s","egress":"direct"}\n' "$site" "$url" "$profile" >&3
  elapsed=0
  while (( elapsed < seconds )); do
    kill -0 "$browser_pid" 2>/dev/null || die "browser bridge exited during $profile site $site"
    sample "$profile" "$site" "$browser_pid"
    sleep 5
    elapsed=$((elapsed + 5))
  done
  printf '{"command":"stop"}\n' >&3
  exec 3>&-
  wait "$browser_pid"
  rm -rf -- "$runtime"
}

for profile in 1080p30 720p15 480p10; do
  per_site=$((PROFILE_SECONDS / 20))
  remainder=$((PROFILE_SECONDS % 20))
  for index in "${!URLS[@]}"; do
    seconds="$per_site"
    if (( index < remainder )); then seconds=$((seconds + 1)); fi
    log "profile=$profile site=$((index + 1))/20 seconds=$seconds url=${URLS[$index]}"
    run_site "$profile" "$((index + 1))" "${URLS[$index]}" "$seconds"
  done
done

awk -F, 'NR > 1 { if (min_mem == 0 || $4 < min_mem) min_mem=$4; if ($5 > max_temp) max_temp=$5; if ($8 > peak_rss) peak_rss=$8 } END { printf "min_mem_available_mib=%s\nmax_temperature_c=%s\nbridge_peak_rss_kib=%s\n", min_mem, max_temp, peak_rss }' "$OUTPUT/samples.csv" > "$OUTPUT/summary.txt"
printf 'completed_at=%s\n' "$(date -u +%FT%TZ)" >> "$OUTPUT/summary.txt"
log "capture complete: $OUTPUT"
log "add frame-drop, packet-loss, and input-to-paint p95 measurements before running evaluate-x0.sh"
