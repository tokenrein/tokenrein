# Operating Tokenrein

Runbooks for the things that go wrong, how to rotate secrets, and what to monitor. For
installing and upgrading see [install.md](install.md).

## Health endpoints

| Endpoint | Listener | Answers | Use it for |
|---|---|---|---|
| `GET /healthz` | both (8787 and 8788) | `200 ok` while the process serves HTTP | Liveness and readiness probes |
| `GET /readyz` | admin (8788) | `200 ready`, or `503` naming Redis or the database when one is unreachable | Startup probe, dashboards, blackbox monitoring |
| `GET /metrics` | admin (8788) | Prometheus text | Scraping (below) |

None of them needs the admin token. `/readyz` deliberately does not gate traffic: when Redis is
down, orgs that fail open must keep being served, which needs the pods to stay in the load
balancer.

A container image has no shell or curl; healthchecks run the binary itself:
`tokenrein probe http://127.0.0.1:8787/healthz http://127.0.0.1:8788/readyz` exits 0 only
if every URL answers 2xx within 3 seconds.

## Redis is down

Redis is the budget ledger, so this is the outage that matters. Every Redis call fails after
`[redis] timeout_ms` (1 second by default; at once if the connection is refused) and is not
retried inside the call. What happens next depends on each org's **fail mode** (Keys & limits,
or `fail_mode` in the admin API):

| | Org fails **open** (default) | Org fails **closed** |
|---|---|---|
| Proxy calls (`/v1/chat/completions`, `/v1/responses`, `/v1/messages`) | Forwarded to the provider with `x-tr-degraded: true`. **Not budgeted**: limits are not checked, paused or killed runs are not stopped, the spend is not recorded | Refused: `503` with error code `tokenrein_unavailable` |
| `/v1/usage`, `/v1/check` | `503 tokenrein_unavailable` | `503 tokenrein_unavailable` |
| Claude Code hooks | Allowed (hooks never block an agent because of Tokenrein) | Allowed |
| Metrics | `decision="allowed_degraded"` | `decision="unavailable"` |

In both cases `tokenrein_ledger_errors_total` climbs and `/readyz` returns
`503 redis unavailable`. The dashboard's spend, runs and activity pages return `503`.

Config edits keep working when Postgres is configured (the config lives there). Without
Postgres the config lives in Redis too, so edits fail and each replica keeps serving the config
it last loaded.

**Do:**

1. Check the Redis endpoint from a proxy pod's network (DNS, security groups, credentials in
   `TOKENREIN_REDIS_URL`), and Redis itself (`redis-cli ping`, memory: `INFO memory`; at `maxmemory`
   with `noeviction` writes fail).
2. **Do not restart or scale out the proxy** during the outage. A starting proxy needs Redis and
   exits if it cannot reach it (Kubernetes shows CrashLoopBackOff). Running replicas keep
   serving.
3. If unbudgeted spend on fail-open orgs is not acceptable for the duration, switch those orgs to
   fail closed (needs Postgres, since the setting is stored with the config).

**After it comes back:** nothing to do. The next call reconnects. Reservations made before
the outage are released by the sweeper once they expire. Spend from degraded requests is not
in the budgets; to reconcile it, use the per-request events on stdout (`"degraded": true`,
with model, usage and cost).

## Postgres is down

Postgres holds config history and the audit log. It is never called per request.

- **Traffic keeps flowing and budgets are enforced** for every org (both fail modes): the ledger
  is Redis.
- **Config edits are refused** (`503`) in the dashboard and the admin API: creating keys, changing
  budgets, saving provider keys, prices. Each replica keeps serving the config it last loaded and
  logs `managed config sync failed; keeping the current config` every 2 seconds.
- **Run controls still work** (pause, resume, kill), but their audit entries cannot be stored.
  They are still written to the log (`target: "audit"`), and each failure adds to
  `tokenrein_audit_write_failures_total`.
- `/readyz` returns `503 database unavailable` (after the 5-second connection timeout).

