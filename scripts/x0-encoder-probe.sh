#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
readonly MODEL_PATH="${XANHTAB_MODEL_PATH:-/proc/device-tree/model}"
readonly VIDEO_DEVICE="${XANHTAB_VIDEO_DEVICE:-/dev/video11}"
readonly GST_LAUNCH_BIN="${XANHTAB_GST_LAUNCH_BIN:-gst-launch-1.0}"
readonly GST_INSPECT_BIN="${XANHTAB_GST_INSPECT_BIN:-gst-inspect-1.0}"
readonly TIMEOUT_BIN="${XANHTAB_TIMEOUT_BIN:-timeout}"
readonly PREFLIGHT_BIN="${XANHTAB_PREFLIGHT_BIN:-$ROOT/scripts/x0-preflight-json.sh}"
readonly DMESG_BIN="${XANHTAB_DMESG_BIN:-dmesg}"
readonly ARCHITECTURE="${XANHTAB_ARCHITECTURE:-$(uname -m)}"
readonly PROFILES=(
  "480p10:854:480:10"
  "720p15:1280:720:15"
  "720p30:1280:720:30"
  "1080p30:1920:1080:30"
)

OUTPUT=""
FRAMES=300
TIMEOUT_SECONDS=30
DRY_RUN=0

usage() {
  printf '%s\n' \
    'Usage: scripts/x0-encoder-probe.sh [options]' \
    '  --output DIR          New evidence directory (default .x0-results/encoder-<UTC>).' \
    '  --frames N            Frames per profile (default 300).' \
    '  --timeout-seconds N   Per-profile deadline (default 30).' \
    '  --dry-run             Validate hardware and print the planned matrix.'
}

die() { printf 'x0-encoder-probe: error: %s\n' "$*" >&2; exit 1; }
log() { printf 'x0-encoder-probe: %s\n' "$*" >&2; }

require_command() {
  local candidate="$1"
  if [[ "$candidate" == */* ]]; then
    [[ -x "$candidate" ]] || die "required executable is unavailable: $candidate"
  else
    command -v "$candidate" >/dev/null 2>&1 || die "required command is unavailable: $candidate"
  fi
}

now_ms() {
  local seconds fraction
  seconds="$(date +%s)"
  fraction="$(date +%N 2>/dev/null || true)"
  if [[ "$fraction" =~ ^[0-9]{9}$ ]]; then
    printf '%s%s\n' "$seconds" "${fraction:0:3}"
  else
    printf '%s000\n' "$seconds"
  fi
}

classification_for() {
  local exit_code="$1" log_file="$2"
  if [[ "$exit_code" == 0 ]]; then
    printf 'passed\n'
  elif [[ "$exit_code" == 124 || "$exit_code" == 137 ]]; then
    printf 'timeout\n'
  elif grep -Eqi 'failed to process frame|not enough memory|failing driver' "$log_file"; then
    printf 'frame-processing-error\n'
  elif grep -Eqi 'not-negotiated|could not link|failed to configure' "$log_file"; then
    printf 'negotiation-error\n'
  elif grep -Eqi 'permission denied|could not open device' "$log_file"; then
    printf 'device-access-error\n'
  else
    printf 'pipeline-error\n'
  fi
}

capture_dmesg() {
  local destination="$1"
  if require_optional_command "$DMESG_BIN" \
    && "$DMESG_BIN" 2>/dev/null \
      | sed -E \
        -e 's/([[:xdigit:]]{2}:){5}[[:xdigit:]]{2}/<redacted-mac>/g' \
        -e 's/(PARTUUID|UUID)=[^[:space:]]+/\1=<redacted>/g' \
        -e 's/(ds=nocloud;i=)[^[:space:]]+/\1<redacted>/g' \
        -e 's/(SerialNumber:)[[:space:]]*[^[:space:]]+/\1 <redacted>/g' \
        -e 's/([0-9]{1,3}\.){3}[0-9]{1,3}/<redacted-ipv4>/g' \
      > "$destination"; then
    return
  fi
  printf '%s\n' 'dmesg unavailable to the invoking user' > "$destination"
}

require_optional_command() {
  local candidate="$1"
  if [[ "$candidate" == */* ]]; then
    [[ -x "$candidate" ]]
  else
    command -v "$candidate" >/dev/null 2>&1
  fi
}

