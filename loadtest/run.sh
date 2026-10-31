#!/usr/bin/env bash
# Measures what Tokenrein adds to each call. Starts a throwaway Redis, a fake OpenAI upstream
# and a release build of Tokenrein on spare ports, then runs k6 at each rate in RATES:
#   direct  chat completions straight to the fake upstream (the baseline)
#   proxy   the same requests through Tokenrein (budgets enforced, loop guard on)
#   check   POST /v1/check, what the SDKs call before each model call
#   usage   POST /v1/usage, what the SDKs and connectors call after it
# Needs k6, node and redis-server. Summaries land in loadtest/results/.
#   cargo build --release -p tokenrein && loadtest/run.sh
set -euo pipefail
cd "$(dirname "$0")"

bin=${BIN:-../target/release/tokenrein}
rates=${RATES:-"500 2000"}
duration=${DURATION:-60s}
out=results
mkdir -p "$out"
work=$(mktemp -d)
pids=()
cleanup() {
  kill "${pids[@]}" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

redis-server --port 16379 --bind 127.0.0.1 --save '' --appendonly no >/dev/null &
pids+=($!)
node mock-upstream.mjs &
pids+=($!)

keys=$("$bin" gen-key)
key=$(awk 'NR == 1 { print $NF }' <<<"$keys")
hash=$(awk 'NR == 2 { print $NF }' <<<"$keys")
sed "s/@KEY_HASH@/$hash/" config.toml >"$work/tokenrein.toml"

# One JSON line per request goes to stdout; a real deployment ships it to its log pipeline.
TOKENREIN_CONFIG="$work/tokenrein.toml" TOKENREIN_ADMIN_TOKEN=$(openssl rand -hex 16) \
  LOADTEST_UPSTREAM_KEY=not-a-real-key RUST_LOG=warn "$bin" >/dev/null &
pids+=($!)
for _ in $(seq 50); do
  curl -fs http://127.0.0.1:18788/readyz >/dev/null && break
  sleep 0.2
done
curl -fs http://127.0.0.1:18788/readyz >/dev/null || { echo "tokenrein did not start" >&2; exit 1; }

k() { k6 run --quiet --no-usage-report -e KEY="$key" -e DURATION="$duration" "$@"; }
k -e TARGET=http://127.0.0.1:18787 -e RATE=200 -e DURATION=10s chat.js >/dev/null # warm-up

for rate in $rates; do
  k -e TARGET=http://127.0.0.1:18080 -e RATE="$rate" --summary-export "$out/direct-$rate.json" chat.js
  k -e TARGET=http://127.0.0.1:18787 -e RATE="$rate" --summary-export "$out/proxy-$rate.json" chat.js
  k -e ENDPOINT=check -e RATE="$rate" --summary-export "$out/check-$rate.json" sdk.js
  k -e ENDPOINT=usage -e RATE="$rate" --summary-export "$out/usage-$rate.json" sdk.js
done

node summary.mjs "$out" $rates
# Proof the requests went through the budget checks, and that nothing was dropped or failed.
echo
curl -fs http://127.0.0.1:18788/metrics |
  grep -E '^tokenrein_(decisions_total|ledger_errors_total|activity_events_dropped_total)'
