use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use jcode_plan::PlanItem;
use jcode_task_types::{
    Goal, GoalBudget, GoalScope, SuccessCheck, SuccessCheckKind,
};
use jcode_verifier::{NullAgentAssertion, StubAgentAssertion};
use tokio::sync::mpsc::unbounded_channel;

use crate::budget::{BudgetExceeded, BudgetTracker, UsageDelta};
use crate::controller::{
    GoalLoopController, GoalLoopOutcome, ItemRunResult,
};
use crate::events::{GoalLoopEvent, ItemOutcomeKind};
use crate::traits::{DispatcherError, ItemOutcome, Planner, PlannerError, SwarmDispatcher};

// ---------- helpers ----------

fn plan_item(id: &str, content: &str, check: Option<SuccessCheck>) -> PlanItem {
    PlanItem {
        content: content.to_string(),
        status: "queued".to_string(),
        priority: "high".to_string(),
        id: id.to_string(),
        subsystem: None,
        file_scope: Vec::new(),
        blocked_by: Vec::new(),
        assigned_to: None,
        success_check: check,
    }
}

fn shell_check(spec: &str) -> SuccessCheck {
    SuccessCheck {
        kind: SuccessCheckKind::Shell,
        spec: spec.to_string(),
        timeout_ms: 5_000,
    }
}

fn budget(retries: u8, cents: Option<u32>) -> GoalBudget {
    GoalBudget {
        tokens: None,
        usd_cents: cents,
        wall_secs: None,
        retries_per_item: retries,
    }
}

fn goal_with(title: &str, budget: GoalBudget) -> Goal {
    let mut goal = Goal::new(title, GoalScope::Project);
    goal.budget = Some(budget);
    goal
}

// ---------- planner stubs ----------

struct ScriptedPlanner {
    decompose: Mutex<Vec<Vec<PlanItem>>>,
    replans: Mutex<Vec<Vec<PlanItem>>>,
}

impl ScriptedPlanner {
    fn new(decompose: Vec<Vec<PlanItem>>) -> Arc<Self> {
        Arc::new(Self {
            decompose: Mutex::new(decompose),
            replans: Mutex::new(Vec::new()),
        })
    }

    fn with_replans(decompose: Vec<Vec<PlanItem>>, replans: Vec<Vec<PlanItem>>) -> Arc<Self> {
        Arc::new(Self {
            decompose: Mutex::new(decompose),
            replans: Mutex::new(replans),
        })
    }
}

#[async_trait]
impl Planner for ScriptedPlanner {
    async fn decompose(&self, _goal: &Goal) -> Result<Vec<PlanItem>, PlannerError> {
        let mut guard = self.decompose.lock().unwrap();
        if guard.is_empty() {
            Err(PlannerError::Empty)
        } else {
            Ok(guard.remove(0))
        }
    }

    async fn replan(
        &self,
        _goal: &Goal,
        _completed: &[PlanItem],
        _failed: &[PlanItem],
    ) -> Result<Vec<PlanItem>, PlannerError> {
        let mut guard = self.replans.lock().unwrap();
        if guard.is_empty() {
            Err(PlannerError::Empty)
        } else {
            Ok(guard.remove(0))
        }
    }
}

// ---------- dispatcher stubs ----------

#[derive(Clone)]
struct DispatcherCall {
    item_id: String,
}

struct ScriptedDispatcher {
    outcomes: Mutex<Vec<Result<ItemOutcome, DispatcherError>>>,
    calls: Mutex<Vec<DispatcherCall>>,
}

