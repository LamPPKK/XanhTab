#!/usr/bin/env bash
set -Eeuo pipefail

BASE_URL="https://xanhtab.local:8443"
ORIGIN=""
CA_CERT="/etc/xanhtab/tls/server.crt"
PAIRING_FILE="/run/xanhtab/pairing.txt"
RUNTIME_DIR="/run/xanhtab-session"
BROWSER_SERVICE="xanhtab-browser.service"
OUTPUT="/run/xanhtab/x1-burn-audit.json"
BURN_SLO_MS=5000
DRY_RUN=0
WORK_DIR=""
COOKIE_JAR=""
AUTH_CONFIG=""
STALE_COOKIE_CONFIG=""
START_RESPONSE=""
BURN_RESPONSE=""
PAIRED=0
PAIRING_ATTEMPTED=0
BURN_FINALIZED=0
SESSION_ID=""

usage() {
  printf '%s\n' \
    'Usage: sudo scripts/x1-burn-audit.sh [options]' \
    '  --base-url URL          Appliance HTTPS origin.' \
    '  --origin ORIGIN         Expected browser Origin (default: base URL).' \
    '  --ca-cert FILE          TLS certificate used to verify the appliance.' \
    '  --pairing-file FILE     Root-readable one-time pairing material.' \
    '  --runtime-dir DIR       Sensitive session tmpfs directory.' \
    '  --browser-service UNIT  systemd browser bridge service.' \
    '  --output FILE           Redacted JSON report path.' \
    '  --burn-slo-ms N         Maximum accepted burn duration (default: 5000).' \
    '  --dry-run               Validate arguments and print the audit plan.'
}

die() { printf 'x1-burn-audit: error: %s\n' "$*" >&2; exit 2; }

safe_run_path() {
  local path="$1"
  [[ "$path" == /run/* && "$path" != /run/ && "$path" != *'//'* \
    && "$path" != *'/../'* && "$path" != */.. \
    && "$path" != *'/./'* && "$path" != */. ]]
}

attempt_emergency_burn() {
  local recovery_id="$SESSION_ID" recovery_status
  [[ "$PAIRED" == 1 && "$BURN_FINALIZED" == 0 \
    && -s "$AUTH_CONFIG" && -s "$STALE_COOKIE_CONFIG" ]] || return 0
  set +e
  if [[ -z "$recovery_id" ]]; then
    recovery_status="$(curl --silent --show-error \
      --cacert "$CA_CERT" \
      --config "$AUTH_CONFIG" \
      --config "$STALE_COOKIE_CONFIG" \
      --header "Origin: $ORIGIN" \
      --header 'Content-Type: application/json' \
      --cookie-jar "$COOKIE_JAR" \
      --output "$START_RESPONSE" \
      --write-out '%{http_code}' \
      --request POST \
      --data '{}' \
      "$BASE_URL/api/v1/session")"
    if [[ "$recovery_status" == 200 ]]; then
      recovery_id="$(jq -r '.id // empty' "$START_RESPONSE")"
    fi
  fi
  if [[ -n "$recovery_id" ]]; then
    recovery_status="$(curl --silent --show-error \
      --cacert "$CA_CERT" \
      --config "$AUTH_CONFIG" \
      --config "$STALE_COOKIE_CONFIG" \
      --header "Origin: $ORIGIN" \
      --cookie-jar "$COOKIE_JAR" \
      --output "$BURN_RESPONSE" \
      --write-out '%{http_code}' \
      --request DELETE \
      "$BASE_URL/api/v1/session/$recovery_id")"
    if [[ "$recovery_status" == 200 ]]; then
      BURN_FINALIZED=1
    fi
  fi
  set -e
}

browser_session_processes() {
  local main_pid control_group cgroup_file
  main_pid="$(systemctl show --property MainPID --value "$BROWSER_SERVICE")"
  control_group="$(systemctl show --property ControlGroup --value "$BROWSER_SERVICE")"
  [[ "$main_pid" =~ ^[1-9][0-9]*$ ]] || return 1
  [[ "$control_group" == /* && "$control_group" != / && "$control_group" != *'//'* \
    && "$control_group" != *'/../'* && "$control_group" != */.. ]] || return 1
  cgroup_file="/sys/fs/cgroup${control_group}/cgroup.procs"
  [[ -r "$cgroup_file" ]] || return 1
  awk -v main="$main_pid" '$1 != main { count++ } END { print count + 0 }' "$cgroup_file"
}

