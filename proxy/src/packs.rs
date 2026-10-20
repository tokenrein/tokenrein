//! Guardrail packs: starter budget sets for common kinds of agents.
//!
//! Applying a pack to a project sets its budgets in Check mode (allow and warn), so nothing is
//! blocked until someone reviews what would have been blocked and switches a budget to Enforce.

use serde::Serialize;

use crate::config::{BudgetPolicy, Budgets, Mode};

#[derive(Debug, Clone, Serialize)]
pub struct Pack {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub budgets: Budgets,
}

fn check(limit_usd: f64) -> Option<BudgetPolicy> {
    Some(BudgetPolicy {
        limit_usd,
        mode: Mode::Check,
    })
}

pub fn all() -> Vec<Pack> {
    vec![
        Pack {
            id: "coding-agents",
            title: "Coding agents",
            description: "Claude Code, Codex, Cursor and similar tools on developer machines: \
                          a daily budget per developer agent and a cap per session.",
            budgets: Budgets {
                org_monthly: None,
                project_monthly: check(1000.0),
                agent_daily: check(25.0),
                run: check(10.0),
                max_run_usd: Some(50.0),
            },
        },
        Pack {
            id: "production-agents",
            title: "Production agents",
            description: "Customer-facing agents: a small budget per run (one task or \
                          conversation) so a loop is stopped early, plus a daily budget per agent.",
            budgets: Budgets {
                org_monthly: None,
                project_monthly: check(2000.0),
                agent_daily: check(100.0),
                run: check(2.0),
                max_run_usd: Some(10.0),
            },
        },
        Pack {
            id: "batch-jobs",
            title: "Batch and background jobs",
            description: "Long-running pipelines and evaluations: a large per-run budget and a \
                          monthly budget for the project.",
            budgets: Budgets {
                org_monthly: None,
                project_monthly: check(5000.0),
                agent_daily: None,
                run: check(50.0),
                max_run_usd: Some(200.0),
            },
        },
    ]
}

pub fn find(id: &str) -> Option<Pack> {
    all().into_iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_start_in_check_mode_and_never_set_org_budgets() {
        for p in all() {
            let b = &p.budgets;
            assert!(b.org_monthly.is_none(), "{}", p.id);
            for policy in [b.project_monthly, b.agent_daily, b.run]
                .into_iter()
                .flatten()
            {
                assert_eq!(policy.mode, Mode::Check, "{}", p.id);
            }
        }
        assert!(find("coding-agents").is_some());
        assert!(find("nope").is_none());
    }
}
