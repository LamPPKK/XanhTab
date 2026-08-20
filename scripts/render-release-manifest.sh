#!/usr/bin/env bash
set -Eeuo pipefail

[[ $# == 7 ]] || {
  printf '%s\n' 'Usage: scripts/render-release-manifest.sh VERSION ARTIFACT_NAME ARTIFACT_URL SHA256 WPE_VERSION GSTREAMER_VERSION RS_WEBRTC_VERSION' >&2
  exit 2
}

readonly VERSION="$1"
readonly ARTIFACT_NAME="$2"
readonly ARTIFACT_URL="$3"
readonly SHA256="$4"
readonly WPE_VERSION="$5"
readonly GSTREAMER_VERSION="$6"
readonly RS_WEBRTC_VERSION="$7"

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] \
  || { printf '%s\n' 'VERSION is invalid' >&2; exit 2; }
[[ "$ARTIFACT_NAME" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] \
  || { printf '%s\n' 'ARTIFACT_NAME must be a safe basename' >&2; exit 2; }
[[ "$ARTIFACT_URL" =~ ^https://[^/?#@]+/[^[:space:]]+$ ]] \
  || { printf '%s\n' 'ARTIFACT_URL must use HTTPS' >&2; exit 2; }
[[ "$SHA256" =~ ^[0-9a-f]{64}$ ]] \
  || { printf '%s\n' 'SHA256 must contain 64 lowercase hexadecimal characters' >&2; exit 2; }
[[ -n "$WPE_VERSION" && -n "$GSTREAMER_VERSION" && -n "$RS_WEBRTC_VERSION" ]] \
  || { printf '%s\n' 'component versions must not be empty' >&2; exit 2; }

jq -n \
  --arg version "$VERSION" \
  --arg name "$ARTIFACT_NAME" \
  --arg url "$ARTIFACT_URL" \
  --arg sha256 "$SHA256" \
  --arg wpe "$WPE_VERSION" \
  --arg gstreamer "$GSTREAMER_VERSION" \
  --arg rswebrtc "$RS_WEBRTC_VERSION" \
  '{
    schema_version: 1,
    version: $version,
    config_schema_version: 1,
    gstreamer_abi: "1.0",
    component_versions: {
      wpe_webkit: $wpe,
      gstreamer: $gstreamer,
      rswebrtc: $rswebrtc
    },
    artifacts: [{
      platform: "linux-aarch64",
      name: $name,
      url: $url,
      sha256: $sha256
    }]
  }'
