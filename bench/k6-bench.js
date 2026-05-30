// k6 bench mirroring the official Rinha load (ramping-arrival-rate 1->900 req/s over 120s,
// 100/250 VUs, keep-alive). Reports http_req_duration p99. Reads the labeled dataset from
// K6_DATA (default ./test-data.json next to this script on the VM).
//
// Run (on the VM): K6_DATA=$HOME/rinha/test-data.json k6 run --summary-export=/tmp/k6-summary.json k6-bench.js
import http from 'k6/http';
import { check } from 'k6';

const RAW = open(__ENV.K6_DATA || 'test-data.json');
const parsed = JSON.parse(RAW);
const ENTRIES = Array.isArray(parsed) ? parsed : (parsed.entries || []);

export const options = {
  summaryTrendStats: ['avg', 'p(95)', 'p(99)', 'max'],
  scenarios: {
    default: {
      executor: 'ramping-arrival-rate',
      startRate: 1,
      timeUnit: '1s',
      preAllocatedVUs: 100,
      maxVUs: 250,
      gracefulStop: '10s',
      stages: [{ duration: '120s', target: 900 }],
    },
  },
};

export default function () {
  // pick an entry; .request is the payload (matches test/test-data.json / official test.js)
  const e = ENTRIES[(__ITER || 0) % ENTRIES.length] || ENTRIES[Math.floor(Math.random() * ENTRIES.length)];
  const body = JSON.stringify(e.request !== undefined ? e.request : e);
  const res = http.post('http://localhost:9999/fraud-score', body, {
    headers: { 'Content-Type': 'application/json' },
    timeout: '2001ms',
  });
  check(res, { 'status is 200': (r) => r.status === 200 });
}
