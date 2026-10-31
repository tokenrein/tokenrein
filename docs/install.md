# Installing Tokenrein

Tokenrein is one stateless binary, `tokenrein`, plus two stores:

| Component | What it holds | Required |
|---|---|---|
| `tokenrein` | Nothing on disk. Port **8787**: the proxy and the usage/check API that agents and SDKs call. Port **8788**: the dashboard, admin API, `/metrics`, `/readyz` | Yes |
| Redis | The budget ledger (reservations, spend counters per run, agent, project and org), run state (paused / killed), recent activity for the dashboard | Yes |
| Postgres | Every version of the managed config (orgs, projects, keys, budgets, prices) and the audit log | Recommended. Without it both live in Redis: current config only, last 10,000 audit entries |
| Usage history folder | Every request event as Parquet files, read with DuckDB (built into the binary): spend reports past 40 days, CSV exports, run timelines older than 7 days | Recommended. Without it spend reports cover the last 40 days |

Run as many proxy replicas as you like: they share Redis and Postgres, and each picks up config
changes made on any other within 2 seconds.

## Requirements

**Redis** 7.x (6.2 should work; CI and the bundled images use 7.4).

- A single primary: standalone, or a managed service with failover behind one endpoint.
  Redis Cluster mode and Sentinel discovery are not supported yet.
- `maxmemory-policy noeviction`. An evicting policy can silently drop budget counters, which
  resets spend. Size memory for the activity streams (5,000 events per org, 500 per run).