cleanup() {
  local status=$?
  trap - EXIT
  attempt_emergency_burn
  if [[ -n "$WORK_DIR" && "$WORK_DIR" == /run/xanhtab-audit.* && -d "$WORK_DIR" ]]; then
    if [[ "$PAIRING_ATTEMPTED" == 1 && "$BURN_FINALIZED" == 0 ]]; then
      printf 'x1-burn-audit: recovery material retained at %s; confirm Burn or pairing rotation before removing it\n' "$WORK_DIR" >&2
    else
      rm -rf -- "$WORK_DIR"
    fi
  fi
  exit "$status"
}
trap cleanup EXIT

while (($#)); do
  case "$1" in
    --base-url) [[ $# -ge 2 ]] || die 'missing base URL'; BASE_URL="${2%/}"; shift 2 ;;
    --origin) [[ $# -ge 2 ]] || die 'missing origin'; ORIGIN="${2%/}"; shift 2 ;;
    --ca-cert) [[ $# -ge 2 ]] || die 'missing CA certificate'; CA_CERT="$2"; shift 2 ;;
    --pairing-file) [[ $# -ge 2 ]] || die 'missing pairing file'; PAIRING_FILE="$2"; shift 2 ;;
    --runtime-dir) [[ $# -ge 2 ]] || die 'missing runtime directory'; RUNTIME_DIR="$2"; shift 2 ;;
    --browser-service) [[ $# -ge 2 ]] || die 'missing browser service'; BROWSER_SERVICE="$2"; shift 2 ;;
    --output) [[ $# -ge 2 ]] || die 'missing output path'; OUTPUT="$2"; shift 2 ;;
    --burn-slo-ms) [[ $# -ge 2 ]] || die 'missing burn SLO'; BURN_SLO_MS="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$ORIGIN" ]] || ORIGIN="$BASE_URL"
[[ "$BASE_URL" =~ ^https://[^/?#@]+$ ]] || die '--base-url must be an HTTPS origin without credentials, path, query, or fragment'
[[ "$ORIGIN" == "$BASE_URL" ]] || die '--origin must match --base-url'
[[ "$BURN_SLO_MS" =~ ^[0-9]+$ && "$BURN_SLO_MS" -gt 0 ]] || die '--burn-slo-ms must be positive'
safe_run_path "$RUNTIME_DIR" || die '--runtime-dir must be a specific normalized path under /run'
safe_run_path "$OUTPUT" || die '--output must stay under /run without traversal'
[[ "$BROWSER_SERVICE" =~ ^[A-Za-z0-9_.@][A-Za-z0-9_.@-]*\.service$ ]] || die '--browser-service must be a systemd service unit name'

if [[ "$DRY_RUN" == 1 ]]; then
  jq -n \
    --arg base_origin "$BASE_URL" \
    --arg runtime_dir "$RUNTIME_DIR" \
    --arg browser_service "$BROWSER_SERVICE" \
    --arg output "$OUTPUT" \
    --argjson burn_slo_ms "$BURN_SLO_MS" \
    '{schema_version: 1, dry_run: true, base_origin: $base_origin, runtime_dir: $runtime_dir, browser_service: $browser_service, output: $output, burn_slo_ms: $burn_slo_ms}'
  exit 0
fi

[[ "$(id -u)" == 0 ]] || die 'run as root on the XanhTab appliance'
for command in awk curl date dirname find install jq mktemp mv realpath sed sha256sum systemctl; do
  command -v "$command" >/dev/null 2>&1 || die "missing dependency: $command"
done
[[ -r "$CA_CERT" ]] || die "cannot read CA certificate: $CA_CERT"
[[ -r "$PAIRING_FILE" ]] || die "cannot read pairing material: $PAIRING_FILE"
[[ -d "$RUNTIME_DIR" ]] || die "runtime directory is unavailable: $RUNTIME_DIR"
runtime_resolved="$(realpath "$RUNTIME_DIR")"
[[ "$runtime_resolved" == /run/* ]] || die 'runtime directory resolves outside /run'

umask 077
WORK_DIR="$(mktemp -d /run/xanhtab-audit.XXXXXX)"
chmod 0700 "$WORK_DIR"
COOKIE_JAR="$WORK_DIR/cookies.txt"
PAIR_REQUEST="$WORK_DIR/pair.json.request"
PAIR_RESPONSE="$WORK_DIR/pair.json"
START_RESPONSE="$WORK_DIR/start.json"
BURN_RESPONSE="$WORK_DIR/burn.json"
STATUS_RESPONSE="$WORK_DIR/status.json"
AUTH_CONFIG="$WORK_DIR/auth.curlrc"
STALE_COOKIE_CONFIG="$WORK_DIR/stale-cookie.curlrc"

pairing_url="$(sed -n 's/^PAIRING_URL=//p' "$PAIRING_FILE")"
[[ "$pairing_url" == *'#pair='* ]] || die 'pairing file does not contain a fragment secret'
pairing_secret="${pairing_url#*#pair=}"
[[ ${#pairing_secret} -ge 40 ]] || die 'pairing secret is unexpectedly short'
pairing_hash_before="$(sha256sum "$PAIRING_FILE" | awk '{print $1}')"
printf '%s\n' "$pairing_secret" | jq -R '{secret: .}' > "$PAIR_REQUEST"
unset pairing_secret pairing_url

PAIRING_ATTEMPTED=1
pair_status="$(curl --silent --show-error \
  --cacert "$CA_CERT" \
  --header "Origin: $ORIGIN" \
  --header 'Content-Type: application/json' \
  --cookie-jar "$COOKIE_JAR" \
  --output "$PAIR_RESPONSE" \
  --write-out '%{http_code}' \
  --request POST \
  --data-binary "@$PAIR_REQUEST" \
  "$BASE_URL/api/v1/pair/exchange")"
[[ "$pair_status" == 200 ]] || die "pairing exchange returned HTTP $pair_status"
csrf="$(jq -er '.csrf_token' "$PAIR_RESPONSE")"
stale_cookie="$(awk '$6 == "xanhtab_session" {print $7; exit}' "$COOKIE_JAR")"
[[ "$csrf" =~ ^[A-Za-z0-9_-]{40,128}$ && "$stale_cookie" =~ ^[A-Za-z0-9_-]{40,128}$ ]] || die 'auth response did not contain expected high-entropy material'
printf 'header = "X-XanhTab-CSRF: %s"\n' "$csrf" > "$AUTH_CONFIG"
printf 'header = "Cookie: xanhtab_session=%s"\n' "$stale_cookie" > "$STALE_COOKIE_CONFIG"
chmod 0600 "$AUTH_CONFIG" "$STALE_COOKIE_CONFIG"
unset csrf stale_cookie
PAIRED=1

start_status="$(curl --silent --show-error \
  --cacert "$CA_CERT" \
  --config "$AUTH_CONFIG" \
  --header "Origin: $ORIGIN" \
  --header 'Content-Type: application/json' \
  --cookie "$COOKIE_JAR" \
  --cookie-jar "$COOKIE_JAR" \
  --output "$START_RESPONSE" \
  --write-out '%{http_code}' \
  --request POST \
  --data '{}' \
  "$BASE_URL/api/v1/session")"
[[ "$start_status" == 200 ]] || die "session start returned HTTP $start_status"
SESSION_ID="$(jq -er '.id' "$START_RESPONSE")"
[[ "$SESSION_ID" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-8][0-9a-fA-F]{3}-[89aAbB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$ ]] || die 'session response did not contain a UUID'

browser_processes_before=0
for _ in {1..100}; do
  browser_processes_before="$(browser_session_processes || printf 0)"
  (( browser_processes_before > 0 )) && break
  sleep 0.1
done

started_ns="$(date +%s%N)"
burn_status="$(curl --silent --show-error \
  --cacert "$CA_CERT" \
  --config "$AUTH_CONFIG" \
  --header "Origin: $ORIGIN" \
  --cookie "$COOKIE_JAR" \
  --cookie-jar "$COOKIE_JAR" \
  --output "$BURN_RESPONSE" \
  --write-out '%{http_code}' \
  --request DELETE \
  "$BASE_URL/api/v1/session/$SESSION_ID")"
completed_ns="$(date +%s%N)"
burn_duration_ms="$(( (completed_ns - started_ns) / 1000000 ))"
[[ "$burn_status" == 200 ]] && BURN_FINALIZED=1

stale_status="$(curl --silent --show-error \
  --cacert "$CA_CERT" \
  --config "$STALE_COOKIE_CONFIG" \
  --header "Origin: $ORIGIN" \
  --output /dev/null \
  --write-out '%{http_code}' \
  "$BASE_URL/api/v1/session")"
public_status_code="$(curl --silent --show-error \
  --cacert "$CA_CERT" \
  --output "$STATUS_RESPONSE" \
  --write-out '%{http_code}' \
  "$BASE_URL/api/v1/status")"
phase="$(jq -r '.phase // "unknown"' "$STATUS_RESPONSE" 2>/dev/null || printf unknown)"

for _ in {1..50}; do
  browser_processes_after="$(browser_session_processes || printf 1)"
  [[ "$browser_processes_after" == 0 ]] && break
  sleep 0.1
done

runtime_entries="$(find "$RUNTIME_DIR" -mindepth 1 -maxdepth 1 -print | wc -l | tr -d ' ')"
runtime_sockets="$(find "$RUNTIME_DIR" -type s -print | wc -l | tr -d ' ')"
pairing_hash_after="$(sha256sum "$PAIRING_FILE" | awk '{print $1}')"

burn_ok=false
phase_idle=false
cookie_revoked=false
runtime_empty=false
browser_tree_observed=false
browser_tree_stopped=false
pairing_rotated=false
slo_met=false
[[ "$burn_status" == 200 ]] && burn_ok=true
[[ "$public_status_code" == 200 && "$phase" == idle ]] && phase_idle=true
[[ "$stale_status" == 401 ]] && cookie_revoked=true
[[ "$runtime_entries" == 0 && "$runtime_sockets" == 0 ]] && runtime_empty=true
(( browser_processes_before > 0 )) && browser_tree_observed=true
[[ "${browser_processes_after:-1}" == 0 ]] && browser_tree_stopped=true
[[ "$pairing_hash_before" != "$pairing_hash_after" ]] && pairing_rotated=true
(( burn_duration_ms < BURN_SLO_MS )) && slo_met=true

passed=false
if [[ "$burn_ok" == true && "$phase_idle" == true && "$cookie_revoked" == true \
  && "$runtime_empty" == true && "$browser_tree_observed" == true \
  && "$browser_tree_stopped" == true \
  && "$pairing_rotated" == true && "$slo_met" == true ]]; then
  passed=true
fi

output_parent="$(dirname "$OUTPUT")"
if [[ ! -d "$output_parent" ]]; then
  install -d -m 0700 "$output_parent"
fi
output_parent_resolved="$(realpath "$output_parent")"
[[ "$output_parent_resolved" == /run/* || "$output_parent_resolved" == /run ]] || die 'output directory resolves outside /run'
report_tmp="$(mktemp "$output_parent/.x1-burn-audit.XXXXXX")"
jq -n \
  --arg captured_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg base_origin "$BASE_URL" \
  --arg browser_service "$BROWSER_SERVICE" \
  --argjson burn_slo_ms "$BURN_SLO_MS" \
  --argjson burn_duration_ms "$burn_duration_ms" \
  --argjson burn_http_status "$burn_status" \
  --argjson stale_cookie_http_status "$stale_status" \
  --argjson runtime_entries "$runtime_entries" \
  --argjson runtime_sockets "$runtime_sockets" \
  --argjson browser_processes_before "$browser_processes_before" \
  --argjson browser_processes_after "${browser_processes_after:-1}" \
  --argjson burn_ok "$burn_ok" \
  --argjson phase_idle "$phase_idle" \
  --argjson cookie_revoked "$cookie_revoked" \
  --argjson runtime_empty "$runtime_empty" \
  --argjson browser_tree_observed "$browser_tree_observed" \
  --argjson browser_tree_stopped "$browser_tree_stopped" \
  --argjson pairing_rotated "$pairing_rotated" \
  --argjson slo_met "$slo_met" \
  --argjson passed "$passed" \
  '{
    schema_version: 1,
    captured_at: $captured_at,
    base_origin: $base_origin,
    browser_service: $browser_service,
    burn_slo_ms: $burn_slo_ms,
    burn_duration_ms: $burn_duration_ms,
    observations: {
      burn_http_status: $burn_http_status,
      stale_cookie_http_status: $stale_cookie_http_status,
      runtime_entries: $runtime_entries,
      runtime_sockets: $runtime_sockets,
      browser_session_processes_before_burn: $browser_processes_before,
      browser_session_processes_after_burn: $browser_processes_after
    },
    checks: {
      burn_completed: $burn_ok,
      phase_idle: $phase_idle,
      stale_cookie_rejected: $cookie_revoked,
      runtime_empty: $runtime_empty,
      browser_process_tree_observed: $browser_tree_observed,
      browser_process_tree_stopped: $browser_tree_stopped,
      pairing_rotated: $pairing_rotated,
      burn_slo_met: $slo_met
    },
    passed: $passed
  }' > "$report_tmp"
chmod 0600 "$report_tmp"
mv -f -- "$report_tmp" "$OUTPUT"

jq . "$OUTPUT"
[[ "$passed" == true ]]
