export const PLANS = {
  free: {
    id: 'free',
    name: 'Free',
    priceMonthly: 0,
    observationsIncluded: 50_000,
    stripePriceEnv: null,
  },
  starter: {
    id: 'starter',
    name: 'Starter',
    priceMonthly: 15,
    observationsIncluded: 500_000,
    stripePriceEnv: 'STRIPE_PRICE_STARTER',
  },
  growth: {
    id: 'growth',
    name: 'Growth',
    priceMonthly: 49,
    observationsIncluded: 3_000_000,
    stripePriceEnv: 'STRIPE_PRICE_GROWTH',
  },
};

export function getPlan(planId) {
  return PLANS[planId] ?? PLANS.free;
}

export function listPlansForClient() {
  return Object.values(PLANS).map(({ id, name, priceMonthly, observationsIncluded }) => ({
    id,
    name,
    priceMonthly,
    observationsIncluded,
  }));
}
