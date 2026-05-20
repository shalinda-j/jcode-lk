//! `jcode goal …` subcommand.
//!
//! Currently only the `--dry-run` path is wired end-to-end. The real path
//! needs a live `LlmCaller` backed by the configured provider and a real
//! `SwarmDispatcher` that talks to `src/server/swarm.rs`. Both are tracked
//! against the goal-loop traits in `crates/jcode-goal-loop`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use jcode_goal_loop::{
    GoalLoopController, GoalLoopEvent, GoalLoopOutcome, ItemOutcome, Planner, PlannerError,
    SwarmDispatcher, UsageDelta,
};
use jcode_plan::PlanItem;
use jcode_task_types::{Goal, GoalBudget, GoalScope, SuccessCheck, SuccessCheckKind};
use jcode_verifier::NullAgentAssertion;
use tokio::sync::mpsc::unbounded_channel;

use super::args::GoalCommand;

pub(crate) async fn dispatch(cmd: GoalCommand) -> Result<()> {
    match cmd {
        GoalCommand::Run {
            message,
            budget_tokens,
            budget_usd_cents,
            budget_secs,
            retries_per_item,
            dry_run,
            ndjson,
        } => {
            let mut budget = GoalBudget::default_interactive();
            budget.tokens = budget_tokens;
            if let Some(v) = budget_usd_cents {
                budget.usd_cents = Some(v);
            }
            if let Some(v) = budget_secs {
                budget.wall_secs = Some(v);
            }
            budget.retries_per_item = retries_per_item;

            if !dry_run {
                return Err(anyhow!(
                    "live goal-loop mode is not yet wired — pass --dry-run to exercise the controller. \
                     The real path needs jcode-provider-* and src/server/swarm.rs integration; \
                     see docs/GOAL_LOOPS.md and the Planner / SwarmDispatcher traits in \
                     jcode-goal-loop."
                ));
            }

            run_dry(&message, budget, ndjson).await
        }
    }
}

async fn run_dry(message: &str, budget: GoalBudget, ndjson: bool) -> Result<()> {
    crate::telemetry::record_goal_loop_started();

    let mut goal = Goal::new(message, GoalScope::Project);
    goal.budget = Some(budget);

    let cwd = std::env::current_dir()?;
    let planner: Arc<dyn Planner> = Arc::new(DryRunPlanner::new());
    let dispatcher: Arc<dyn SwarmDispatcher> = Arc::new(DryRunDispatcher::new());
    let (tx, mut rx) = unbounded_channel::<GoalLoopEvent>();

    let ctrl = GoalLoopController::new(
        goal,
        cwd,
        planner,
        dispatcher,
        Arc::new(NullAgentAssertion),
    )
    .with_event_sink(tx);

    let printer_handle = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if ndjson {
                if let Ok(json) = serde_json::to_string(&event) {
                    println!("{json}");
                }
            } else {
                print_event(&event);
            }
        }
    });

    let outcome = ctrl.run().await;
    drop(printer_handle.await); // flush

    match &outcome {
        GoalLoopOutcome::Done { items_completed } => {
            crate::telemetry::record_goal_loop_done(*items_completed);
        }
        GoalLoopOutcome::Aborted { reason, .. } => {
            let cat = crate::telemetry::GoalLoopAbortReason::classify(reason);
            crate::telemetry::record_goal_loop_aborted(cat);
        }
    }

    if ndjson {
        let json = serde_json::to_string(&outcome)?;
        println!("{json}");
    } else {
        match outcome {
            GoalLoopOutcome::Done { items_completed } => {
                println!("goal DONE — {items_completed} item(s) passed");
            }
            GoalLoopOutcome::Aborted {
                reason,
                items_completed,
            } => {
                println!(
                    "goal ABORTED — {items_completed} item(s) passed before stop; reason: {reason}"
                );
            }
        }
    }

    Ok(())
}

