#!/usr/bin/env bash
set -Eeuo pipefail

if [[ $# -ne 4 ]]; then
  printf '%s\n' 'usage: validate-blocklist-release.sh METADATA FST BLOCKLIST_BIN DAEMON_BIN' >&2
  exit 2
fi

metadata="$1"
fst="$2"
blocklist_bin="$3"
daemon_bin="$4"

for path in "$metadata" "$fst" "$blocklist_bin" "$daemon_bin"; do
  [[ -f "$path" && ! -L "$path" ]] || {
    printf 'blocklist release input must be a regular non-symlink file: %s\n' "$path" >&2
    exit 1
  }
done
[[ -x "$blocklist_bin" && -x "$daemon_bin" ]] || {
  printf '%s\n' 'blocklist validator binaries must be executable' >&2
  exit 1
}

validation_dir="$(mktemp -d -t xanhtab-blocklist-release.XXXXXXXX)"
trap 'rm -rf -- "$validation_dir"' EXIT
readonly validation_dir
install -m 0600 "$metadata" "$validation_dir/blocklist-metadata.json"

"$daemon_bin" --check-public-config-dir "$validation_dir" >/dev/null
jq -e '.sources | length > 0 and all(.[]; .redistribution == "reviewed")' \
  "$validation_dir/blocklist-metadata.json" >/dev/null || {
  printf '%s\n' 'every packaged blocklist source must have redistribution=reviewed' >&2
  exit 1
}
expected_count="$(jq -er '.entry_count' "$validation_dir/blocklist-metadata.json")"
"$blocklist_bin" \
  --check-fst "$fst" \
  --require-non-empty \
  --expected-count "$expected_count" >/dev/null
