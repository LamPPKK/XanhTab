#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
readonly NETD_BIN="${XANHTAB_NETD_BIN:-$ROOT/target/debug/xanhtab-netd}"
[[ -x "$NETD_BIN" ]] || {
  printf '%s\n' 'build xanhtab-netd before this test' >&2
  exit 1
}

fixture_dir="$(mktemp -d -t xanhtab-egress-secrets.XXXXXXXX)"
trap 'rm -rf -- "$fixture_dir"' EXIT
readonly fixture_dir
readonly key='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='

write_wireguard() {
  local output="$1" extra_interface="${2:-}" allowed_ips="${3:-0.0.0.0/0, ::/0}"
  install -m 0600 /dev/null "$output"
  printf '%s\n' \
    '[Interface]' \
    "PrivateKey = $key" \
    'Address = 10.0.0.2/32' \
    'Table = off' \
    "$extra_interface" \
    '' \
    '[Peer]' \
    "PublicKey = $key" \
    "AllowedIPs = $allowed_ips" \
    'Endpoint = 192.0.2.1:51820' \
    > "$output"
}

write_wireguard "$fixture_dir/wg0.conf"
"$NETD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --check-wireguard-config "$fixture_dir/wg0.conf"

write_wireguard "$fixture_dir/wg-hook.conf" 'PostUp = /bin/sh -c id'
if "$NETD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --check-wireguard-config "$fixture_dir/wg-hook.conf" >/dev/null 2>&1; then
  printf '%s\n' 'WireGuard validator accepted an executable hook' >&2
  exit 1
fi

write_wireguard "$fixture_dir/wg-split.conf" '' '10.0.0.0/8'
if "$NETD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --check-wireguard-config "$fixture_dir/wg-split.conf" >/dev/null 2>&1; then
  printf '%s\n' 'WireGuard validator accepted a split-tunnel-only peer' >&2
  exit 1
fi

write_wireguard "$fixture_dir/wg-dns.conf" 'DNS = 1.1.1.1'
if "$NETD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --check-wireguard-config "$fixture_dir/wg-dns.conf" >/dev/null 2>&1; then
  printf '%s\n' 'WireGuard validator accepted a wg-quick DNS directive' >&2
  exit 1
fi

write_wireguard "$fixture_dir/wg-save.conf" 'SaveConfig = true'
if "$NETD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --check-wireguard-config "$fixture_dir/wg-save.conf" >/dev/null 2>&1; then
  printf '%s\n' 'WireGuard validator accepted SaveConfig' >&2
  exit 1
fi

printf '%s\n' 'socks5h://user:password@192.0.2.10:1080' > "$fixture_dir/proxy-url"
chmod 0600 "$fixture_dir/proxy-url"
endpoint="$("$NETD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --check-proxy-url "$fixture_dir/proxy-url" \
  --print-proxy-endpoint)"
[[ "$endpoint" == '192.0.2.10:1080' ]] || {
  printf '%s\n' 'proxy validator did not return the credential-free endpoint' >&2
  exit 1
}
if "$NETD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --check-proxy-url "$fixture_dir/proxy-url" >/dev/null 2>&1; then
  printf '%s\n' 'proxy validator accepted endpoint drift from the kill-switch config' >&2
  exit 1
fi

printf '%s\n' 'socks5h://proxy.example:1080' > "$fixture_dir/proxy-domain"
chmod 0600 "$fixture_dir/proxy-domain"
if "$NETD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --check-proxy-url "$fixture_dir/proxy-domain" \
  --print-proxy-endpoint >/dev/null 2>&1; then
  printf '%s\n' 'proxy validator accepted a hostname that nftables cannot pin' >&2
  exit 1
fi

chmod 0644 "$fixture_dir/proxy-url"
if "$NETD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --check-proxy-url "$fixture_dir/proxy-url" \
  --print-proxy-endpoint >/dev/null 2>&1; then
  printf '%s\n' 'proxy validator accepted world-readable credentials' >&2
  exit 1
fi
chmod 0600 "$fixture_dir/proxy-url"

ln -s "$fixture_dir/wg0.conf" "$fixture_dir/wg-link.conf"
if "$NETD_BIN" \
  --config "$ROOT/config/xanhtab.production.toml" \
  --check-wireguard-config "$fixture_dir/wg-link.conf" >/dev/null 2>&1; then
  printf '%s\n' 'WireGuard validator accepted a symlinked secret' >&2
  exit 1
fi

prepare_line="$(grep -n '^  prepare_and_validate_secrets$' "$ROOT/install.sh" | cut -d: -f1)"
apt_line="$(grep -n '^  install_system_dependencies$' "$ROOT/install.sh" | cut -d: -f1)"
[[ -n "$prepare_line" && -n "$apt_line" && "$prepare_line" -lt "$apt_line" ]] || {
  printf '%s\n' 'installer no longer validates egress secrets before APT mutation' >&2
  exit 1
}
grep -Fx 'BindsTo=xanhtab-netd.service' "$ROOT/systemd/xanhtab-browser.service" >/dev/null
grep -F 'BindsTo=xanhtab-browser.service xanhtab-netd.service' "$ROOT/systemd/xanhtabd.service" >/dev/null
grep -F -- '--cleanup' "$ROOT/systemd/xanhtab-netd.service" >/dev/null
