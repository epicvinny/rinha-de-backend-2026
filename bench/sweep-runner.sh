#!/usr/bin/env bash
# Runs ON the GCP VM. Iterates a config matrix; for each, generates a compose via
# gen_compose.sh, runs remote-bench.sh N times, records median p99 + max fail.
# Experiments file (~/rinha/experiments.tsv): LABEL<TAB>KEY=val KEY=val...
set -uo pipefail
DIR="$HOME/rinha"
GEN="$DIR/gen_compose.sh"
EXP="${1:-$DIR/experiments.tsv}"
REPEATS="${REPEATS:-3}"
OUT="${OUT:-$DIR/sweep-results.tsv}"
TMP="$DIR/_exp.yml"
TAB=$(printf "\t")

median() { printf "%s\n" "$@" | sort -n | awk "{a[NR]=\$1} END{n=NR; if(n%2)print a[(n+1)/2]; else printf \"%.4f\n\",(a[n/2]+a[n/2+1])/2}"; }

[ -f "$OUT" ] || printf "ts\tlabel\tmedian_p99_ms\truns\tmax_fail\tmax_chkfail\tall_p99\toverrides\n" > "$OUT"

while IFS="$TAB" read -r label overrides; do
  [ -z "${label:-}" ] && continue
  case "$label" in \#*) continue;; esac
  echo "=== [$label] $overrides @ $(date -u +%H:%M:%S)UTC ==="
  env $overrides bash "$GEN" > "$TMP" 2>/tmp/generr || { echo "GEN FAIL: $(cat /tmp/generr)"; continue; }
  ps=(); maxfail=0; maxchk=0
  for r in $(seq 1 "$REPEATS"); do
    j=$(bash "$DIR/remote-bench.sh" "$TMP" 2>/dev/null | tail -1)
    p=$(printf "%s" "$j" | jq -r ".p99_ms // empty" 2>/dev/null)
    f=$(printf "%s" "$j" | jq -r ".http_req_failed // 0" 2>/dev/null)
    c=$(printf "%s" "$j" | jq -r ".checks_failed // 0" 2>/dev/null)
    if [ -z "$p" ]; then echo "  run $r: BAD ($j)"; continue; fi
    echo "  run $r: p99=$p fail=$f chkfail=$c"
    ps+=("$p")
    awk "BEGIN{exit !($f>$maxfail)}" && maxfail=$f
    awk "BEGIN{exit !($c>$maxchk)}" && maxchk=$c
  done
  if [ ${#ps[@]} -eq 0 ]; then echo "  ALL RUNS BAD for $label"; continue; fi
  med=$(median "${ps[@]}")
  allp=$(IFS=,; echo "${ps[*]}")
  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$label" "$med" "${#ps[@]}" "$maxfail" "$maxchk" "$allp" "$overrides" >> "$OUT"
  echo ">>> [$label] MEDIAN p99=$med ms (runs=${#ps[@]}, maxfail=$maxfail, chkfail=$maxchk)"
done < "$EXP"
echo "=== sweep complete @ $(date -u +%H:%M:%S)UTC ==="
echo "=== RESULTS ==="; column -t -s"$TAB" "$OUT"
