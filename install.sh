#!/usr/bin/env bash
set -Eeuo pipefail

readonly PROGRAM="xanhtab-installer"
readonly DEFAULT_MANIFEST_URL="https://github.com/LamPPKK/XanhTab/releases/latest/download/release-manifest.json"
readonly BACKUP_ROOT="/var/backups/xanhtab"

DRY_RUN=0
NON_INTERACTIVE=0
REPAIR=0
UNINSTALL=0
CONFIG_REPO=""
CONFIG_REF=""
NETWORK="direct"
SECRETS_FILE=""
MANIFEST_URL="$DEFAULT_MANIFEST_URL"
PUBLIC_KEY=""
WORK_DIR=""
BACKUP_DIR=""
MUTATION_STARTED=0
FILES_MUTATED=0
INSTALL_COMMITTED=0
ROLLBACK_ACTIVE=0
SERVICE_UNITS=(xanhtab-netd.service xanhtab-browser.service xanhtabd.service)
STOP_UNITS=(xanhtabd.service xanhtab-browser.service xanhtab-netd.service)
INSTALL_TARGETS=(
  /usr/local/bin/xanhtabd
  /usr/local/libexec/xanhtab-browser
  /usr/local/libexec/xanhtab-netd
  /usr/local/libexec/xanhtab-blocklist
  /usr/local/libexec/xanhtab-validate-blocklist-release
  /usr/local/libexec/xanhtab-x1-burn-audit
  /usr/share/xanhtab
  /usr/lib/xanhtab
  /etc/xanhtab
  /var/lib/xanhtab
  /etc/systemd/system/xanhtabd.service
  /etc/systemd/system/xanhtab-browser.service
  /etc/systemd/system/xanhtab-netd.service
  /usr/lib/tmpfiles.d/xanhtab.conf
  /usr/lib/sysusers.d/xanhtab.conf
)
PREVIOUS_ENABLED=()
PREVIOUS_ACTIVE=()

usage() {
  printf '%s\n' \
    'Usage: ./install.sh [options]' \
    '  --dry-run                 Validate and print mutations.' \
    '  --non-interactive         Use non-interactive package installation.' \
    '  --config-repo URL         Non-secret Git configuration repository.' \
    '  --config-ref TAG|SHA      Immutable release tag or full commit SHA.' \
    '  --network MODE            direct|tor|warp|wireguard|proxy.' \
    '  --secrets-file FILE       Root-owned mode-0600 JSON secret input.' \
    '  --manifest-url URL        Signed release manifest URL.' \
    '  --public-key FILE         Trusted minisign public key.' \
    '  --repair                  Reinstall while preserving configuration.' \
    '  --uninstall               Remove installed runtime files.'
  exit 0
}

# XanhTab verified installer
#
# Usage:
#   ./install.sh [options]
#
# Options:
#   --dry-run                 Validate and print mutations without applying them.
#   --non-interactive         Fail instead of prompting.
#   --config-repo URL         Git repository containing non-secret configuration.
#   --config-ref TAG|SHA      Immutable tag or 40-character commit SHA (required with repo).
#   --network MODE            direct|tor|warp|wireguard|proxy.
#   --secrets-file FILE       Root-owned mode-0600 JSON secret input.
#   --manifest-url URL        Signed release manifest URL.
#   --public-key FILE         Trusted minisign public key obtained out-of-band.
#   --repair                  Reinstall artifacts and preserve current configuration backup.
#   --uninstall               Stop services and remove installed runtime files.
#   --help                    Show this help.

log() { printf '%s: %s\n' "$PROGRAM" "$*"; }
die() { printf '%s: error: %s\n' "$PROGRAM" "$*" >&2; exit 1; }

run() {
  if [[ "$DRY_RUN" == 1 ]]; then
    printf '+ '
    printf '%q ' "$@"
    printf '\n'
  else
    "$@"
  fi
}

cleanup() {
  local status=$?
  trap - EXIT
  if [[ "$MUTATION_STARTED" == 1 && "$INSTALL_COMMITTED" == 0 ]] \
    && declare -F rollback_install >/dev/null; then
    if ! rollback_install; then
      [[ "$status" != 0 ]] || status=1
    fi
  fi
  if [[ -n "$WORK_DIR" && -d "$WORK_DIR" ]]; then
    rm -rf -- "$WORK_DIR"
  fi
  exit "$status"
}