- Persistence (AOF `appendonly yes`, or RDB snapshots). Losing Redis loses this month's spend
  counters and run states (see [Backups](#backups)).
- TLS (`rediss://`) is not supported yet: keep Redis on a private network.
- URL: `redis://[:password@]host:6379[/db]`, set as `TOKENREIN_REDIS_URL`.

**Postgres** 17 (what CI and the bundled images use; 14 or later should work).

- One database and a user that owns it. The schema is created and migrated automatically at
  start-up (migrations take a lock, so several replicas starting at once are fine).
- URL: `postgres://user:password@host:5432/db`, set as `TOKENREIN_DATABASE_URL`. For TLS add
  `?sslmode=verify-full` (certificate checked against the image's system roots) or
  `?sslmode=require` (encrypted, certificate not checked).
- Pool size: `TOKENREIN_DATABASE_MAX_CONNECTIONS` (default 10 per replica). Postgres is never called
  per request; it is used for config edits, the 2-second config poll and the audit log.

**Secrets** every replica needs the same values of:

| Variable | What | Create with |
|---|---|---|
| `TOKENREIN_ADMIN_TOKEN` | Admin API and dashboard sign-in, 16+ characters. Without it the admin API is off | `openssl rand -hex 24` |
| `TOKENREIN_SECRETS_KEY` | 64 hex characters; encrypts provider API keys saved in the dashboard. **Back it up**: without it those keys cannot be decrypted | `openssl rand -hex 32` |
| `TOKENREIN_DATABASE_URL` | Postgres URL (above) | |
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | Optional. Used for every org that has no key saved in the dashboard | |

Any variable can instead be read from a file: set `TOKENREIN_ADMIN_TOKEN_FILE=/run/secrets/admin_token`
(and so on). If both are set, the variable wins. A trailing newline in the file is ignored.

## Configuration

The image ships a config at `/etc/tokenrein/tokenrein.toml` ([deploy/tokenrein.toml](../deploy/tokenrein.toml))
that is enough for most installs: listen on all interfaces, OpenAI and Anthropic upstreams, keys
and Postgres URL from the variables above. These override the file from the environment:

| Variable | Overrides | Default in the image |
|---|---|---|
| `TOKENREIN_LISTEN` | `server.listen` | `0.0.0.0:8787` |
| `TOKENREIN_ADMIN_LISTEN` | `server.admin_listen` | `0.0.0.0:8788` |
| `TOKENREIN_REDIS_URL` | `redis.url` | `redis://127.0.0.1:6379` |
| `TOKENREIN_DATABASE_MAX_CONNECTIONS` | `database.max_connections` | `10` |
| `TOKENREIN_HISTORY_DIR` | `history.dir` | unset (Compose: `/var/lib/tokenrein/history`) |
| `TOKENREIN_HISTORY_RETENTION_DAYS` | `history.retention_days` | `400` |

For anything else (provider base URLs, upstream timeout, the Redis call timeout `[redis]
timeout_ms`), mount your own TOML file and point `TOKENREIN_CONFIG` at it. Orgs, projects, keys and
budgets are not in the file: create them in the dashboard. (A `[[orgs]]` section in the file
seeds a fresh install once; after that the database is the source of truth.)

### Usage history

Redis keeps 40 days of daily spend totals and 7 days of run detail. For longer reports, set
`TOKENREIN_HISTORY_DIR` to a writable folder (the Compose file does, on the `history_data`
volume; Helm: `history.enabled: true`). Every request event is then also written there as
compressed Parquet files, one folder per org and day, every 30 seconds (`[history]
flush_secs`), and read back with DuckDB for:

- the Spend page's 90-day, 6-month and 12-month ranges (`/admin/orgs/{org}/spend?days=`),
- **Download CSV**: spend per day, project, agent, user and model,
- run timelines older than 7 days.

Each replica writes its own files, so every replica must see the **same** folder: a Docker
volume on one host, or ReadWriteMany storage in Kubernetes (EFS, Filestore, Azure Files, NFS,
CephFS). Finished days are merged into one file each hour, and days older than
`retention_days` (400) are deleted. Plan on roughly 100 bytes per request event: a million
events a month is about 100 MB a month. The files are plain Parquet, so the same folder also
works with any tool that reads Parquet (DuckDB, Spark, pandas).

`RUST_LOG` sets the log level (`info` by default). Logs are JSON on stderr; stdout carries one
JSON event per request for an analytics pipeline.

## Docker Compose

`deploy/docker-compose.yml` runs Tokenrein, Redis 7.4 and Postgres 17 on one host, with
persistent volumes and healthchecks. It works with Docker Compose and with `podman compose`.

```bash
cd deploy
./setup.sh                                  # or: ./setup.sh https://tokenrein.example.com
```

`setup.sh` writes `.env` from `.env.example` with a new admin token, secrets key and Postgres
password, and the dashboard address (default `http://127.0.0.1:8788`). To fill it in yourself
instead, copy `.env.example` to `.env` and set `TOKENREIN_ADMIN_TOKEN`, `TOKENREIN_SECRETS_KEY`,
`POSTGRES_PASSWORD` and `TOKENREIN_DATABASE_URL` with the same password (`openssl rand -hex 16`).
Then:

```bash
docker compose up -d
```

- The proxy is on port 8787 (`TOKENREIN_PROXY_PORT`); the dashboard is on `http://127.0.0.1:8788/`
  (`TOKENREIN_ADMIN_PORT`), bound to localhost only. Put it behind your SSO proxy or VPN rather than
  publishing it.
- `docker compose ps` shows `healthy` once both listeners answer and `/readyz` reaches Redis
  and Postgres.
- The image comes from `ghcr.io/tokenrein/tokenrein:latest`; pin a version with
  `TOKENREIN_IMAGE=ghcr.io/tokenrein/tokenrein:0.1.2` in `.env`. `docker compose build`
  builds it from this checkout instead.
- To keep secrets out of the environment, use Compose `secrets:` and the `*_FILE` variables
  (for example `TOKENREIN_SECRETS_KEY_FILE: /run/secrets/tl_secrets_key`).

## Kubernetes (Helm)

```bash
helm install tokenrein deploy/helm/tokenrein -n tokenrein --create-namespace
```

With the defaults this runs 2 proxy replicas plus a single-pod Redis and Postgres for
evaluation, and generates the admin token, secrets key and Postgres password into a Secret
named after the release (kept on uninstall, reused on upgrade). `helm status` prints how to reach
the dashboard and read the admin token.

For production, bring your own Secret and managed Redis and Postgres:

```bash
kubectl -n tokenrein create secret generic tokenrein-secrets \
  --from-literal=TOKENREIN_ADMIN_TOKEN="$(openssl rand -hex 24)" \
  --from-literal=TOKENREIN_SECRETS_KEY="$(openssl rand -hex 32)" \
  --from-literal=TOKENREIN_DATABASE_URL='postgres://tokenrein:...@db.internal:5432/tokenrein?sslmode=verify-full'
kubectl -n tokenrein create secret generic tokenrein-redis \
  --from-literal=TOKENREIN_REDIS_URL='redis://:...@redis.internal:6379'
```

```yaml
# values-prod.yaml
image:
  tag: "0.1.2"
secrets:
  existingSecret: tokenrein-secrets
  mountAsFiles: true          # TOKENREIN_*_FILE instead of environment variables
redis:
  enabled: false
externalRedis:
  existingSecret: tokenrein-redis
postgresql:
  enabled: false
externalDatabase:
  enabled: true               # TOKENREIN_DATABASE_URL comes from the Secret
autoscaling:
  enabled: true
  minReplicas: 3
  maxReplicas: 20
metrics:
  serviceMonitor:
    enabled: true             # needs the Prometheus Operator
```

```bash
helm upgrade --install tokenrein deploy/helm/tokenrein -n tokenrein -f values-prod.yaml
```

What the chart creates:

- A Deployment (non-root, read-only root filesystem, no capabilities, no service account token)
  with three probes. **Startup**: `/readyz`, so a new pod only takes traffic once it has reached
  Redis and Postgres. **Liveness and readiness**: `/healthz`, deliberately not `/readyz`, so a
  Redis or Postgres outage does not pull every pod out of the Service. That would refuse traffic
  even for orgs that fail open (see [operations](operations.md#redis-is-down)).
- Two Services: `<release>-tokenrein` (port 8787, for agents) and `<release>-tokenrein-admin`
  (port 8788, dashboard, admin API and metrics). Expose only the first to agents; put the second
  behind an SSO-protected ingress or keep it internal.
- A ConfigMap from `values.config` (the TOML), mounted at `/etc/tokenrein/tokenrein.toml`.
  Changing it rolls the pods.
- A HorizontalPodAutoscaler on CPU (`autoscaling.enabled`), a PodDisruptionBudget
  (`minAvailable: 1`), an optional ServiceMonitor, and `prometheus.io/*` pod annotations.
- `providerKeys.existingSecret` adds `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` from a Secret.

Rolling updates use `maxUnavailable: 0`. The proxy finishes in-flight requests on SIGTERM; raise
`terminationGracePeriodSeconds` if your agents stream long responses.

## Backups

**Postgres** is the system of record for configuration and the audit log. Back it up like any
other database (managed snapshots or `pg_dump`):

```bash
pg_dump --format=custom --file=tokenrein-$(date +%F).dump "$TOKENREIN_DATABASE_URL"
```

With Compose: `docker compose exec postgres pg_dump -U tokenrein -Fc tokenrein > tokenrein.dump`.

**The usage history folder** holds immutable Parquet files: copy or snapshot it like any other
volume (`rsync`, volume snapshots, or object storage sync). Losing it loses spend reports past
40 days and old run timelines, nothing else.

**`TOKENREIN_SECRETS_KEY`**: store it in your secret manager, apart from the database backups. A database
backup without it still restores every setting, but the provider keys saved in the dashboard are
unreadable and must be entered again.

**Redis** holds live state: spend counters for the current day and month, reservations, run
states and recent activity. Keep AOF or RDB persistence on so a restart loses nothing. If Redis
is lost entirely, Tokenrein keeps working with an empty ledger: the configuration reloads from
Postgres, but budgets count from zero again for the current periods, paused or killed runs are
released, and the dashboard's activity starts over. (Without Postgres, the configuration is in
Redis too and must be restored from a Redis backup.)

## Upgrades

1. Read the [changelog](../CHANGELOG.md) for the version you are moving to.
2. Back up Postgres.
3. Roll out the new image (`helm upgrade` with a new `image.tag`, or change `TOKENREIN_IMAGE`
   and run `docker compose up -d`). Database migrations run automatically when the first new pod
   starts; replicas still running the old version keep serving during the rollout.
4. Watch `/readyz` and the [metrics](operations.md#metrics-and-alerts) until all replicas are on
   the new version (`tokenrein_build_info`).

To roll back to a version without the latest migrations, restore the Postgres backup taken in
step 2: an older binary refuses to start against a schema newer than it knows.
