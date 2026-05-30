#!/usr/bin/env bash
# Runs ON the GCP VM. Brings up the compose stack, runs co-located k6 against
# localhost:9999, prints {"p99_ms":..,"http_req_failed":..,"checks_failed":..}, tears down.
# Uses `sudo docker` so it works before the docker-group re-login takes effect.
set -uo pipefail
COMPOSE="${1:-$HOME/rinha/docker-compose.yml}"
DIR="$(dirname "$COMPOSE")"
cd "$DIR"
DC="sudo docker compose -f $COMPOSE"

$DC down -v >/dev/null 2>&1 || true
$DC pull -q  >/dev/null 2>&1 || true     # config sweeps pull the public image; local-built tags are no-ops
$DC up -d    >/dev/null 2>&1

# wait until the LB answers /ready (LB :9999 -> handoff -> reactor -> 200)
ready=0
for i in $(seq 1 90); do
  if curl -fsS http://localhost:9999/ready >/dev/null 2>&1; then ready=1; break; fi
  sleep 2
done
if [ "$ready" != 1 ]; then echo '{"error":"not ready after 180s"}'; $DC logs --tail=40; $DC down -v >/dev/null 2>&1; exit 1; fi

# k6 (mirrors the official ramping-arrival-rate 1->900/120s); reads test-data.json from this dir
K6_DATA="$DIR/test-data.json" k6 run --summary-export=/tmp/k6-summary.json "$DIR/k6-bench.js" >/tmp/k6.out 2>&1 || true

p99=$(jq -r '.metrics.http_req_duration["p(99)"] // empty' /tmp/k6-summary.json 2>/dev/null)
fail=$(jq -r '.metrics.http_req_failed.value // 0' /tmp/k6-summary.json 2>/dev/null)
chkfail=$(jq -r '.metrics.checks.fails // 0' /tmp/k6-summary.json 2>/dev/null)
$DC down -v >/dev/null 2>&1 || true

if [ -z "${p99:-}" ]; then echo '{"error":"no p99 in summary"}'; tail -30 /tmp/k6.out; exit 1; fi
echo "{\"p99_ms\": $p99, \"http_req_failed\": $fail, \"checks_failed\": $chkfail}"
