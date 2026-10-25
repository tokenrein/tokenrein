use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{AsyncCommands, Script};

use crate::config::Mode;

pub const RUN_TTL_SECS: u64 = 7 * 24 * 3600;
pub const DAILY_TTL_SECS: u64 = 3 * 24 * 3600;
pub const MONTHLY_TTL_SECS: u64 = 40 * 24 * 3600;
pub const MAX_RUN_DEPTH: usize = 16;
const DONE_TTL_SECS: u64 = 24 * 3600;
const RESPONSE_TTL_SECS: u64 = 30 * 24 * 3600;
const ANCESTRY_CACHE_MAX: usize = 100_000;

// Every key for an org shares the `{org}` hash tag, so all keys a script touches live in one cluster slot.
const RESERVE_LUA: &str = r#"
local n = tonumber(ARGV[1])
local m = tonumber(ARGV[2])
local est = tonumber(ARGV[3])
local req = ARGV[4]
local expire_at = tonumber(ARGV[5])
local record_ttl = tonumber(ARGV[6])
for j = 1, m do
  local st = redis.call('GET', KEYS[n + j])
  if st then return {'stopped', tostring(j), st} end
end
local exceeded = {}
for i = 1, n do
  local key = KEYS[i]
  local base = 6 + (i - 1) * 3
  local spec = ARGV[base + 1]
  local limit, mode
  if spec == 'stored' then
    limit = tonumber(redis.call('HGET', key, 'limit') or '-1')
    mode = redis.call('HGET', key, 'mode') or 'skip'
  elseif string.sub(spec, 1, 3) == 'nx:' then
    redis.call('HSETNX', key, 'limit', string.sub(spec, 4))
    redis.call('HSETNX', key, 'mode', ARGV[base + 2])
    limit = tonumber(redis.call('HGET', key, 'limit'))
    mode = redis.call('HGET', key, 'mode')
  else
    limit = tonumber(spec)
    mode = ARGV[base + 2]
    redis.call('HSET', key, 'limit', spec, 'mode', mode)
  end
  if limit >= 0 and mode ~= 'skip' then
    local spent = tonumber(redis.call('HGET', key, 'spent') or '0')
    local reserved = tonumber(redis.call('HGET', key, 'reserved') or '0')
    if spent + reserved + est > limit then
      if mode == 'enforce' then
        return {'over', tostring(i), tostring(spent), tostring(reserved), tostring(limit)}
      end
      table.insert(exceeded, tostring(i))
    end
  end
end
local names = {}
for i = 1, n do
  redis.call('HINCRBY', KEYS[i], 'reserved', est)
  redis.call('EXPIRE', KEYS[i], tonumber(ARGV[6 + (i - 1) * 3 + 3]))
  table.insert(names, KEYS[i])
end
local res_key = KEYS[n + m + 1]
redis.call('HSET', res_key, 'est', est, 'keys', table.concat(names, '\n'))
redis.call('EXPIRE', res_key, record_ttl)
redis.call('ZADD', KEYS[n + m + 2], expire_at, req)
return {'ok', table.concat(exceeded, ',')}
"#;

// KEYS: reservation, expiry zset, done marker, budget keys...; ARGV: request id, actual, done ttl, then one ttl per budget key.
const SETTLE_LUA: &str = r#"
if not redis.call('SET', KEYS[3], '1', 'NX', 'EX', tonumber(ARGV[3])) then
  return 'duplicate'
end
local actual = tonumber(ARGV[2])
local est = redis.call('HGET', KEYS[1], 'est')
for i = 4, #KEYS do
  if est then redis.call('HINCRBY', KEYS[i], 'reserved', -tonumber(est)) end
  redis.call('HINCRBY', KEYS[i], 'spent', actual)
  redis.call('EXPIRE', KEYS[i], tonumber(ARGV[i]))
end
redis.call('DEL', KEYS[1])
redis.call('ZREM', KEYS[2], ARGV[1])
if est then return 'settled' end
return 'late'
"#;

// Read-only budget check. KEYS: budget keys..., run state keys...;
// ARGV: n, m, estimate, then (limit spec, mode, ttl) per budget key.
const CHECK_LUA: &str = r#"
local n = tonumber(ARGV[1])
local m = tonumber(ARGV[2])
local est = tonumber(ARGV[3])
for j = 1, m do
  local st = redis.call('GET', KEYS[n + j])
  if st then return {'stopped', tostring(j), st} end
