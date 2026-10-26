mod common;

use std::sync::Arc;

use tokenrein::config::Mode;
use tokenrein::ledger::{Keys, Ledger, LimitSpec, Reservation, Scope, Settlement, unix_now};

async fn ledger() -> Option<(common::RedisServer, Ledger)> {
    let server = common::RedisServer::start()?;
    let ledger = Ledger::connect(&server.url)
        .await
        .expect("connect to test redis");
    Some((server, ledger))
}

fn scope(label: &str, key: String, limit: LimitSpec, mode: Mode) -> Scope {
    Scope {
        label: label.into(),
        key,
        limit,
        mode,
        ttl_secs: 3600,
    }
}

fn scopes(org: &str, run: &str, run_limit: u64, run_mode: Mode) -> Vec<Scope> {
    let k = Keys::new(org);
    vec![
        scope(
            "run",
            k.run_budget(run),
            LimitSpec::SetIfAbsent(run_limit),
            run_mode,
        ),
        scope(
            "agent",
            k.agent_daily("p", "a", "20261018"),
            LimitSpec::Unlimited,
            Mode::Skip,
        ),
        scope(
            "project",
            k.project_monthly("p", "202610"),
            LimitSpec::Unlimited,
            Mode::Skip,
        ),
        scope(
            "org",
            k.org_monthly("202610"),
            LimitSpec::Set(1_000_000_000),
            Mode::Enforce,
        ),
    ]
}

