#!/usr/bin/env bash
# A markdown table of criterion's results: each benchmark's mean on the base and on the head, the
# change and its 95% confidence interval, and a verdict. A change whose interval includes 0, or that
# is smaller than the noise threshold, is no change.
#
#   bench-table.sh CRITERION_DIR [NOISE]   # NOISE: a fraction (default 0.03, 3%)
#
# The base's results are CRITERION_DIR/**/base (cargo bench -- --save-baseline base), the head's are
# **/new and the comparison **/change (cargo bench -- --baseline-lenient base).
set -euo pipefail
dir=$1
noise=${2:-0.03}
if [ ! -d "$dir" ]; then
  echo "No benchmark results ($dir is not there)."
  exit 0
fi

# One row, from the estimates.json files of a benchmark (null where there is none)
# shellcheck disable=SC2016 # (a jq program: its $ are jq's)
row='
def time:
  if . == null then "-"
  elif . >= 1e9 then "\(. / 1e7 | round / 100) s"
  elif . >= 1e6 then "\(. / 1e4 | round / 100) ms"
  elif . >= 1e3 then "\(. / 1e1 | round / 100) µs"
  else "\(. * 100 | round / 100) ns" end;
def pct: (. * 10000 | round / 100) as $p | if $p > 0 then "+\($p)%" else "\($p)%" end;
($base.mean.point_estimate // null) as $b
| ($head.mean.point_estimate // null) as $h
| ($change.mean // null) as $c
| (if $h == null then "removed"
   elif $b == null or $c == null then "new"
   elif ($c.confidence_interval.lower_bound <= 0 and $c.confidence_interval.upper_bound >= 0)
        or ($c.point_estimate | fabs) < $noise then "no change"
   elif $c.point_estimate < 0 then "improved"
   else "**regressed**" end) as $verdict
| [$id, ($b | time), ($h | time),
   (if $c == null then "-" else ($c.point_estimate | pct) end),
   (if $c == null then "-"
    else "\($c.confidence_interval.lower_bound | pct) … \($c.confidence_interval.upper_bound | pct)" end),
   $verdict]
| "| " + join(" | ") + " |"
'

json() { if [ -f "$1" ]; then cat "$1"; else echo null; fi; }

rows=$(
  find "$dir" -type f -name benchmark.json \( -path '*/base/*' -o -path '*/new/*' \) |
    sed 's|/[^/]*/benchmark.json$||' | sort -u |
    while IFS= read -r d; do
      meta="$d/new/benchmark.json"
      [ -f "$meta" ] || meta="$d/base/benchmark.json"
      jq -rn -f <(printf '%s' "$row") \
        --arg id "$(jq -r .full_id "$meta")" \
        --argjson noise "$noise" \
        --argjson base "$(json "$d/base/estimates.json")" \
        --argjson head "$(json "$d/new/estimates.json")" \
        --argjson change "$(json "$d/change/estimates.json")"
    done | sort
)

if [ -z "$rows" ]; then
  echo "No benchmark results in $dir."
  exit 0
fi
regressed=$(grep -c 'regressed\*\* |$' <<<"$rows" || true)
improved=$(grep -c '| improved |$' <<<"$rows" || true)
total=$(wc -l <<<"$rows" | tr -d ' ')
echo "$total benchmarks: $regressed regressed, $improved improved, the rest within noise (|change| < $(jq -rn --argjson n "$noise" '$n * 100')%, or a 95% interval that includes 0)."
echo
echo "| benchmark | base | head | change | 95% CI | verdict |"
echo "|---|--:|--:|--:|--:|---|"
echo "$rows"
