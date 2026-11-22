//! Usage history beyond what Redis keeps (40 days of daily totals, 7 days of run detail): every
//! request event is also written to Parquet files, one folder per org and day, and read back with
//! DuckDB for long spend ranges, CSV exports and old run timelines.
//!
//! Each process only ever adds its own files, so several replicas can share one folder (a Docker
//! volume, or a ReadWriteMany volume in Kubernetes). Files are immutable; once a day is over its
//! files are merged into one, and days past the retention period are deleted. Writing is best
//! effort and off the request path: a failure is logged and counted, never seen by an agent.
//!
//! Layout: `<dir>/usage/org=<org>/day=<YYYY-MM-DD>/<node>-<micros>.parquet`. Agent steps (tool
//! calls and results, prompts) are kept the same way under `<dir>/steps/`, as metadata only:
//! names, outcomes, sizes, timings and input fingerprints, never their content.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use duckdb::{Connection, params};
use serde_json::{Value, json};

use crate::AppState;
use crate::config::HistoryConfig;

/// Rows held in memory between flushes; more are dropped (and counted).
const BUFFER_MAX: usize = 200_000;
/// A flush starts early once this many rows are waiting.
const FLUSH_ROWS: usize = 20_000;
/// Rows in one run timeline.
const RUN_EVENTS_MAX: usize = 500;

const SCHEMA: &str = r#"CREATE TABLE u (
    ts_us BIGINT, project VARCHAR, agent VARCHAR, "user" VARCHAR, run_id VARCHAR,
    parent_run_id VARCHAR, provider VARCHAR, model VARCHAR, decision VARCHAR,
    blocked_scope VARCHAR, source VARCHAR, request_id VARCHAR, cost_micros BIGINT,
    input_tokens BIGINT, cached_input_tokens BIGINT, cache_write_tokens BIGINT,
    output_tokens BIGINT, upstream_ms BIGINT, cache_write_long_tokens BIGINT,
    reasoning_tokens BIGINT)"#;

/// Columns added after the first release, with the value older files are read as.
const ADDED_COLUMNS: &str =
    "ALTER TABLE e ADD COLUMN IF NOT EXISTS cache_write_long_tokens BIGINT DEFAULT 0;
     ALTER TABLE e ADD COLUMN IF NOT EXISTS reasoning_tokens BIGINT DEFAULT 0;";

/// One request event, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub org: String,
    pub day: NaiveDate,
    pub ts_us: i64,
    pub project: String,
    pub agent: String,
    pub user: Option<String>,
    pub run_id: String,
    pub parent_run_id: Option<String>,
    pub provider: Option<String>,
    pub model: String,
    pub decision: String,
    pub blocked_scope: Option<String>,
    pub source: Option<String>,
    pub request_id: Option<String>,
    pub cost_micros: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
    pub upstream_ms: i64,
    pub cache_write_long_tokens: i64,
    pub reasoning_tokens: i64,
}

impl Row {
    /// The stored form of a dashboard activity event; `None` for events without an org and run
    /// (the same ones Redis skips) and for agent steps, which are kept only as run detail.
    pub fn from_event(e: &Value) -> Option<Row> {
        let s = |k: &str| e.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
        if s("event") == Some("step") {
            return None;
        }
        let n = |v: Option<&Value>| v.and_then(Value::as_i64).unwrap_or(0);
        let org = s("org").filter(|o| safe_id(o))?.to_string();
        let run_id = s("run_id")?.to_string();
        let ts = s("ts")
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);
        let usage = e.get("usage");
        let u = |k: &str| n(usage.and_then(|u| u.get(k)));
        Some(Row {
            org,
            day: ts.date_naive(),
            ts_us: ts.timestamp_micros(),
            project: s("project").unwrap_or("").to_string(),
            agent: s("agent").unwrap_or("").to_string(),
            user: s("user").map(str::to_string),
            run_id,
            parent_run_id: s("parent_run_id").map(str::to_string),
            provider: s("provider").map(str::to_string),
            model: s("model").unwrap_or("").to_string(),
            decision: s("decision").unwrap_or("").to_string(),
            blocked_scope: s("blocked_scope").map(str::to_string),
            source: s("source").map(str::to_string),
            request_id: s("request_id").map(str::to_string),
            cost_micros: n(e.get("actual_micros")),
            input_tokens: u("input"),
            cached_input_tokens: u("cached_input"),
            cache_write_tokens: u("cache_write"),
            output_tokens: u("output"),
            upstream_ms: n(e.get("upstream_ms")),
            cache_write_long_tokens: u("cache_write_long"),
            reasoning_tokens: u("reasoning"),
        })
    }
}

/// Add rows to table `u` (created with [`SCHEMA`]).
fn append_rows(conn: &Connection, rows: &[Row]) -> duckdb::Result<()> {
    let mut app = conn.appender("u")?;
    for r in rows {
        app.append_row(params![
            r.ts_us,
            r.project,
            r.agent,
            r.user,
            r.run_id,
            r.parent_run_id,
            r.provider,
            r.model,
            r.decision,
            r.blocked_scope,
            r.source,
            r.request_id,
            r.cost_micros,
            r.input_tokens,
            r.cached_input_tokens,
            r.cache_write_tokens,
            r.output_tokens,
            r.upstream_ms,
            r.cache_write_long_tokens,
            r.reasoning_tokens,
        ])?;
    }
    app.flush()
}

