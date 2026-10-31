// Prints the k6 summaries from run.sh as a table: requests per second, latency and errors,
// and for chat the time Tokenrein adds over the direct baseline.
//   node summary.mjs results 500 2000
import { readFileSync } from "node:fs";

const [dir, ...rates] = process.argv.slice(2);
const read = (name) => JSON.parse(readFileSync(`${dir}/${name}.json`, "utf8")).metrics;
const ms = (v) => `${v.toFixed(2)} ms`;

console.log("| Test | Rate | Achieved req/s | p50 | p90 | p99 | Failed |");
console.log("|---|---|---|---|---|---|---|");
for (const rate of rates) {
  const row = (label, name) => {
    const m = read(`${name}-${rate}`);
    const d = m.http_req_duration;
    const failed = (m.http_req_failed?.value ?? 0) * 100;
    console.log(
      `| ${label} | ${rate}/s | ${m.http_reqs.rate.toFixed(0)} | ${ms(d.med)} | ${ms(d["p(90)"])} | ${ms(d["p(99)"])} | ${failed.toFixed(2)}% |`,
    );
    return d;
  };
  const direct = row("Chat, direct to upstream", "direct");
  const proxied = row("Chat, through Tokenrein", "proxy");
  row("POST /v1/check", "check");
  row("POST /v1/usage", "usage");
  console.log(
    `| **Added by Tokenrein** | ${rate}/s | | **${ms(proxied.med - direct.med)}** | ${ms(proxied["p(90)"] - direct["p(90)"])} | **${ms(proxied["p(99)"] - direct["p(99)"])}** | |`,
  );
}
