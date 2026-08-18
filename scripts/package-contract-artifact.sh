#!/usr/bin/env bash
set -Eeuo pipefail

VERSION="${1:-0.1.0-dev.1}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+-dev\.[0-9]+$ ]] || { printf 'development version must look like 0.1.0-dev.1\n' >&2; exit 2; }

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist/contracts"
STAGE="$DIST/xanhtab-v$VERSION"
ARCHIVE="$DIST/xanhtab-v$VERSION-contracts.tar.zst"
rm -rf -- "$STAGE"
install -d -m 0755 "$STAGE/schemas" "$STAGE/web" "$STAGE/docs"
cp -a "$ROOT/schemas/." "$STAGE/schemas/"
cp -a "$ROOT/web/." "$STAGE/web/"
install -m 0644 "$ROOT/docs/protocol-v1.md" "$STAGE/docs/protocol-v1.md"
checksum_file="$DIST/checksums.sha256.tmp"
(cd "$STAGE" && find . -type f -print0 | sort -z | xargs -0 sha256sum) > "$checksum_file"
mv "$checksum_file" "$STAGE/checksums.sha256"
tar --zstd -cf "$ARCHIVE" -C "$STAGE" .
(cd "$DIST" && sha256sum "$(basename "$ARCHIVE")") > "$ARCHIVE.sha256"
printf 'Unsigned development contract artifact staged at %s\n' "$ARCHIVE"
