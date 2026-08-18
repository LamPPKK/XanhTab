#!/usr/bin/env bash
set -Eeuo pipefail

VERSION="${1:-}"
BASE_URL="${2:-}"
MINISIGN_SECRET_KEY="${MINISIGN_SECRET_KEY:-}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || { printf 'Usage: MINISIGN_SECRET_KEY=... scripts/package-release.sh VERSION BASE_URL\n' >&2; exit 2; }
[[ "$BASE_URL" =~ ^https:// ]] || { printf 'BASE_URL must use HTTPS\n' >&2; exit 2; }
[[ -f "$MINISIGN_SECRET_KEY" ]] || { printf 'MINISIGN_SECRET_KEY must name the offline signing key\n' >&2; exit 2; }

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist/xanhtab-$VERSION"
STAGE="$DIST/stage"
ARCHIVE="xanhtab-$VERSION-linux-aarch64.tar.zst"
rm -rf -- "$DIST"
install -d -m 0755 "$STAGE/bin" "$STAGE/libexec" "$STAGE/config" "$STAGE/web" "$STAGE/systemd"

"${CARGO:-cargo}" test --all-targets --locked
"${CARGO:-cargo}" build --release --locked --target aarch64-unknown-linux-gnu
install -m 0755 "$ROOT/target/aarch64-unknown-linux-gnu/release/xanhtabd" "$STAGE/bin/xanhtabd"
install -m 0755 "$ROOT/target/aarch64-unknown-linux-gnu/release/xanhtab-browser" "$STAGE/libexec/xanhtab-browser"
install -m 0755 "$ROOT/target/aarch64-unknown-linux-gnu/release/xanhtab-netd" "$STAGE/libexec/xanhtab-netd"
install -m 0755 "$ROOT/target/aarch64-unknown-linux-gnu/release/xanhtab-blocklist" "$STAGE/libexec/xanhtab-blocklist"
install -m 0644 "$ROOT/config/xanhtab.production.toml" "$STAGE/config/xanhtab.production.toml"
cp -a "$ROOT/web/." "$STAGE/web/"
cp -a "$ROOT/systemd/." "$STAGE/systemd/"
tar --zstd -cf "$DIST/$ARCHIVE" -C "$STAGE" .

checksum="$(sha256sum "$DIST/$ARCHIVE" | awk '{print $1}')"
jq -n \
  --arg version "$VERSION" \
  --arg name "$ARCHIVE" \
  --arg url "${BASE_URL%/}/$ARCHIVE" \
  --arg sha256 "$checksum" \
  '{schema_version: 1, version: $version, config_schema_version: 1, artifacts: [{platform: "linux-aarch64", name: $name, url: $url, sha256: $sha256}]}' \
  > "$DIST/release-manifest.json"
minisign -Sm "$DIST/release-manifest.json" -s "$MINISIGN_SECRET_KEY" -x "$DIST/release-manifest.json.minisig"
install -m 0755 "$ROOT/install.sh" "$DIST/install.sh"
minisign -Sm "$DIST/install.sh" -s "$MINISIGN_SECRET_KEY" -x "$DIST/install.sh.minisig"
printf 'Release staged at %s\n' "$DIST"
