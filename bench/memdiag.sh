#!/usr/bin/env bash
set -uo pipefail
cd ~/rinha
DC="sudo docker compose -f _md.yml"
DOWN="$DC down -v --timeout 5 --remove-orphans"
bash gen_compose.sh > _md.yml 2>/tmp/ge
$DOWN >/dev/null 2>&1 || true
$DC pull -q >/dev/null 2>&1 || true
rm -f memdiag.log memdiag-k6.log
$DC up -d >/dev/null 2>&1 || true
ready=0; for i in $(seq 1 40); do curl -fsS --max-time 2 http://localhost:9999/ready >/dev/null 2>&1 && { ready=1; break; }; sleep 1; done
echo "CFG PREFER=${PREFER:-1} BUSY_US=${BUSY_US:-50} ready=$ready $(date -u +%H:%M:%S)" > memdiag.log
( for s in $(seq 1 38); do A=$(awk "/MemAvailable/{print \$2}" /proc/meminfo); echo "T=${s}s MemAvail=${A}kB $(date -u +%H:%M:%S)"; ps -eo rss,comm --sort=-rss | head -6 | tail -5; echo ---; sleep 1; done >> memdiag.log 2>&1 ) &
SAMP=$!
K6_DATA=$HOME/rinha/test-data.json taskset -c 2,3,6,7 timeout 40 k6 run --summary-export=/tmp/mk.json $HOME/rinha/k6-light.js > memdiag-k6.log 2>&1 || true
kill $SAMP 2>/dev/null
p99=$(jq -r ".metrics.http_req_duration[\"p(99)\"]//empty" /tmp/mk.json 2>/dev/null)
$DOWN >/dev/null 2>&1 || true
echo "MEMDIAG DONE p99=$p99 $(date -u +%H:%M:%S)" >> memdiag.log
