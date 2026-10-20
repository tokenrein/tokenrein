import pg from 'pg';

const { Pool } = pg;

let pgPool;

function getPgPool() {
  if (!pgPool) {
    pgPool = new Pool({ connectionString: process.env.LANGFUSE_PG_URL });
  }
  return pgPool;
}

// Best-effort: joins Langfuse's own multi-tenant schema (users -> org memberships
// -> organizations/projects) to find which Langfuse org a billing email belongs to.
// Table/column names match Langfuse OSS as of v4; re-verify against the deployed
// image's schema before relying on this for billing enforcement (see README).
export async function findLangfuseOrgIdByEmail(email) {
  const { rows } = await getPgPool().query(
    `SELECT o.id AS org_id, o.name AS org_name
     FROM users u
     JOIN organization_memberships om ON om.user_id = u.id
     JOIN organizations o ON o.id = om.org_id
     WHERE u.email = $1
     LIMIT 1`,
    [email]
  );
  return rows[0] ?? null;
}

async function queryClickhouse(sql) {
  const url = `${process.env.LANGFUSE_CLICKHOUSE_URL}/?query=${encodeURIComponent(sql)}&default_format=JSON`;
  const auth = Buffer.from(
    `${process.env.LANGFUSE_CLICKHOUSE_USER}:${process.env.LANGFUSE_CLICKHOUSE_PASSWORD}`
  ).toString('base64');
  const res = await fetch(url, { headers: { Authorization: `Basic ${auth}` } });
  if (!res.ok) throw new Error(`ClickHouse query failed: ${res.status} ${await res.text()}`);
  return res.json();
}

export async function getMonthlyObservationCount(orgId) {
  const sql = `
    SELECT count(*) AS count
    FROM traces
    WHERE project_id IN (
      SELECT id FROM projects WHERE org_id = '${orgId}'
    )
    AND timestamp >= toStartOfMonth(now())
  `.replace(/\s+/g, ' ');
  try {
    const result = await queryClickhouse(sql);
    return Number(result.data?.[0]?.count ?? 0);
  } catch {
    return null;
  }
}
