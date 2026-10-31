# Free tier and license keys

Tokenrein is free to self-host with every guardrail. A license key adds the features larger
teams need. Plans and prices: [tokenrein.com/#pricing](https://tokenrein.com/#pricing).

## What is free

Everything that stops runaway spend, for up to **25 developers** and **$10,000 of AI spend a
month**:

- budgets per run, agent, project, person and org, in Enforce, Check, Ask or Skip mode
- the loop guard, approvals in the dashboard, kill and pause
- alerts to Slack, Microsoft Teams and signed webhooks
- the Claude Code, Codex CLI and Gemini CLI connectors, the SDKs and the proxy
- local accounts with roles, personal API tokens, and the audit log in the dashboard
- spend reports and CSV downloads for the last 90 days, and one org with any number of projects

**Going over never blocks agents.** Admins see a reminder at the top of the dashboard, the org's
alert channels get one message a week (subscribe them to "License reminders" to choose which),
the server logs a warning once a day, and `/metrics` reports `tokenrein_license_developers`,
`tokenrein_license_spend_usd` and `tokenrein_license_over_limit`.

**Developers** are the distinct people on runs in the last 30 days: the `user` the connectors
and SDKs send (`x-tr-user` on the proxy). **Spend** is the cost Tokenrein recorded this calendar
month, across orgs.

## What a key adds

| Feature | Team | Enterprise |
|---|---|---|
| No developer or spend limit (Team is priced per developer) | ✓ | ✓ |
| Single sign-on (OpenID Connect: Okta, Entra ID, Google, any OIDC provider) | ✓ | ✓ |
| Audit log export (CSV, for your SIEM) | ✓ | ✓ |
| Spend reports and exports past 90 days (with `TOKENREIN_HISTORY_DIR`) | ✓ | ✓ |
| PagerDuty alerts | ✓ | ✓ |
| Cursor team-usage import | ✓ | ✓ |
| More than one org | | ✓ |
| Support with agreed response times, security review help, roadmap input | | ✓ |

Without a key these answer `403` with `"code": "license_required"`, the feature and the plan that
includes it. Usage history keeps being written either way, so it is all there once a key is added.

## Adding a key

Either:

- **Dashboard:** an instance admin opens **Keys & limits → License**, pastes the key and saves.
  It is kept with the managed config, so every instance picks it up within seconds, and the change
  is in the audit log (the key itself is not).
- **Environment:** set `TOKENREIN_LICENSE` (or `TOKENREIN_LICENSE_FILE`). With Docker Compose, add
  it to `deploy/.env`; with Helm, put it in a Secret and pass it with `extraEnv` (see
  `values.yaml`). A key saved in the dashboard wins over the environment.

`GET /admin/license` shows the plan, expiry, usage against the limits and which features are on.

Keys are checked offline against a public key built into the server: no call home, so it works
in air-gapped installs. A key that does not verify is ignored and the server runs as free (the
dashboard and the log say why).

## Renewal and expiry

Reminders start 30 days before a key expires. After it expires, paid features keep working for
another **30 days**; then the install is back on the free tier. Nothing is deleted: SSO settings,
extra orgs and their projects stay and keep enforcing budgets, and paid features come back as
soon as a new key is saved. People who signed in with SSO can sign in with a password or the admin
token in the meantime. Write to hello@tokenrein.com to renew.

## For the vendor: issuing keys

Keys are Ed25519-signed JWTs. The private key is not in this repository; keep it in a password
manager. To issue one:

```bash
cargo run --example issue_license -- --key <private-key.pem> --customer "Acme Corp" \
  --plan team --developers 50 --days 365
```

The tool refuses to print a key that the built-in public key (`proxy/src/license_pub.pem`) would
not accept.