/// One kind of activity event a history store keeps, as Parquet files under `DIR`.
pub trait Record: Sized + Clone + Send + 'static {
    /// Folder under the history directory.
    const DIR: &'static str;
    /// Table the rows are written through.
    const TABLE: &'static str;
    /// Creates `TABLE` with the stored columns.
    const SCHEMA: &'static str;
    /// Run on table `e` after files are read: columns added after the first release.
    const ADDED: &'static str;
    fn from_event(e: &Value) -> Option<Self>;
    fn org(&self) -> &str;
    fn day(&self) -> NaiveDate;
    fn ts_us(&self) -> i64;
    fn append(conn: &Connection, rows: &[Self]) -> duckdb::Result<()>;
}

impl Record for Row {
    const DIR: &'static str = "usage";
    const TABLE: &'static str = "u";
    const SCHEMA: &'static str = SCHEMA;
    const ADDED: &'static str = ADDED_COLUMNS;
    fn from_event(e: &Value) -> Option<Self> {
        Row::from_event(e)
    }
    fn org(&self) -> &str {
        &self.org
    }
    fn day(&self) -> NaiveDate {
        self.day
    }
    fn ts_us(&self) -> i64 {
        self.ts_us
    }
    fn append(conn: &Connection, rows: &[Self]) -> duckdb::Result<()> {
        append_rows(conn, rows)
    }
}

const STEP_SCHEMA: &str = r#"CREATE TABLE s (
    ts_us BIGINT, project VARCHAR, agent VARCHAR, "user" VARCHAR, run_id VARCHAR,
    parent_run_id VARCHAR, source VARCHAR, kind VARCHAR, name VARCHAR, call_id VARCHAR,
    status VARCHAR, fingerprint VARCHAR, size BIGINT, duration_ms BIGINT)"#;

/// One agent step, as stored: what happened, never what was said.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRow {
    pub org: String,
    pub day: NaiveDate,
    pub ts_us: i64,
    pub project: String,
    pub agent: String,
    pub user: Option<String>,
    pub run_id: String,
    pub parent_run_id: Option<String>,
    pub source: Option<String>,
    pub kind: String,
    pub name: Option<String>,
    pub call_id: Option<String>,
    pub status: Option<String>,
    pub fingerprint: Option<String>,
    pub size: Option<i64>,
    pub duration_ms: Option<i64>,
}

impl Record for StepRow {
    const DIR: &'static str = "steps";
    const TABLE: &'static str = "s";
    const SCHEMA: &'static str = STEP_SCHEMA;
    const ADDED: &'static str = "";
    /// Step events only; model calls are already in the usage history.
    fn from_event(e: &Value) -> Option<Self> {
        let s = |k: &str| e.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
        if s("event") != Some("step") {
            return None;
        }
        let kind = s("kind").filter(|k| *k != "model_call")?.to_string();
        let org = s("org").filter(|o| safe_id(o))?.to_string();
        let run_id = s("run_id")?.to_string();
        let ts = s("ts")
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);
        let opt = |k: &str| s(k).map(str::to_string);
        let n = |k: &str| e.get(k).and_then(Value::as_i64);
        Some(StepRow {
            org,
            day: ts.date_naive(),
            ts_us: ts.timestamp_micros(),
            project: s("project").unwrap_or("").to_string(),
            agent: s("agent").unwrap_or("").to_string(),
            user: opt("user"),
            run_id,
            parent_run_id: opt("parent_run_id"),
            source: opt("source"),
            kind,
            name: opt("name"),
            call_id: opt("call_id"),
            status: opt("status"),
            fingerprint: opt("fingerprint"),
            size: n("size"),
            duration_ms: n("duration_ms"),
        })
    }
    fn org(&self) -> &str {
        &self.org
    }
    fn day(&self) -> NaiveDate {
        self.day
    }
    fn ts_us(&self) -> i64 {
        self.ts_us
    }
    fn append(conn: &Connection, rows: &[Self]) -> duckdb::Result<()> {
        let mut app = conn.appender("s")?;
        for r in rows {
            app.append_row(params![
                r.ts_us,
                r.project,
                r.agent,
                r.user,
                r.run_id,
                r.parent_run_id,
                r.source,
                r.kind,
                r.name,
                r.call_id,
                r.status,
                r.fingerprint,
                r.size,
                r.duration_ms,
            ])?;
        }
        app.flush()
    }
}

/// Org ids become folder names: no dots in front (`.`, `..`).
fn safe_id(s: &str) -> bool {
    crate::config::valid_id(s) && !s.starts_with('.')
}

/// An in-memory DuckDB connection kept small: the process shares its memory with the proxy.
fn duck() -> duckdb::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("SET memory_limit = '256MB'; SET threads = 2;")?;
    Ok(conn)
}

/// A path as a DuckDB string literal.
fn sql_str(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "''"))
}

/// The breakdown dimensions and the SQL for each one's key, as the dashboard shows it: a model
/// without a provider is just its name, a request without a user is `-`, an agent is
/// `project/agent`.
const DIMS: [(&str, &str); 4] = [
    (
        "model",
        "CASE WHEN coalesce(provider, '') = '' THEN model ELSE provider || '/' || model END",
    ),
    ("user", r#"coalesce("user", '-')"#),
    ("project", "project"),
    ("agent", "project || '/' || agent"),
];

/// The SQL key of a dimension in [`DIMS`].
fn dim_sql(dim: &str) -> &'static str {
    DIMS.iter()
        .find(|(d, _)| *d == dim)
        .map_or("NULL", |(_, e)| e)
}