end
local exceeded = {}
for i = 1, n do
  local key = KEYS[i]
  local base = 3 + (i - 1) * 3
  local spec = ARGV[base + 1]
  local limit, mode
  if spec == 'stored' then
    limit = tonumber(redis.call('HGET', key, 'limit') or '-1')
    mode = redis.call('HGET', key, 'mode') or 'skip'
  elseif string.sub(spec, 1, 3) == 'nx:' then
    local l = redis.call('HGET', key, 'limit')
    if l then
      limit = tonumber(l)
      mode = redis.call('HGET', key, 'mode') or ARGV[base + 2]
    else
      limit = tonumber(string.sub(spec, 4))
      mode = ARGV[base + 2]
    end
  else
    limit = tonumber(spec)
    mode = ARGV[base + 2]
  end
  if limit >= 0 and mode ~= 'skip' then
    local spent = tonumber(redis.call('HGET', key, 'spent') or '0')
    local reserved = tonumber(redis.call('HGET', key, 'reserved') or '0')
    local over
    if est > 0 then over = spent + reserved + est > limit else over = spent + reserved >= limit end
    if over then
      if mode == 'enforce' then
        return {'over', tostring(i), tostring(spent), tostring(reserved), tostring(limit)}
      end
      table.insert(exceeded, tostring(i))
    end
  end
end
return {'ok', table.concat(exceeded, ',')}
"#;

// Record spend that already happened. KEYS: budget keys...; ARGV: n, actual, then
// (limit spec, mode, ttl) per key. Returns "index:mode" for each key now over its limit.
const CHARGE_LUA: &str = r#"
local n = tonumber(ARGV[1])
local actual = tonumber(ARGV[2])
local over = {}
for i = 1, n do
  local key = KEYS[i]
  local base = 2 + (i - 1) * 3
  local spec = ARGV[base + 1]
  if string.sub(spec, 1, 3) == 'nx:' then
    redis.call('HSETNX', key, 'limit', string.sub(spec, 4))
    redis.call('HSETNX', key, 'mode', ARGV[base + 2])
  elseif spec ~= 'stored' then
    redis.call('HSET', key, 'limit', spec, 'mode', ARGV[base + 2])
  end
  local spent = redis.call('HINCRBY', key, 'spent', actual)
  redis.call('EXPIRE', key, tonumber(ARGV[base + 3]))
  local limit = tonumber(redis.call('HGET', key, 'limit') or '-1')
  local mode = redis.call('HGET', key, 'mode') or 'skip'
  if limit >= 0 and mode ~= 'skip' and spent >= limit then
    table.insert(over, tostring(i) .. ':' .. mode)
  end
end
return over
"#;

// KEYS: reservation, expiry zset, budget keys...; ARGV: request id.
const RELEASE_LUA: &str = r#"
local est = redis.call('HGET', KEYS[1], 'est')
if est then
  for i = 3, #KEYS do
    if redis.call('EXISTS', KEYS[i]) == 1 then
      redis.call('HINCRBY', KEYS[i], 'reserved', -tonumber(est))
    end
  end
end
redis.call('DEL', KEYS[1])
redis.call('ZREM', KEYS[2], ARGV[1])
if est then return 1 end
return 0
"#;

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct Keys<'a> {
    org: &'a str,
}

