#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
fixture_dir="$(mktemp -d -t xanhtab-encoder-probe.XXXXXXXX)"
trap 'rm -rf -- "$fixture_dir"' EXIT

model="$fixture_dir/model"
fake_gst_launch="$fixture_dir/gst-launch-1.0"
fake_gst_inspect="$fixture_dir/gst-inspect-1.0"
fake_timeout="$fixture_dir/timeout"
fake_dmesg="$fixture_dir/dmesg"

printf 'Raspberry Pi Zero 2 W Rev 1.0\0' > "$model"

# These single-quoted lines are the literal body of the fake executable.
# shellcheck disable=SC2016
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -Eeuo pipefail' \
  'if [[ "${FAKE_ALL_PASS:-0}" != 1 && "$*" == *"width=1280,height=720,framerate=15/1"* ]]; then' \
  '  printf "%s\n" "Failed to process frame" >&2' \
  '  exit 5' \
  'fi' \
  'exit 0' > "$fake_gst_launch"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'exit 0' > "$fake_gst_inspect"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'printf "%s\n" "mac=AA:BB:CC:DD:EE:FF root=PARTUUID=abcd-02 ds=nocloud;i=fixture-id ip=192.168.1.35"' \
  'printf "%s\n" "usb SerialNumber: unique-fixture"' > "$fake_dmesg"

# These single-quoted lines are the literal body of the fake executable.
# shellcheck disable=SC2016
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -Eeuo pipefail' \
  'while (($#)) && [[ "$1" == --* ]]; do shift; done' \
  '[[ $# -ge 2 ]] || exit 64' \
  'shift' \
  'exec "$@"' > "$fake_timeout"

chmod 0755 "$fake_gst_launch" "$fake_gst_inspect" "$fake_timeout" "$fake_dmesg"

run_probe() {
  XANHTAB_MODEL_PATH="$model" \
  XANHTAB_VIDEO_DEVICE=/dev/null \
  XANHTAB_ARCHITECTURE=aarch64 \
  XANHTAB_GST_LAUNCH_BIN="$fake_gst_launch" \
  XANHTAB_GST_INSPECT_BIN="$fake_gst_inspect" \
  XANHTAB_TIMEOUT_BIN="$fake_timeout" \
  XANHTAB_DMESG_BIN="$fake_dmesg" \
    "$ROOT/scripts/x0-encoder-probe.sh" "$@"
}

failed_output="$fixture_dir/failed"
set +e
run_probe --output "$failed_output" --frames 2 --timeout-seconds 5
failed_status=$?
set -e
[[ "$failed_status" == 1 ]]

jq -e '
  .schema_version == 1 and
  (.profiles | map(.profile)) == ["480p10", "720p15", "720p30", "1080p30"] and
  (.profiles | length) == 4 and
  (.profiles[] | select(.profile == "720p15") |
    .passed == false and .exit_code == 5 and .classification == "frame-processing-error") and
  .verdict == {
    "emergency_480p10": true,
    "release_floor_720p15": false,
    "all_profiles": false,
    "status": "no-go"
  }
' "$failed_output/summary.json" >/dev/null
[[ "$(find "$failed_output/logs" -type f -name '*.log' | wc -l | tr -d ' ')" == 4 ]]
[[ ! -e "$failed_output/.profile-results.jsonl" ]]
(cd "$failed_output" && sha256sum -c SHA256SUMS >/dev/null)
for dmesg_capture in "$failed_output/dmesg-before.log" "$failed_output/dmesg-after.log"; do
  grep -Fq '<redacted-mac>' "$dmesg_capture"
  grep -Fq 'PARTUUID=<redacted>' "$dmesg_capture"
  grep -Fq 'ds=nocloud;i=<redacted>' "$dmesg_capture"
  grep -Fq '<redacted-ipv4>' "$dmesg_capture"
  grep -Fq 'SerialNumber: <redacted>' "$dmesg_capture"
  if grep -Eq 'AA:BB:CC:DD:EE:FF|abcd-02|fixture-id|192\.168\.1\.35|unique-fixture' "$dmesg_capture"; then
    printf 'unredacted identifier in %s\n' "$dmesg_capture" >&2
    exit 1
  fi
done

passed_output="$fixture_dir/passed"
FAKE_ALL_PASS=1 run_probe --output "$passed_output" --frames 2 --timeout-seconds 5
jq -e '
  .verdict.release_floor_720p15 == true and
  .verdict.all_profiles == true and
  .verdict.status == "release-floor-pass" and
  all(.profiles[]; .passed and .classification == "passed")
' "$passed_output/summary.json" >/dev/null

dry_run="$(run_probe --dry-run --frames 2 --timeout-seconds 5)"
[[ "$dry_run" == *'profiles=480p10,720p15,720p30,1080p30'* ]]
[[ "$dry_run" != *, ]]