const SUMS: &str =
    "CAST(sum(cost_micros) AS BIGINT), count(*), count_if(starts_with(decision, 'blocked'))";

/// How finely a range report is split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Minute,
    Hour,
    Day,
}

impl Bucket {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "minute" => Some(Self::Minute),
            "hour" => Some(Self::Hour),
            "day" => Some(Self::Day),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }

    pub fn micros(self) -> i64 {
        match self {
            Self::Minute => 60_000_000,
            Self::Hour => 3_600_000_000,
            Self::Day => 86_400_000_000,
        }
    }

    /// The start of the bucket holding `us` (days start at midnight UTC).
    pub fn floor(self, us: i64) -> i64 {
        us.div_euclid(self.micros()) * self.micros()
    }
}

/// Which requests a range report counts; `None` matches all. Values are the breakdown keys
/// (see [`DIMS`]); an agent without a `/` matches that agent name in any project.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub project: Option<String>,
    pub agent: Option<String>,
    pub user: Option<String>,
    pub model: Option<String>,
    /// `Some(true)`: blocked requests only; `Some(false)`: allowed ones only.
    pub blocked: Option<bool>,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// SQL conditions (joined with AND) and their parameters, in order.
    fn sql(&self) -> (String, Vec<String>) {
        let mut sql = String::new();
        let mut params = Vec::new();
        let mut eq = |expr: &str, v: &Option<String>| {
            if let Some(v) = v {
                sql.push_str(&format!(" AND ({expr}) = ?"));
                params.push(v.clone());
            }
        };
        eq("project", &self.project);
        let agent = match &self.agent {
            Some(a) if a.contains('/') => dim_sql("agent"),
            _ => "agent",
        };
        eq(agent, &self.agent);
        eq(dim_sql("user"), &self.user);
        eq(dim_sql("model"), &self.model);
        match self.blocked {
            Some(true) => sql.push_str(" AND starts_with(decision, 'blocked')"),
            Some(false) => sql.push_str(" AND NOT starts_with(decision, 'blocked')"),
            None => {}
        }
        (sql, params)
    }
}

/// A spend report over any window: `[from_us, to_us)` in `bucket`s, narrowed by `filter`.
#[derive(Debug, Clone)]
pub struct Range {
    pub from_us: i64,
    pub to_us: i64,
    pub bucket: Bucket,
    pub filter: Filter,
}

/// What [`History::spend_range`] returns.
#[derive(Debug, Default)]
pub struct RangeSpend {
    /// Buckets that have requests, oldest first: (bucket start in microseconds, totals).
    pub buckets: Vec<(i64, Totals)>,
    /// Per dimension, the top 50 by cost among the filtered requests.
    pub by: Vec<(&'static str, Vec<Totals>)>,
    /// Per dimension, every value seen in the window ignoring the filter (top 200 by cost), for
    /// the filter menus.
    pub options: Vec<(&'static str, Vec<String>)>,
}

/// Cost, requests and blocked requests for one day or one key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub key: String,
    pub cost_micros: i64,
    pub requests: i64,
    pub blocked: i64,
}

/// A spend report over a date range.
#[derive(Debug, Default)]
pub struct Spend {
    /// One entry per day that has events, oldest first (`key` is `YYYY-MM-DD`).
    pub days: Vec<Totals>,
    /// Per dimension (`model`, `user`, `project`, `agent`): the top 50 by cost.
    pub by: Vec<(&'static str, Vec<Totals>)>,
}

/// Usage history: request events.
pub type History = Store<Row>;
/// Agent step history: tool calls and results, prompts, sub-agents (metadata only).
pub type StepHistory = Store<StepRow>;

pub struct Store<R: Record> {
    dir: PathBuf,
    /// Prefix of this process's file names, so replicas sharing a folder never collide.
    node: String,
    pub retention_days: u32,
    pub flush_secs: u64,
    buffer: Mutex<Vec<R>>,
    flushing: tokio::sync::Mutex<()>,
    wake: tokio::sync::Notify,
}

impl<R: Record> Store<R> {
    pub fn from_config(cfg: &HistoryConfig) -> Option<Arc<Self>> {
        let dir = cfg
            .dir
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())?;
        let host: String = std::env::var("HOSTNAME")
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(40)
            .collect();
        let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
        Some(Arc::new(Self {
            dir: PathBuf::from(dir),
            node: if host.is_empty() {
                tag.to_string()
            } else {
                format!("{host}-{tag}")
            },
            retention_days: cfg.retention_days,
            flush_secs: cfg.flush_secs,
            buffer: Mutex::new(Vec::new()),
            flushing: tokio::sync::Mutex::new(()),
            wake: tokio::sync::Notify::new(),
        }))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn org_dir(&self, org: &str) -> PathBuf {
        self.dir.join(R::DIR).join(format!("org={org}"))
    }

    fn day_dir(&self, org: &str, day: NaiveDate) -> PathBuf {
        self.org_dir(org).join(format!("day={day}"))
    }

    /// Queue activity events for the next flush. Never blocks on disk.
    pub fn push(&self, events: &[Value]) {
        let rows: Vec<R> = events.iter().filter_map(R::from_event).collect();
        if rows.is_empty() {
            return;
        }
        let mut buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        let room = BUFFER_MAX.saturating_sub(buf.len());
        if rows.len() > room {
            crate::metrics::history_dropped(rows.len() - room);
        }
        buf.extend(rows.into_iter().take(room));
        if buf.len() >= FLUSH_ROWS {
            self.wake.notify_one();
        }
    }