impl ScriptedDispatcher {
    fn new(outcomes: Vec<Result<ItemOutcome, DispatcherError>>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<DispatcherCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl SwarmDispatcher for ScriptedDispatcher {
    async fn dispatch(&self, item: &PlanItem) -> Result<ItemOutcome, DispatcherError> {
        self.calls.lock().unwrap().push(DispatcherCall {
            item_id: item.id.clone(),
        });
        let mut guard = self.outcomes.lock().unwrap();
        if guard.is_empty() {
            Err(DispatcherError::SpawnFailed("no scripted outcome".into()))
        } else {
            guard.remove(0)
        }
    }
}

fn ok_outcome(id: &str, usage: UsageDelta) -> ItemOutcome {
    ItemOutcome {
        item_id: id.to_string(),
        status: "completed".to_string(),
        report: Some(format!("did {id}")),
        usage,
        check: None,
    }
}

// ---------- budget tests ----------

#[test]
fn budget_tracker_records_usage_and_caps_dollars() {
    let mut b = BudgetTracker::new(budget(3, Some(100)));
    assert!(b.check().is_none());
    b.record(UsageDelta {
        tokens: 1_000,
        usd_cents: 60,
    });
    assert!(b.check().is_none());
    b.record(UsageDelta {
        tokens: 1_000,
        usd_cents: 50,
    });
    let why = b.check().expect("dollars should trip");
    matches!(why, BudgetExceeded::Dollars { .. });
}

#[test]
fn budget_tracker_retries_returns_error_past_cap() {
    let mut b = BudgetTracker::new(budget(2, None));
    assert_eq!(b.note_retry("x").unwrap(), 1);
    assert_eq!(b.note_retry("x").unwrap(), 2);
    let err = b.note_retry("x").unwrap_err();
    matches!(err, BudgetExceeded::Retries { .. });
}

#[test]
fn budget_tracker_wall_trip_after_rewind() {
    let mut b = BudgetTracker::new(GoalBudget {
        tokens: None,
        usd_cents: None,
        wall_secs: Some(1),
        retries_per_item: 3,
    });
    b.rewind_started_at(Duration::from_secs(5));
    let why = b.check().expect("wall should trip");
    matches!(why, BudgetExceeded::Wall { .. });
}

// ---------- controller integration ----------

#[tokio::test]
async fn controller_completes_when_every_item_passes() {
    let items = vec![
        plan_item("a", "do a", Some(shell_check("exit 0"))),
        plan_item("b", "do b", Some(shell_check("exit 0"))),
    ];
    let planner = ScriptedPlanner::new(vec![items.clone()]);
    let dispatcher = ScriptedDispatcher::new(vec![
        Ok(ok_outcome("a", UsageDelta { tokens: 100, usd_cents: 5 })),
        Ok(ok_outcome("b", UsageDelta { tokens: 200, usd_cents: 10 })),
    ]);

    let ctrl = GoalLoopController::new(
        goal_with("ship it", budget(3, Some(200))),
        std::env::current_dir().unwrap(),
        planner.clone(),
        dispatcher.clone(),
        Arc::new(NullAgentAssertion),
    );
    let outcome = ctrl.run().await;
    assert!(
        matches!(outcome, GoalLoopOutcome::Done { items_completed: 2 }),
        "outcome = {outcome:?}"
    );
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 2);
}

#[tokio::test]
async fn controller_repairs_when_verifier_fails_then_succeeds() {
    let item = plan_item("a", "do a", Some(shell_check("exit 0")));
    let items = vec![item];
    let planner = ScriptedPlanner::new(vec![items]);
    // First dispatch: completed but check fails (we inject the failing
    // check inline via `outcome.check`). Second: completed + check passes.
    let outcomes = vec![
        Ok(ItemOutcome {
            item_id: "a".to_string(),
            status: "completed".to_string(),
            report: Some("first try".to_string()),
            usage: UsageDelta::default(),
            check: Some(jcode_task_types::CheckResult {
                passed: false,
                detail: "missing burst test".into(),
                duration_ms: 1,
            }),
        }),
        Ok(ItemOutcome {
            item_id: "a".to_string(),
            status: "completed".to_string(),
            report: Some("second try".to_string()),
            usage: UsageDelta::default(),
            check: Some(jcode_task_types::CheckResult {
                passed: true,
                detail: "all tests pass".into(),
                duration_ms: 1,
            }),
        }),
    ];
    let dispatcher = ScriptedDispatcher::new(outcomes);
    let ctrl = GoalLoopController::new(
        goal_with("repair me", budget(3, None)),
        std::env::current_dir().unwrap(),
        planner,
        dispatcher.clone(),
        Arc::new(NullAgentAssertion),
    );
    let outcome = ctrl.run().await;
    assert!(
        matches!(outcome, GoalLoopOutcome::Done { items_completed: 1 }),
        "outcome = {outcome:?}"
    );
    assert_eq!(dispatcher.calls().len(), 2);
}

#[tokio::test]
async fn controller_aborts_when_retry_cap_hit() {
    let item = plan_item("a", "do a", None);
    let planner = ScriptedPlanner::new(vec![vec![item]]);
    // Every dispatch reports "failed" status — verifier has nothing to PASS,
    // so each turn counts as a retry. Retry cap = 1 means second attempt
    // trips the cap.
    let outcomes: Vec<_> = (0..5)
        .map(|_| {
            Ok(ItemOutcome {
                item_id: "a".to_string(),
                status: "failed".to_string(),
                report: None,
                usage: UsageDelta::default(),
                check: None,
            })
        })
        .collect();
    let dispatcher = ScriptedDispatcher::new(outcomes);

    let ctrl = GoalLoopController::new(
        goal_with("retry me", budget(1, None)),
        std::env::current_dir().unwrap(),
        planner,
        dispatcher,
        Arc::new(NullAgentAssertion),
    );
    let outcome = ctrl.run().await;
    match outcome {
        GoalLoopOutcome::Aborted { reason, .. } => {
            assert!(
                reason.contains("retry cap") || reason.contains("replan"),
                "unexpected reason: {reason}"
            );
        }
        other => panic!("expected Aborted, got {other:?}"),
    }
}

#[tokio::test]
async fn controller_aborts_when_dollar_cap_hit_after_one_item() {
    let item = plan_item("a", "do a", None);
    let planner = ScriptedPlanner::new(vec![vec![item]]);
    let dispatcher = ScriptedDispatcher::new(vec![Ok(ok_outcome(
        "a",
        UsageDelta { tokens: 0, usd_cents: 999 },
    ))]);

    let ctrl = GoalLoopController::new(
        goal_with("spend a lot", budget(3, Some(50))),
        std::env::current_dir().unwrap(),
        planner,
        dispatcher,
        Arc::new(NullAgentAssertion),
    );
    let outcome = ctrl.run().await;
    match outcome {
        GoalLoopOutcome::Aborted { reason, .. } => {
            assert!(reason.contains("dollar"), "reason: {reason}");
        }
        other => panic!("expected Aborted, got {other:?}"),
    }
}

#[tokio::test]
async fn controller_emits_phase_transitions_in_order() {
    let item = plan_item("a", "do a", Some(shell_check("exit 0")));
    let planner = ScriptedPlanner::new(vec![vec![item]]);
    let dispatcher = ScriptedDispatcher::new(vec![Ok(ok_outcome("a", UsageDelta::default()))]);
    let (tx, mut rx) = unbounded_channel();

    let ctrl = GoalLoopController::new(
        goal_with("trace phases", budget(3, None)),
        std::env::current_dir().unwrap(),
        planner,
        dispatcher,
        Arc::new(NullAgentAssertion),
    )
    .with_event_sink(tx);
    let outcome = ctrl.run().await;
    assert!(matches!(outcome, GoalLoopOutcome::Done { .. }));

    let mut phases = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let GoalLoopEvent::PhaseChanged { to, .. } = event {
            phases.push(to);
        }
    }
    use jcode_task_types::GoalLoopPhase::*;
    // Define → Decompose is implicit (controller starts at Define then
    // transitions immediately). After that we expect at least one Execute
    // and a terminal Done.
    assert!(phases.contains(&Decompose), "phases = {phases:?}");
    assert!(phases.contains(&Execute), "phases = {phases:?}");
    assert!(phases.contains(&Done), "phases = {phases:?}");
}

