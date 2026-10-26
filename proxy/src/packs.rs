//! Guardrail packs: starter budget sets for common kinds of agents.
//!
//! Applying a pack to a project sets its budgets in Check mode (allow and warn), so nothing is
//! blocked until someone reviews what would have been blocked and switches a budget to Enforce.
//! Loop detection likewise only alerts until it is switched to pause or kill.

use serde::Serialize;

use crate::config::{BudgetPolicy, Budgets, LoopAction, LoopPolicy, Mode};

#[derive(Debug, Clone, Serialize)]
pub struct Pack {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub budgets: Budgets,
}

/// Loop detection that only alerts, like Check mode for budgets.
fn warn_loops(max_repeats: u32, usd_per_minute: f64, max_calls: Option<u32>) -> Option<LoopPolicy> {
    Some(LoopPolicy {
        action: LoopAction::Warn,
        max_repeats,
        repeat_window_secs: 600,
        max_usd_per_minute: Some(usd_per_minute),
        max_calls_per_run: max_calls,
    })
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
                loops: warn_loops(8, 5.0, Some(400)),
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
                loops: warn_loops(5, 1.0, Some(100)),
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
                loops: warn_loops(20, 10.0, None),
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