    /// Write everything queued to Parquet; returns the rows written. Rows of a file that could
    /// not be written go back in the queue for the next flush.
    pub async fn flush(self: &Arc<Self>) -> Result<usize> {
        let _one = self.flushing.lock().await;
        let rows = std::mem::take(&mut *self.buffer.lock().unwrap_or_else(|e| e.into_inner()));
        if rows.is_empty() {
            return Ok(0);
        }
        let this = self.clone();
        let (written, failed, err) = tokio::task::spawn_blocking(move || this.write(rows))
            .await
            .context("history writer panicked")?;
        if !failed.is_empty() {
            let n = failed.len();
            let mut buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
            let room = BUFFER_MAX.saturating_sub(buf.len());
            crate::metrics::history_dropped(n.saturating_sub(room));
            buf.extend(failed.into_iter().take(room));
        }
        crate::metrics::history_written(written);
        match err {
            Some(e) => Err(e),
            None => Ok(written),
        }
    }

    /// Blocking: one Parquet file per org and day. Returns (rows written, rows not written, error).
    fn write(&self, rows: Vec<R>) -> (usize, Vec<R>, Option<anyhow::Error>) {
        let mut groups: BTreeMap<(String, NaiveDate), Vec<R>> = BTreeMap::new();
        for r in rows {
            groups
                .entry((r.org().to_string(), r.day()))
                .or_default()
                .push(r);
        }
        let conn = match duck().and_then(|c| c.execute_batch(R::SCHEMA).map(|_| c)) {
            Ok(c) => c,
            Err(e) => return (0, groups.into_values().flatten().collect(), Some(e.into())),
        };
        let mut written = 0;
        let mut groups = groups.into_iter();
        while let Some(((org, day), rows)) = groups.next() {
            if let Err(e) = self.write_file(&conn, &org, day, &rows) {
                let mut failed = rows;
                failed.extend(groups.flat_map(|(_, r)| r));
                return (written, failed, Some(e));
            }
            written += rows.len();
        }
        (written, Vec::new(), None)
    }

    fn write_file(&self, conn: &Connection, org: &str, day: NaiveDate, rows: &[R]) -> Result<()> {
        conn.execute_batch(&format!("DELETE FROM {}", R::TABLE))?;
        R::append(conn, rows)?;
        let dir = self.day_dir(org, day);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let name = format!("{}-{}.parquet", self.node, Utc::now().timestamp_micros());
        let tmp = dir.join(format!(".{name}.tmp"));
        conn.execute_batch(&format!(
            "COPY {} TO {} (FORMAT parquet, COMPRESSION zstd)",
            R::TABLE,
            sql_str(&tmp)
        ))?;
        std::fs::rename(&tmp, dir.join(&name)).context("publishing history file")?;
        Ok(())
    }

    /// The Parquet files of one day, without hidden (temporary) ones.
    fn day_files(dir: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                !name.starts_with('.') && name.ends_with(".parquet")
            })
            .collect();
        files.sort();
        files
    }

    /// The days an org has files for, oldest first.
    fn days(&self, org: &str) -> Vec<NaiveDate> {
        let Ok(rd) = std::fs::read_dir(self.org_dir(org)) else {
            return Vec::new();
        };
        let mut days: Vec<NaiveDate> = rd
            .filter_map(|e| {
                e.ok()?
                    .file_name()
                    .to_str()?
                    .strip_prefix("day=")?
                    .parse()
                    .ok()
            })
            .collect();
        days.sort();
        days
    }

    /// A DuckDB connection with the org's events between `from` and `to` (inclusive) in table
    /// `e`, or `None` when there are none.
    fn open(&self, org: &str, from: NaiveDate, to: NaiveDate) -> Result<Option<Connection>> {
        anyhow::ensure!(safe_id(org), "invalid org id");
        let files: Vec<String> = self
            .days(org)
            .into_iter()
            .filter(|d| *d >= from && *d <= to)
            .flat_map(|d| Self::day_files(&self.day_dir(org, d)))
            .map(|p| sql_str(&p))
            .collect();
        if files.is_empty() {
            return Ok(None);
        }
        let conn = duck()?;
        conn.execute_batch(&format!(
            "CREATE TEMP TABLE e AS
               SELECT *, CAST(day AS DATE) AS d
               FROM read_parquet([{}], hive_partitioning = true, union_by_name = true);
             {}",
            files.join(", "),
            R::ADDED
        ))?;
        Ok(Some(conn))
    }

    /// Blocking: a DuckDB connection with table `e` holding every stored column of the org's
    /// rows in `[from_us, to_us)`, the queued ones included, or `None` when there are none.
    pub fn open_window(&self, org: &str, from_us: i64, to_us: i64) -> Result<Option<Connection>> {
        anyhow::ensure!(safe_id(org), "invalid org id");
        let day = |us: i64| {
            DateTime::from_timestamp_micros(us)
                .unwrap_or_default()
                .date_naive()
        };
        let (from, to) = (day(from_us), day(to_us));
        let files: Vec<String> = self
            .days(org)
            .into_iter()
            .filter(|d| *d >= from && *d <= to)
            .flat_map(|d| Self::day_files(&self.day_dir(org, d)))
            .map(|p| sql_str(&p))
            .collect();
        let pending: Vec<R> = {
            let buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
            buf.iter()
                .filter(|r| r.org() == org && r.ts_us() >= from_us && r.ts_us() < to_us)
                .cloned()
                .collect()
        };
        if files.is_empty() && pending.is_empty() {
            return Ok(None);
        }
        let conn = duck()?;
        conn.execute_batch(R::SCHEMA)?;
        R::append(&conn, &pending)?;
        let stored = if files.is_empty() {
            String::new()
        } else {
            format!(
                "SELECT * FROM read_parquet([{}], hive_partitioning = false, union_by_name = true) \
                 UNION ALL BY NAME ",
                files.join(", ")
            )
        };
        conn.execute_batch(&format!(
            "CREATE TEMP TABLE e AS {stored}SELECT * FROM {}",
            R::TABLE
        ))?;
        Ok(Some(conn))
    }
}

