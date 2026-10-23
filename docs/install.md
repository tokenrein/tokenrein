# Installing TokenLens

TokenLens is one stateless binary, `tokenlens-proxy`, plus two stores:

| Component | What it holds | Required |
|---|---|---|
| `tokenlens-proxy` | Nothing on disk. Port **8787**: the proxy and the usage/check API that agents and SDKs call. Port **8788**: the dashboard, admin API, `/metrics`, `/readyz` | Yes |
| Redis | The budget ledger (reservations, spend counters per run, agent, project and org), run state (paused / killed), recent activity for the dashboard | Yes |
| Postgres | Every version of the managed config (orgs, projects, keys, budgets, prices) and the audit log | Recommended. Without it both live in Redis: current config only, last 10,000 audit entries |

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
- URL: `redis://[:password@]host:6379[/db]`, set as `TL_REDIS_URL`.

**Postgres** 17 (what CI and the bundled images use; 14 or later should work).

- One database and a user that owns it. The schema is created and migrated automatically at
  start-up (migrations take a lock, so several replicas starting at once are fine).
- URL: `postgres://user:password@host:5432/db`, set as `TL_DATABASE_URL`. For TLS add
  `?sslmode=verify-full` (certificate checked against the image's system roots) or
  `?sslmode=require` (encrypted, certificate not checked).
- Pool size: `TL_DATABASE_MAX_CONNECTIONS` (default 10 per replica). Postgres is never called
  per request; it is used for config edits, the 2-second config poll and the audit log.

**Secrets** every replica needs the same values of:

| Variable | What | Create with |
|---|---|---|
| `TL_ADMIN_TOKEN` | Admin API and dashboard sign-in, 16+ characters. Without it the admin API is off | `openssl rand -hex 24` |
| `TL_SECRETS_KEY` | 64 hex characters; encrypts provider API keys saved in the dashboard. **Back it up**: without it those keys cannot be decrypted | `openssl rand -hex 32` |
| `TL_DATABASE_URL` | Postgres URL (above) | |
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | Optional. Used for every org that has no key saved in the dashboard | |

Any variable can instead be read from a file: set `TL_ADMIN_TOKEN_FILE=/run/secrets/admin_token`
(and so on). If both are set, the variable wins. A trailing newline in the file is ignored.

## Configuration

The image ships a config at `/etc/tokenlens/tokenlens.toml` ([deploy/tokenlens.toml](../deploy/tokenlens.toml))
that is enough for most installs: listen on all interfaces, OpenAI and Anthropic upstreams, keys
and Postgres URL from the variables above. These override the file from the environment:

| Variable | Overrides | Default in the image |
|---|---|---|
| `TL_LISTEN` | `server.listen` | `0.0.0.0:8787` |
| `TL_ADMIN_LISTEN` | `server.admin_listen` | `0.0.0.0:8788` |
| `TL_REDIS_URL` | `redis.url` | `redis://127.0.0.1:6379` |
| `TL_DATABASE_MAX_CONNECTIONS` | `database.max_connections` | `10` |

For anything else (provider base URLs, upstream timeout, the Redis call timeout `[redis]
timeout_ms`), mount your own TOML file and point `TL_CONFIG` at it. Orgs, projects, keys and
budgets are not in the file: create them in the dashboard. (A `[[orgs]]` section in the file
seeds a fresh install once; after that the database is the source of truth.)

`RUST_LOG` sets the log level (`info` by default). Logs are JSON on stderr; stdout carries one
JSON event per request for an analytics pipeline.

## Docker Compose

`deploy/docker-compose.yml` runs TokenLens, Redis 7.4 and Postgres 17 on one host, with
persistent volumes and healthchecks. It works with Docker Compose and with `podman compose`.

```bash
cd deploy
cp .env.example .env
```

Fill in `.env`: `TL_ADMIN_TOKEN`, `TL_SECRETS_KEY`, `POSTGRES_PASSWORD`, and `TL_DATABASE_URL`
with the same password (letters and digits keep it URL-safe; `openssl rand -hex 16`). Then:

```bash
docker compose up -d
```

- The proxy is on port 8787 (`TL_PROXY_PORT`); the dashboard is on `http://127.0.0.1:8788/`
  (`TL_ADMIN_PORT`), bound to localhost only. Put it behind your SSO proxy or VPN rather than
  publishing it.
- `docker compose ps` shows `healthy` once both listeners answer and `/readyz` reaches Redis
  and Postgres.
- The image comes from `ghcr.io/geektuhin123/tokenlens:latest`; pin a version with
  `TOKENLENS_IMAGE=ghcr.io/geektuhin123/tokenlens:0.1.0` in `.env`. `docker compose build`
  builds it from this checkout instead.
- To keep secrets out of the environment, use Compose `secrets:` and the `*_FILE` variables
  (for example `TL_SECRETS_KEY_FILE: /run/secrets/tl_secrets_key`).

## Kubernetes (Helm)

```bash
helm install tokenlens deploy/helm/tokenlens -n tokenlens --create-namespace
```

With the defaults this runs 2 proxy replicas plus a single-pod Redis and Postgres for
evaluation, and generates the admin token, secrets key and Postgres password into a Secret
named after the release (kept on uninstall, reused on upgrade). `helm status` prints how to reach
the dashboard and read the admin token.

For production, bring your own Secret and managed Redis and Postgres:

```bash
kubectl -n tokenlens create secret generic tokenlens-secrets \
  --from-literal=TL_ADMIN_TOKEN="$(openssl rand -hex 24)" \
  --from-literal=TL_SECRETS_KEY="$(openssl rand -hex 32)" \
  --from-literal=TL_DATABASE_URL='postgres://tokenlens:...@db.internal:5432/tokenlens?sslmode=verify-full'
kubectl -n tokenlens create secret generic tokenlens-redis \
  --from-literal=TL_REDIS_URL='redis://:...@redis.internal:6379'
```

```yaml
# values-prod.yaml
image:
  tag: "0.1.0"
secrets:
  existingSecret: tokenlens-secrets
  mountAsFiles: true          # TL_*_FILE instead of environment variables
redis:
  enabled: false
externalRedis:
  existingSecret: tokenlens-redis
postgresql:
  enabled: false
externalDatabase:
  enabled: true               # TL_DATABASE_URL comes from the Secret
autoscaling:
  enabled: true
  minReplicas: 3
  maxReplicas: 20
metrics:
  serviceMonitor:
    enabled: true             # needs the Prometheus Operator
```

```bash
helm upgrade --install tokenlens deploy/helm/tokenlens -n tokenlens -f values-prod.yaml
```

What the chart creates:

- A Deployment (non-root, read-only root filesystem, no capabilities, no service account token)
  with three probes. **Startup**: `/readyz`, so a new pod only takes traffic once it has reached
  Redis and Postgres. **Liveness and readiness**: `/healthz`, deliberately not `/readyz`, so a
  Redis or Postgres outage does not pull every pod out of the Service. That would refuse traffic
  even for orgs that fail open (see [operations](operations.md#redis-is-down)).
- Two Services: `<release>-tokenlens` (port 8787, for agents) and `<release>-tokenlens-admin`
  (port 8788, dashboard, admin API and metrics). Expose only the first to agents; put the second
  behind an SSO-protected ingress or keep it internal.
- A ConfigMap from `values.config` (the TOML), mounted at `/etc/tokenlens/tokenlens.toml`.
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
pg_dump --format=custom --file=tokenlens-$(date +%F).dump "$TL_DATABASE_URL"
```

With Compose: `docker compose exec postgres pg_dump -U tokenlens -Fc tokenlens > tokenlens.dump`.

**`TL_SECRETS_KEY`**: store it in your secret manager, apart from the database backups. A database
backup without it still restores every setting, but the provider keys saved in the dashboard are
unreadable and must be entered again.

**Redis** holds live state: spend counters for the current day and month, reservations, run
states and recent activity. Keep AOF or RDB persistence on so a restart loses nothing. If Redis
is lost entirely, TokenLens keeps working with an empty ledger: the configuration reloads from
Postgres, but budgets count from zero again for the current periods, paused or killed runs are
released, and the dashboard's activity starts over. (Without Postgres, the configuration is in
Redis too and must be restored from a Redis backup.)

## Upgrades

1. Read the [changelog](../CHANGELOG.md) for the version you are moving to.
2. Back up Postgres.
3. Roll out the new image (`helm upgrade` with a new `image.tag`, or change `TOKENLENS_IMAGE`
   and run `docker compose up -d`). Database migrations run automatically when the first new pod
   starts; replicas still running the old version keep serving during the rollout.
4. Watch `/readyz` and the [metrics](operations.md#metrics-and-alerts) until all replicas are on
   the new version (`tokenlens_build_info`).

To roll back to a version without the latest migrations, restore the Postgres backup taken in
step 2: an older binary refuses to start against a schema newer than it knows.
