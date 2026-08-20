#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
# shellcheck source=install.sh
source "$ROOT/install.sh"

fixture_dir="$(mktemp -d -t xanhtab-release-manifest.XXXXXXXX)"
trap 'rm -rf -- "$fixture_dir"' EXIT
readonly fixture_dir

valid="$fixture_dir/valid.json"
"$ROOT/scripts/render-release-manifest.sh" \
  0.1.0-dev.1 \
  xanhtab-0.1.0-dev.1-linux-aarch64.tar.zst \
  https://releases.example/xanhtab-0.1.0-dev.1-linux-aarch64.tar.zst \
  0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  2.48.3-1 \
  1.26.2 \
  0.14.0 \
  > "$valid"
validate_release_manifest "$valid"

assert_rejected() {
  local label="$1" filter="$2" output="$fixture_dir/$1.json"
  jq "$filter" "$valid" > "$output"
  if validate_release_manifest "$output"; then
    printf 'manifest fixture unexpectedly accepted: %s\n' "$label" >&2
    exit 1
  fi
}

assert_rejected unknown_top_level '.unreviewed = true'
assert_rejected duplicate_artifact '.artifacts += [.artifacts[0]]'
assert_rejected missing_component 'del(.component_versions.rswebrtc)'
assert_rejected empty_component '.component_versions.gstreamer = ""'
assert_rejected unsafe_name '.artifacts[0].name = "../xanhtab.tar.zst"'
assert_rejected insecure_url '.artifacts[0].url = "http://releases.example/xanhtab.tar.zst"'
assert_rejected credential_url '.artifacts[0].url = "https://user@releases.example/xanhtab.tar.zst"'
assert_rejected malformed_checksum '.artifacts[0].sha256 = "ABCDEF"'
assert_rejected wrong_platform '.artifacts[0].platform = "linux-amd64"'

if "$ROOT/scripts/render-release-manifest.sh" \
  0.1.0-dev.1 \
  xanhtab.tar.zst \
  https://user@releases.example/xanhtab.tar.zst \
  0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  2.48.3-1 \
  1.26.2 \
  0.14.0 \
  >/dev/null 2>&1; then
  printf '%s\n' 'release renderer accepted a credential-bearing URL' >&2
  exit 1
fi

if "$ROOT/scripts/render-release-manifest.sh" \
  0.1.0-dev.1 \
  ../xanhtab.tar.zst \
  https://releases.example/xanhtab.tar.zst \
  0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  2.48.3-1 \
  1.26.2 \
  0.14.0 \
  >/dev/null 2>&1; then
  printf '%s\n' 'release renderer accepted an unsafe artifact name' >&2
  exit 1
fi