impl History {
    /// Blocking: daily totals and top keys per dimension between `from` and `to`.
    pub fn spend(&self, org: &str, from: NaiveDate, to: NaiveDate) -> Result<Spend> {
        let Some(conn) = self.open(org, from, to)? else {
            return Ok(Spend::default());
        };
        let totals = |sql: &str| -> Result<Vec<Totals>> {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map([], |r| {
                Ok(Totals {
                    key: r.get(0)?,
                    cost_micros: r.get(1)?,
                    requests: r.get(2)?,
                    blocked: r.get(3)?,
                })
            })?;
            Ok(rows.collect::<duckdb::Result<_>>()?)
        };
        let days = totals(&format!(
            "SELECT CAST(d AS VARCHAR), {SUMS} FROM e GROUP BY 1 ORDER BY 1"
        ))?;
        let mut by = Vec::new();
        for (dim, expr) in DIMS {
            by.push((
                dim,
                totals(&format!(
                    "SELECT {expr}, {SUMS} FROM e GROUP BY 1 ORDER BY 2 DESC, 3 DESC LIMIT 50"
                ))?,
            ));
        }
        Ok(Spend { days, by })
    }

    /// Rows of `org` waiting for the next flush, in `[from_us, to_us)`, so range reports include
    /// the last few seconds. (Rows being written at that moment are briefly in neither place.)
    pub fn pending(&self, org: &str, from_us: i64, to_us: i64) -> Vec<Row> {
        let buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        buf.iter()
            .filter(|r| r.org == org && r.ts_us >= from_us && r.ts_us < to_us)
            .cloned()
            .collect()
    }

    /// A DuckDB connection with table `e`: the org's events of the days in `[from_us, to_us)`
    /// plus `pending` rows, or `None` when there are none.
    fn open_range(
        &self,
        org: &str,
        from_us: i64,
        to_us: i64,
        pending: &[Row],
    ) -> Result<Option<Connection>> {
        anyhow::ensure!(safe_id(org), "invalid org id");
        let day = |us: i64| DateTime::from_timestamp_micros(us).map(|t| t.date_naive());
        let (Some(from), Some(to)) = (day(from_us), day(to_us.saturating_sub(1).max(from_us)))
        else {
            anyhow::bail!("time out of range");
        };
        let files: Vec<String> = self
            .days(org)
            .into_iter()
            .filter(|d| *d >= from && *d <= to)
            .flat_map(|d| Self::day_files(&self.day_dir(org, d)))
            .map(|p| sql_str(&p))
            .collect();
        if files.is_empty() && pending.is_empty() {
            return Ok(None);
        }
        let conn = duck()?;
        conn.execute_batch(SCHEMA)?;
        append_rows(&conn, pending)?;
        const COLS: &str =
            r#"ts_us, project, agent, "user", provider, model, decision, cost_micros"#;
        let files = if files.is_empty() {
            String::new()
        } else {
            format!(
                "SELECT {COLS} FROM read_parquet([{}], union_by_name = true) UNION ALL ",
                files.join(", ")
            )
        };
        conn.execute_batch(&format!(
            "CREATE TEMP TABLE e AS {files}SELECT {COLS} FROM u"
        ))?;
        Ok(Some(conn))
    }

    /// Blocking: totals per bucket and top keys per dimension for a [`Range`], plus `pending`
    /// rows (see [`History::pending`]).
    pub fn spend_range(&self, org: &str, r: &Range, pending: &[Row]) -> Result<RangeSpend> {
        let Some(conn) = self.open_range(org, r.from_us, r.to_us, pending)? else {
            return Ok(RangeSpend::default());
        };
        let window = format!("ts_us >= {} AND ts_us < {}", r.from_us, r.to_us);
        let (filter, params) = r.filter.sql();
        let filtered = format!("{window}{filter}");
        let totals = |sql: &str| -> Result<Vec<Totals>> {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map(duckdb::params_from_iter(params.iter()), |row| {
                Ok(Totals {
                    key: row.get(0)?,
                    cost_micros: row.get(1)?,
                    requests: row.get(2)?,
                    blocked: row.get(3)?,
                })
            })?;
            Ok(rows.collect::<duckdb::Result<_>>()?)
        };
        let step = r.bucket.micros();
        let buckets = totals(&format!(
            "SELECT CAST((ts_us // {step}) * {step} AS VARCHAR), {SUMS} FROM e WHERE {filtered}
             GROUP BY 1"
        ))?
        .into_iter()
        .filter_map(|t| Some((t.key.parse().ok()?, t)))
        .collect::<Vec<(i64, Totals)>>();
        let mut buckets = buckets;
        buckets.sort_by_key(|b| b.0); // keys were text, so order here
        let mut by = Vec::new();
        let mut options = Vec::new();
        for (dim, expr) in DIMS {
            by.push((
                dim,
                totals(&format!(
                    "SELECT {expr}, {SUMS} FROM e WHERE {filtered}
                     GROUP BY 1 ORDER BY 2 DESC, 3 DESC LIMIT 50"
                ))?,
            ));
            let mut stmt = conn.prepare(&format!(
                "SELECT {expr} FROM e WHERE {window}
                 GROUP BY 1 ORDER BY sum(cost_micros) DESC, count(*) DESC LIMIT 200"
            ))?;
            let keys = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<duckdb::Result<Vec<_>>>()?;
            options.push((dim, keys));
        }
        Ok(RangeSpend {
            buckets,
            by,
            options,
        })
    }