fn print_event(e: &GoalLoopEvent) {
    use GoalLoopEvent::*;
    match e {
        PhaseChanged { from, to } => {
            println!("→ phase: {} → {}", from.as_str(), to.as_str());
        }
        PlanReady { item_count, attempt } => {
            println!("· plan ready (attempt {attempt}): {item_count} item(s)");
        }
        ItemStarted { item_id } => println!("· start  {item_id}"),
        ItemFinished {
            item_id,
            outcome,
            check,
        } => {
            let suffix = check
                .as_ref()
                .map(|c| {
                    let mark = if c.passed { "PASS" } else { "FAIL" };
                    format!(" [{mark}: {}]", c.detail)
                })
                .unwrap_or_default();
            println!("· finish {item_id} {outcome:?}{suffix}");
        }
        BudgetUpdate {
            tokens_used,
            cents_used,
            elapsed_secs,
        } => {
            println!(
                "· budget: {tokens_used} tok / ${:.2} / {elapsed_secs}s",
                *cents_used as f32 / 100.0
            );
        }
        Aborted { reason } => println!("✗ aborted: {reason}"),
        Done => println!("✓ done"),
    }
}

// ---------- dry-run stubs ----------

struct DryRunPlanner;

impl DryRunPlanner {
    fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Planner for DryRunPlanner {
    async fn decompose(&self, goal: &Goal) -> Result<Vec<PlanItem>, PlannerError> {
        // Mechanical decomposition: produce three items wrapped around the
        // goal's title so a CI smoke test has something deterministic to
        // assert against.
        let title = &goal.title;
        Ok(vec![
            item("research", &format!("investigate: {title}")),
            item("implement", &format!("implement: {title}")),
            item(
                "verify",
                &format!("verify: {title}"),
            ),
        ])
    }

    async fn replan(
        &self,
        _goal: &Goal,
        _completed: &[PlanItem],
        failed: &[PlanItem],
    ) -> Result<Vec<PlanItem>, PlannerError> {
        if failed.is_empty() {
            return Err(PlannerError::Empty);
        }
        Ok(vec![item("fixup", "retry failed work with smaller scope")])
    }
}

fn item(id: &str, content: &str) -> PlanItem {
    PlanItem {
        content: content.to_string(),
        status: "queued".to_string(),
        priority: "medium".to_string(),
        id: id.to_string(),
        subsystem: None,
        file_scope: Vec::new(),
        blocked_by: Vec::new(),
        assigned_to: None,
        success_check: Some(SuccessCheck {
            kind: SuccessCheckKind::Shell,
            spec: "exit 0".to_string(),
            timeout_ms: 5_000,
        }),
    }
}

struct DryRunDispatcher {
    calls: Mutex<u32>,
}

impl DryRunDispatcher {
    fn new() -> Self {
        Self {
            calls: Mutex::new(0),
        }
    }
}

#[async_trait]
impl SwarmDispatcher for DryRunDispatcher {
    async fn dispatch(
        &self,
        item: &PlanItem,
    ) -> Result<ItemOutcome, jcode_goal_loop::DispatcherError> {
        let mut g = self.calls.lock().unwrap();
        *g += 1;
        Ok(ItemOutcome {
            item_id: item.id.clone(),
            status: "completed".to_string(),
            report: Some(format!("[dry-run] simulated {}", item.id)),
            usage: UsageDelta {
                tokens: 250,
                usd_cents: 1,
            },
            check: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dry_run_completes_for_simple_goal() {
        let _cwd_guard = std::env::current_dir().unwrap();
        let r = run_dry(
            "smoke test goal",
            GoalBudget::default_interactive(),
            false,
        )
        .await;
        assert!(r.is_ok(), "{:?}", r);
    }

    #[tokio::test]
    async fn dry_run_ndjson_outputs_events() {
        let r = run_dry(
            "ndjson goal",
            GoalBudget::default_interactive(),
            true,
        )
        .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn live_mode_errors_with_helpful_message() {
        let cmd = GoalCommand::Run {
            message: "x".to_string(),
            budget_tokens: None,
            budget_usd_cents: None,
            budget_secs: None,
            retries_per_item: 3,
            dry_run: false,
            ndjson: false,
        };
        let err = dispatch(cmd).await.unwrap_err();
        assert!(format!("{err}").contains("--dry-run"));
    }
}

// Suppress unused import warnings on PathBuf when this module is built without
// future planned wiring that uses it.
#[allow(dead_code)]
fn _placeholder_cwd() -> PathBuf {
    PathBuf::from(".")
}