while (($#)); do
  case "$1" in
    --output) [[ $# -ge 2 ]] || die "--output requires a value"; OUTPUT="$2"; shift 2 ;;
    --frames) [[ $# -ge 2 ]] || die "--frames requires a value"; FRAMES="$2"; shift 2 ;;
    --timeout-seconds) [[ $# -ge 2 ]] || die "--timeout-seconds requires a value"; TIMEOUT_SECONDS="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$FRAMES" =~ ^[0-9]+$ && "$FRAMES" -gt 0 && "$FRAMES" -le 3600 ]] \
  || die "--frames must be an integer between 1 and 3600"
[[ "$TIMEOUT_SECONDS" =~ ^[0-9]+$ && "$TIMEOUT_SECONDS" -ge 5 && "$TIMEOUT_SECONDS" -le 300 ]] \
  || die "--timeout-seconds must be an integer between 5 and 300"
[[ "$ARCHITECTURE" == aarch64 ]] || die "encoder probe must run on aarch64 hardware"
[[ -r "$MODEL_PATH" ]] || die "Raspberry Pi model information is unavailable"
grep -aq 'Raspberry Pi Zero 2 W' "$MODEL_PATH" || die "encoder probe must run on a Raspberry Pi Zero 2 W"
[[ -e "$VIDEO_DEVICE" ]] || die "encoder device is unavailable: $VIDEO_DEVICE"

for executable in "$GST_LAUNCH_BIN" "$GST_INSPECT_BIN" "$TIMEOUT_BIN" "$PREFLIGHT_BIN" jq sha256sum; do
  require_command "$executable"
done
for element in videotestsrc v4l2h264enc h264parse; do
  "$GST_INSPECT_BIN" "$element" >/dev/null 2>&1 || die "missing GStreamer element: $element"
done

if [[ "$DRY_RUN" == 1 ]]; then
  profile_names=()
  for profile_row in "${PROFILES[@]}"; do
    profile_names+=("${profile_row%%:*}")
  done
  printf 'device=%s frames=%s timeout_seconds=%s profiles=' "$VIDEO_DEVICE" "$FRAMES" "$TIMEOUT_SECONDS"
  (IFS=,; printf '%s\n' "${profile_names[*]}")
  exit 0
fi

if [[ -z "$OUTPUT" ]]; then
  OUTPUT="$ROOT/.x0-results/encoder-$(date -u +%Y%m%dT%H%M%SZ)"
fi
[[ ! -e "$OUTPUT" ]] || die "output path already exists: $OUTPUT"
mkdir -p "$OUTPUT/logs"

"$PREFLIGHT_BIN" > "$OUTPUT/preflight.json"
jq -e '.schema_version == 1' "$OUTPUT/preflight.json" >/dev/null \
  || die "preflight output is not schema version 1 JSON"
capture_dmesg "$OUTPUT/dmesg-before.log"

profile_results="$OUTPUT/.profile-results.jsonl"
: > "$profile_results"

for profile_row in "${PROFILES[@]}"; do
  IFS=: read -r profile width height fps <<< "$profile_row"
  log_file="$OUTPUT/logs/$profile.log"
  start_ms="$(now_ms)"
  log "profile=$profile width=$width height=$height fps=$fps frames=$FRAMES"

  set +e
  "$TIMEOUT_BIN" --signal=TERM --kill-after=5s "${TIMEOUT_SECONDS}s" \
    "$GST_LAUNCH_BIN" -q \
      videotestsrc "num-buffers=$FRAMES" is-live=true ! \
      "video/x-raw,format=I420,width=$width,height=$height,framerate=$fps/1" ! \
      v4l2h264enc ! h264parse ! fakesink sync=false \
      > "$log_file" 2>&1
  exit_code=$?
  set -e

  end_ms="$(now_ms)"
  duration_ms=$((end_ms - start_ms))
  ((duration_ms >= 0)) || duration_ms=0
  classification="$(classification_for "$exit_code" "$log_file")"
  passed=false
  [[ "$exit_code" == 0 ]] && passed=true

  jq -cn \
    --arg profile "$profile" \
    --arg classification "$classification" \
    --arg log_file "logs/$profile.log" \
    --argjson width "$width" \
    --argjson height "$height" \
    --argjson fps "$fps" \
    --argjson frames "$FRAMES" \
    --argjson exit_code "$exit_code" \
    --argjson duration_ms "$duration_ms" \
    --argjson passed "$passed" \
    '{
      profile: $profile,
      width: $width,
      height: $height,
      fps: $fps,
      frames: $frames,
      exit_code: $exit_code,
      duration_ms: $duration_ms,
      passed: $passed,
      classification: $classification,
      log_file: $log_file
    }' >> "$profile_results"
done

capture_dmesg "$OUTPUT/dmesg-after.log"
summary_tmp="$OUTPUT/.summary.json"
jq -s \
  --slurpfile preflight "$OUTPUT/preflight.json" \
  --arg captured_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg device "$VIDEO_DEVICE" '
    def passed($name): any(.[]; .profile == $name and .passed);
    {
      schema_version: 1,
      captured_at: $captured_at,
      device: $device,
      preflight: $preflight[0],
      profiles: .,
      verdict: {
        emergency_480p10: passed("480p10"),
        release_floor_720p15: passed("720p15"),
        all_profiles: all(.[]; .passed),
        status: (if passed("720p15") then "release-floor-pass" else "no-go" end)
      }
    }
  ' "$profile_results" > "$summary_tmp"
mv "$summary_tmp" "$OUTPUT/summary.json"
rm -f -- "$profile_results"
(
  cd "$OUTPUT"
  sha256sum preflight.json summary.json dmesg-before.log dmesg-after.log logs/*.log > SHA256SUMS
)

if jq -e '.verdict.release_floor_720p15 == true' "$OUTPUT/summary.json" >/dev/null; then
  log "release-floor-pass: $OUTPUT/summary.json"
  exit 0
fi

log "no-go: $OUTPUT/summary.json"
exit 1