impl<'a> Keys<'a> {
    pub fn new(org: &'a str) -> Self {
        Self { org }
    }
    pub fn run_budget(&self, run: &str) -> String {
        format!("tl:{{{}}}:b:run:{run}", self.org)
    }
    pub fn agent_daily(&self, project: &str, agent: &str, ymd: &str) -> String {
        format!("tl:{{{}}}:b:agent:{project}:{agent}:d:{ymd}", self.org)
    }
    pub fn project_monthly(&self, project: &str, ym: &str) -> String {
        format!("tl:{{{}}}:b:proj:{project}:m:{ym}", self.org)
    }
    pub fn org_monthly(&self, ym: &str) -> String {
        format!("tl:{{{}}}:b:org:m:{ym}", self.org)
    }
    pub fn run_parent(&self, run: &str) -> String {
        format!("tl:{{{}}}:rp:{run}", self.org)
    }
    pub fn run_state(&self, run: &str) -> String {
        format!("tl:{{{}}}:rs:{run}", self.org)
    }
    pub fn reservation(&self, request_id: &str) -> String {
        format!("tl:{{{}}}:res:{request_id}", self.org)
    }
    pub fn reservation_expiry(&self) -> String {
        format!("tl:{{{}}}:resexp", self.org)
    }
    pub fn done(&self, request_id: &str) -> String {
        format!("tl:{{{}}}:done:{request_id}", self.org)
    }
    /// Recent request events (capped stream) for the dashboard.
    pub fn events(&self) -> String {
        format!("tl:{{{}}}:ev", self.org)
    }
    /// Run ids by last activity.
    pub fn runs_seen(&self) -> String {
        format!("tl:{{{}}}:runs", self.org)
    }
    /// Project, agent, request counts and timestamps of a run.
    pub fn run_info(&self, run: &str) -> String {
        format!("tl:{{{}}}:ri:{run}", self.org)
    }
    /// `project/agent` pairs by last activity.
    pub fn agents_seen(&self) -> String {
        format!("tl:{{{}}}:agents", self.org)
    }
    /// The latest request events of one run, newest first.
    pub fn run_events(&self, run: &str) -> String {
        format!("tl:{{{}}}:re:{run}", self.org)
    }
    /// Runs whose parent is `run`.
    pub fn run_children(&self, run: &str) -> String {
        format!("tl:{{{}}}:rc:{run}", self.org)
    }
    /// Spend and request counts for one UTC day, by model, user, project and agent.
    pub fn spend_day(&self, ymd: &str) -> String {
        format!("tl:{{{}}}:sp:{ymd}", self.org)
    }
    pub fn response_tokens(&self, response_id: &str) -> String {
        format!("tl:{{{}}}:resp:{response_id}", self.org)
    }
    /// Progress of a usage import (e.g. `cursor`).
    pub fn import_state(&self, source: &str) -> String {
        format!("tl:{{{}}}:imp:{source}", self.org)
    }
    /// An imported usage event, so a re-read window is not charged twice.
    pub fn import_seen(&self, source: &str, id: &str) -> String {
        format!("tl:{{{}}}:imp:{source}:seen:{id}", self.org)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitSpec {
    /// Track spend only.
    Unlimited,
    /// Overwrite limit and mode on every request (period budgets follow the current policy).
    Set(u64),
    /// Fix limit and mode the first time the run is seen.
    SetIfAbsent(u64),
    /// Use whatever limit and mode the run was created with.
    Stored,
}

impl LimitSpec {
    fn arg(&self) -> String {
        match self {
            LimitSpec::Unlimited => "-1".into(),
            LimitSpec::Set(n) => n.to_string(),
            LimitSpec::SetIfAbsent(n) => format!("nx:{n}"),
            LimitSpec::Stored => "stored".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Scope {
    pub label: String,
    pub key: String,
    pub limit: LimitSpec,
    pub mode: Mode,
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reservation {
    Allowed {
        check_exceeded: Vec<String>,
    },
    OverBudget {
        label: String,
        spent: i64,
        reserved: i64,
        limit: i64,
    },
    RunStopped {
        run_id: String,
        state: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    Settled,
    Late,
    Duplicate,
}

pub struct Ledger {
    conn: ConnectionManager,
    reserve: Script,
    settle: Script,
    release: Script,
    check: Script,
    charge: Script,
    control_cas: Script,
    ancestry: Mutex<HashMap<String, Arc<Vec<String>>>>,
}

/// See [`Ledger::connect_with_timeout`].
pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

fn lua_num(s: &str) -> i64 {
    s.parse::<f64>().map(|f| f as i64).unwrap_or(0)
}

impl Ledger {
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with_timeout(url, DEFAULT_TIMEOUT).await
    }

    /// Every Redis call (and every connection attempt) fails after `timeout`, so a Redis outage
    /// becomes an error at once and the org's fail mode applies, instead of a hung request.
    pub async fn connect_with_timeout(url: &str, timeout: std::time::Duration) -> Result<Self> {
        let client = redis::Client::open(url)?;
        // No retries inside a call: the client's backoff starts at one second and grows to a
        // minute. A failed call reconnects on the next one instead.
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(timeout)
            .set_response_timeout(timeout)
            .set_number_of_retries(0);
        let conn = ConnectionManager::new_with_config(client, config).await?;
        Ok(Self {
            conn,
            reserve: Script::new(RESERVE_LUA),
            settle: Script::new(SETTLE_LUA),
            release: Script::new(RELEASE_LUA),
            check: Script::new(CHECK_LUA),
            charge: Script::new(CHARGE_LUA),
            control_cas: Script::new(CONTROL_CAS_LUA),
            ancestry: Mutex::new(HashMap::new()),
        })
    }

    pub async fn ping(&self) -> Result<()> {
        let mut conn = self.conn.clone();
        let _: String = redis::cmd("PING").query_async(&mut conn).await?;
        Ok(())
    }

    /// Atomically checks run state and every scope, then reserves `estimate` micro-USD on all of them or none.
    #[allow(clippy::too_many_arguments)]
    pub async fn reserve(
        &self,
        org: &str,
        request_id: &str,
        estimate: u64,
        scopes: &[Scope],
        runs: &[String],
        expire_at: i64,
        record_ttl_secs: u64,
    ) -> Result<Reservation> {
        let keys = Keys::new(org);
        let mut inv = self.reserve.prepare_invoke();
        for s in scopes {
            inv.key(&s.key);
        }
        for r in runs {
            inv.key(keys.run_state(r));
        }
        inv.key(keys.reservation(request_id))
            .key(keys.reservation_expiry());
        inv.arg(scopes.len())
            .arg(runs.len())
            .arg(estimate)
            .arg(request_id)
            .arg(expire_at)
            .arg(record_ttl_secs);
        for s in scopes {
            inv.arg(s.limit.arg()).arg(s.mode.as_str()).arg(s.ttl_secs);
        }
        let mut conn = self.conn.clone();
        let out: Vec<String> = inv.invoke_async(&mut conn).await?;
        parse_reservation(&out, scopes, runs)
    }

    /// Would a call costing up to `estimate` be allowed right now? Reserves nothing; used where
    /// TokenLens is not in the request path (agent hooks, SDK direct mode). With `estimate` 0 it
    /// asks whether a budget is already used up.
    pub async fn check(
        &self,
        org: &str,
        estimate: u64,
        scopes: &[Scope],
        runs: &[String],
    ) -> Result<Reservation> {
        let keys = Keys::new(org);
        let mut inv = self.check.prepare_invoke();
        for s in scopes {
            inv.key(&s.key);
        }
        for r in runs {
            inv.key(keys.run_state(r));
        }
        inv.arg(scopes.len()).arg(runs.len()).arg(estimate);
        for s in scopes {
            inv.arg(s.limit.arg()).arg(s.mode.as_str()).arg(s.ttl_secs);
        }
        let mut conn = self.conn.clone();
        let out: Vec<String> = inv.invoke_async(&mut conn).await?;
        parse_reservation(&out, scopes, runs)
    }

    /// Record spend that already happened (reported usage). Returns the scopes now over their
    /// limit, with their mode.
    pub async fn charge(&self, scopes: &[Scope], actual: u64) -> Result<Vec<(String, Mode)>> {
        let mut inv = self.charge.prepare_invoke();
        for s in scopes {
            inv.key(&s.key);
        }
        inv.arg(scopes.len()).arg(actual);
        for s in scopes {
            inv.arg(s.limit.arg()).arg(s.mode.as_str()).arg(s.ttl_secs);
        }
        let mut conn = self.conn.clone();
        let out: Vec<String> = inv.invoke_async(&mut conn).await?;
        Ok(out
            .iter()
            .filter_map(|item| {
                let (i, mode) = item.split_once(':')?;
                let scope = scopes.get(i.parse::<usize>().ok()?.checked_sub(1)?)?;
                let mode = match mode {
                    "enforce" => Mode::Enforce,
                    "check" => Mode::Check,
                    _ => Mode::Skip,
                };
                Some((scope.label.clone(), mode))
            })
            .collect())
    }

    /// Mark a run and agent as active without a request (agent hooks: prompt submitted, tool
    /// used, session ended), for live status.
    #[allow(clippy::too_many_arguments)]
    pub async fn touch(
        &self,
        org: &str,
        project: &str,
        agent: &str,
        user: Option<&str>,
        run: &str,
        parent: Option<&str>,
        source: &str,
        ended: bool,
    ) -> Result<()> {
        let keys = Keys::new(org);
        let now = unix_now();
        let info = keys.run_info(run);
        let mut pipe = redis::pipe();
        pipe.zadd(keys.runs_seen(), run, now).ignore();
        pipe.zadd(keys.agents_seen(), format!("{project}/{agent}"), now)
            .ignore();
        pipe.hset_multiple(
            &info,
            &[("project", project), ("agent", agent), ("source", source)],
        )
        .ignore();
        if let Some(u) = user {
            pipe.hset(&info, "user", u).ignore();
        }
        if let Some(p) = parent {
            pipe.hset(&info, "parent", p).ignore();
            pipe.set_ex(keys.run_parent(run), p, RUN_TTL_SECS).ignore();
            let children = keys.run_children(p);
            pipe.sadd(&children, run).ignore();
            pipe.expire(&children, RUN_TTL_SECS as i64).ignore();
        }
        pipe.hset(&info, "last_seen", now).ignore();
        pipe.hset_nx(&info, "first_seen", now).ignore();
        if ended {
            pipe.hset(&info, "ended_at", now).ignore();
        } else {
            pipe.hdel(&info, "ended_at").ignore();
        }
        pipe.expire(&info, RUN_TTL_SECS as i64).ignore();
        let mut conn = self.conn.clone();
        let _: () = pipe.query_async(&mut conn).await?;
        Ok(())
    }
}

fn parse_reservation(out: &[String], scopes: &[Scope], runs: &[String]) -> Result<Reservation> {
    {
        let index = |i: Option<&String>| {
            i.and_then(|s| s.parse::<usize>().ok())
                .and_then(|i| i.checked_sub(1))
        };
        match out.first().map(String::as_str) {
            Some("ok") => {
                let check_exceeded = out
                    .get(1)
                    .map(|list| {
                        list.split(',')
                            .filter_map(|i| i.parse::<usize>().ok()?.checked_sub(1))
                            .filter_map(|i| scopes.get(i))
                            .map(|s| s.label.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Reservation::Allowed { check_exceeded })
            }
            Some("over") => {
                let scope = index(out.get(1))
                    .and_then(|i| scopes.get(i))
                    .ok_or_else(|| anyhow!("bad reserve reply {out:?}"))?;
                Ok(Reservation::OverBudget {
                    label: scope.label.clone(),
                    spent: lua_num(out.get(2).map(String::as_str).unwrap_or("0")),
                    reserved: lua_num(out.get(3).map(String::as_str).unwrap_or("0")),
                    limit: lua_num(out.get(4).map(String::as_str).unwrap_or("0")),
                })
            }
            Some("stopped") => {
                let run_id = index(out.get(1))
                    .and_then(|i| runs.get(i))
                    .ok_or_else(|| anyhow!("bad reserve reply {out:?}"))?;
                Ok(Reservation::RunStopped {
                    run_id: run_id.clone(),
                    state: out.get(2).cloned().unwrap_or_default(),
                })
            }
            _ => Err(anyhow!("unexpected reserve reply {out:?}")),
        }
    }
}

impl Ledger {
    /// Replaces the reservation with the actual cost; idempotent per request id, and still records spend if the reservation already expired.
    pub async fn settle(
        &self,
        org: &str,
        request_id: &str,
        actual: u64,
        scopes: &[Scope],
    ) -> Result<Settlement> {
        let keys = Keys::new(org);
        let mut inv = self.settle.prepare_invoke();
        inv.key(keys.reservation(request_id))
            .key(keys.reservation_expiry())
            .key(keys.done(request_id));
        for s in scopes {
            inv.key(&s.key);
        }
        inv.arg(request_id).arg(actual).arg(DONE_TTL_SECS);
        for s in scopes {
            inv.arg(s.ttl_secs);
        }
        let mut conn = self.conn.clone();
        let out: String = inv.invoke_async(&mut conn).await?;
        Ok(match out.as_str() {
            "settled" => Settlement::Settled,
            "late" => Settlement::Late,
            _ => Settlement::Duplicate,
        })
    }

    /// Releases reservations whose requests died without settling.
    pub async fn sweep(&self, org: &str, now: i64, limit: usize) -> Result<usize> {
        let keys = Keys::new(org);
        let expiry = keys.reservation_expiry();
        let mut conn = self.conn.clone();
        let ids: Vec<String> = redis::cmd("ZRANGEBYSCORE")
            .arg(&expiry)
            .arg("-inf")
            .arg(now)
            .arg("LIMIT")
            .arg(0)
            .arg(limit)
            .query_async(&mut conn)
            .await?;
        let mut released = 0;
        for id in ids {
            let res_key = keys.reservation(&id);
            let budget_keys: Option<String> = conn.hget(&res_key, "keys").await?;
            let mut inv = self.release.prepare_invoke();
            inv.key(&res_key).key(&expiry);
            for k in budget_keys
                .as_deref()
                .unwrap_or("")
                .split('\n')
                .filter(|k| !k.is_empty())
            {
                inv.key(k);
            }
            inv.arg(&id);
            let n: i64 = inv.invoke_async(&mut conn).await?;
            released += n as usize;
        }
        Ok(released)
    }

    /// Registers `run`'s parent (first one wins) and returns its ancestors, nearest first.
    pub async fn ancestors(&self, org: &str, run: &str, parent: &str) -> Result<Arc<Vec<String>>> {
        let cache_key = format!("{org}\u{0}{run}");
        if let Some(hit) = self.cache().get(&cache_key) {
            return Ok(hit.clone());
        }
        let keys = Keys::new(org);
        let mut conn = self.conn.clone();
        let _: Option<String> = redis::cmd("SET")
            .arg(keys.run_parent(run))
            .arg(parent)
            .arg("NX")
            .arg("EX")
            .arg(RUN_TTL_SECS)
            .query_async(&mut conn)
            .await?;
        let mut chain: Vec<String> = Vec::new();
        let mut cur = run.to_string();
        while chain.len() < MAX_RUN_DEPTH {
            let next: Option<String> = conn.get(keys.run_parent(&cur)).await?;
            match next {
                Some(p) if p != run && !chain.contains(&p) => {
                    chain.push(p.clone());
                    cur = p;
                }
                _ => break,
            }
        }
        let chain = Arc::new(chain);
        let mut cache = self.cache();
        if cache.len() >= ANCESTRY_CACHE_MAX {
            cache.clear();
        }
        cache.insert(cache_key, chain.clone());
        Ok(chain)
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Vec<String>>>> {
        self.ancestry.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub async fn response_context_tokens(
        &self,
        org: &str,
        response_id: &str,
    ) -> Result<Option<u64>> {
        let mut conn = self.conn.clone();
        Ok(conn
            .get(Keys::new(org).response_tokens(response_id))
            .await?)
    }

    pub async fn remember_response_tokens(
        &self,
        org: &str,
        response_id: &str,
        tokens: u64,
    ) -> Result<()> {
        let mut conn = self.conn.clone();
        let _: () = conn
            .set_ex(
                Keys::new(org).response_tokens(response_id),
                tokens,
                RESPONSE_TTL_SECS,
            )
            .await?;
        Ok(())
    }

    pub async fn run_state(&self, org: &str, run: &str) -> Result<Option<String>> {
        let mut conn = self.conn.clone();
        Ok(conn.get(Keys::new(org).run_state(run)).await?)
    }

    pub async fn set_run_state(&self, org: &str, run: &str, state: &str) -> Result<()> {
        let mut conn = self.conn.clone();
        let _: () = conn
            .set_ex(Keys::new(org).run_state(run), state, RUN_TTL_SECS)
            .await?;
        Ok(())
    }

    pub async fn clear_run_state(&self, org: &str, run: &str) -> Result<()> {
        let mut conn = self.conn.clone();
        let _: () = conn.del(Keys::new(org).run_state(run)).await?;
        Ok(())
    }

    pub async fn run_parent(&self, org: &str, run: &str) -> Result<Option<String>> {
        let mut conn = self.conn.clone();
        Ok(conn.get(Keys::new(org).run_parent(run)).await?)
    }

    pub async fn budget(&self, key: &str) -> Result<HashMap<String, String>> {
        let mut conn = self.conn.clone();
        Ok(conn.hgetall(key).await?)
    }

    pub async fn budgets(&self, keys: &[String]) -> Result<Vec<HashMap<String, String>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut pipe = redis::pipe();
        for k in keys {
            pipe.hgetall(k);
        }
        let mut conn = self.conn.clone();
        Ok(pipe.query_async(&mut conn).await?)
    }

    // --- Managed orgs / projects / keys / budgets (admin API and dashboard) ---

    /// The managed org list (JSON) and its version, if it has been seeded.
    pub async fn control_load(&self) -> Result<Option<(u64, String)>> {
        let mut conn = self.conn.clone();
        let (doc, version): (Option<String>, Option<u64>) = redis::pipe()
            .get(CONTROL_ORGS)
            .get(CONTROL_VERSION)
            .query_async(&mut conn)
            .await?;
        Ok(doc.map(|d| (version.unwrap_or(0), d)))
    }

    pub async fn control_version(&self) -> Result<u64> {
        let mut conn = self.conn.clone();
        let v: Option<u64> = conn.get(CONTROL_VERSION).await?;
        Ok(v.unwrap_or(0))
    }

    /// Store `doc` if the version is still `expected` (0 = not seeded yet). Returns the new
    /// version, or None if someone else wrote first.
    pub async fn control_store(&self, expected: u64, doc: &str) -> Result<Option<u64>> {
        let mut conn = self.conn.clone();
        let v: i64 = self
            .control_cas
            .key(CONTROL_ORGS)
            .key(CONTROL_VERSION)
            .arg(expected)
            .arg(doc)
            .invoke_async(&mut conn)
            .await?;
        Ok(u64::try_from(v).ok())
    }

    // --- Activity for the dashboard (written off the request path) ---

    pub async fn record_activity(&self, events: &[serde_json::Value]) -> Result<()> {
        let now = unix_now();
        let ymd = chrono::Utc::now().format("%Y%m%d").to_string();
        let mut pipe = redis::pipe();
        for e in events {
            let s = |k: &str| e.get(k).and_then(|v| v.as_str()).unwrap_or("");
            let (org, run, project, agent) = (s("org"), s("run_id"), s("project"), s("agent"));
            if org.is_empty() || run.is_empty() {
                continue;
            }
            let keys = Keys::new(org);
            pipe.cmd("XADD")
                .arg(keys.events())
                .arg("MAXLEN")
                .arg("~")
                .arg(EVENTS_MAX)
                .arg("*")
                .arg("e")
                .arg(e.to_string())
                .ignore();
            pipe.zadd(keys.runs_seen(), run, now).ignore();
            pipe.zadd(keys.agents_seen(), format!("{project}/{agent}"), now)
                .ignore();
            let info = keys.run_info(run);
            pipe.hset_multiple(
                &info,
                &[
                    ("project", project),
                    ("agent", agent),
                    ("model", s("model")),
                    ("last_decision", s("decision")),
                ],
            )
            .ignore();
            for field in ["source", "user"] {
                if !s(field).is_empty() {
                    pipe.hset(&info, field, s(field)).ignore();
                }
            }
            if let Some(p) = e.get("parent_run_id").and_then(|v| v.as_str()) {
                pipe.hset(&info, "parent", p).ignore();
                let children = keys.run_children(p);
                pipe.sadd(&children, run).ignore();
                pipe.expire(&children, RUN_TTL_SECS as i64).ignore();
            }
            let run_events = keys.run_events(run);
            pipe.lpush(&run_events, e.to_string()).ignore();
            pipe.ltrim(&run_events, 0, RUN_EVENTS_MAX as isize - 1)
                .ignore();
            pipe.expire(&run_events, RUN_TTL_SECS as i64).ignore();

            let spend = keys.spend_day(&ymd);
            let cost = e.get("actual_micros").and_then(|v| v.as_i64()).unwrap_or(0);
            let blocked = s("decision").starts_with("blocked");
            let model = format!("{}/{}", s("provider"), s("model"));
            let user = e.get("user").and_then(|v| v.as_str()).unwrap_or("-");
            let agent_key = format!("{project}/{agent}");
            for (dim, value) in [
                ("model", model.as_str()),
                ("user", user),
                ("project", project),
                ("agent", agent_key.as_str()),
            ] {
                pipe.hincr(&spend, format!("{dim}:{value}:c"), cost)
                    .ignore();
                pipe.hincr(&spend, format!("{dim}:{value}:n"), 1).ignore();
                if blocked {
                    pipe.hincr(&spend, format!("{dim}:{value}:b"), 1).ignore();
                }
            }
            pipe.expire(&spend, MONTHLY_TTL_SECS as i64).ignore();
            pipe.hset(&info, "last_seen", now).ignore();
            pipe.hset_nx(&info, "first_seen", now).ignore();
            pipe.hincr(&info, "requests", 1).ignore();
            if s("decision").starts_with("blocked") {
                pipe.hincr(&info, "blocked", 1).ignore();
            }
            pipe.expire(&info, RUN_TTL_SECS as i64).ignore();
        }
        let mut conn = self.conn.clone();
        let _: () = pipe.query_async(&mut conn).await?;
        Ok(())
    }

    /// Drop activity index entries older than the run TTL (their run data has expired).
    pub async fn trim_activity(&self, org: &str) -> Result<()> {
        let keys = Keys::new(org);
        let cutoff = unix_now() - RUN_TTL_SECS as i64;
        let mut conn = self.conn.clone();
        let _: () = redis::pipe()
            .zrembyscore(keys.runs_seen(), "-inf", cutoff)
            .ignore()
            .zrembyscore(keys.agents_seen(), "-inf", cutoff)
            .ignore()
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    /// Most recently active runs (newest first) with their last-activity time.
    pub async fn recent_runs(&self, org: &str, limit: usize) -> Result<Vec<(String, i64)>> {
        let mut conn = self.conn.clone();
        Ok(conn
            .zrevrange_withscores(Keys::new(org).runs_seen(), 0, limit as isize - 1)
            .await?)
    }

    /// For each run: info hash, run budget hash, state, parent.
    pub async fn run_details(&self, org: &str, runs: &[String]) -> Result<Vec<RunDetail>> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let keys = Keys::new(org);
        let mut pipe = redis::pipe();
        for r in runs {
            pipe.hgetall(keys.run_info(r))
                .hgetall(keys.run_budget(r))
                .get(keys.run_state(r))
                .get(keys.run_parent(r));
        }
        let mut conn = self.conn.clone();
        let flat: Vec<redis::Value> = pipe.query_async(&mut conn).await?;
        let mut out = Vec::with_capacity(runs.len());
        for chunk in flat.chunks(4) {
            out.push(RunDetail {
                info: redis::from_redis_value(&chunk[0])?,
                budget: redis::from_redis_value(&chunk[1])?,
                state: redis::from_redis_value(&chunk[2])?,
                parent: redis::from_redis_value(&chunk[3])?,
            });
        }
        Ok(out)
    }

    pub async fn recent_agents(&self, org: &str, limit: usize) -> Result<Vec<(String, i64)>> {
        let mut conn = self.conn.clone();
        Ok(conn
            .zrevrange_withscores(Keys::new(org).agents_seen(), 0, limit as isize - 1)
            .await?)
    }

    /// Events of `root` and its descendant runs (up to `max_runs` runs), oldest first.
    pub async fn run_timeline(
        &self,
        org: &str,
        root: &str,
        max_runs: usize,
    ) -> Result<(Vec<String>, Vec<serde_json::Value>)> {
        let keys = Keys::new(org);
        let mut conn = self.conn.clone();
        let mut runs = vec![root.to_string()];
        let mut frontier = vec![root.to_string()];
        for _ in 0..MAX_RUN_DEPTH {
            if frontier.is_empty() || runs.len() >= max_runs {
                break;
            }
            let mut pipe = redis::pipe();
            for r in &frontier {
                pipe.smembers(keys.run_children(r));
            }
            let kids: Vec<Vec<String>> = pipe.query_async(&mut conn).await?;
            frontier = Vec::new();
            for k in kids.into_iter().flatten() {
                if runs.len() >= max_runs {
                    break;
                }
                if !runs.contains(&k) {
                    runs.push(k.clone());
                    frontier.push(k);
                }
            }
        }
        let mut pipe = redis::pipe();
        for r in &runs {
            pipe.lrange(keys.run_events(r), 0, -1);
        }
        let lists: Vec<Vec<String>> = pipe.query_async(&mut conn).await?;
        let mut events: Vec<serde_json::Value> = lists
            .into_iter()
            .flatten()
            .filter_map(|e| serde_json::from_str(&e).ok())
            .collect();
        events.sort_by(|a, b| {
            let ts = |v: &serde_json::Value| {
                v.get("ts")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            ts(a).cmp(&ts(b))
        });
        Ok((runs, events))
    }

    /// Spend hashes for the given UTC days (`YYYYMMDD`).
    pub async fn spend_days(
        &self,
        org: &str,
        days: &[String],
    ) -> Result<Vec<HashMap<String, i64>>> {
        let keys: Vec<String> = days.iter().map(|d| Keys::new(org).spend_day(d)).collect();
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut pipe = redis::pipe();
        for k in &keys {
            pipe.hgetall(k);
        }
        let mut conn = self.conn.clone();
        Ok(pipe.query_async(&mut conn).await?)
    }

    /// Sets `key` unless it exists; true the first time.
    pub async fn first_time(&self, key: &str, ttl_secs: u64) -> Result<bool> {
        let mut conn = self.conn.clone();
        let set: Option<String> = redis::cmd("SET")
            .arg(key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut conn)
            .await?;
        Ok(set.is_some())
    }

    pub async fn get_value(&self, key: &str) -> Result<Option<String>> {
        let mut conn = self.conn.clone();
        Ok(conn.get(key).await?)
    }

    pub async fn set_value(&self, key: &str, value: &str) -> Result<()> {
        let mut conn = self.conn.clone();
        let _: () = conn.set(key, value).await?;
        Ok(())
    }

    /// Newest request events first.
    pub async fn recent_events(&self, org: &str, limit: usize) -> Result<Vec<serde_json::Value>> {
        let mut conn = self.conn.clone();
        let reply: redis::streams::StreamRangeReply = redis::cmd("XREVRANGE")
            .arg(Keys::new(org).events())
            .arg("+")
            .arg("-")
            .arg("COUNT")
            .arg(limit)
            .query_async(&mut conn)
            .await?;
        Ok(reply
            .ids
            .iter()
            .filter_map(|id| id.get::<String>("e"))
            .filter_map(|e| serde_json::from_str(&e).ok())
            .collect())
    }

    // --- Audit log when there is no database (capped; Postgres keeps everything) ---

    pub async fn audit_append(
        &self,
        e: &crate::audit::Entry,
        config_version: Option<u64>,
    ) -> Result<()> {
        let mut conn = self.conn.clone();
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut doc = serde_json::to_value(e)?;
        doc["at"] = serde_json::json!(at);
        doc["config_version"] = serde_json::json!(config_version);
        let _: String = redis::cmd("XADD")
            .arg(AUDIT_STREAM)
            .arg("MAXLEN")
            .arg("~")
            .arg(AUDIT_MAX)
            .arg("*")
            .arg("e")
            .arg(doc.to_string())
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    /// Newest first, optionally only one org's entries; `before` is an entry id (exclusive).
    pub async fn audit_list(
        &self,
        org: Option<&str>,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let mut conn = self.conn.clone();
        let mut out = Vec::new();
        let mut end = match before {
            Some(id) if valid_stream_id(id) => format!("({id}"),
            _ => "+".to_string(),
        };
        // Filtering by org happens here, so scan a bounded number of pages.
        for _ in 0..20 {
            let reply: redis::streams::StreamRangeReply = redis::cmd("XREVRANGE")
                .arg(AUDIT_STREAM)
                .arg(&end)
                .arg("-")
                .arg("COUNT")
                .arg(limit.max(100))
                .query_async(&mut conn)
                .await?;
            let Some(last) = reply.ids.last() else { break };
            end = format!("({}", last.id);
            for item in &reply.ids {
                let Some(mut e) = item
                    .get::<String>("e")
                    .and_then(|e| serde_json::from_str::<serde_json::Value>(&e).ok())
                else {
                    continue;
                };
                if org.is_some_and(|o| e["org"].as_str() != Some(o)) {
                    continue;
                }
                e["id"] = serde_json::json!(item.id);
                out.push(e);
                if out.len() == limit {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }
}

fn valid_stream_id(id: &str) -> bool {
    let mut parts = id.split('-');
    let ok =
        |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

pub struct RunDetail {
    pub info: HashMap<String, String>,
    pub budget: HashMap<String, String>,
    pub state: Option<String>,
    pub parent: Option<String>,
}

const CONTROL_ORGS: &str = "tl:control:orgs";
const CONTROL_VERSION: &str = "tl:control:version";
const EVENTS_MAX: usize = 5000;
const AUDIT_STREAM: &str = "tl:audit";
const AUDIT_MAX: usize = 10_000;
const RUN_EVENTS_MAX: usize = 500;

// KEYS: doc, version; ARGV: expected version, new doc. Returns the new version or -1.
const CONTROL_CAS_LUA: &str = r#"
local v = tonumber(redis.call('GET', KEYS[2]) or '0')
if v ~= tonumber(ARGV[1]) then return -1 end
redis.call('SET', KEYS[1], ARGV[2])
return redis.call('INCR', KEYS[2])
"#;