    /// Blocking: a [`Range`] as CSV, one row per bucket, project, agent, user and model.
    pub fn export_range_csv(&self, org: &str, r: &Range, pending: &[Row]) -> Result<String> {
        let mut out = format!(
            "{},project,agent,user,provider,model,requests,blocked,cost_usd\n",
            r.bucket.name()
        );
        let Some(conn) = self.open_range(org, r.from_us, r.to_us, pending)? else {
            return Ok(out);
        };
        let (filter, params) = r.filter.sql();
        let step = r.bucket.micros();
        let mut stmt = conn.prepare(&format!(
            r#"SELECT (ts_us // {step}) * {step}, project, agent, coalesce("user", ''),
                      coalesce(provider, ''), model, count(*),
                      count_if(starts_with(decision, 'blocked')), CAST(sum(cost_micros) AS BIGINT)
               FROM e WHERE ts_us >= {} AND ts_us < {}{filter}
               GROUP BY ALL ORDER BY 1, 9 DESC"#,
            r.from_us, r.to_us
        ))?;
        let mut rows = stmt.query(duckdb::params_from_iter(params.iter()))?;
        while let Some(row) = rows.next()? {
            let start = DateTime::from_timestamp_micros(row.get::<_, i64>(0)?)
                .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_default();
            let mut cells = vec![start];
            for i in 1..6 {
                cells.push(csv_cell(&row.get::<_, String>(i)?));
            }
            cells.push(row.get::<_, i64>(6)?.to_string());
            cells.push(row.get::<_, i64>(7)?.to_string());
            cells.push(format!("{:.6}", row.get::<_, i64>(8)? as f64 / 1e6));
            out.push_str(&cells.join(","));
            out.push('\n');
        }
        Ok(out)
    }

    /// Blocking: spend between `from` and `to` as CSV, one row per day, project, agent, user and
    /// model.
    pub fn export_csv(&self, org: &str, from: NaiveDate, to: NaiveDate) -> Result<String> {
        let mut out =
            String::from("day,project,agent,user,provider,model,requests,blocked,cost_usd\n");
        let Some(conn) = self.open(org, from, to)? else {
            return Ok(out);
        };
        let mut stmt = conn.prepare(
            r#"SELECT CAST(d AS VARCHAR), project, agent, coalesce("user", ''), coalesce(provider, ''),
                      model, count(*), count_if(starts_with(decision, 'blocked')),
                      CAST(sum(cost_micros) AS BIGINT)
               FROM e GROUP BY ALL ORDER BY 1, 9 DESC"#,
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let mut cells: Vec<String> = (0..6)
                .map(|i| r.get::<_, String>(i))
                .collect::<duckdb::Result<_>>()?;
            cells.push(r.get::<_, i64>(6)?.to_string());
            cells.push(r.get::<_, i64>(7)?.to_string());
            cells.push(format!("{:.6}", r.get::<_, i64>(8)? as f64 / 1e6));
            let line: Vec<String> = cells.iter().map(|c| csv_cell(c)).collect();
            out.push_str(&line.join(","));
            out.push('\n');
        }
        Ok(out)
    }

    /// Blocking: the newest events of a run and its sub-agents, newest first, in the shape of the
    /// dashboard's activity events.
    pub fn run_events(&self, org: &str, run: &str) -> Result<Vec<Value>> {
        let today = Utc::now().date_naive();
        let from = today - chrono::Duration::days(i64::from(self.retention_days));
        let Some(conn) = self.open(org, from, today)? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            r#"SELECT ts_us, project, agent, "user", run_id, parent_run_id, provider, model, decision,
                      blocked_scope, source, request_id, cost_micros, input_tokens,
                      cached_input_tokens, cache_write_tokens, output_tokens, upstream_ms,
                      COALESCE(cache_write_long_tokens, 0), COALESCE(reasoning_tokens, 0)
               FROM e WHERE run_id = ? OR parent_run_id = ? ORDER BY ts_us DESC LIMIT ?"#,
        )?;
        let rows = stmt.query_map(params![run, run, RUN_EVENTS_MAX as i64], |r| {
            let ts = DateTime::from_timestamp_micros(r.get::<_, i64>(0)?).unwrap_or_default();
            Ok(json!({
                "event": "request",
                "ts": ts.to_rfc3339(),
                "org": org,
                "project": r.get::<_, String>(1)?,
                "agent": r.get::<_, String>(2)?,
                "user": r.get::<_, Option<String>>(3)?,
                "run_id": r.get::<_, String>(4)?,
                "parent_run_id": r.get::<_, Option<String>>(5)?,
                "provider": r.get::<_, Option<String>>(6)?,
                "model": r.get::<_, String>(7)?,
                "decision": r.get::<_, String>(8)?,
                "blocked_scope": r.get::<_, Option<String>>(9)?,
                "source": r.get::<_, Option<String>>(10)?,
                "request_id": r.get::<_, Option<String>>(11)?,
                "actual_micros": r.get::<_, i64>(12)?,
                "usage": {
                    "input": r.get::<_, i64>(13)?,
                    "cached_input": r.get::<_, i64>(14)?,
                    "cache_write": r.get::<_, i64>(15)?,
                    "output": r.get::<_, i64>(16)?,
                    "cache_write_long": r.get::<_, i64>(18)?,
                    "reasoning": r.get::<_, i64>(19)?,
                },
                "upstream_ms": r.get::<_, i64>(17)?,
                "from_history": true,
            }))
        })?;
        Ok(rows.collect::<duckdb::Result<_>>()?)
    }
}

