#!/usr/bin/env bash
# Runs ON the GCP VM. Brings up the compose stack, runs co-located k6 against
# localhost:9999, prints {"p99_ms":..,"http_req_failed":..,"checks_failed":..}, tears down.
# Hardened: fast force-teardown, bounded up, optional k6 cpu-pin (K6_CPUS), k6 hard timeout
# -> a pathological config can never wedge the box (lesson from the 4-vCPU starvation).
set -uo pipefail
COMPOSE="${1:-$HOME/rinha/docker-compose.yml}"
DIR="$(dirname "$COMPOSE")"
cd "$DIR"
DC="sudo docker compose -f $COMPOSE"
DOWN="$DC down -v --timeout 5 --remove-orphans"
K6_CPUS="${K6_CPUS:-}"   # e.g. "2-7" to keep k6 off the stack cores (cpu0/cpu1)

$DOWN >/dev/null 2>&1 || true
$DC pull -q  >/dev/null 2>&1 || true
timeout 130 $DC up -d >/dev/null 2>&1 || true

# wait until the LB answers /ready (LB :9999 -> handoff -> reactor -> 200)
ready=0
for i in $(seq 1 75); do
  if curl -fsS --max-time 2 http://localhost:9999/ready >/dev/null 2>&1; then ready=1; break; fi
  sleep 2
done
if [ "$ready" != 1 ]; then echo "{\"error\":\"not ready after 150s\"}"; $DC logs --tail=30 2>&1 | tail -30; $DOWN >/dev/null 2>&1; exit 1; fi

# k6 (mirrors official ramping-arrival-rate 1->900/120s); optionally pinned off stack cores
TS=""; [ -n "$K6_CPUS" ] && TS="taskset -c $K6_CPUS"
K6_DATA="$DIR/test-data.json" timeout 200 $TS k6 run --summary-export=/tmp/k6-summary.json "$DIR/k6-bench.js" >/tmp/k6.out 2>&1 || true

p99=$(jq -r ".metrics.http_req_duration[\"p(99)\"] // empty" /tmp/k6-summary.json 2>/dev/null)
fail=$(jq -r ".metrics.http_req_failed.value // 0" /tmp/k6-summary.json 2>/dev/null)
chkfail=$(jq -r ".metrics.checks.fails // 0" /tmp/k6-summary.json 2>/dev/null)
$DOWN >/dev/null 2>&1 || true

if [ -z "${p99:-}" ]; then echo "{\"error\":\"no p99 in summary\"}"; tail -20 /tmp/k6.out; exit 1; fi
echo "{\"p99_ms\": $p99, \"http_req_failed\": $fail, \"checks_failed\": $chkfail}"
