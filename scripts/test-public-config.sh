#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
readonly XANHTABD_BIN="${XANHTABD_BIN:-$ROOT/target/debug/xanhtabd}"
[[ -x "$XANHTABD_BIN" ]] || { printf 'missing test binary: %s\n' "$XANHTABD_BIN" >&2; exit 1; }

fixture_dir="$(mktemp -d -t xanhtab-public-config.XXXXXXXX)"
trap 'rm -rf -- "$fixture_dir"' EXIT
readonly fixture_dir

"$XANHTABD_BIN" --check-public-config-dir "$ROOT/tests/fixtures/remote-config"

output="$fixture_dir/effective.toml"
"$XANHTABD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --apply-public-config "$ROOT/tests/fixtures/remote-config/config.json" \
  --write-config "$output"
"$XANHTABD_BIN" --config "$output" --check-config

grep -Fx 'initial_url = "https://example.com/start"' "$output" >/dev/null
grep -Fx 'initial_profile = "720p15"' "$output" >/dev/null
grep -Fx 'auto_burn_seconds = 600' "$output" >/dev/null
grep -Fx 'tls_cert = "/etc/xanhtab/tls/server.crt"' "$output" >/dev/null
grep -Fx 'initial_mode = "direct"' "$output" >/dev/null
grep -Fx 'proxy_url_file = "/etc/xanhtab/secrets/proxy-url"' "$output" >/dev/null

if "$XANHTABD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --apply-public-config "$ROOT/tests/fixtures/remote-config/config.json" \
  --write-config "$output" \
  >/dev/null 2>&1; then
  printf '%s\n' 'public config merge overwrote an existing output file' >&2
  exit 1
fi
