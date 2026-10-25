# Tokenrein — Runtime spend guardrails for AI agents

Implementation plan + go-to-market. Working name "Tokenrein" — rename before
launch (run a trademark + domain check first).

**One-line product:** a drop-in proxy that enforces per-run budgets and kills
runaway agent loops *before the next paid call*, instead of alerting after the
bill arrives.

---

## 1. What we learned from Turbot Guardrails

Turbot Guardrails is enterprise cloud governance: it watches AWS/Azure/GCP/OCI
(and GitHub) resources and auto-remediates misconfigurations. It is not a
competitor in the request path, but its product model is the best-tested
"guardrails" design on the market, and we should borrow it deliberately.

| Turbot concept | How it works there | What we borrow |
|---|---|---|
| **Policy hierarchy with inheritance** | Policies set at a higher level (Turbot → folder → account → resource) flow down to everything below | Budgets/policies set at **org → project → environment → agent → run**, inherited and overridable |
| **Modes: Skip / Check / Enforce** | Check raises an *alarm* only; Enforce remediates; policy packs **default to Check** | Every policy has a mode. New customers start in **Check (shadow mode)** — zero risk of breaking prod. This becomes our main conversion lever (see §9) |
| **Control states** | OK / Alarm / Error / Invalid / Skipped / TBD; "Resource State + Policy Setting = Control State" | Run states: `ok` / `alarm` / `throttled` / `paused` / `killed` / `error` — same clarity in the UI |
| **Policy packs** | Curated bundles of policies, attached to a level of the hierarchy | **Guardrail packs**: "Production agent baseline", "Background batch jobs", "Internal coding agents" |
| **Policy simulation before rollout** | Test a policy before org-wide enforcement | **Replay simulation**: run a proposed policy against the last 30 days of events and show what *would* have been blocked |
| **Natural-language policies** | Describe a guardrail in plain English | Phase 3: "pause any run that spends more than $20 or repeats itself 5 times" → policy |
| **Event handlers** | Pipe cloud events into the engine in real time | Our proxy *is* the event source — we see every call synchronously, which Turbot can't |

**Where Turbot stops (our gap):**
- Its AI controls are **configuration-level**: which Bedrock/Vertex/OpenAI
  models and endpoints are allowed. It can't see an individual agent run, its
  cost, or a loop. Guardian (coding-agent EDR, early preview) *meters* spend per
  turn but does **not enforce budgets or detect loops**, and only covers
  laptop coding agents with hooks.
- **Pricing is enterprise-only:** $0.05/control/month on Cloud with control
  packs from **$25,000**; self-hosted from **$50,000**. A 20-person AI startup
  can't buy it. That is our segment.
- **Complement, not just competitor:** Turbot's new per-provider *endpoint*
  policies can force all OpenAI/Anthropic/Bedrock traffic through a custom
  endpoint. That endpoint can be us — a partnership/integration story for
  enterprise later.

**Turbot GTM patterns worth copying:** open-source distribution (Steampipe and
its plugins built the brand before Guardrails sold), quarterly **launch weeks**,
a public Slack community, a public "hub" of policies, SOC 2 Type II, AWS
Marketplace billing, support sold as a % of usage, and booths at buyer events
(fwd:cloudsec, Black Hat, Ai4).

## 2. Competitive map (Oct 2026)

| Product | Enforces | Gap we exploit |
|---|---|---|
| OpenAI/Anthropic native limits | Org/project monthly caps | Blunt, single provider, no run/loop awareness |
| AWS Bedrock | No default hard cap | Customers must wire their own |
| Cloudflare AI Gateway | $ limits per model/provider/metadata, 429 | **Eventually consistent** — concurrent bursts overshoot |
| Portkey | USD/token budget per virtual key, expires key | Key-level, not run-level; not retroactive |
| LiteLLM (OSS) | Global/team/key budgets, RPM/TPM | Flat caps; tiers need enterprise license; you run it |
| **Waxell** | Agent-level budgets, pre-execution, throttling | **Closest competitor, free in beta.** We must win on depth (loop detection, simulation, pause/approve) and speed |
| Turbot Guardrails / Guardian | Cloud config; coding-agent action policies | No request-path spend enforcement; $25k+ minimum |