impl<R: Record> Store<R> {
    /// Blocking: delete the days past retention, and finish any merge a crash interrupted.
    /// Returns the days (org, day) that are ready to be merged.
    fn sweep(&self, today: NaiveDate) -> Vec<(String, NaiveDate)> {
        let cutoff = today - chrono::Duration::days(i64::from(self.retention_days));
        let mut ready = Vec::new();
        let Ok(orgs) = std::fs::read_dir(self.dir.join(R::DIR)) else {
            return ready;
        };
        let orgs = orgs.filter_map(|e| {
            let name = e.ok()?.file_name();
            name.to_str()?.strip_prefix("org=").map(str::to_string)
        });
        for org in orgs.filter(|o| safe_id(o)) {
            for day in self.days(&org) {
                let dir = self.day_dir(&org, day);
                if day < cutoff {
                    match std::fs::remove_dir_all(&dir) {
                        Ok(()) => {
                            tracing::info!(org = %org, %day, "deleted usage history past retention")
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            tracing::warn!(org = %org, %day, error = %e, "could not delete old usage history")
                        }
                    }
                    continue;
                }
                finish_merge(&dir);
                // Late events (imports, reports with an old timestamp) can land on any day; a
                // finished day is merged again whenever it has more than one file.
                if day < today - chrono::Duration::days(1) && Self::day_files(&dir).len() > 1 {
                    ready.push((org.clone(), day));
                }
            }
        }
        ready
    }

    /// Blocking: merge one day's files into one. The list of files it replaces is written next to
    /// the merged file first, so a crash between publishing it and deleting them is finished by
    /// the next sweep instead of counting those events twice.
    fn merge(&self, org: &str, day: NaiveDate) -> Result<bool> {
        let dir = self.day_dir(org, day);
        let files = Self::day_files(&dir);
        if files.len() < 2 {
            return Ok(false);
        }
        let name = format!(
            "{}-{}-merged.parquet",
            self.node,
            Utc::now().timestamp_micros()
        );
        let tmp = dir.join(format!(".{name}.tmp"));
        let conn = duck()?;
        let list: Vec<String> = files.iter().map(|p| sql_str(p)).collect();
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM read_parquet([{}], union_by_name = true) ORDER BY ts_us)
             TO {} (FORMAT parquet, COMPRESSION zstd)",
            list.join(", "),
            sql_str(&tmp)
        ))?;
        let names: Vec<String> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str().map(str::to_string))
            .collect();
        std::fs::write(dir.join(format!(".{name}.replaces")), names.join("\n"))?;
        std::fs::rename(&tmp, dir.join(&name))?;
        finish_merge(&dir);
        Ok(true)
    }
}

/// Delete the files a published merge replaces, then its list. Also removes temporary files a
/// crash left behind more than an hour ago.
fn finish_merge(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(merged) = name
            .strip_prefix('.')
            .and_then(|n| n.strip_suffix(".replaces"))
        else {
            if name.starts_with('.') && name.ends_with(".tmp") {
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().is_ok_and(|e| e > Duration::from_secs(3600)));
                if stale {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            continue;
        };
        if !dir.join(merged).exists() {
            // Never published: the merge failed before the rename, so its sources stay.
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        if let Ok(list) = std::fs::read_to_string(entry.path()) {
            for f in list
                .lines()
                .filter(|f| !f.is_empty() && !f.contains('/') && *f != merged)
            {
                match std::fs::remove_file(dir.join(f)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!(file = %f, error = %e, "could not delete a merged history file");
                        return;
                    }
                }
            }
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

fn csv_cell(v: &str) -> String {
    // A cell a spreadsheet would run as a formula gets a leading quote.
    let v = if v.starts_with(['=', '+', '-', '@']) {
        format!("'{v}")
    } else {
        v.to_string()
    };
    if v.contains([',', '"', '\n']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

/// Flush every `flush_secs` (sooner when many rows wait), and once an hour delete old days and
/// merge finished ones. Replicas sharing the folder take turns through a Redis lock per day.
pub fn spawn(state: Arc<AppState>) {
    if let Some(h) = state.history.clone() {
        spawn_store(&state, h);
    }
    if let Some(h) = state.step_history.clone() {
        spawn_store(&state, h);
    }
}

fn spawn_store<R: Record>(state: &Arc<AppState>, history: Arc<Store<R>>) {
    let h = history.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(h.flush_secs)) => {}
                _ = h.wake.notified() => {}
            }
            if let Err(e) = h.flush().await {
                tracing::warn!(error = %e, kind = R::DIR, "could not write history");
                crate::metrics::history_write_failed();
            }
        }
    });
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let Some(state) = weak.upgrade() else { return };
            if let Err(e) = maintain(&state, &history).await {
                tracing::warn!(error = %e, kind = R::DIR, "history maintenance failed");
            }
        }
    });
}

