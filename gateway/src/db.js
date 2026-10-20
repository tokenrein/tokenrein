import Database from 'better-sqlite3';
import { mkdirSync } from 'node:fs';
import { dirname } from 'node:path';

const DB_PATH = process.env.GATEWAY_DB_PATH || '/app/data/accounts.db';

let db;

export function getDb() {
  if (db) return db;
  mkdirSync(dirname(DB_PATH), { recursive: true });
  db = new Database(DB_PATH);
  db.pragma('journal_mode = WAL');
  db.exec(`
    CREATE TABLE IF NOT EXISTS accounts (
      id TEXT PRIMARY KEY,
      email TEXT UNIQUE NOT NULL,
      stripe_customer_id TEXT,
      stripe_subscription_id TEXT,
      plan TEXT NOT NULL DEFAULT 'free',
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );
  `);
  return db;
}

export function upsertAccountByEmail(email) {
  const now = new Date().toISOString();
  const existing = getDb().prepare('SELECT * FROM accounts WHERE email = ?').get(email);
  if (existing) return existing;
  const id = crypto.randomUUID();
  getDb()
    .prepare(
      'INSERT INTO accounts (id, email, plan, created_at, updated_at) VALUES (?, ?, ?, ?, ?)'
    )
    .run(id, email, 'free', now, now);
  return getDb().prepare('SELECT * FROM accounts WHERE id = ?').get(id);
}

export function setStripeInfo(email, { stripeCustomerId, stripeSubscriptionId, plan }) {
  const now = new Date().toISOString();
  getDb()
    .prepare(
      `UPDATE accounts
       SET stripe_customer_id = ?, stripe_subscription_id = ?, plan = ?, updated_at = ?
       WHERE email = ?`
    )
    .run(stripeCustomerId, stripeSubscriptionId, plan, now, email);
}

export function getAccountByEmail(email) {
  return getDb().prepare('SELECT * FROM accounts WHERE email = ?').get(email);
}

export function getAccountByStripeCustomerId(stripeCustomerId) {
  return getDb()
    .prepare('SELECT * FROM accounts WHERE stripe_customer_id = ?')
    .get(stripeCustomerId);
}
