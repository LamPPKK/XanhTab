#!/usr/bin/env bash
set -Eeuo pipefail

readonly PROC_CMDLINE_PATH="${XANHTAB_PROC_CMDLINE_PATH:-/proc/cmdline}"
readonly CGROUP_CONTROLLERS_PATH="${XANHTAB_CGROUP_CONTROLLERS_PATH:-/sys/fs/cgroup/cgroup.controllers}"
readonly BOOT_CMDLINE_PATH="${XANHTAB_BOOT_CMDLINE_PATH:-/boot/firmware/cmdline.txt}"

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

file_has_token() {
  local path="$1" token="$2"
  [[ -r "$path" ]] && grep -Eq "(^|[[:space:]])${token}($|[[:space:]])" "$path"
}

package_installed_version() {
  local package="$1" record status version
  command -v dpkg-query >/dev/null 2>&1 || { printf 'unavailable'; return; }
  if ! record="$(dpkg-query -W -f='${db:Status-Abbrev}\t${Version}\n' "$package" 2>/dev/null)"; then
    printf 'not-installed'
    return
  fi
  status="${record%%$'\t'*}"
  version="${record#*$'\t'}"
  if [[ "$status" == 'ii ' && -n "$version" ]]; then
    printf '%s' "$version"
  else
    printf 'not-installed'
  fi
}

package_candidate_version() {
  local package="$1" candidate
  command -v apt-cache >/dev/null 2>&1 || { printf 'unavailable'; return; }
  candidate="$(apt-cache policy "$package" 2>/dev/null | awk -F': ' '/^[[:space:]]+Candidate:/ {print $2; exit}')"
  printf '%s' "${candidate:-unavailable}"
}

print_package_record() {
  local package="$1"
  printf '{"installed": '
  json_string "$(package_installed_version "$package")"
  printf ', "candidate": '
  json_string "$(package_candidate_version "$package")"
  printf '}'
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
cma_free_kib="$(awk '/^CmaFree:/ {print $2; found=1} END {if (!found) print 0}' /proc/meminfo 2>/dev/null || printf 0)"
firmware="unavailable"
if command -v vcgencmd >/dev/null 2>&1; then
  firmware="$(vcgencmd version 2>/dev/null | tr '\n' ' ' | sed 's/[[:space:]]\+$//' || printf unknown)"
fi
gstreamer="$(command_version gst-launch-1.0)"
rswebrtc_version="unavailable"
if command -v gst-inspect-1.0 >/dev/null 2>&1 && gst-inspect-1.0 webrtcsink >/dev/null 2>&1; then
  rswebrtc_version="$(gst-inspect-1.0 webrtcsink 2>/dev/null | awk -F: '/^[[:space:]]+Version[[:space:]]*:/ {sub(/^[[:space:]]+/, "", $2); print $2; exit}')"
fi
cgroup_version="unavailable"
memory_controller=false
cmdline_memory_disabled=false
boot_file_enable_memory=false
boot_file_memory_accounting=false
if [[ -r "$PROC_CMDLINE_PATH" ]] \
  && grep -Eq '(^|[[:space:]])cgroup_disable=memory($|[[:space:]])' "$PROC_CMDLINE_PATH"; then
  cmdline_memory_disabled=true
fi
if file_has_token "$BOOT_CMDLINE_PATH" 'cgroup_enable=memory'; then
  boot_file_enable_memory=true
fi
if file_has_token "$BOOT_CMDLINE_PATH" 'cgroup_memory=1'; then
  boot_file_memory_accounting=true
fi
if [[ -r "$CGROUP_CONTROLLERS_PATH" ]]; then
  cgroup_version="v2"
  if grep -qw memory "$CGROUP_CONTROLLERS_PATH"; then
    memory_controller=true
  fi
fi

printf '{\n'
printf '  "schema_version": 1,\n'
printf '  "captured_at": '; json_string "$(date -u +%Y-%m-%dT%H:%M:%SZ)"; printf ',\n'
printf '  "os": {"id": '; json_string "$os_id"; printf ', "codename": '; json_string "$os_codename"; printf '},\n'
printf '  "hardware": {"model": '; json_string "$model"; printf ', "architecture": '; json_string "$architecture"; printf ', "kernel": '; json_string "$kernel"; printf ', "firmware": '; json_string "$firmware"; printf ', "cma_kib": %s, "cma_free_kib": %s},\n' "$cma_kib" "$cma_free_kib"
printf '  "devices": {"encoder_video11": %s, "render_node": %s},\n' "$([[ -e /dev/video11 ]] && printf true || printf false)" "$([[ -e /dev/dri/renderD128 ]] && printf true || printf false)"
printf '  "cgroups": {"version": '; json_string "$cgroup_version"; printf ', "memory_controller": %s, "cmdline_memory_disabled": %s, "boot_file_enable_memory": %s, "boot_file_memory_accounting": %s},\n' "$memory_controller" "$cmdline_memory_disabled" "$boot_file_enable_memory" "$boot_file_memory_accounting"
printf '  "packages": {'
printf '"linux_image_rpi_v8": '; print_package_record linux-image-rpi-v8; printf ', '
printf '"raspi_firmware": '; print_package_record raspi-firmware; printf ', '
printf '"gstreamer_tools": '; print_package_record gstreamer1.0-tools; printf ', '
printf '"gstreamer_wpe": '; print_package_record gstreamer1.0-wpe; printf ', '
printf '"wpewebkit": '; print_package_record libwpewebkit-2.0-1
printf '},\n'
printf '  "gstreamer": {"version": '; json_string "$gstreamer"; printf ', "plugin_abi": "1.0", "rswebrtc_version": '; json_string "$rswebrtc_version"; printf ', "elements": {'
printf '"wpesrc": %s, "webrtcsink": %s, "v4l2h264enc": %s, "h264parse": %s, "opusenc": %s' \
  "$(gst_element wpesrc)" "$(gst_element webrtcsink)" "$(gst_element v4l2h264enc)" "$(gst_element h264parse)" "$(gst_element opusenc)"
printf '}}\n}\n'
