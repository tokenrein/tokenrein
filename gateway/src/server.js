import 'dotenv/config';
import express from 'express';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { upsertAccountByEmail, getAccountByEmail } from './db.js';
import { listPlansForClient } from './plans.js';
import { createCheckoutSession, verifyWebhookSignature, handleStripeEvent, stripeEnabled } from './stripe.js';
import { findLangfuseOrgIdByEmail, getMonthlyObservationCount } from './usage.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const app = express();
const PORT = process.env.PORT || 8080;
const PUBLIC_BASE_URL = process.env.PUBLIC_BASE_URL || `http://localhost:${PORT}`;
const LANGFUSE_URL = process.env.LANGFUSE_URL || 'http://localhost:3000';

app.get('/_health', (_req, res) => res.json({ ok: true }));

app.get('/pricing.json', (_req, res) => {
  res.json({ plans: listPlansForClient(), langfuseUrl: LANGFUSE_URL, stripeEnabled: stripeEnabled() });
});

app.post('/billing/checkout', express.json(), async (req, res) => {
  const { email, planId } = req.body ?? {};
  if (!email || !planId) return res.status(400).json({ error: 'email and planId are required' });
  try {
    upsertAccountByEmail(email);
    if (planId === 'free') {
      return res.json({ url: `${PUBLIC_BASE_URL}/?signup=free` });
    }
    const session = await createCheckoutSession({ email, planId, baseUrl: PUBLIC_BASE_URL });
    res.json({ url: session.url });
  } catch (err) {
    res.status(400).json({ error: err.message });
  }
});

// Stripe needs the raw body to verify the webhook signature, so this route
// is registered before the global express.json() middleware is attached.
app.post('/billing/webhook', express.raw({ type: 'application/json' }), async (req, res) => {
  try {
    const event = verifyWebhookSignature(req.body, req.headers['stripe-signature']);
    await handleStripeEvent(event);
    res.json({ received: true });
  } catch (err) {
    res.status(400).send(`Webhook Error: ${err.message}`);
  }
});

app.get('/account/:email', async (req, res) => {
  const account = getAccountByEmail(req.params.email);
  if (!account) return res.status(404).json({ error: 'not found' });

  let usage = null;
  try {
    const org = await findLangfuseOrgIdByEmail(req.params.email);
    if (org) {
      usage = { orgId: org.org_id, observationsThisMonth: await getMonthlyObservationCount(org.org_id) };
    }
  } catch {
    usage = { error: 'usage lookup unavailable (is Langfuse reachable?)' };
  }

  res.json({ ...account, langfuseUrl: LANGFUSE_URL, usage });
});

app.use(express.static(join(__dirname, '..', 'public')));

app.listen(PORT, () => {
  console.log(`tokenrein gateway listening on :${PORT}`);
});