/// Delete days past retention and merge finished days; returns the days merged.
pub async fn maintain<R: Record>(state: &AppState, history: &Arc<Store<R>>) -> Result<usize> {
    let h = history.clone();
    let today = Utc::now().date_naive();
    let ready = tokio::task::spawn_blocking(move || h.sweep(today)).await?;
    let mut merged = 0;
    for (org, day) in ready {
        let mut conn = state.ledger.conn();
        let got: Option<String> = redis::cmd("SET")
            .arg(format!("tl:history:merge:{}:{org}:{day}", R::DIR))
            .arg(&history.node)
            .arg("NX")
            .arg("EX")
            .arg(900)
            .query_async(&mut conn)
            .await
            .unwrap_or(None);
        if got.is_none() {
            continue;
        }
        let h = history.clone();
        let o = org.clone();
        match tokio::task::spawn_blocking(move || h.merge(&o, day)).await? {
            Ok(true) => merged += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(org = %org, %day, error = %e, kind = R::DIR, "could not merge history")
            }
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tokenrein-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn history(dir: &Path) -> Arc<History> {
        History::from_config(&HistoryConfig {
            dir: Some(dir.to_string_lossy().to_string()),
            retention_days: 30,
            flush_secs: 60,
        })
        .unwrap()
    }

    fn event(ts: &str, run: &str, cost: i64, decision: &str, user: Option<&str>) -> Value {
        json!({
            "event": "request", "ts": ts, "org": "acme", "project": "bot", "agent": "writer",
            "user": user, "run_id": run, "parent_run_id": null, "provider": "openai",
            "model": "gpt-4o-mini", "decision": decision, "actual_micros": cost,
            "usage": {"input": 100, "cached_input": 0, "cache_write": 0, "output": 20},
            "upstream_ms": 30,
        })
    }

    #[test]
    fn rows_need_an_org_and_a_run() {
        assert!(Row::from_event(&json!({"org": "acme"})).is_none());
        assert!(Row::from_event(&json!({"org": "..", "run_id": "r"})).is_none());
        let r =
            Row::from_event(&event("2026-03-04T10:00:00Z", "r1", 1500, "allowed", None)).unwrap();
        assert_eq!(r.day, NaiveDate::from_ymd_opt(2026, 3, 4).unwrap());
        assert_eq!(
            (r.cost_micros, r.input_tokens, r.output_tokens),
            (1500, 100, 20)
        );
    }

    #[test]
    fn csv_cells_are_quoted_and_defused() {
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("=1+1"), "'=1+1");
        assert_eq!(csv_cell("plain"), "plain");
    }

    #[tokio::test]
    async fn writes_reads_merges_and_expires() {
        let dir = temp_dir();
        let h = history(&dir);
        let today = Utc::now().date_naive();
        let at = |days: i64| {
            (today - chrono::Duration::days(days))
                .format("%Y-%m-%dT12:00:00Z")
                .to_string()
        };
        let (old, expired) = (at(10), at(45));
        h.push(&[
            event(&old, "r1", 1_000_000, "allowed", Some("ann@example.com")),
            event(&old, "r1", 500_000, "allowed", Some("ann@example.com")),
            event(&old, "r2", 0, "blocked_budget", None),
            event(&expired, "r0", 9_000_000, "allowed", None),
        ]);
        assert_eq!(h.flush().await.unwrap(), 4);
        // A second file for the same day, as from another flush or replica.
        h.push(&[event(&old, "r3", 250_000, "allowed", None)]);
        assert_eq!(h.flush().await.unwrap(), 1);

        let from = today - chrono::Duration::days(30);
        let s = h.spend("acme", from, today).unwrap();
        assert_eq!(s.days.len(), 1);
        assert_eq!(s.days[0].cost_micros, 1_750_000);
        assert_eq!((s.days[0].requests, s.days[0].blocked), (4, 1));
        let users = &s.by.iter().find(|(d, _)| *d == "user").unwrap().1;
        assert_eq!(users[0].key, "ann@example.com");
        assert_eq!(users[0].cost_micros, 1_500_000);

        let csv = h.export_csv("acme", from, today).unwrap();
        assert!(
            csv.starts_with("day,project,agent,user,provider,model,requests,blocked,cost_usd\n")
        );
        assert!(
            csv.contains("ann@example.com,openai,gpt-4o-mini,2,0,1.500000"),
            "{csv}"
        );

        let events = h.run_events("acme", "r1").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["usage"]["output"], 20);

        let day = NaiveDate::parse_from_str(&old[..10], "%Y-%m-%d").unwrap();
        assert_eq!(h.sweep(today), vec![("acme".to_string(), day)]);
        assert!(
            !h.day_dir("acme", today - chrono::Duration::days(45))
                .exists(),
            "the day past retention is deleted"
        );
        assert!(h.merge("acme", day).unwrap());
        assert_eq!(History::day_files(&h.day_dir("acme", day)).len(), 1);
        let after = h.spend("acme", from, today).unwrap();
        assert_eq!(
            after.days[0].cost_micros, 1_750_000,
            "merging keeps every event once"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_interrupted_merge_is_finished_without_double_counting() {
        let dir = temp_dir();
        for f in ["a.parquet", "b.parquet", "m.parquet"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        std::fs::write(dir.join(".m.parquet.replaces"), "a.parquet\nb.parquet").unwrap();
        finish_merge(&dir);
        let left: Vec<String> = History::day_files(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, vec!["m.parquet"]);
        assert!(!dir.join(".m.parquet.replaces").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
