// The proxy-free endpoints the SDKs and connectors call, at a fixed request rate:
// ENDPOINT=check (POST /v1/check before a call) or ENDPOINT=usage (POST /v1/usage after it).
//   k6 run -e ENDPOINT=check -e KEY=tr_... -e RATE=1000 sdk.js
import http from "k6/http";
import { check } from "k6";

const target = __ENV.TARGET || "http://127.0.0.1:18787";
const key = __ENV.KEY || "";
const endpoint = __ENV.ENDPOINT || "check";

export const options = {
  scenarios: {
    sdk: {
      executor: "constant-arrival-rate",
      rate: Number(__ENV.RATE || 1000),
      timeUnit: "1s",
      duration: __ENV.DURATION || "60s",
      preAllocatedVUs: 100,
      maxVUs: 1000,
    },
  },
  summaryTrendStats: ["avg", "med", "p(90)", "p(99)", "max"],
  thresholds: { checks: ["rate>0.999"] },
};

export default function () {
  const run = `lt-${endpoint}-${__VU}-${Math.floor(__ITER / 10)}`;
  const body =
    endpoint === "usage"
      ? {
          events: [
            {
              run_id: run,
              agent: "loadtest",
              provider: "openai",
              model: "gpt-4o-mini",
              input_tokens: 20,
              output_tokens: 10,
              duration_ms: 500,
            },
          ],
        }
      : { run_id: run, agent: "loadtest", model: "gpt-4o-mini", max_output_tokens: 64 };
  const res = http.post(`${target}/v1/${endpoint}`, JSON.stringify(body), {
    headers: { "content-type": "application/json", authorization: `Bearer ${key}` },
  });
  check(res, { "status 200": (r) => r.status === 200 });
}
