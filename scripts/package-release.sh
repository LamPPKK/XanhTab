#!/usr/bin/env bash
set -Eeuo pipefail

VERSION="${1:-}"
BASE_URL="${2:-}"
MINISIGN_SECRET_KEY="${MINISIGN_SECRET_KEY:-}"
RSWEBRTC_PLUGIN_DIR="${RSWEBRTC_PLUGIN_DIR:-}"
WPE_VERSION="${WPE_VERSION:-}"
GSTREAMER_VERSION="${GSTREAMER_VERSION:-}"
RSWEBRTC_VERSION="${RSWEBRTC_VERSION:-}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || { printf 'Usage: MINISIGN_SECRET_KEY=... scripts/package-release.sh VERSION BASE_URL\n' >&2; exit 2; }
[[ "$BASE_URL" =~ ^https:// ]] || { printf 'BASE_URL must use HTTPS\n' >&2; exit 2; }
[[ -f "$MINISIGN_SECRET_KEY" ]] || { printf 'MINISIGN_SECRET_KEY must name the offline signing key\n' >&2; exit 2; }
[[ -f "$RSWEBRTC_PLUGIN_DIR/libgstrswebrtc.so" ]] || { printf 'RSWEBRTC_PLUGIN_DIR must contain the pinned ARM64 libgstrswebrtc.so\n' >&2; exit 2; }
[[ -n "$WPE_VERSION" && -n "$GSTREAMER_VERSION" && -n "$RSWEBRTC_VERSION" ]] || { printf 'WPE_VERSION, GSTREAMER_VERSION, and RSWEBRTC_VERSION are required\n' >&2; exit 2; }

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist/xanhtab-$VERSION"
STAGE="$DIST/stage"
ARCHIVE="xanhtab-$VERSION-linux-aarch64.tar.zst"
rm -rf -- "$DIST"
install -d -m 0755 "$STAGE/bin" "$STAGE/libexec" "$STAGE/config" "$STAGE/web" "$STAGE/systemd" "$STAGE/schemas" "$STAGE/plugins/gstreamer-1.0"

XANHTAB_BUILD_WEBKIT_VERSION="$WPE_VERSION" \
XANHTAB_BUILD_GSTREAMER_VERSION="$GSTREAMER_VERSION" \
XANHTAB_BUILD_RS_WEBRTC_VERSION="$RSWEBRTC_VERSION" \
  "${CARGO:-cargo}" test --all-targets --locked
XANHTAB_BUILD_WEBKIT_VERSION="$WPE_VERSION" \
XANHTAB_BUILD_GSTREAMER_VERSION="$GSTREAMER_VERSION" \
XANHTAB_BUILD_RS_WEBRTC_VERSION="$RSWEBRTC_VERSION" \
  "${CARGO:-cargo}" build --release --locked --target aarch64-unknown-linux-gnu
install -m 0755 "$ROOT/target/aarch64-unknown-linux-gnu/release/xanhtabd" "$STAGE/bin/xanhtabd"
install -m 0755 "$ROOT/target/aarch64-unknown-linux-gnu/release/xanhtab-browser" "$STAGE/libexec/xanhtab-browser"
install -m 0755 "$ROOT/target/aarch64-unknown-linux-gnu/release/xanhtab-netd" "$STAGE/libexec/xanhtab-netd"
install -m 0755 "$ROOT/target/aarch64-unknown-linux-gnu/release/xanhtab-blocklist" "$STAGE/libexec/xanhtab-blocklist"
install -m 0755 "$ROOT/scripts/x1-burn-audit.sh" "$STAGE/libexec/xanhtab-x1-burn-audit"
install -m 0644 "$ROOT/config/xanhtab.production.toml" "$STAGE/config/xanhtab.production.toml"
install -m 0644 "$ROOT/config/custom_hosts.txt" "$STAGE/config/custom_hosts.txt"
cp -a "$ROOT/web/." "$STAGE/web/"
cp -a "$ROOT/systemd/." "$STAGE/systemd/"
install -m 0644 "$ROOT/schemas/openapi-v1.yaml" "$STAGE/schemas/openapi-v1.yaml"
install -m 0644 "$ROOT/schemas/config.schema.json" "$STAGE/schemas/config.schema.json"
install -m 0644 "$ROOT/schemas/release-manifest.schema.json" "$STAGE/schemas/release-manifest.schema.json"
install -m 0644 "$ROOT/schemas/burn-audit.schema.json" "$STAGE/schemas/burn-audit.schema.json"
cp -a "$RSWEBRTC_PLUGIN_DIR/." "$STAGE/plugins/gstreamer-1.0/"
checksum_file="$DIST/checksums.sha256.tmp"
(cd "$STAGE" && find . -type f -print0 | sort -z | xargs -0 sha256sum) > "$checksum_file"
mv "$checksum_file" "$STAGE/checksums.sha256"
tar --zstd -cf "$DIST/$ARCHIVE" -C "$STAGE" .

checksum="$(sha256sum "$DIST/$ARCHIVE" | awk '{print $1}')"
jq -n \
  --arg version "$VERSION" \
  --arg name "$ARCHIVE" \
  --arg url "${BASE_URL%/}/$ARCHIVE" \
  --arg sha256 "$checksum" \
  --arg wpe "$WPE_VERSION" \
  --arg gstreamer "$GSTREAMER_VERSION" \
  --arg rswebrtc "$RSWEBRTC_VERSION" \
  '{schema_version: 1, version: $version, config_schema_version: 1, gstreamer_abi: "1.0", component_versions: {wpe_webkit: $wpe, gstreamer: $gstreamer, rswebrtc: $rswebrtc}, artifacts: [{platform: "linux-aarch64", name: $name, url: $url, sha256: $sha256}]}' \
  > "$DIST/release-manifest.json"
minisign -Sm "$DIST/release-manifest.json" -s "$MINISIGN_SECRET_KEY" -x "$DIST/release-manifest.json.minisig"
install -m 0755 "$ROOT/install.sh" "$DIST/install.sh"
minisign -Sm "$DIST/install.sh" -s "$MINISIGN_SECRET_KEY" -x "$DIST/install.sh.minisig"
printf 'Release staged at %s\n' "$DIST"