async fn field(l: &Ledger, key: &str, name: &str) -> i64 {
    l.budget(key)
        .await
        .unwrap()
        .get(name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn later() -> i64 {
    unix_now() + 600
}

#[tokio::test]
async fn blocks_when_reservation_would_exceed_limit_then_settles() {
    let Some((_redis, l)) = ledger().await else {
        return;
    };
    let s = scopes("o1", "r1", 1_000_000, Mode::Enforce);
    let runs = vec!["r1".to_string()];
    let allowed = Reservation::Allowed {
        check_exceeded: vec![],
    };

    assert_eq!(
        l.reserve("o1", "q1", 400_000, &s, &runs, later(), 3600)
            .await
            .unwrap(),
        allowed
    );
    assert_eq!(
        l.reserve("o1", "q2", 400_000, &s, &runs, later(), 3600)
            .await
            .unwrap(),
        allowed
    );
    assert_eq!(
        l.reserve("o1", "q3", 400_000, &s, &runs, later(), 3600)
            .await
            .unwrap(),
        Reservation::OverBudget {
            label: "run".into(),
            spent: 0,
            reserved: 800_000,
            limit: 1_000_000,
            mode: Mode::Enforce,
        }
    );

    assert_eq!(
        l.settle("o1", "q1", 100_000, &s).await.unwrap(),
        Settlement::Settled
    );
    let run_key = Keys::new("o1").run_budget("r1");
    assert_eq!(field(&l, &run_key, "spent").await, 100_000);
    assert_eq!(field(&l, &run_key, "reserved").await, 400_000);
    assert_eq!(
        l.reserve("o1", "q3", 400_000, &s, &runs, later(), 3600)
            .await
            .unwrap(),
        allowed
    );
    let org_key = Keys::new("o1").org_monthly("202610");
    assert_eq!(field(&l, &org_key, "spent").await, 100_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_reservations_never_exceed_the_limit() {
    let Some((_redis, l)) = ledger().await else {
        return;
    };
    let l = Arc::new(l);
    let s = Arc::new(scopes("o2", "r1", 100_000, Mode::Enforce));
    let mut tasks = Vec::new();
    for i in 0..500 {
        let (l, s) = (l.clone(), s.clone());
        tasks.push(tokio::spawn(async move {
            l.reserve(
                "o2",
                &format!("q{i}"),
                1_000,
                &s,
                &["r1".to_string()],
                later(),
                3600,
            )
            .await
            .unwrap()
        }));
    }
    let mut allowed = 0;
    for t in tasks {
        if matches!(t.await.unwrap(), Reservation::Allowed { .. }) {
            allowed += 1;
        }
    }
    assert_eq!(allowed, 100);
    let run_key = Keys::new("o2").run_budget("r1");
    assert_eq!(field(&l, &run_key, "reserved").await, 100_000);

    for i in 0..500 {
        l.settle("o2", &format!("q{i}"), 900, &s).await.unwrap();
    }
    assert_eq!(field(&l, &run_key, "reserved").await, 0);
    assert_eq!(field(&l, &run_key, "spent").await, 100 * 900 + 400 * 900);
}

#[tokio::test]
async fn check_mode_allows_and_reports() {
    let Some((_redis, l)) = ledger().await else {
        return;
    };
    let s = scopes("o3", "r1", 1_000, Mode::Check);
    let out = l
        .reserve("o3", "q1", 5_000, &s, &["r1".to_string()], later(), 3600)
        .await
        .unwrap();
    assert_eq!(
        out,
        Reservation::Allowed {
            check_exceeded: vec!["run".into()]
        }
    );
}

#[tokio::test]
async fn run_budget_is_fixed_at_creation() {
    let Some((_redis, l)) = ledger().await else {
        return;
    };
    let runs = vec!["r1".to_string()];
    l.reserve(
        "o4",
        "q1",
        10,
        &scopes("o4", "r1", 1_000, Mode::Enforce),
        &runs,
        later(),
        3600,
    )
    .await
    .unwrap();
    let raised = scopes("o4", "r1", 1_000_000, Mode::Skip);
    let out = l
        .reserve("o4", "q2", 5_000, &raised, &runs, later(), 3600)
        .await
        .unwrap();
    assert!(
        matches!(out, Reservation::OverBudget { limit: 1_000, .. }),
        "{out:?}"
    );
}

#[tokio::test]
async fn paused_ancestor_blocks_children_until_resumed() {
    let Some((_redis, l)) = ledger().await else {
        return;
    };
    let s = scopes("o5", "child", 1_000_000, Mode::Enforce);
    let runs = vec!["child".to_string(), "parent".to_string()];
    l.set_run_state("o5", "parent", "paused").await.unwrap();
    assert_eq!(
        l.reserve("o5", "q1", 10, &s, &runs, later(), 3600)
            .await
            .unwrap(),
        Reservation::RunStopped {
            run_id: "parent".into(),
            state: "paused".into()
        }
    );
    l.clear_run_state("o5", "parent").await.unwrap();
    assert!(matches!(
        l.reserve("o5", "q1", 10, &s, &runs, later(), 3600)
            .await
            .unwrap(),
        Reservation::Allowed { .. }
    ));
}

#[tokio::test]
async fn sweeper_releases_expired_reservations_and_late_settle_still_counts() {
    let Some((_redis, l)) = ledger().await else {
        return;
    };
    let s = scopes("o6", "r1", 1_000_000, Mode::Enforce);
    l.reserve(
        "o6",
        "q1",
        50_000,
        &s,
        &["r1".to_string()],
        unix_now() - 1,
        3600,
    )
    .await
    .unwrap();
    assert_eq!(l.sweep("o6", unix_now(), 100).await.unwrap(), 1);
    let run_key = Keys::new("o6").run_budget("r1");
    assert_eq!(field(&l, &run_key, "reserved").await, 0);

    assert_eq!(
        l.settle("o6", "q1", 7_000, &s).await.unwrap(),
        Settlement::Late
    );
    assert_eq!(field(&l, &run_key, "spent").await, 7_000);
    assert_eq!(field(&l, &run_key, "reserved").await, 0);
    assert_eq!(l.sweep("o6", unix_now(), 100).await.unwrap(), 0);
}

#[tokio::test]
async fn settle_is_idempotent() {
    let Some((_redis, l)) = ledger().await else {
        return;
    };
    let s = scopes("o7", "r1", 1_000_000, Mode::Enforce);
    l.reserve("o7", "q1", 5_000, &s, &["r1".to_string()], later(), 3600)
        .await
        .unwrap();
    assert_eq!(
        l.settle("o7", "q1", 3_000, &s).await.unwrap(),
        Settlement::Settled
    );
    assert_eq!(
        l.settle("o7", "q1", 3_000, &s).await.unwrap(),
        Settlement::Duplicate
    );
    let run_key = Keys::new("o7").run_budget("r1");
    assert_eq!(field(&l, &run_key, "spent").await, 3_000);
}

#[tokio::test]
async fn ancestry_follows_the_parent_chain() {
    let Some((_redis, l)) = ledger().await else {
        return;
    };
    assert_eq!(
        *l.ancestors("o8", "child", "root").await.unwrap(),
        vec!["root".to_string()]
    );
    assert_eq!(
        *l.ancestors("o8", "grandchild", "child").await.unwrap(),
        vec!["child".to_string(), "root".to_string()]
    );
}