**Do:** as with Redis, **do not restart or scale out the proxy**: a starting proxy runs database
migrations and exits if Postgres is unreachable. Fix the database (connections, disk, credentials
in `TOKENREIN_DATABASE_URL`). When it is back, edits work again with no restart. If audit entries were
lost, recover them from the logs (`target: "audit"`) for the outage window.

## A proxy will not start

The process logs the reason as JSON on stderr and exits. The usual causes:

| Log message | Fix |
|---|---|
| `connecting to Redis` | `TOKENREIN_REDIS_URL` wrong or Redis unreachable |
| `connecting to Postgres` / `running database migrations` | `TOKENREIN_DATABASE_URL` wrong, Postgres unreachable, or the user cannot create tables. An older binary against a newer schema fails here too: roll forward or restore the backup |
| `invalid TOKENREIN_SECRETS_KEY` | Must be exactly 64 hex characters |
| `TOKENREIN_SECRETS_KEY_PREVIOUS is set but TOKENREIN_SECRETS_KEY is not` | Set both, or neither |
| `cannot read secret file` | A `*_FILE` variable points at a file that does not exist or is not readable by uid 65532 |
| `binding 0.0.0.0:8787` | Port in use, or `TOKENREIN_LISTEN` / `TOKENREIN_ADMIN_LISTEN` malformed |
| `admin API disabled` (a warning, not fatal) | `TOKENREIN_ADMIN_TOKEN` unset or shorter than 16 characters |

## Rotating secrets

### The secrets key (`TOKENREIN_SECRETS_KEY`)

It encrypts the provider API keys saved in the dashboard. Rotating it takes no downtime:

1. Create a new key: `openssl rand -hex 32`.
2. On every replica, set `TOKENREIN_SECRETS_KEY` to the **new** key and `TOKENREIN_SECRETS_KEY_PREVIOUS` to the
   **old** one, and roll out. Saved keys keep working (opened with the previous key); keys saved
   from now on use the new one.
   - Helm with the chart's Secret: `--set secrets.secretsKey=<new> --set secrets.previousSecretsKey=<old>`.
   - Helm with `existingSecret`: add `TOKENREIN_SECRETS_KEY_PREVIOUS` to the Secret, change
     `TOKENREIN_SECRETS_KEY`, and (with `mountAsFiles`) set `secrets.rotating=true`.
   - Compose: edit `.env`, then `docker compose up -d`.
3. Re-encrypt everything once, from any replica:

   ```bash
   kubectl -n tokenrein exec deploy/tokenrein -- tokenrein rotate-secrets-key
   # or: docker compose exec tokenrein tokenrein rotate-secrets-key
   ```

   It lists the `org/provider` keys it re-encrypted and records a `secrets_key.rotate` entry in the
   audit log. If any saved key cannot be opened with either key it changes nothing and names
   them. Running it again is safe ("nothing to do").
4. Remove `TOKENREIN_SECRETS_KEY_PREVIOUS` (and `secrets.rotating`) and roll out again. Check Keys &
   limits: no provider key should show an error.

### Other secrets

- **Admin token** (`TOKENREIN_ADMIN_TOKEN`): change it and roll out. Dashboard users sign in again.
- **Provider API keys**: save the new key in the dashboard (all replicas use it within 2
  seconds), then revoke the old one at the provider. For keys from the environment, update the
  Secret and roll out.
- **Virtual keys** (`tr_…`, used by agents): create a new key for the project, deploy it to the
  agent, then revoke the old one in Keys & limits. Revocation applies within 2 seconds.
- **Database password**: add the new password in Postgres (or create a second user), update
  `TOKENREIN_DATABASE_URL`, roll out, then drop the old one.

## Metrics and alerts

