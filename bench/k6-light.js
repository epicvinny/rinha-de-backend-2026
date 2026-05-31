import http from "k6/http";
import { check } from "k6";
import { SharedArray } from "k6/data";
// SharedArray: parse the (large) dataset ONCE; all VUs share one read-only copy.
const ENTRIES = new SharedArray("entries", function () {
  const parsed = JSON.parse(open(__ENV.K6_DATA || "test-data.json"));
  return Array.isArray(parsed) ? parsed : (parsed.entries || []);
});
export const options = {
  summaryTrendStats: ["avg","p(95)","p(99)","max"],
  discardResponseBodies: true,
  scenarios: { default: {
    executor: "ramping-arrival-rate", startRate: 1, timeUnit: "1s",
    preAllocatedVUs: 30, maxVUs: 60, gracefulStop: "3s",
    stages: [{ duration: "25s", target: 50 }],
  }},
};
export default function () {
  const e = ENTRIES[(__ITER || 0) % ENTRIES.length];
  const body = JSON.stringify(e.request !== undefined ? e.request : e);
  const res = http.post("http://localhost:9999/fraud-score", body, { headers: {"Content-Type":"application/json"}, timeout: "2001ms" });
  check(res, { "status is 200": (r) => r.status === 200 });
}
