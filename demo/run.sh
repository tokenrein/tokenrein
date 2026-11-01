#!/usr/bin/env bash
# Try Tokenrein with misbehaving agents and no API key. Starts a throwaway Redis, Postgres, a fake
# model and a release build of Tokenrein on spare ports, creates an org with a limit in each mode,
# then runs demo/agents.py. Approve or stop the paused runs in the dashboard it prints.
# Needs redis-server, postgres, node and python3.
#   cargo build --release -p tokenrein && demo/run.sh
# With arguments, runs them instead of the agents (demo/record.cjs uses this).
set -euo pipefail
cd "$(dirname "$0")"

bin=${BIN:-../target/release/tokenrein}
admin=http://127.0.0.1:19788
work=$(mktemp -d)
pids=()
cleanup() {
  kill "${pids[@]}" 2>/dev/null || true
  pg_ctl -D "$work/pg" -m fast stop >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

redis-server --port 19379 --bind 127.0.0.1 --save '' --appendonly no >/dev/null &
pids+=($!)
node mock-model.mjs &
pids+=($!)

LC_ALL=C initdb -D "$work/pg" -U tokenrein -A trust -E UTF8 >/dev/null
LC_ALL=C pg_ctl -D "$work/pg" -o "-p 19432 -c listen_addresses=127.0.0.1 -c unix_socket_directories=''" -w -l "$work/pg.log" start >/dev/null
createdb -h 127.0.0.1 -p 19432 -U tokenrein tokenrein

export TOKENREIN_CONFIG=config.toml TOKENREIN_ADMIN_TOKEN=$(openssl rand -hex 16) DEMO_UPSTREAM_KEY=not-a-real-key
export TOKENREIN_DATABASE_URL=postgres://tokenrein@127.0.0.1:19432/tokenrein
RUST_LOG=warn "$bin" >/dev/null &
pids+=($!)
for _ in $(seq 100); do
  curl -fs "$admin/readyz" >/dev/null && break
  sleep 0.2
done
curl -fs "$admin/readyz" >/dev/null || { echo "tokenrein did not start" >&2; exit 1; }

api() { curl -fsS -H "authorization: Bearer $TOKENREIN_ADMIN_TOKEN" -H 'content-type: application/json' "$@"; }

# A person to sign in as.
export DEMO_EMAIL=priya@example.com DEMO_PASSWORD=$(openssl rand -hex 12)
link=$("$bin" create-owner "$DEMO_EMAIL" | grep -Eo 'https?://[^ ]+|/[^ ]*token=[^ ]+' | head -1)
curl -fsS -H 'content-type: application/json' "$admin/auth/setup" \
  -d "{\"token\": \"${link##*[=#]}\", \"password\": \"$DEMO_PASSWORD\", \"name\": \"Priya\"}" >/dev/null

# Every run gets $1 and then asks; the loop guard pauses after 5 repeats.
api "$admin/admin/orgs" -d '{"id": "acme", "budgets": {
  "run": {"limit_usd": 1, "mode": "ask"}, "max_run_usd": 10,
  "loops": {"action": "pause", "max_repeats": 5, "repeat_window_secs": 600}}}' >/dev/null
project() { api "$admin/admin/orgs/acme/projects" -d "{\"id\": \"$1\", \"budgets\": $2}" >/dev/null; }
project support '{}'
project research '{}'
project data-pipeline '{"project_monthly": {"limit_usd": 0.4, "mode": "check"}, "run": {"limit_usd": 10, "mode": "skip"}}'
project batch-jobs '{"project_monthly": {"limit_usd": 0.5, "mode": "enforce"}, "run": {"limit_usd": 10, "mode": "skip"}}'

keys='{}'
for p in support research data-pipeline batch-jobs; do
  key=$(api "$admin/admin/orgs/acme/projects/$p/keys" -d '{"name": "demo"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])')
  keys=$(python3 -c 'import json,sys; d=json.loads(sys.argv[1]); d[sys.argv[2]]=sys.argv[3]; print(json.dumps(d))' "$keys" "$p" "$key")
done
export DEMO_KEYS=$keys

if [ $# -gt 0 ]; then
  "$@"
else
  echo "Dashboard: $admin   sign in as $DEMO_EMAIL / $DEMO_PASSWORD"
  echo
  python3 agents.py
  echo
  read -r -p "Press Enter to stop the demo. " _
fi
