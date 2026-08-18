#!/usr/bin/env bash
set -Eeuo pipefail

json_string() {
  local value="${1//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//$'\n'/\\n}"
  printf '"%s"' "$value"
}

read_text() {
  local path="$1"
  [[ -r "$path" ]] || { printf 'unavailable'; return; }
  tr -d '\0' < "$path" | tr '\n' ' ' | sed 's/[[:space:]]\+$//'
}

command_version() {
  local command="$1"
  command -v "$command" >/dev/null 2>&1 || { printf 'unavailable'; return; }
  "$command" --version 2>/dev/null | head -n 1 || printf 'unknown'
}

gst_element() {
  local element="$1"
  if command -v gst-inspect-1.0 >/dev/null 2>&1 && gst-inspect-1.0 "$element" >/dev/null 2>&1; then
    printf 'true'
  else
    printf 'false'
  fi
}

os_id="unknown"
os_codename="unknown"
if [[ -r /etc/os-release ]]; then
  # shellcheck disable=SC1091
  source /etc/os-release
  os_id="${ID:-unknown}"
  os_codename="${VERSION_CODENAME:-unknown}"
fi

model="$(read_text /proc/device-tree/model)"
kernel="$(uname -r)"
architecture="$(uname -m)"
cma_kib="$(awk '/^CmaTotal:/ {print $2; found=1} END {if (!found) print 0}' /proc/meminfo 2>/dev/null || printf 0)"
firmware="unavailable"
if command -v vcgencmd >/dev/null 2>&1; then
  firmware="$(vcgencmd version 2>/dev/null | tr '\n' ' ' | sed 's/[[:space:]]\+$//' || printf unknown)"
fi
gstreamer="$(command_version gst-launch-1.0)"
rswebrtc_version="unavailable"
if command -v gst-inspect-1.0 >/dev/null 2>&1 && gst-inspect-1.0 webrtcsink >/dev/null 2>&1; then
  rswebrtc_version="$(gst-inspect-1.0 webrtcsink 2>/dev/null | awk -F: '/^[[:space:]]+Version[[:space:]]*:/ {sub(/^[[:space:]]+/, "", $2); print $2; exit}')"
fi

printf '{\n'
printf '  "schema_version": 1,\n'
printf '  "captured_at": '; json_string "$(date -u +%Y-%m-%dT%H:%M:%SZ)"; printf ',\n'
printf '  "os": {"id": '; json_string "$os_id"; printf ', "codename": '; json_string "$os_codename"; printf '},\n'
printf '  "hardware": {"model": '; json_string "$model"; printf ', "architecture": '; json_string "$architecture"; printf ', "kernel": '; json_string "$kernel"; printf ', "firmware": '; json_string "$firmware"; printf ', "cma_kib": %s},\n' "$cma_kib"
printf '  "devices": {"encoder_video11": %s, "render_node": %s},\n' "$([[ -e /dev/video11 ]] && printf true || printf false)" "$([[ -e /dev/dri/renderD128 ]] && printf true || printf false)"
printf '  "gstreamer": {"version": '; json_string "$gstreamer"; printf ', "plugin_abi": "1.0", "rswebrtc_version": '; json_string "$rswebrtc_version"; printf ', "elements": {'
printf '"wpesrc": %s, "webrtcsink": %s, "v4l2h264enc": %s, "h264parse": %s, "opusenc": %s' \
  "$(gst_element wpesrc)" "$(gst_element webrtcsink)" "$(gst_element v4l2h264enc)" "$(gst_element h264parse)" "$(gst_element opusenc)"
printf '}}\n}\n'