**Our positioning:** the only product that combines (1) budgets per agent
*run*, (2) behavior-based loop detection that trips before the budget,
(3) strictly consistent reservations (no burst overshoot), (4) pause → human
approve/raise/kill, and (5) shadow mode + replay simulation so customers can
adopt without risk.

## 3. Product definition

**Ideal customer (ICP):** engineering lead or founder at a 10–200 person
company running LLM agents *in production on servers* (LangChain, LangGraph,
CrewAI, OpenAI Agents SDK, Pydantic AI, Vercel AI SDK, or custom) with
$2k–$100k/month LLM spend. Secondary: platform teams at mid-market companies
rolling out internal agents who need a spend control to pass review.

**Not our first market:** coding agents on laptops (Turbot Guardian, agent
vendors' own limits, hook tools are converging there).

**Core promise:** "Two lines to install. Start in shadow mode. When an agent
loops, we stop it before the next call — and ask you in Slack whether to
resume."

**Privacy promise:** we do not store prompts or completions by default — only
token counts, costs, and one-way hashes used for loop detection. This removes
the main security-review objection that observability tools face.

### MVP scope (must ship for paid launch)
- OpenAI-compatible (`/v1/chat/completions`, `/v1/responses`) and Anthropic
  (`/v1/messages`) endpoints, streaming and non-streaming, tool calls.
- Virtual keys; customer provider keys stored encrypted.
- Hierarchical budgets: run, agent (daily), project (monthly), org (monthly).
- Modes: Skip / Check / Enforce per policy.
- Loop detectors: identical-call repeats, spend velocity, run structure
  (depth/fan-out/call count/wall clock), retry storms.
- Actions: warn → throttle → pause → kill.
- Slack + email alerts with Resume / Raise budget / Kill buttons.
- Dashboard: live runs, trips, spend by project/agent/model, shadow-mode
  "would have saved" report, policy editor, replay simulation.
- Python + TypeScript SDKs (set headers, run context, local fallback cap).
- Stripe billing, self-serve signup, docs site, status page.

### Explicitly later (Phase 3)
Bedrock/Azure/Vertex/Gemini, paid tool-call budgets (MCP/HTTP tools), SimHash
near-duplicate and no-progress detectors, per-agent learned baselines,
policy-as-code/Terraform provider, SSO/SAML, EU region, BYOC data plane,
SOC 2, AWS Marketplace, natural-language policies.

## 4. Concept model

```
Org ──▶ Project ──▶ Environment (prod/staging) ──▶ Agent ──▶ Run ──▶ Call
            policies inherit downward; a lower level may override
```

- **Run** = one task execution. Identified by `X-TR-Run-Id` (SDK sets it);
  sub-agents send `X-TR-Parent-Run-Id`, forming a run tree. Budgets on a run
  cover its whole subtree.
- **Policy** = `{scope, kind, mode, params, actions}`. Kinds:
  `budget.run`, `budget.agent.daily`, `budget.project.monthly`,
  `budget.org.monthly`, `loop.identical`, `loop.velocity`, `loop.structure`,
  `loop.retry_storm`.
- **Trip** = a policy firing on a run. Records the mode, action taken, spend
  at trip time, and a "would have saved" estimate (spend that continued after
  the trip in Check mode).
- **Pack** = named bundle of policies attachable at any scope.

## 5. Architecture

```
 Agent (any framework, any language)
   │  base_url → https://proxy.tokenrein.dev   api_key → tr_live_… (virtual key)
   │  headers: X-TR-Run-Id, X-TR-Parent-Run-Id, X-TR-Agent, X-TR-Project
   ▼
┌───────────────────── DATA PLANE (Rust, stateless, multi-AZ) ───────────────────┐
│ 1 authenticate virtual key  → org config from in-memory cache (pushed on change)│
│ 2 normalise request, compute fingerprint, estimate worst-case cost             │
│ 3 RESERVE across run-tree/agent/project/org  ───────────▶  Valkey/Redis (Lua)   │
│   + loop checks on the run's recent fingerprints          (one round trip)     │
│ 4 decision: allow | throttle | pause | kill  (Check mode = allow + record trip)│
│ 5 decrypt provider key (KMS, cached) → forward upstream, stream back to client │
│ 6 parse usage → SETTLE actual cost, release surplus                            │
│ 7 emit event (async, never blocks the request)  ──▶ SQS ──▶ ingest worker      │
└───────────────────────────────────────────────────────────────────────────────┘
        │ events                                        ▲ config push / pause state
        ▼                                               │
  ClickHouse (events, analytics,          CONTROL PLANE (TypeScript / Next.js)
  replay simulation)                      Postgres: orgs, users, keys, policies,
  Detector (Rust): slower detectors       trips, price catalog · Stripe billing ·
  over windows, baselines                 Slack app · email · dashboard · public API
```

### 5.1 Request lifecycle details

**Cost estimate (pre-call).** `input_tokens` from a deliberately pessimistic
byte count (~2.5 UTF-8 bytes per token; images counted as a fixed 1,600
tokens; a real tokenizer such as tiktoken-rs is a later refinement) × input
price + `max_tokens` × output price. If the request has no
`max_tokens`, the proxy injects the policy default (e.g. 4,096) — otherwise the
worst case is unbounded and reservation is meaningless.

**Money is integer micro-USD everywhere** (no float drift in Redis).

**Reserve (atomic, all-or-nothing).** One Lua script, all keys for an org share
the `{org_id}` hash tag so they live on one cluster slot:

```lua
-- KEYS: budget hashes for each scope in the run tree + agent + project + org
-- ARGV: request_id, estimate_micros, ttl_s, and per-key limit/mode
for i, key in ipairs(KEYS) do
  local spent    = tonumber(redis.call('HGET', key, 'spent') or 0)
  local reserved = tonumber(redis.call('HGET', key, 'reserved') or 0)
  if mode[i] == 'enforce' and spent + reserved + est > limit[i] then
    return {'blocked', key}            -- nothing has been reserved yet
  end
end
for _, key in ipairs(KEYS) do redis.call('HINCRBY', key, 'reserved', est) end
redis.call('HSET', resKey, ...)                       -- reservation record
redis.call('ZADD', 'resexp:{org}', now + ttl, request_id)  -- for the sweeper
return {'ok'}
```

**Settle.** On response: `reserved -= estimate; spent += actual` on every key,
delete the reservation, `ZREM` from expiry set. Idempotent by `request_id`.

**Crash safety.** If a proxy dies mid-request, the reservation expires; a
sweeper releases expired reservations (`ZRANGEBYSCORE resexp:{org} 0 now`).
Late settlement after expiry still records `spent` so money is never lost.

**Streaming.** Proxy tees the SSE stream. For OpenAI it injects
`stream_options.include_usage=true`; for Anthropic it reads `message_delta`
usage. It counts output tokens as chunks pass; if the run's remaining budget
reaches zero mid-stream in Enforce mode, it ends the stream with a
provider-shaped error event.

**Responses that block must not trigger SDK auto-retries.** OpenAI and
Anthropic SDKs retry 429s automatically. Use **402** (budget exhausted) and
**403** (run paused/killed) with a provider-shaped JSON error body
(`type: "tokenrein_budget_exceeded" | "tokenrein_run_paused"`, `run_id`,
`resume_url`) so SDKs raise a clean exception. Throttle = server-side delay,
not an error.

### 5.2 Loop detection

All per-run state lives in Redis next to the budget keys, checked in the same
Lua call where possible.

| Detector | Signal | Default trip |
|---|---|---|
| `loop.identical` | fingerprint = sha256 of normalised last 3 messages + tool-call names/args (sorted) | same fingerprint ≥5× in 10 min within one run |
| `loop.velocity` | run/agent spend per minute | > $2/min absolute, or > 5× the agent's 7-day p95 (Phase 3) |
| `loop.structure` | run-tree depth, children per run, calls per run, wall clock | depth > 5, children > 20, calls > 200, wall clock > 60 min |
| `loop.retry_storm` | same fingerprint following upstream 4xx/5xx | > 8 in 5 min |
| `loop.no_progress` (Phase 3) | tool results unchanged across iterations while context grows | 4 consecutive iterations |
| `loop.near_duplicate` (Phase 3) | SimHash of last user/tool message, Hamming ≤ 3 | ≥6× in 10 min |

Action ladder per policy: **warn** (response header `X-TR-Warning` + alert) →
**throttle** (token bucket delay) → **pause** (all calls in the run tree get 403
until a human resumes) → **kill** (permanent 403 for that run).

### 5.3 Failure modes

| Failure | Behaviour |
|---|---|
| Redis primary fails | Multi-AZ replica promotes (~seconds). During the gap, the org's **fail mode** applies: *fail-open* (allow; SDK local cap + in-process proxy counters act as degraded guard; events spooled) or *fail-closed* (503). Default: fail-open. |
| Proxy instance dies | Clients retry to another instance; reservations expire and are swept |
| Upstream provider error | Pass through unchanged; release reservation (settle 0 unless partial stream) |
| Control plane down | Data plane keeps running on cached config; pause/resume queued |
| Price unknown for a model | Use the most expensive known price in that provider family + raise an internal alert |
| Whole proxy region down | Status page + SDK fallback: SDK can switch to direct provider calls with the local cap if the customer opts in |

### 5.4 Data model

**Postgres (control plane)**
- `orgs(id, name, plan, stripe_customer_id, fail_mode, created_at)`
- `users`, `memberships(org_id, user_id, role)`
- `projects(id, org_id, name, environment)`
- `provider_credentials(id, org_id, provider, ciphertext, kms_key_id, created_at, rotated_at)`
- `virtual_keys(id, org_id, project_id, prefix, hash, name, created_at, revoked_at)`
- `policies(id, org_id, scope_type, scope_id, kind, mode, params jsonb, actions jsonb, pack_id, version, updated_by, updated_at)`
- `packs(id, name, policies jsonb)`
- `agents(id, project_id, name, first_seen_at)` — auto-registered from headers
- `trips(id, org_id, run_id, agent_id, policy_id, kind, mode, action, spend_at_trip_micros, would_have_saved_micros, state, resolved_by, resolution, created_at, resolved_at)`
- `price_catalog(provider, model, input_micros_per_mtok, output_micros_per_mtok, cached_input_micros_per_mtok, effective_from)`
- `audit_log(id, org_id, actor, action, target, diff jsonb, created_at)`

**ClickHouse (events)** — `request_events(ts, org_id, project_id, agent, run_id,
parent_run_id, request_id, provider, model, input_tokens, output_tokens,
cached_tokens, reserved_micros, actual_micros, overhead_ms, upstream_ms,
upstream_status, decision, fingerprint, trip_policy_ids)`, partitioned by
month, TTL by plan.

**Redis** — `bud:{org}:<scope>:<id>[:<period>]` hashes (`limit`, `spent`,
`reserved`), `res:{org}:<request_id>`, `resexp:{org}` zset,
`fp:{org}:run:<run_id>` (recent fingerprints), `state:{org}:run:<run_id>`.

### 5.5 Security

- Provider keys: envelope encryption with AWS KMS; plaintext only in proxy
  memory with a short TTL cache; never logged.
- Virtual keys: shown once, stored as SHA-256 hash with lookup prefix.
- Tenant isolation: every Redis key and query is scoped by `org_id`; tests
  that try cross-tenant access are part of CI.
- No prompt/completion storage by default; opt-in payload sampling later.
- Audit log for policy changes and every resume/raise/kill decision.
- TLS 1.2+ everywhere; WAF + AWS Shield Standard on the load balancer.
- Pen test before public launch; SOC 2 Type I in Phase 3 (Vanta/Drata).

### 5.6 Infrastructure

- **Region:** AWS us-east-1 first (close to OpenAI/Anthropic API edges); EU
  (eu-west-1) in Phase 3 for data residency.
- **Data plane:** ECS Fargate, ≥3 tasks across 3 AZs, autoscale on CPU and
  in-flight requests; ALB with idle timeout ≥ 15 min for long streams.
- **Ledger:** ElastiCache Valkey, cluster mode, Multi-AZ with auto-failover.
- **Control plane:** Next.js on ECS (or Vercel) + RDS Postgres Multi-AZ.
- **Events:** SQS → Rust ingest worker → ClickHouse Cloud.
- **IaC:** Terraform for everything; separate staging and prod accounts.
- **Observability:** OpenTelemetry traces, Prometheus metrics (overhead
  histogram, ledger latency, decisions/sec, trips), Grafana, PagerDuty,
  public status page (Better Stack or Instatus).

### 5.7 SDKs and install

Python and TypeScript, open source (Apache-2.0):

```python
from tokenrein import guard
client = guard.openai(OpenAI(), agent="support-bot")   # sets base_url + key
with guard.run("ticket-4812", budget_usd=5):            # run id + run budget
    agent.invoke(...)
```

Also documented: raw `base_url` + headers (any language, no SDK), LangChain /
LangGraph callback, CrewAI, OpenAI Agents SDK, Vercel AI SDK, Pydantic AI,
LiteLLM upstream config. The SDK includes a **local fallback guard** (in-process
call/spend cap) used when the proxy is unreachable and fail-open is on.

## 6. Production-readiness checklist (gate for public launch)

**SLOs**
- Data-plane availability ≥ 99.95% / month.
- Added latency (excluding upstream) p50 < 3 ms, p99 < 10 ms.
- Ledger correctness: **zero overshoot** beyond in-flight reservations.

**Testing**
- Unit tests for estimator, fingerprinting, detectors, price math.
- Ledger property test: 1,000 concurrent requests against a budget → total
  `spent + reserved` never exceeds the limit; repeated in CI.
- Provider contract tests against recorded fixtures (streaming, tool calls,
  errors, usage fields) + nightly live tests against real APIs with $1 budgets.
- Load test (k6) at 2,000 req/s sustained with streaming mix.
- Chaos: kill Redis primary, kill proxy mid-stream, drop SQS, slow upstream.
- Security: cross-tenant access tests, key-leak scanning in logs, dependency
  scanning (cargo audit, npm audit), pen test.

**Operations**
- Runbooks: Redis failover, provider outage, price catalog error, stuck pause.
- On-call rotation + PagerDuty alerts on SLO burn.
- Backups: RDS PITR; ClickHouse backups; restore drill done once before GA.
- Price catalog: daily automated diff against provider pricing pages + manual
  approval; alert when an unknown model is seen.

**Business/legal**
- Terms of Service with clear limitation of liability (we reduce risk; we
  don't guarantee prevention), Privacy Policy, DPA template, sub-processor list.
- Stripe billing live with tax; invoices; dunning.
- Support: shared Slack Connect for paid plans; email for free.

## 7. Phased implementation

Durations below are estimates for one full-time builder plus AI help; adjust
after Phase 1.

### Phase 0 — Validate (weeks 0–2, runs alongside Phase 1)
- 15+ interviews with teams running agents in production (see §9 outreach).
- Waitlist landing page live.
- **Exit:** ≥3 design partners committed to install in Check mode.

### Phase 1 — Private beta core (weeks 1–6)
1. Monorepo + CI (GitHub Actions), Terraform staging env.
2. Rust proxy: OpenAI chat/responses + Anthropic messages, streaming, tool calls.
3. Virtual keys + KMS-encrypted provider keys.
4. Redis ledger (reserve/settle/sweeper) + property tests.
5. Policies with Skip/Check/Enforce + inheritance resolver; packs.
6. Detectors: identical, velocity (absolute), structure, retry storm.
7. Event pipeline → ClickHouse.
8. Control plane: signup, orgs, keys, policies, live runs, trips.
9. Slack app + email with Resume / Raise / Kill.
10. Python + TS SDKs; quickstart docs.
- **Exit:** design partners running Check mode in prod for 2 weeks; p99
  overhead < 10 ms; ledger property test green; zero data-loss incidents.

### Phase 2 — Production hardening + public launch (weeks 7–10)
1. Multi-AZ prod environment; failure-mode handling (§5.3) verified by chaos tests.
2. Load tests at target; SLO dashboards; on-call + status page.
3. Stripe plans + metered guarded spend; upgrade/downgrade; dunning.
4. Shadow-mode "would have saved" report (dashboard + weekly email).
5. Replay simulation over the last 30 days.
6. Docs site (Mintlify/Docusaurus): quickstarts per framework, API reference.
7. Onboarding ≤ 5 minutes from signup to first guarded call.
8. Pen test, ToS/Privacy/DPA, security page.
- **Exit:** 10 orgs in production, ≥3 paying, 30 days at ≥99.95% before GA.

### Phase 3 — Expansion (months 3–6)
Bedrock (SigV4 re-signing), Azure OpenAI, Vertex/Gemini, OpenRouter; tool-call
budgets (MCP + HTTP tools via SDK); no-progress + near-duplicate detectors;
learned per-agent baselines; policy-as-code (YAML in git + Terraform
provider); SSO/SAML; SOC 2 Type I; EU region; BYOC data plane; AWS
Marketplace; natural-language policies; Turbot endpoint-policy integration.

### Repository layout

```
tokenrein/
  proxy/           Rust data plane
  detector/        Rust window detectors + ingest worker
  control-plane/   Next.js dashboard + API + Stripe (evolves from gateway/)
  sdk/python/      Apache-2.0
  sdk/typescript/  Apache-2.0
  infra/terraform/
  docs/            docs site source
```

**What happens to the current scaffold:** the self-hosted Langfuse stack is
retired. `gateway/` (Stripe checkout, plans, landing page) moves into
`control-plane/`.

## 8. Pricing and packaging (hypothesis — validate in Phase 0)

Priced on **guarded spend** (LLM spend that passes through the proxy), which
scales with the value at risk.

| Plan | Price | Guarded spend / month | Includes |
|---|---|---|---|
| Free | $0 | up to $1,000 | All detectors, Check + Enforce, 1 project, 7-day history, Slack alerts |
| Team | $99/mo | up to $25,000 | Unlimited projects, 30-day history, replay simulation, approvals, email support |
| Business | $499/mo | up to $200,000 | 1-year history, SSO, audit export, 99.95% SLA, Slack Connect support |
| Enterprise | Custom | unlimited | BYOC data plane, DPA, SOC 2 report, custom retention, support tiers |

Overage: 0.25% of guarded spend above the plan limit. Annual plans: 2 months
free. Reference points: Turbot starts at $25,000; Portkey $49; Waxell free in
beta — our Free tier must be genuinely useful to compete with "free".

## 9. Marketing strategy

### 9.1 Positioning and message

- **Headline:** "Stop runaway AI agents before the next call."
- **Sub:** "Per-run budgets and loop detection for production agents. Two lines
  to install. Start in shadow mode — see what we would have stopped."
- **Four message pillars** (use in every post):
  1. *Alerts tell you after. We stop it before.* (enforcement vs alerting)
  2. *Zero-risk adoption.* (shadow mode + replay simulation)
  3. *We never store your prompts.* (privacy)
  4. *Any framework, any language, two lines.* (ease)
- **Proof asset:** a 60-second video — an agent starts looping, spend ticks up,
  Tokenrein pauses it, Slack asks "Resume / Raise / Kill", one click.

### 9.2 Distribution engine (the Turbot playbook, scaled down)
1. **Open source the SDK + local guard** (Apache-2.0): useful on its own
   (in-process loop and spend caps), and it leads to the hosted proxy for
   consistent cross-process budgets, approvals, and history.
2. **Content that teaches** the problem (loop patterns, enforcement vs
   alerts) — engineers share education, not ads.
3. **Integrations as distribution:** every framework docs page that lists us is
   permanent inbound traffic.
4. **Shadow-mode report as the sales rep:** "In the last 14 days we would have
   stopped 3 runaway runs ($2,140)." Upgrade prompts and founder emails are
   triggered by this number.
5. **Launch weeks every quarter** once there is a steady feature cadence.

### 9.3 Step-by-step calendar — where and what to post

**Rules for every community post:** read the subreddit/Discord rules first,
lead with the technical substance, disclose you're the builder, link at most
once, and reply to every comment. Don't post the same text in multiple places.

#### Pre-launch (build weeks 1–6)

| Week | Where | What to post / do | Goal |
|---|---|---|---|
| 1 | Domain, landing page, X + LinkedIn + GitHub org | Waitlist page with headline, 60-sec loop explainer, and a **"Runaway cost calculator"** (calls/min × model × hours unnoticed = $) | Waitlist capture |
| 1 | LinkedIn (founder profile) | Story post: "A $47k agent bill and why budget alerts aren't budget enforcement" — cite the public incident, end with a question, no link in body (link in first comment) | Reach engineering leaders |
| 1 | X | Same idea as a 6-tweet thread with a diagram of "alert vs enforce" timelines | Reach AI engineers |
| 1–2 | X, Reddit, LinkedIn search | Find 50 people who posted about runaway agent bills ("agent bill", "API bill overnight", "infinite loop tokens"); send a personal DM asking for a 20-min call — no pitch | 15 interviews, design partners |
| 2 | Own blog (canonical) → cross-post dev.to + Hashnode | "Anatomy of a runaway agent: 6 loop patterns and how to detect each" (deep technical, code snippets, CTA = waitlist in footer) | SEO + credibility |
| 2 | r/AI_Agents, r/LangChain | Discussion post: "How are you capping spend per agent run in production?" — genuine question, share what you've learned, no link | Pain research + karma |
| 2–6 | Hacker News | Don't submit yet. Comment helpfully on threads about agent costs/reliability | Reputation |
| 3 | GitHub | Release OSS SDK v0.1 (local guard) with README GIF; submit PRs to awesome-ai-agents, awesome-llmops, awesome-langchain lists | Stars, distribution |
| 3 | X + LinkedIn | "I open-sourced a tiny library that kills agent loops in-process" + GIF | Stars |
| 3 | Own blog → dev.to | Build-in-public: "Atomic budget reservations across 4 nested scopes in one Redis round trip" | Engineer credibility |
| 4 | LangChain, LlamaIndex, CrewAI, Pydantic AI Discords (#showcase) | Short showcase of the OSS lib with one code example | Framework users |
| 4 | r/LangChain (or r/Python if allowed) | "I built an in-process loop guard for agents — feedback wanted" | Feedback + stars |
| 4–6 | Email + calls | Onboard 3–5 design partners in Check mode; weekly check-ins | Real data |
| 5 | Newsletters | Submit OSS lib to Console.dev, TLDR AI, Ben's Bites, Python Weekly, JavaScript Weekly | Reach |
| 5–6 | Own blog | Design-partner case study (with written permission): "Shadow mode found N loops in 14 days" | Social proof |
| 6 | Prep | Record demo video, screenshots, Show HN text, Product Hunt page, launch email | Launch readiness |

#### Launch (weeks 7–10)

| Week | Where | What to post / do | Goal |
|---|---|---|---|
| 7 (Tue/Wed, 8–9am PT) | **Hacker News** | `Show HN: Tokenrein – per-run budgets and a kill switch for looping AI agents`. First comment: why a proxy, how the ledger avoids overshoot, what's open source, honest limits, free tier. Stay in comments all day | Biggest single spike |
| 7 (same day) | X, LinkedIn, waitlist email | "We're live" thread + demo video; email waitlist with free-tier link | Convert waitlist |
| 8 (Tue, 12:01am PT) | **Product Hunt** | Launch page with demo video, 3 screenshots, maker comment telling the $47k story; tell your network you launched (ask for feedback, not votes) | Second spike, backlinks |
| 8 | r/AI_Agents, r/LangChain, r/SaaS, r/OpenAI, r/ClaudeAI | One tailored post per sub (respect showcase flair/days): technical for AI subs, founder story for r/SaaS | Targeted signups |
| 9 | Own site | Honest comparison pages: vs Portkey budget limits, vs Cloudflare AI Gateway spend limits, vs LiteLLM budgets, vs Waxell | Capture evaluators' searches |
| 9 | Framework docs (PRs) | Integration pages/examples in LangChain, LangGraph, CrewAI, LlamaIndex, Vercel AI SDK, Pydantic AI, OpenAI Agents SDK, LiteLLM | Permanent inbound |
| 10 | Blog + LinkedIn + press pitch | First **"Runaway Report"**: anonymised aggregate beta data (loops caught, median cost, top patterns). Pitch to The New Stack, InfoQ, and AI newsletters | Earned media |

#### Ongoing (month 3+)

| Cadence | Where | What |
|---|---|---|
| Weekly | Blog → dev.to | One search-intent post: "OpenAI API spending limit per user", "Anthropic API budget cap", "LangChain agent infinite loop", "AWS Bedrock spending limit", "Claude API cost control" |
| Weekly | X (3 posts), LinkedIn (2 posts) | Loop patterns seen (anonymised), product updates, short demos |
| Weekly | Reddit, Discords, HN | Answer 5 real questions about agent cost/reliability; link only when directly relevant |
| Monthly | Blog + newsletter + LinkedIn | Runaway Report |
| Quarterly | All channels | Launch week (one feature per day, wrap-up post) |
| Per event | AI Engineer World's Fair / Summit, local AI meetups, Ai4 | CFP talk: "Anatomy of a $47k agent loop"; demo at meetups |
| After product–market signals | Google Ads, TLDR AI sponsorship, Reddit ads (r/LangChain) | Only on high-intent keywords once free→paid conversion is known |

### 9.4 Sales motion
- **Self-serve** for Free/Team.
- **Founder-led** for Business/Enterprise: trigger when a shadow report shows
  > $500 would-have-saved in 14 days → personal email offering a 20-min review.
- **Lifecycle emails:** Day 0 welcome + 2-line install → Day 1 "first guarded
  call?" (or help offer) → Day 7 shadow report → Day 14 shadow report +
  upgrade → instant email on every trip.
- **Partners (Phase 3):** AI development agencies (20% rev share), LLM
  consultancies, and Turbot endpoint-policy integration for enterprise.

### 9.5 Targets (goals to steer by, not predictions)

| Milestone | Target |
|---|---|
| End of pre-launch | 500 waitlist, 300 GitHub stars, 5 design partners |
| 30 days after launch | 150 signups, 50 orgs with guarded traffic |
| 60 days after launch | 10 paying orgs |
| 6 months | $10k MRR, SOC 2 Type I started |

Track weekly: signups → first guarded call (activation) → first trip seen →
paid. Activation (first guarded call within 24h) is the number to fix first.

## 10. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Waxell (free) or Portkey/Cloudflare add run budgets + loop detection | Ship depth fast (simulation, approvals, structure detectors); OSS SDK for distribution; be the specialist |
| OpenAI/Anthropic ship native per-project/run limits | Multi-provider in one place, loop detection, approvals — things a single provider won't do |
| We're in the request path: our outage = customer outage | Fail-open default, multi-AZ, SDK fallback, public SLOs, chaos tests |
| Customers won't hand us provider keys | No prompt storage, SOC 2, BYOC data plane for enterprise |
| Wrong prices → wrong enforcement | Automated catalog diff + unknown-model alert + conservative fallback price |
| Liability when a loop isn't caught | ToS limitation of liability; position as risk reduction, not insurance |
| "Free" is good enough (LiteLLM caps) | Shadow report shows value flat caps miss (loops caught well before the cap) |

## Sources

- Turbot Guardrails pricing — https://turbot.com/guardrails/pricing
- Turbot Guardrails controls concepts — https://turbot.com/guardrails/docs/concepts/controls
- Turbot glossary (policy inheritance, Check/Enforce) — https://turbot.com/guardrails/docs/reference/glossary
- Turbot Guardrails Hub policy packs (default Check mode) — https://hub.guardrails.turbot.com/policy-packs/aws_guardrails_enable_global_event_handlers
- Turbot Guardian announcement — https://turbot.com/blog/2026/06/introducing-guardian
- Turbot Launch Week 13 B-sides — https://turbot.com/blog/2026/06/launch-week-13-b-sides
- Cloudflare AI Gateway spend limits — https://developers.cloudflare.com/ai-gateway/features/spend-limits
- Portkey budget limits — https://portkey.ai/docs/aigw/product/ai-gateway/virtual-keys/budget-limits
- Waxell budgets — https://waxell.ai/capabilities/budgets
- LiteLLM budgets & rate limits — https://docs.litellm.ai/docs/proxy/users
- The $47,000 agent loop — https://dev.to/waxell/the-47000-agent-loop-why-token-budget-alerts-arent-budget-enforcement-389i
- Bedrock agent loop $30,000 — https://aiweekly.co/alerts/aws-bedrock-agent-loop-costs-developer-30000