parse_arguments() {
  while (($#)); do
    case "$1" in
      --dry-run) DRY_RUN=1; shift ;;
      --non-interactive) NON_INTERACTIVE=1; shift ;;
      --repair) REPAIR=1; shift ;;
      --uninstall) UNINSTALL=1; shift ;;
      --config-repo) [[ $# -ge 2 ]] || die "--config-repo requires a value"; CONFIG_REPO="$2"; shift 2 ;;
      --config-ref) [[ $# -ge 2 ]] || die "--config-ref requires a value"; CONFIG_REF="$2"; shift 2 ;;
      --network) [[ $# -ge 2 ]] || die "--network requires a value"; NETWORK="$2"; shift 2 ;;
      --secrets-file) [[ $# -ge 2 ]] || die "--secrets-file requires a value"; SECRETS_FILE="$2"; shift 2 ;;
      --manifest-url) [[ $# -ge 2 ]] || die "--manifest-url requires a value"; MANIFEST_URL="$2"; shift 2 ;;
      --public-key) [[ $# -ge 2 ]] || die "--public-key requires a value"; PUBLIC_KEY="$2"; shift 2 ;;
      --help|-h) usage ;;
      *) die "unknown argument: $1" ;;
    esac
  done
}

validate_arguments() {
  [[ "$NETWORK" =~ ^(direct|tor|warp|wireguard|proxy)$ ]] || die "unsupported network mode: $NETWORK"
  [[ "$MANIFEST_URL" =~ ^https:// ]] || die "--manifest-url must use HTTPS"
  if [[ -n "$CONFIG_REPO" || -n "$CONFIG_REF" ]]; then
    [[ -n "$CONFIG_REPO" && -n "$CONFIG_REF" ]] || die "--config-repo and --config-ref must be used together"
    [[ "$CONFIG_REF" =~ ^[0-9a-fA-F]{40}$ || "$CONFIG_REF" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+([._-][A-Za-z0-9.-]+)?$ ]] || die "--config-ref must be a release tag or full commit SHA"
    [[ "$CONFIG_REPO" =~ ^https:// ]] || die "--config-repo must use HTTPS"
  fi
  if [[ -n "$SECRETS_FILE" ]]; then
    [[ -f "$SECRETS_FILE" ]] || die "secrets file not found"
    [[ "$(stat -c '%u' "$SECRETS_FILE")" == 0 ]] || die "secrets file must be owned by root"
    local mode
    mode="$(stat -c '%a' "$SECRETS_FILE")"
    (( (8#$mode & 8#077) == 0 )) || die "secrets file must not be group/world accessible"
  fi
}

preflight() {
  [[ "$(id -u)" == 0 || "$DRY_RUN" == 1 ]] || die "run the verified local installer as root"
  [[ -r /etc/os-release ]] || die "cannot identify operating system"
  # shellcheck disable=SC1091
  source /etc/os-release
  [[ "${ID:-}" == "debian" && "${VERSION_CODENAME:-}" == "trixie" ]] || die "only fresh Raspberry Pi OS Lite 64-bit based on Debian Trixie is supported"
  [[ "$(uname -m)" == "aarch64" ]] || die "only aarch64 is supported"
  [[ -r /proc/device-tree/model ]] || die "Raspberry Pi model information is unavailable"
  grep -aq "Raspberry Pi Zero 2 W" /proc/device-tree/model || die "only Raspberry Pi Zero 2 W is supported"
  [[ -r /proc/cmdline ]] || die "kernel command line is unavailable"
  if grep -Eq '(^|[[:space:]])cgroup_disable=memory($|[[:space:]])' /proc/cmdline; then
    die "memory cgroup is disabled by the kernel command line; remove cgroup_disable=memory and reboot before installing"
  fi
  [[ -r /sys/fs/cgroup/cgroup.controllers ]] || die "cgroup v2 controllers are unavailable"
  if ! grep -qw memory /sys/fs/cgroup/cgroup.controllers; then
    die "cgroup v2 memory controller is unavailable; service memory ceilings cannot be enforced"
  fi
  [[ -n "$PUBLIC_KEY" && -f "$PUBLIC_KEY" ]] || die "--public-key is required; obtain and verify it out-of-band"
  command -v apt-get >/dev/null || die "apt-get is unavailable"
  local tool
  for tool in curl jq minisign sha256sum tar zstd file find; do
    command -v "$tool" >/dev/null || die "$tool is required before any system mutation"
  done
  if [[ -n "$CONFIG_REPO" ]]; then
    command -v git >/dev/null || die "git is required before any system mutation when --config-repo is used"
  fi
}

install_system_dependencies() {
  local packages=(
    ca-certificates curl jq git minisign zstd qrencode openssl avahi-daemon
    nftables wireguard-tools tor
    libwpewebkit-2.0-1 gstreamer1.0-wpe gstreamer1.0-tools
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad
  )
  run apt-get update
  if [[ "$NON_INTERACTIVE" == 1 ]]; then
    run env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${packages[@]}"
  else
    run apt-get install -y --no-install-recommends "${packages[@]}"
  fi
}

backup_path() {
  local source="$1" destination="$2"
  if [[ -e "$source" ]]; then
    run install -d -m 0700 "$destination"
    run cp -a -- "$source" "$destination/"
  fi
}

uninstall_xanhtab() {
  log "stopping XanhTab services"
  run systemctl disable --now xanhtabd.service xanhtab-browser.service xanhtab-netd.service
  local stamp
  stamp="$(date -u +%Y%m%dT%H%M%SZ)"
  backup_path /etc/xanhtab "$BACKUP_ROOT/uninstall-$stamp"
  for target in \
    /usr/local/bin/xanhtabd \
    /usr/local/libexec/xanhtab-browser \
    /usr/local/libexec/xanhtab-netd \
    /usr/local/libexec/xanhtab-blocklist \
    /usr/local/libexec/xanhtab-validate-blocklist-release \
    /usr/local/libexec/xanhtab-x1-burn-audit \
    /etc/systemd/system/xanhtabd.service \
    /etc/systemd/system/xanhtab-browser.service \
    /etc/systemd/system/xanhtab-netd.service \
    /usr/lib/tmpfiles.d/xanhtab.conf \
    /usr/lib/sysusers.d/xanhtab.conf; do
    [[ ! -e "$target" ]] || run rm -f -- "$target"
  done
  [[ ! -d /usr/share/xanhtab ]] || run rm -rf -- /usr/share/xanhtab
  [[ ! -d /usr/lib/xanhtab ]] || run rm -rf -- /usr/lib/xanhtab
  [[ ! -d /etc/xanhtab ]] || run rm -rf -- /etc/xanhtab
  run systemctl daemon-reload
  log "uninstalled; persistent blocklist and backups were retained"
}

validate_release_manifest() {
  local manifest="$1"
  jq -e '
    def exact_keys($expected):
      type == "object" and keys == $expected;
    def matching_string($pattern):
      type == "string" and (try test($pattern) catch false);

    exact_keys([
      "artifacts",
      "component_versions",
      "config_schema_version",
      "gstreamer_abi",
      "schema_version",
      "version"
    ]) and
    .schema_version == 1 and
    (.version | matching_string("^[0-9]+\\.[0-9]+\\.[0-9]+([.-][A-Za-z0-9.-]+)?$")) and
    .config_schema_version == 1 and
    .gstreamer_abi == "1.0" and
    (.component_versions |
      exact_keys(["gstreamer", "rswebrtc", "wpe_webkit"]) and
      (.gstreamer | type == "string" and length > 0) and
      (.rswebrtc | type == "string" and length > 0) and
      (.wpe_webkit | type == "string" and length > 0)) and
    (.artifacts |
      type == "array" and
      length == 1 and
      all(.[];
        exact_keys(["name", "platform", "sha256", "url"]) and
        .platform == "linux-aarch64" and
        (.name | matching_string("^[A-Za-z0-9][A-Za-z0-9._-]*$")) and
        (.url | matching_string("^https://[^/?#@]+/[^[:space:]]+$")) and
        (.sha256 | matching_string("^[0-9a-f]{64}$"))))
  ' "$manifest" >/dev/null
}

download_and_verify_release() {
  local manifest="$WORK_DIR/release-manifest.json"
  local signature="$WORK_DIR/release-manifest.json.minisig"
  run curl --fail --location --proto '=https' --tlsv1.2 --output "$manifest" "$MANIFEST_URL"
  run curl --fail --location --proto '=https' --tlsv1.2 --output "$signature" "${MANIFEST_URL}.minisig"
  if [[ "$DRY_RUN" == 1 ]]; then
    return
  fi
  minisign -Vm "$manifest" -x "$signature" -p "$PUBLIC_KEY" >/dev/null
  validate_release_manifest "$manifest" || die "invalid release manifest schema"
  local url expected name archive actual
  url="$(jq -er '.artifacts[0].url' "$manifest")"
  expected="$(jq -er '.artifacts[0].sha256' "$manifest")"
  name="$(jq -er '.artifacts[0].name' "$manifest")"
  archive="$WORK_DIR/$name"
  curl --fail --location --proto '=https' --tlsv1.2 --output "$archive" "$url"
  actual="$(sha256sum "$archive" | awk '{print $1}')"
  [[ "$actual" == "$expected" ]] || die "release artifact checksum mismatch"
  local member
  while IFS= read -r member; do
    [[ "$member" != /* && "$member" != ".." && "$member" != ../* \
      && "$member" != */../* && "$member" != */.. ]] || die "release archive contains an unsafe path"
  done < <(tar --zstd -tf "$archive")
  install -d -m 0700 "$WORK_DIR/release"
  tar --zstd -xf "$archive" -C "$WORK_DIR/release"
  [[ -z "$(find "$WORK_DIR/release" -type l -print -quit)" ]] || die "release archive must not contain symbolic links"
  for relative in \
    bin/xanhtabd \
    libexec/xanhtab-browser \
    libexec/xanhtab-netd \
    libexec/xanhtab-blocklist \
    libexec/xanhtab-validate-blocklist-release \
    libexec/xanhtab-x1-burn-audit \
    config/xanhtab.production.toml \
    config/custom_hosts.txt \
    config/base-blocklist.fst \
    config/blocklist-metadata.json \
    web/index.html \
    web/internal-home.html \
    systemd/xanhtabd.service \
    systemd/xanhtab-browser.service \
    systemd/xanhtab-netd.service \
    systemd/xanhtab-tmpfiles.conf \
    systemd/xanhtab.sysusers \
    schemas/openapi-v1.yaml \
    schemas/config.schema.json \
    schemas/release-manifest.schema.json \
    schemas/burn-audit.schema.json \
    schemas/remote-config.schema.json \
    schemas/bookmarks.schema.json \
    schemas/blocklist-metadata.schema.json \
    plugins/gstreamer-1.0/libgstrswebrtc.so \
    checksums.sha256; do
    [[ -f "$WORK_DIR/release/$relative" ]] || die "release is missing $relative"
  done
  (cd "$WORK_DIR/release" && sha256sum --check --strict checksums.sha256 >/dev/null) || die "release component checksum mismatch"
  file "$WORK_DIR/release/plugins/gstreamer-1.0/libgstrswebrtc.so" | grep -Eq 'ARM aarch64|ARM64' || die "rswebrtc plugin is not an ARM64 artifact"
  "$WORK_DIR/release/libexec/xanhtab-validate-blocklist-release" \
    "$WORK_DIR/release/config/blocklist-metadata.json" \
    "$WORK_DIR/release/config/base-blocklist.fst" \
    "$WORK_DIR/release/libexec/xanhtab-blocklist" \
    "$WORK_DIR/release/bin/xanhtabd" || die "packaged blocklist artifact is invalid"
}

validate_runtime_abi() {
  [[ "$DRY_RUN" == 1 ]] && return
  GST_PLUGIN_PATH_1_0="$WORK_DIR/release/plugins/gstreamer-1.0" \
    gst-inspect-1.0 webrtcsink >/dev/null || die "pinned rswebrtc plugin does not load with the installed GStreamer ABI"
  GST_PLUGIN_PATH_1_0="$WORK_DIR/release/plugins/gstreamer-1.0" \
    gst-inspect-1.0 wpesrc >/dev/null || die "wpesrc is unavailable after dependency installation"
}

sync_public_config() {
  [[ -n "$CONFIG_REPO" ]] || return 0
  local checkout="$WORK_DIR/config-repo"
  git init -q "$checkout"
  git -C "$checkout" remote add origin "$CONFIG_REPO"
  git -C "$checkout" fetch -q --depth 1 origin "$CONFIG_REF"
  git -C "$checkout" checkout -q --detach FETCH_HEAD
  [[ -f "$checkout/checksums.sha256" && ! -L "$checkout/checksums.sha256" ]] || die "config repository must provide a regular checksums.sha256 file"
  (( $(wc -c < "$checkout/checksums.sha256") <= 1048576 )) || die "checksums.sha256 exceeds the 1 MiB limit"
  local allowed=(config.json custom_hosts.txt bookmarks.json blocklist-metadata.json)
  local name size expected actual
  install -d -m 0750 "$WORK_DIR/public-config"
  for name in "${allowed[@]}"; do
    [[ ! -L "$checkout/$name" ]] || die "$name must not be a symbolic link"
    [[ -f "$checkout/$name" ]] || continue
    size="$(wc -c < "$checkout/$name")"
    (( size <= 1048576 )) || die "$name exceeds the 1 MiB limit"
    expected="$(awk -v file="$name" '$2 == file || $2 == "*" file { print $1 }' "$checkout/checksums.sha256")"
    [[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] || die "missing checksum for $name"
    actual="$(sha256sum "$checkout/$name" | awk '{print $1}')"
    [[ "${actual,,}" == "${expected,,}" ]] || die "checksum mismatch for $name"
    install -m 0640 "$checkout/$name" "$WORK_DIR/public-config/$name"
  done
  "$WORK_DIR/release/bin/xanhtabd" --check-public-config-dir "$WORK_DIR/public-config"
  if [[ -f "$WORK_DIR/public-config/config.json" ]]; then
    "$WORK_DIR/release/bin/xanhtabd" \
      --config "$WORK_DIR/release/config/xanhtab.production.toml" \
      --apply-public-config "$WORK_DIR/public-config/config.json" \
      --write-config "$WORK_DIR/effective-config.toml"
  fi
}

install_secrets() {
  [[ -n "$SECRETS_FILE" ]] || return 0
  jq -e 'keys - ["proxy_url", "stun_config_base64", "turn_config_base64", "wireguard_config_base64"] | length == 0' "$SECRETS_FILE" >/dev/null || die "unknown key in secrets file"
  run install -d -o root -g xanhtab -m 0750 /etc/xanhtab/secrets
  if [[ "$DRY_RUN" == 1 ]]; then
    log "would install selected secret values without printing them"
    return
  fi
  umask 077
  if jq -e 'has("wireguard_config_base64")' "$SECRETS_FILE" >/dev/null; then
    jq -er '.wireguard_config_base64' "$SECRETS_FILE" | base64 --decode > /etc/xanhtab/secrets/wg0.conf
  fi
  if jq -e 'has("proxy_url")' "$SECRETS_FILE" >/dev/null; then
    jq -er '.proxy_url | select(test("^(https?|socks5h?)://"))' "$SECRETS_FILE" > /etc/xanhtab/secrets/proxy-url
  fi
  local ice_key ice_target
  for ice_key in stun_config_base64 turn_config_base64; do
    [[ "$ice_key" == "stun_config_base64" ]] && ice_target="stun.json" || ice_target="turn.json"
    if jq -e --arg key "$ice_key" 'has($key)' "$SECRETS_FILE" >/dev/null; then
      jq -er --arg key "$ice_key" '.[$key]' "$SECRETS_FILE" | base64 --decode > "/etc/xanhtab/secrets/$ice_target"
      jq -e 'type == "object"' "/etc/xanhtab/secrets/$ice_target" >/dev/null || die "$ice_target must contain a JSON object"
    fi
  done
  if [[ -f /etc/xanhtab/secrets/wg0.conf ]]; then
    chown root:root /etc/xanhtab/secrets/wg0.conf
    chmod 0600 /etc/xanhtab/secrets/wg0.conf
  fi
  local secret_path
  for secret_path in /etc/xanhtab/secrets/proxy-url /etc/xanhtab/secrets/stun.json /etc/xanhtab/secrets/turn.json; do
    if [[ -f "$secret_path" ]]; then
      chown root:xanhtab "$secret_path"
      chmod 0640 "$secret_path"
    fi
  done
}

install_release() {
  local release="$WORK_DIR/release"
  local config_source="$release/config/xanhtab.production.toml"
  [[ ! -f "$WORK_DIR/effective-config.toml" ]] || config_source="$WORK_DIR/effective-config.toml"
  local stamp
  stamp="$(date -u +%Y%m%dT%H%M%SZ)"

  if [[ "$DRY_RUN" == 0 ]]; then
    capture_service_state
    MUTATION_STARTED=1
    local unit
    for unit in "${STOP_UNITS[@]}"; do
      if systemctl is-active --quiet "$unit" 2>/dev/null; then
        systemctl stop "$unit"
      fi
    done
    install -d -m 0700 "$BACKUP_ROOT"
    BACKUP_DIR="$(mktemp -d "$BACKUP_ROOT/install-$stamp.XXXXXX")"
    chmod 0700 "$BACKUP_DIR"
  else
    BACKUP_DIR="$BACKUP_ROOT/install-$stamp.DRYRUN"
    run systemctl stop "${STOP_UNITS[@]}"
    run install -d -m 0700 "$BACKUP_DIR"
  fi
  local target
  for target in "${INSTALL_TARGETS[@]}"; do
    if [[ -e "$target" ]]; then run cp -a --parents "$target" "$BACKUP_DIR"; fi
  done
  [[ "$DRY_RUN" == 1 ]] || FILES_MUTATED=1
  run install -d -m 0755 /usr/local/bin /usr/local/libexec /usr/share/xanhtab/web /usr/lib/xanhtab/gstreamer-1.0 /etc/xanhtab /etc/systemd/system /usr/lib/tmpfiles.d /usr/lib/sysusers.d
  run install -m 0755 "$release/bin/xanhtabd" /usr/local/bin/xanhtabd
  run install -m 0755 "$release/libexec/xanhtab-browser" /usr/local/libexec/xanhtab-browser
  run install -m 0755 "$release/libexec/xanhtab-netd" /usr/local/libexec/xanhtab-netd
  run install -m 0755 "$release/libexec/xanhtab-blocklist" /usr/local/libexec/xanhtab-blocklist
  run install -m 0755 "$release/libexec/xanhtab-validate-blocklist-release" /usr/local/libexec/xanhtab-validate-blocklist-release
  run install -m 0755 "$release/libexec/xanhtab-x1-burn-audit" /usr/local/libexec/xanhtab-x1-burn-audit
  run cp -a "$release/web/." /usr/share/xanhtab/web/
  run cp -a "$release/plugins/gstreamer-1.0/." /usr/lib/xanhtab/gstreamer-1.0/
  run install -m 0644 "$release/systemd/xanhtabd.service" /etc/systemd/system/xanhtabd.service
  run install -m 0644 "$release/systemd/xanhtab-browser.service" /etc/systemd/system/xanhtab-browser.service
  run install -m 0644 "$release/systemd/xanhtab-netd.service" /etc/systemd/system/xanhtab-netd.service
  run install -m 0644 "$release/systemd/xanhtab-tmpfiles.conf" /usr/lib/tmpfiles.d/xanhtab.conf
  run install -m 0644 "$release/systemd/xanhtab.sysusers" /usr/lib/sysusers.d/xanhtab.conf
  run systemd-sysusers /usr/lib/sysusers.d/xanhtab.conf
  if [[ "$REPAIR" == 0 || ! -f /etc/xanhtab/xanhtab.toml ]]; then
    run install -m 0640 "$config_source" /etc/xanhtab/xanhtab.toml
  fi
  if [[ "$DRY_RUN" == 0 ]]; then
    local control_uid browser_uid
    control_uid="$(id -u xanhtab)"
    browser_uid="$(id -u xanhtab-browser)"
    [[ "$control_uid" != "$browser_uid" ]] || die "control-plane and browser service UIDs must differ"
    if grep -q '^control_uid = ' /etc/xanhtab/xanhtab.toml; then
      sed -i "s/^control_uid = .*/control_uid = $control_uid/" /etc/xanhtab/xanhtab.toml
    else
      sed -i "/^netd_socket = /a control_uid = $control_uid" /etc/xanhtab/xanhtab.toml
    fi
    sed -i "s/^browser_uid = .*/browser_uid = $browser_uid/" /etc/xanhtab/xanhtab.toml
  else
    log "would pin network control/browser UIDs to the resolved service accounts"
  fi
  run chown root:xanhtab /etc/xanhtab/xanhtab.toml
  if [[ "$REPAIR" == 0 || ! -f /etc/xanhtab/custom_hosts.txt ]]; then
    run install -o root -g xanhtab -m 0640 "$release/config/custom_hosts.txt" /etc/xanhtab/custom_hosts.txt
  fi
  run install -d -o xanhtab -g xanhtab -m 0750 /var/lib/xanhtab
  run install -o xanhtab -g xanhtab -m 0644 "$release/config/blocklist-metadata.json" /var/lib/xanhtab/blocklist-metadata.json
  if [[ -d "$WORK_DIR/public-config" ]]; then
    run install -d -o xanhtab -g xanhtab -m 0750 /var/lib/xanhtab/remote-config
    run cp -a "$WORK_DIR/public-config/." /var/lib/xanhtab/remote-config/
    run chown -R xanhtab:xanhtab /var/lib/xanhtab/remote-config
    if [[ -f "$WORK_DIR/public-config/custom_hosts.txt" ]]; then
      run install -o root -g xanhtab -m 0640 "$WORK_DIR/public-config/custom_hosts.txt" /etc/xanhtab/custom_hosts.txt
    fi
  fi
  install_secrets
  run systemd-tmpfiles --create /usr/lib/tmpfiles.d/xanhtab.conf
  if [[ "$DRY_RUN" == 0 ]]; then
    sed -i "s/^initial_mode = .*/initial_mode = \"$NETWORK\"/" /etc/xanhtab/xanhtab.toml
    ensure_tls
    /usr/local/libexec/xanhtab-blocklist \
      --base-fst "$release/config/base-blocklist.fst" \
      --input /etc/xanhtab/custom_hosts.txt \
      --output /var/lib/xanhtab/blocklist.fst
    chown xanhtab:xanhtab /var/lib/xanhtab/blocklist.fst
    chmod 0644 /var/lib/xanhtab/blocklist.fst
    /usr/local/bin/xanhtabd --config /etc/xanhtab/xanhtab.toml --check-config
  fi
}

capture_service_state() {
  local unit
  PREVIOUS_ENABLED=()
  PREVIOUS_ACTIVE=()
  for unit in "${SERVICE_UNITS[@]}"; do
    if systemctl is-enabled --quiet "$unit" 2>/dev/null; then
      PREVIOUS_ENABLED+=(1)
    else
      PREVIOUS_ENABLED+=(0)
    fi
    if systemctl is-active --quiet "$unit" 2>/dev/null; then
      PREVIOUS_ACTIVE+=(1)
    else
      PREVIOUS_ACTIVE+=(0)
    fi
  done
}

ensure_tls() {
  install -d -o root -g xanhtab -m 0750 /etc/xanhtab/tls
  if [[ ! -s /etc/xanhtab/tls/server.crt || ! -s /etc/xanhtab/tls/server.key ]]; then
    openssl req -x509 -newkey rsa:3072 -sha256 -nodes -days 825 \
      -subj '/CN=xanhtab.local' \
      -addext 'subjectAltName=DNS:xanhtab.local' \
      -keyout /etc/xanhtab/tls/server.key \
      -out /etc/xanhtab/tls/server.crt >/dev/null 2>&1
  fi
  chown root:xanhtab /etc/xanhtab/tls/server.crt /etc/xanhtab/tls/server.key
  chmod 0644 /etc/xanhtab/tls/server.crt
  chmod 0640 /etc/xanhtab/tls/server.key
}

rollback_install() {
  [[ "$ROLLBACK_ACTIVE" == 0 ]] || return 0
  ROLLBACK_ACTIVE=1
  set +e
  local rollback_failed=0
  log "installation did not commit; restoring XanhTab files and service state"
  local unit
  for unit in "${STOP_UNITS[@]}"; do
    if systemctl is-active --quiet "$unit" 2>/dev/null; then
      systemctl stop "$unit" >/dev/null 2>&1 || rollback_failed=1
    fi
  done
  for unit in "${SERVICE_UNITS[@]}"; do
    if systemctl is-enabled --quiet "$unit" 2>/dev/null; then
      systemctl disable "$unit" >/dev/null 2>&1 || rollback_failed=1
    fi
  done
  local runtime_path
  for runtime_path in /run/xanhtab-session /run/xanhtab; do
    if [[ -d "$runtime_path" && ! -L "$runtime_path" ]]; then
      find "$runtime_path" -mindepth 1 -delete || rollback_failed=1
    elif [[ -L "$runtime_path" ]]; then
      rm -f -- "$runtime_path" || rollback_failed=1
    fi
  done
  if [[ "$FILES_MUTATED" == 1 ]]; then
    local target
    for target in "${INSTALL_TARGETS[@]}"; do
      if [[ -d "$target" ]]; then
        rm -rf -- "$target" || rollback_failed=1
      elif [[ -e "$target" || -L "$target" ]]; then
        rm -f -- "$target" || rollback_failed=1
      fi
    done
    if [[ -n "$BACKUP_DIR" && -d "$BACKUP_DIR" ]]; then
      cp -a "$BACKUP_DIR/." / || rollback_failed=1
    fi
  fi
  systemctl daemon-reload || rollback_failed=1

  local index
  for index in "${!SERVICE_UNITS[@]}"; do
    unit="${SERVICE_UNITS[$index]}"
    if [[ "${PREVIOUS_ENABLED[$index]:-0}" == 1 ]]; then
      systemctl enable "$unit" >/dev/null 2>&1 || rollback_failed=1
    elif systemctl is-enabled --quiet "$unit" 2>/dev/null; then
      systemctl disable "$unit" >/dev/null 2>&1 || rollback_failed=1
    fi
    if [[ "${PREVIOUS_ACTIVE[$index]:-0}" == 1 ]]; then
      systemctl start "$unit" >/dev/null 2>&1 || rollback_failed=1
    elif systemctl is-active --quiet "$unit" 2>/dev/null; then
      systemctl stop "$unit" >/dev/null 2>&1 || rollback_failed=1
    fi
  done
  if [[ "$rollback_failed" == 1 ]]; then
    log "ERROR: rollback was incomplete; inspect $BACKUP_DIR and service state before retrying"
  else
    log "rollback complete; backup retained at $BACKUP_DIR"
    MUTATION_STARTED=0
  fi
  ROLLBACK_ACTIVE=0
  set -e
  return "$rollback_failed"
}

activate_and_healthcheck() {
  run systemctl daemon-reload
  run systemctl enable --now xanhtab-netd.service xanhtab-browser.service xanhtabd.service
  [[ "$DRY_RUN" == 1 ]] && return
  local _
  for _ in {1..30}; do
    if curl --fail --silent --show-error --cacert /etc/xanhtab/tls/server.crt https://xanhtab.local:8443/healthz >/dev/null; then
      local pairing_url
      pairing_url="$(sed -n 's/^PAIRING_URL=//p' /run/xanhtab/pairing.txt)"
      [[ -n "$pairing_url" ]] || die "healthy service did not publish pairing material"
      log "services healthy; scan this one-time pairing URL"
      qrencode -t ANSIUTF8 "$pairing_url"
      return
    fi
    sleep 1
  done
  systemctl --no-pager --full status xanhtabd.service xanhtab-browser.service xanhtab-netd.service >&2 || true
  die "health check failed; transaction will be rolled back and its backup retained under $BACKUP_ROOT"
}

main() {
  trap cleanup EXIT
  parse_arguments "$@"
  validate_arguments
  if [[ "$UNINSTALL" == 1 ]]; then
    [[ "$(id -u)" == 0 || "$DRY_RUN" == 1 ]] || die "uninstall requires root"
    uninstall_xanhtab
    exit 0
  fi
  preflight
  WORK_DIR="$(mktemp -d -t xanhtab-install.XXXXXXXX)"
  download_and_verify_release
  [[ "$DRY_RUN" == 1 ]] || sync_public_config
  install_system_dependencies
  validate_runtime_abi
  install_release
  activate_and_healthcheck
  INSTALL_COMMITTED=1
  log "installation complete"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
