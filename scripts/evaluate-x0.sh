#!/usr/bin/env bash
set -Eeuo pipefail

RESULT_DIR="${1:-}"
[[ -n "$RESULT_DIR" ]] || { printf 'Usage: scripts/evaluate-x0.sh RESULT_DIR\n' >&2; exit 2; }
for file in samples.csv stream-results.csv latency-results.csv; do
  [[ -s "$RESULT_DIR/$file" ]] || { printf 'X0 INCOMPLETE: missing %s\n' "$file" >&2; exit 2; }
done

# stream-results.csv: profile,drop_percent,packet_loss_percent,oom_count,sustained_throttle_seconds
# latency-results.csv: profile,input_to_paint_p95_ms
release_floor="$(awk -F, '$1 == "720p15" && $2 < 2 && $4 == 0 && $5 == 0 {print "pass"}' "$RESULT_DIR/stream-results.csv")"
memory_floor="$(awk -F, 'NR > 1 { if ($4 < 48) bad=1 } END { if (!bad) print "pass" }' "$RESULT_DIR/samples.csv")"
memory_ceiling="$(awk -F, 'NR > 1 { if ($8 >= 409600) bad=1 } END { if (!bad) print "pass" }' "$RESULT_DIR/samples.csv")"
latency_floor="$(awk -F, '$1 == "720p15" && $2 < 250 {print "pass"}' "$RESULT_DIR/latency-results.csv")"

if [[ "$release_floor" == pass && "$memory_floor" == pass && "$memory_ceiling" == pass && "$latency_floor" == pass ]]; then
  printf 'X0 GO: 720p15 release floor passed. Continue to X1.\n'
  exit 0
fi

emergency_only="$(awk -F, '$1 == "480p10" && $2 < 2 && $4 == 0 && $5 == 0 {print "yes"}' "$RESULT_DIR/stream-results.csv")"
if [[ "$emergency_only" == yes ]]; then
  printf 'X0 NO-GO: only 480p10 is viable. Freeze XanhTab and retain this benchmark.\n'
else
  printf 'X0 NO-GO: release floor failed. Inspect captured evidence before changing budgets.\n'
fi
exit 1
