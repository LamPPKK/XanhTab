#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
readonly BLOCKLIST_BIN="${XANHTAB_BLOCKLIST_BIN:-$ROOT/target/debug/xanhtab-blocklist}"
readonly DAEMON_BIN="${XANHTABD_BIN:-$ROOT/target/debug/xanhtabd}"
[[ -x "$BLOCKLIST_BIN" && -x "$DAEMON_BIN" ]] || {
  printf '%s\n' 'build xanhtabd and xanhtab-blocklist before this test' >&2
  exit 1
}

fixture_dir="$(mktemp -d -t xanhtab-blocklist-contract.XXXXXXXX)"
trap 'rm -rf -- "$fixture_dir"' EXIT
readonly fixture_dir

install -d -m 0700 "$fixture_dir/sources"
printf '0.0.0.0 ads.example.com tracker.example.net\n' > "$fixture_dir/sources/example-source.hosts"
source_checksum="$(sha256sum "$fixture_dir/sources/example-source.hosts" | awk '{print $1}')"
jq --arg checksum "$source_checksum" '.sources[0].sha256 = $checksum' \
  "$ROOT/tests/fixtures/remote-config/blocklist-metadata.json" > "$fixture_dir/metadata.json"

"$ROOT/scripts/build-blocklist-release.sh" \
  "$fixture_dir/metadata.json" \
  "$fixture_dir/sources" \
  "$fixture_dir/base.fst" \
  "$BLOCKLIST_BIN" \
  "$DAEMON_BIN"
"$ROOT/scripts/build-blocklist-release.sh" \
  "$fixture_dir/metadata.json" \
  "$fixture_dir/sources" \
  "$fixture_dir/base-repeat.fst" \
  "$BLOCKLIST_BIN" \
  "$DAEMON_BIN"
cmp -s "$fixture_dir/base.fst" "$fixture_dir/base-repeat.fst" || {
  printf '%s\n' 'repeated blocklist builds were not byte-for-byte deterministic' >&2
  exit 1
}

"$ROOT/scripts/validate-blocklist-release.sh" \
  "$fixture_dir/metadata.json" \
  "$fixture_dir/base.fst" \
  "$BLOCKLIST_BIN" \
  "$DAEMON_BIN"

jq '.sources[0].redistribution = "external_fetch_only"' \
  "$fixture_dir/metadata.json" > "$fixture_dir/external-only.json"
if "$ROOT/scripts/build-blocklist-release.sh" \
  "$fixture_dir/external-only.json" \
  "$fixture_dir/sources" \
  "$fixture_dir/external-only.fst" \
  "$BLOCKLIST_BIN" \
  "$DAEMON_BIN" >/dev/null 2>&1; then
  printf '%s\n' 'release builder accepted an external-fetch-only source' >&2
  exit 1
fi

jq '.sources[0].sha256 = "0000000000000000000000000000000000000000000000000000000000000000"' \
  "$fixture_dir/metadata.json" > "$fixture_dir/checksum-mismatch.json"
if "$ROOT/scripts/build-blocklist-release.sh" \
  "$fixture_dir/checksum-mismatch.json" \
  "$fixture_dir/sources" \
  "$fixture_dir/checksum-mismatch.fst" \
  "$BLOCKLIST_BIN" \
  "$DAEMON_BIN" >/dev/null 2>&1; then
  printf '%s\n' 'release builder accepted a source checksum mismatch' >&2
  exit 1
fi

jq '.entry_count = 999' "$fixture_dir/metadata.json" > "$fixture_dir/count-mismatch.json"
if "$ROOT/scripts/build-blocklist-release.sh" \
  "$fixture_dir/count-mismatch.json" \
  "$fixture_dir/sources" \
  "$fixture_dir/count-mismatch.fst" \
  "$BLOCKLIST_BIN" \
  "$DAEMON_BIN" >/dev/null 2>&1; then
  printf '%s\n' 'release builder accepted an entry-count mismatch' >&2
  exit 1
fi

install -d -m 0700 "$fixture_dir/empty-sources"
printf '%s\n' '# no domains' > "$fixture_dir/empty-sources/example-source.hosts"
empty_checksum="$(sha256sum "$fixture_dir/empty-sources/example-source.hosts" | awk '{print $1}')"
jq --arg checksum "$empty_checksum" '.sources[0].sha256 = $checksum' \
  "$fixture_dir/metadata.json" > "$fixture_dir/empty-metadata.json"
if "$ROOT/scripts/build-blocklist-release.sh" \
  "$fixture_dir/empty-metadata.json" \
  "$fixture_dir/empty-sources" \
  "$fixture_dir/empty.fst" \
  "$BLOCKLIST_BIN" \
  "$DAEMON_BIN" >/dev/null 2>&1; then
  printf '%s\n' 'release builder accepted an empty base FST' >&2
  exit 1
fi
[[ ! -e "$fixture_dir/empty.fst" ]] || {
  printf '%s\n' 'failed blocklist build left a partial output' >&2
  exit 1
}

printf '%s' 'not-an-fst' > "$fixture_dir/invalid.fst"
if "$ROOT/scripts/validate-blocklist-release.sh" \
  "$fixture_dir/metadata.json" \
  "$fixture_dir/invalid.fst" \
  "$BLOCKLIST_BIN" \
  "$DAEMON_BIN" >/dev/null 2>&1; then
  printf '%s\n' 'release validator accepted a malformed FST' >&2
  exit 1
fi

ln -s "$fixture_dir/metadata.json" "$fixture_dir/metadata-link.json"
if "$ROOT/scripts/validate-blocklist-release.sh" \
  "$fixture_dir/metadata-link.json" \
  "$fixture_dir/base.fst" \
  "$BLOCKLIST_BIN" \
  "$DAEMON_BIN" >/dev/null 2>&1; then
  printf '%s\n' 'release validator accepted symlinked metadata' >&2
  exit 1
fi
