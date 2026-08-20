#!/usr/bin/env bash
set -Eeuo pipefail

if [[ $# -ne 5 ]]; then
  printf '%s\n' 'usage: build-blocklist-release.sh METADATA SOURCE_DIR OUTPUT BLOCKLIST_BIN DAEMON_BIN' >&2
  exit 2
fi

metadata="$1"
source_dir="$2"
output="$3"
blocklist_bin="$4"
daemon_bin="$5"

[[ -f "$metadata" && ! -L "$metadata" ]] || {
  printf '%s\n' 'blocklist metadata must be a regular non-symlink file' >&2
  exit 1
}
[[ -d "$source_dir" && ! -L "$source_dir" ]] || {
  printf '%s\n' 'blocklist source directory must be a real directory' >&2
  exit 1
}
[[ ! -e "$output" && ! -L "$output" ]] || {
  printf '%s\n' 'blocklist build output must not already exist' >&2
  exit 1
}
for path in "$blocklist_bin" "$daemon_bin"; do
  [[ -f "$path" && ! -L "$path" && -x "$path" ]] || {
    printf 'blocklist builder binary must be executable and non-symlink: %s\n' "$path" >&2
    exit 1
  }
done
(( $(wc -c < "$metadata") <= 1048576 )) || {
  printf '%s\n' 'blocklist metadata exceeds the 1 MiB limit' >&2
  exit 1
}

validation_dir="$(mktemp -d -t xanhtab-blocklist-build.XXXXXXXX)"
readonly validation_dir
cleanup() {
  local status=$?
  trap - EXIT
  if [[ "$status" -ne 0 ]]; then
    rm -f -- "$output"
  fi
  rm -rf -- "$validation_dir"
  exit "$status"
}
trap cleanup EXIT
install -m 0600 "$metadata" "$validation_dir/blocklist-metadata.json"
"$daemon_bin" --check-public-config-dir "$validation_dir" >/dev/null
jq -e '.sources | length > 0 and all(.[]; .redistribution == "reviewed")' \
  "$validation_dir/blocklist-metadata.json" >/dev/null || {
  printf '%s\n' 'every packaged blocklist source must have redistribution=reviewed' >&2
  exit 1
}

source_args=()
total_size=0
while IFS=$'\t' read -r source_file expected_checksum; do
  [[ "$source_file" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || {
    printf 'unsafe blocklist source filename: %s\n' "$source_file" >&2
    exit 1
  }
  source_path="$source_dir/$source_file"
  [[ -f "$source_path" && ! -L "$source_path" ]] || {
    printf 'missing regular blocklist source: %s\n' "$source_file" >&2
    exit 1
  }
  source_size="$(wc -c < "$source_path")"
  (( source_size <= 33554432 )) || {
    printf 'blocklist source exceeds 32 MiB: %s\n' "$source_file" >&2
    exit 1
  }
  total_size=$((total_size + source_size))
  (( total_size <= 67108864 )) || {
    printf '%s\n' 'combined blocklist sources exceed 64 MiB' >&2
    exit 1
  }
  actual_checksum="$(sha256sum "$source_path" | awk '{print $1}')"
  [[ "$actual_checksum" == "$expected_checksum" ]] || {
    printf 'blocklist source checksum mismatch: %s\n' "$source_file" >&2
    exit 1
  }
  source_args+=(--input "$source_path")
done < <(jq -r '.sources[] | [.file, .sha256] | @tsv' "$validation_dir/blocklist-metadata.json")

(( ${#source_args[@]} > 0 )) || {
  printf '%s\n' 'blocklist metadata did not select any source file' >&2
  exit 1
}
"$blocklist_bin" "${source_args[@]}" --output "$output" >/dev/null
expected_count="$(jq -er '.entry_count' "$validation_dir/blocklist-metadata.json")"
"$blocklist_bin" \
  --check-fst "$output" \
  --require-non-empty \
  --expected-count "$expected_count" >/dev/null
