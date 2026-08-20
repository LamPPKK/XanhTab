#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
fixture_dir="$(mktemp -d -t xanhtab-x0-preflight.XXXXXXXX)"
trap 'rm -rf -- "$fixture_dir"' EXIT

cmdline="$fixture_dir/cmdline"
controllers="$fixture_dir/cgroup.controllers"
boot_cmdline="$fixture_dir/boot-cmdline.txt"

printf '%s\n' 'console=serial0,115200 cgroup_disable=memory rootwait' > "$cmdline"
printf '%s\n' 'cpuset cpu io pids' > "$controllers"
printf '%s\n' 'console=serial0,115200 rootwait' > "$boot_cmdline"

disabled="$({
  XANHTAB_PROC_CMDLINE_PATH="$cmdline" \
  XANHTAB_CGROUP_CONTROLLERS_PATH="$controllers" \
  XANHTAB_BOOT_CMDLINE_PATH="$boot_cmdline" \
    "$ROOT/scripts/x0-preflight-json.sh"
})"

jq -e '
  .cgroups == {
    "version": "v2",
    "memory_controller": false,
    "cmdline_memory_disabled": true,
    "boot_file_enable_memory": false,
    "boot_file_memory_accounting": false
  } and
  (.hardware.cma_free_kib | type == "number") and
  (.packages | keys == ["gstreamer_tools", "gstreamer_wpe", "linux_image_rpi_v8", "raspi_firmware", "wpewebkit"])
' <<< "$disabled" >/dev/null

printf '%s\n' 'console=serial0,115200 rootwait' > "$cmdline"
printf '%s\n' 'cpuset cpu io memory pids' > "$controllers"
printf '%s\n' 'console=serial0,115200 rootwait cgroup_memory=1 cgroup_enable=memory' > "$boot_cmdline"

enabled="$({
  XANHTAB_PROC_CMDLINE_PATH="$cmdline" \
  XANHTAB_CGROUP_CONTROLLERS_PATH="$controllers" \
  XANHTAB_BOOT_CMDLINE_PATH="$boot_cmdline" \
    "$ROOT/scripts/x0-preflight-json.sh"
})"

jq -e '
  .cgroups == {
    "version": "v2",
    "memory_controller": true,
    "cmdline_memory_disabled": false,
    "boot_file_enable_memory": true,
    "boot_file_memory_accounting": true
  }
' <<< "$enabled" >/dev/null
