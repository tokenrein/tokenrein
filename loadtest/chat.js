// Chat completions at a fixed request rate, either straight to the fake upstream (the baseline)
// or through the Tokenrein proxy. Each run makes 10 calls with different messages.
//   k6 run -e TARGET=http://127.0.0.1:18787 -e KEY=tr_... -e RATE=500 chat.js
import http from "k6/http";
import { check } from "k6";

const target = __ENV.TARGET || "http://127.0.0.1:18787";
const key = __ENV.KEY || "";

export const options = {
  scenarios: {
    chat: {
      executor: "constant-arrival-rate",
      rate: Number(__ENV.RATE || 500),
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
  const body = JSON.stringify({
    model: "gpt-4o-mini",
    max_tokens: 64,
    messages: [{ role: "user", content: `load test ${__VU}-${__ITER}` }],
  });
  const res = http.post(`${target}/v1/chat/completions`, body, {
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${key}`,
      "x-tr-run-id": `lt-${__VU}-${Math.floor(__ITER / 10)}`,
      "x-tr-agent": "loadtest",
    },
  });
  check(res, { "status 200": (r) => r.status === 200 });
}
