#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
fixture_dir="$(mktemp -d -t xanhtab-x0-preflight.XXXXXXXX)"
trap 'rm -rf -- "$fixture_dir"' EXIT

cmdline="$fixture_dir/cmdline"
controllers="$fixture_dir/cgroup.controllers"

printf '%s\n' 'console=serial0,115200 cgroup_disable=memory rootwait' > "$cmdline"
printf '%s\n' 'cpuset cpu io pids' > "$controllers"

disabled="$({
  XANHTAB_PROC_CMDLINE_PATH="$cmdline" \
  XANHTAB_CGROUP_CONTROLLERS_PATH="$controllers" \
    "$ROOT/scripts/x0-preflight-json.sh"
})"

jq -e '
  .cgroups == {
    "version": "v2",
    "memory_controller": false,
    "cmdline_memory_disabled": true
  }
' <<< "$disabled" >/dev/null

printf '%s\n' 'console=serial0,115200 rootwait' > "$cmdline"
printf '%s\n' 'cpuset cpu io memory pids' > "$controllers"

enabled="$({
  XANHTAB_PROC_CMDLINE_PATH="$cmdline" \
  XANHTAB_CGROUP_CONTROLLERS_PATH="$controllers" \
    "$ROOT/scripts/x0-preflight-json.sh"
})"

jq -e '
  .cgroups == {
    "version": "v2",
    "memory_controller": true,
    "cmdline_memory_disabled": false
  }
' <<< "$enabled" >/dev/null