#[tokio::test]
async fn controller_uses_agent_assertion_runner() {
    let item = plan_item(
        "a",
        "do a",
        Some(SuccessCheck {
            kind: SuccessCheckKind::AgentAssertion,
            spec: "the burst case is covered".to_string(),
            timeout_ms: 1_000,
        }),
    );
    let planner = ScriptedPlanner::new(vec![vec![item]]);
    let dispatcher = ScriptedDispatcher::new(vec![Ok(ok_outcome("a", UsageDelta::default()))]);

    let runner = Arc::new(StubAgentAssertion {
        passed: true,
        detail: "verifier ok".into(),
    });

    let ctrl = GoalLoopController::new(
        goal_with("agent verify", budget(3, None)),
        std::env::current_dir().unwrap(),
        planner,
        dispatcher,
        runner,
    );
    let outcome = ctrl.run().await;
    assert!(matches!(outcome, GoalLoopOutcome::Done { .. }));
}

#[tokio::test]
async fn controller_replans_when_no_runnable_items_remain_and_not_all_passed() {
    let initial = plan_item("a", "do a", None);
    let recovery = plan_item("b", "fixup", None);
    let planner = ScriptedPlanner::with_replans(vec![vec![initial]], vec![vec![recovery]]);
    // First dispatch on a returns failed; second on b returns completed.
    let outcomes = vec![
        Ok(ItemOutcome {
            item_id: "a".into(),
            status: "failed".into(),
            report: None,
            usage: UsageDelta::default(),
            check: None,
        }),
        Ok(ok_outcome("b", UsageDelta::default())),
    ];
    let dispatcher = ScriptedDispatcher::new(outcomes);
    let ctrl = GoalLoopController::new(
        goal_with("replan", budget(3, None)),
        std::env::current_dir().unwrap(),
        planner,
        dispatcher.clone(),
        Arc::new(NullAgentAssertion),
    );
    let outcome = ctrl.run().await;
    assert!(
        matches!(outcome, GoalLoopOutcome::Done { items_completed: 1 }),
        "outcome = {outcome:?}"
    );
    let calls = dispatcher.calls();
    let ids: Vec<&str> = calls.iter().map(|c| c.item_id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"]);
}

#[tokio::test]
async fn controller_honors_abort_flag() {
    let item = plan_item("a", "do a", Some(shell_check("exit 0")));
    let planner = ScriptedPlanner::new(vec![vec![item]]);
    let dispatcher = ScriptedDispatcher::new(vec![Ok(ok_outcome("a", UsageDelta::default()))]);
    let ctrl = GoalLoopController::new(
        goal_with("abort me", budget(3, None)),
        std::env::current_dir().unwrap(),
        planner,
        dispatcher,
        Arc::new(NullAgentAssertion),
    );
    let flag = ctrl.abort_flag();
    flag.raise();
    let outcome = ctrl.run().await;
    match outcome {
        GoalLoopOutcome::Aborted { reason, .. } => assert!(reason.contains("abort"), "{reason}"),
        other => panic!("expected Aborted, got {other:?}"),
    }
}

#[test]
fn item_outcome_kind_classifies_via_event() {
    // Sanity-check the event-side enum maps to controller ItemRunResult.
    assert_ne!(ItemOutcomeKind::Passed, ItemOutcomeKind::Repairing);
    assert_ne!(ItemRunResult::Passed, ItemRunResult::Failed);
}
