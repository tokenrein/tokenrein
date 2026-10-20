import Stripe from 'stripe';
import { PLANS } from './plans.js';
import { setStripeInfo, getAccountByStripeCustomerId } from './db.js';

const stripe = process.env.STRIPE_SECRET_KEY
  ? new Stripe(process.env.STRIPE_SECRET_KEY)
  : null;

export function stripeEnabled() {
  return Boolean(stripe);
}

export async function createCheckoutSession({ email, planId, baseUrl }) {
  if (!stripe) throw new Error('Stripe is not configured (set STRIPE_SECRET_KEY)');
  const plan = PLANS[planId];
  if (!plan || !plan.stripePriceEnv) throw new Error(`Unknown paid plan: ${planId}`);
  const priceId = process.env[plan.stripePriceEnv];
  if (!priceId) throw new Error(`Missing env var ${plan.stripePriceEnv} for plan ${planId}`);

  return stripe.checkout.sessions.create({
    mode: 'subscription',
    customer_email: email,
    line_items: [{ price: priceId, quantity: 1 }],
    success_url: `${baseUrl}/?checkout=success`,
    cancel_url: `${baseUrl}/?checkout=cancelled`,
    metadata: { planId },
  });
}

export function verifyWebhookSignature(rawBody, signature) {
  if (!stripe) throw new Error('Stripe is not configured (set STRIPE_SECRET_KEY)');
  return stripe.webhooks.constructEvent(rawBody, signature, process.env.STRIPE_WEBHOOK_SECRET);
}

export async function handleStripeEvent(event) {
  switch (event.type) {
    case 'checkout.session.completed': {
      const session = event.data.object;
      const email = session.customer_email ?? session.customer_details?.email;
      const planId = session.metadata?.planId ?? 'starter';
      if (email) {
        setStripeInfo(email, {
          stripeCustomerId: session.customer,
          stripeSubscriptionId: session.subscription,
          plan: planId,
        });
      }
      break;
    }
    case 'customer.subscription.deleted': {
      const subscription = event.data.object;
      const account = getAccountByStripeCustomerId(subscription.customer);
      if (account) {
        setStripeInfo(account.email, {
          stripeCustomerId: account.stripe_customer_id,
          stripeSubscriptionId: null,
          plan: 'free',
        });
      }
      break;
    }
    default:
      break;
  }
}