`GET /metrics` on the admin listener. Counters are per process and reset when it restarts, so
always use `rate()` / `increase()`. No org, project, user or key ids are exported.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `tokenrein_decisions_total` | counter | `path` (`proxy`, `ingest`), `decision` | Requests by outcome: `allowed`, `allowed_degraded` (Redis down, failed open), `blocked_budget`, `blocked_run_stopped`, `upstream_error`, `unavailable` (Redis down, failed closed), `other` |
| `tokenrein_pre_upstream_seconds` | histogram | | Time Tokenrein adds before calling the provider (auth, pricing, budget reservation) |
| `tokenrein_upstream_seconds` | histogram | `provider` | Provider response time, to the end of the body (streams included) |
| `tokenrein_ledger_errors_total` | counter | | Failed Redis calls (request path, sweeper, admin, ingest) |
| `tokenrein_activity_events_dropped_total` | counter | `reason` (`queue_full`, `write_failed`) | Dashboard activity events lost. Budgets are unaffected; spend reports and run timelines are incomplete |
| `tokenrein_audit_write_failures_total` | counter | | Audit entries that could not be stored (they are still in the log) |
| `tokenrein_config_version` | gauge | | Managed config version this process serves |
| `tokenrein_build_info` | gauge | `version` | Always 1 |

Suggested alerts (Prometheus rules; tune the thresholds to your traffic):

```yaml
groups:
  - name: tokenrein
    rules:
      - alert: TokenreinLedgerErrors
        expr: sum(rate(tokenrein_ledger_errors_total[5m])) > 0
        for: 2m
        annotations:
          summary: Redis calls are failing; fail-open orgs are unbudgeted, fail-closed orgs refused
      - alert: TokenreinRefusingTraffic
        expr: sum(rate(tokenrein_decisions_total{decision="unavailable"}[5m])) > 0
        labels: { severity: page }
        annotations:
          summary: Fail-closed orgs are being refused because Redis is unreachable
      - alert: TokenreinUnbudgetedSpend
        expr: sum(rate(tokenrein_decisions_total{decision="allowed_degraded"}[5m])) > 0
        for: 5m
        annotations:
          summary: Requests are passing without budget checks (Redis down, orgs fail open)
      - alert: TokenreinOverheadHigh
        expr: histogram_quantile(0.99, sum by (le) (rate(tokenrein_pre_upstream_seconds_bucket[5m]))) > 0.05
        for: 10m
        annotations:
          summary: p99 time added before the provider call is over 50 ms (check Redis latency)
      - alert: TokenreinUpstreamErrors
        expr: |
          sum(rate(tokenrein_decisions_total{decision="upstream_error"}[5m]))
            / sum(rate(tokenrein_decisions_total{path="proxy"}[5m])) > 0.05
        for: 10m
        annotations:
          summary: Over 5% of proxied calls fail at the provider
      - alert: TokenreinAuditWriteFailures
        expr: increase(tokenrein_audit_write_failures_total[15m]) > 0
        annotations:
          summary: Audit entries are not being stored (Postgres down?); recover them from the logs
      - alert: TokenreinActivityDropped
        expr: increase(tokenrein_activity_events_dropped_total[15m]) > 100
        annotations:
          summary: Dashboard activity is being dropped (Redis slow or the queue is full)
      - alert: TokenreinConfigDrift
        expr: max(tokenrein_config_version) - min(tokenrein_config_version) > 0
        for: 5m
        annotations:
          summary: Replicas serve different config versions; one is not syncing (Postgres or Redis)
```

Watch also, without alerting: `rate(tokenrein_decisions_total{decision=~"blocked_.*"}[1h])`
(guardrails firing, worth a look after enabling Enforce) and the upstream latency per provider.

## Logs

- **stderr**: JSON logs. `RUST_LOG=info` by default; `RUST_LOG=tokenrein=debug` for more.
  Audit entries are logged with `target: "audit"` (actor, action, target, IP, config version):
  ship them to your SIEM.
- **stdout**: one JSON event per request (`event: "request"`): org, project, agent, user, run,
  model, decision, estimated and actual cost, usage, `pre_upstream_us`, `upstream_ms`,
  `degraded`. This is the raw record for analytics and for reconciling spend after an outage.
  Prompts and completions are never logged.
