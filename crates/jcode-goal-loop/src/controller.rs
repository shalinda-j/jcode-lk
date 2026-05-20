use std::path::PathBuf;
use std::sync::Arc;

use jcode_plan::PlanItem;
use jcode_task_types::{CheckResult, Goal, GoalBudget, GoalLoopPhase};
use jcode_verifier::AgentAssertionRunner;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::budget::{BudgetExceeded, BudgetTracker};
use crate::events::{GoalLoopEvent, ItemOutcomeKind};
use crate::traits::{ItemOutcome, Planner, SwarmDispatcher};

/// Final state of a loop run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GoalLoopOutcome {
    Done {
        items_completed: usize,
    },
    Aborted {
        reason: String,
        items_completed: usize,
    },
}

/// Per-item history kept by the controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemRecord {
    pub item: PlanItem,
    pub attempts: u8,
    pub last_check: Option<CheckResult>,
    pub last_outcome: Option<ItemRunResult>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemRunResult {
    Passed,
    Repairing,
    Failed,
}

/// Disk-friendly snapshot for the side panel + `restart_snapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalLoopSnapshot {
    pub goal_id: String,
    pub phase: GoalLoopPhase,
    pub items: Vec<ItemRecord>,
    pub tokens_used: u64,
    pub cents_used: u32,
    pub elapsed_secs: u32,
    pub budget: GoalBudget,
}

pub struct GoalLoopController {
    goal: Goal,
    cwd: PathBuf,
    planner: Arc<dyn Planner>,
    dispatcher: Arc<dyn SwarmDispatcher>,
    agent_runner: Arc<dyn AgentAssertionRunner>,
    budget: BudgetTracker,
    items: Vec<ItemRecord>,
    phase: GoalLoopPhase,
    events: Option<UnboundedSender<GoalLoopEvent>>,
    abort: AbortFlag,
    decompose_attempts: u32,
    last_run_idx: Option<usize>,
}

#[derive(Clone, Default)]
pub struct AbortFlag(Arc<std::sync::atomic::AtomicBool>);

impl AbortFlag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn raise(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_raised(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl GoalLoopController {
    pub fn new(
        goal: Goal,
        cwd: PathBuf,
        planner: Arc<dyn Planner>,
        dispatcher: Arc<dyn SwarmDispatcher>,
        agent_runner: Arc<dyn AgentAssertionRunner>,
    ) -> Self {
        let budget = goal.budget.unwrap_or_else(GoalBudget::default_interactive);
        Self {
            goal,
            cwd,
            planner,
            dispatcher,
            agent_runner,
            budget: BudgetTracker::new(budget),
            items: Vec::new(),
            phase: GoalLoopPhase::Define,
            events: None,
            abort: AbortFlag::new(),
            decompose_attempts: 0,
            last_run_idx: None,
        }
    }

    pub fn with_event_sink(mut self, sink: UnboundedSender<GoalLoopEvent>) -> Self {
        self.events = Some(sink);
        self
    }

    pub fn abort_flag(&self) -> AbortFlag {
        self.abort.clone()
    }

    pub fn snapshot(&self) -> GoalLoopSnapshot {
        GoalLoopSnapshot {
            goal_id: self.goal.id.clone(),
            phase: self.phase,
            items: self.items.clone(),
            tokens_used: self.budget.tokens_used(),
            cents_used: self.budget.cents_used(),
            elapsed_secs: self.budget.elapsed_secs(),
            budget: self.budget.caps(),
        }
    }

    pub async fn run(mut self) -> GoalLoopOutcome {
        // Define → Decompose
        self.transition(GoalLoopPhase::Decompose);

        loop {
            if self.abort.is_raised() {
                return self.abort_with("user abort".to_string());
            }
            if let Some(reason) = self.budget.check() {
                return self.abort_with(reason.to_string());
            }

            match self.phase {
                GoalLoopPhase::Decompose => {
                    if let Err(reason) = self.do_decompose().await {
                        return self.abort_with(reason);
                    }
                    self.transition(GoalLoopPhase::Dispatch);
                }
                GoalLoopPhase::Dispatch | GoalLoopPhase::Repair => {
                    self.transition(GoalLoopPhase::Execute);
                }
                GoalLoopPhase::Execute => {
                    let next = self.pick_runnable_item();
                    match next {
                        Some(idx) => {
                            self.last_run_idx = Some(idx);
                            self.run_item(idx).await;
                            self.transition(GoalLoopPhase::Verify);
                        }
                        None => {
                            // Nothing more to run: are we done or stuck?
                            if self.all_items_passed() {
                                self.transition(GoalLoopPhase::Done);
                            } else {
                                self.transition(GoalLoopPhase::Replan);
                            }
                        }
                    }
                }
                GoalLoopPhase::Verify => {
                    // Verification happens inline in run_item via the
                    // success_check. Drive the next decision based on the
                    // last item state.
                    self.transition(self.next_after_verify());
                }
                GoalLoopPhase::Replan => match self.do_replan().await {
                    Ok(()) => self.transition(GoalLoopPhase::Dispatch),
                    Err(reason) => return self.abort_with(reason),
                },
                GoalLoopPhase::Done => {
                    self.emit(GoalLoopEvent::Done);
                    return GoalLoopOutcome::Done {
                        items_completed: self.passed_count(),
                    };
                }
                GoalLoopPhase::Aborted | GoalLoopPhase::Define => {
                    // Define is set only at start; Aborted is terminal.
                    return GoalLoopOutcome::Aborted {
                        reason: "controller reached unreachable phase".to_string(),
                        items_completed: self.passed_count(),
                    };
                }
            }
        }
    }

    fn next_after_verify(&self) -> GoalLoopPhase {
        let last = self
            .last_run_idx
            .and_then(|i| self.items.get(i))
            .and_then(|r| r.last_outcome);
        match last {
            Some(ItemRunResult::Passed) => {
                if self.all_items_passed() {
                    GoalLoopPhase::Done
                } else {
                    GoalLoopPhase::Execute
                }
            }
            Some(ItemRunResult::Repairing) => GoalLoopPhase::Repair,
            Some(ItemRunResult::Failed) | None => GoalLoopPhase::Replan,
        }
    }

    async fn do_decompose(&mut self) -> Result<(), String> {
        self.decompose_attempts += 1;
        match self.planner.decompose(&self.goal).await {
            Ok(items) if items.is_empty() => Err("planner produced zero items".to_string()),
            Ok(items) => {
                for item in items {
                    self.items.push(ItemRecord {
                        item,
                        attempts: 0,
                        last_check: None,
                        last_outcome: None,
                    });
                }
                self.emit(GoalLoopEvent::PlanReady {
                    item_count: self.items.len(),
                    attempt: self.decompose_attempts,
                });
                Ok(())
            }
            Err(e) => Err(format!("decompose failed: {e}")),
        }
    }

    async fn do_replan(&mut self) -> Result<(), String> {
        if self.decompose_attempts >= 3 {
            return Err("decompose attempts exhausted".to_string());
        }
        let (completed, failed): (Vec<_>, Vec<_>) = self
            .items
            .iter()
            .partition(|r| matches!(r.last_outcome, Some(ItemRunResult::Passed)));
        let completed_items: Vec<PlanItem> = completed.iter().map(|r| r.item.clone()).collect();
        let failed_items: Vec<PlanItem> = failed.iter().map(|r| r.item.clone()).collect();
        match self
            .planner
            .replan(&self.goal, &completed_items, &failed_items)
            .await
        {
            Ok(new_items) if new_items.is_empty() => {
                Err("replan produced zero items".to_string())
            }
            Ok(new_items) => {
                self.items.retain(|r| matches!(r.last_outcome, Some(ItemRunResult::Passed)));
                self.decompose_attempts += 1;
                for item in new_items {
                    self.items.push(ItemRecord {
                        item,
                        attempts: 0,
                        last_check: None,
                        last_outcome: None,
                    });
                }
                self.emit(GoalLoopEvent::PlanReady {
                    item_count: self.items.len(),
                    attempt: self.decompose_attempts,
                });
                Ok(())
            }
            Err(e) => Err(format!("replan failed: {e}")),
        }
    }

    fn pick_runnable_item(&self) -> Option<usize> {
        self.items
            .iter()
            .position(|r| !matches!(r.last_outcome, Some(ItemRunResult::Passed)))
    }

    async fn run_item(&mut self, idx: usize) {
        let item = self.items[idx].item.clone();
        self.items[idx].attempts = self.items[idx].attempts.saturating_add(1);

        if self.items[idx].attempts > 1
            && let Err(reason) = self.budget.note_retry(&item.id)
        {
            self.items[idx].last_outcome = Some(ItemRunResult::Failed);
            self.emit(GoalLoopEvent::Aborted {
                reason: reason.to_string(),
            });
            return;
        }

        self.emit(GoalLoopEvent::ItemStarted {
            item_id: item.id.clone(),
        });

        let outcome = match self.dispatcher.dispatch(&item).await {
            Ok(o) => o,
            Err(e) => {
                self.items[idx].last_outcome = Some(ItemRunResult::Failed);
                self.items[idx].last_check = None;
                self.emit(GoalLoopEvent::ItemFinished {
                    item_id: item.id.clone(),
                    outcome: ItemOutcomeKind::Failed,
                    check: None,
                });
                tracing::warn!(item_id = %item.id, error = %e, "dispatch failed");
                return;
            }
        };

        self.budget.record(outcome.usage);
        self.emit(GoalLoopEvent::BudgetUpdate {
            tokens_used: self.budget.tokens_used(),
            cents_used: self.budget.cents_used(),
            elapsed_secs: self.budget.elapsed_secs(),
        });

        let check_result = self.verify_outcome(&item, &outcome).await;
        let result_kind = classify(&outcome, check_result.as_ref());
        self.items[idx].last_check = check_result.clone();
        self.items[idx].last_outcome = Some(result_kind);

        self.emit(GoalLoopEvent::ItemFinished {
            item_id: item.id.clone(),
            outcome: match result_kind {
                ItemRunResult::Passed => ItemOutcomeKind::Passed,
                ItemRunResult::Repairing => ItemOutcomeKind::Repairing,
                ItemRunResult::Failed => ItemOutcomeKind::Failed,
            },
            check: check_result,
        });
    }

    async fn verify_outcome(
        &self,
        item: &PlanItem,
        outcome: &ItemOutcome,
    ) -> Option<CheckResult> {
        if let Some(existing) = &outcome.check {
            return Some(existing.clone());
        }
        let check = item.success_check.as_ref()?;
        Some(
            jcode_verifier::run_check(check, &self.cwd, Arc::clone(&self.agent_runner)).await,
        )
    }

    fn all_items_passed(&self) -> bool {
        !self.items.is_empty()
            && self
                .items
                .iter()
                .all(|r| matches!(r.last_outcome, Some(ItemRunResult::Passed)))
    }

    fn passed_count(&self) -> usize {
        self.items
            .iter()
            .filter(|r| matches!(r.last_outcome, Some(ItemRunResult::Passed)))
            .count()
    }

    fn transition(&mut self, to: GoalLoopPhase) {
        if to != self.phase {
            self.emit(GoalLoopEvent::PhaseChanged {
                from: self.phase,
                to,
            });
            self.phase = to;
        }
    }

    fn emit(&self, event: GoalLoopEvent) {
        if let Some(sink) = &self.events {
            let _ = sink.send(event);
        }
    }

    fn abort_with(mut self, reason: String) -> GoalLoopOutcome {
        self.transition(GoalLoopPhase::Aborted);
        self.emit(GoalLoopEvent::Aborted {
            reason: reason.clone(),
        });
        GoalLoopOutcome::Aborted {
            reason,
            items_completed: self.passed_count(),
        }
    }
}

fn classify(outcome: &ItemOutcome, check: Option<&CheckResult>) -> ItemRunResult {
    match check {
        Some(c) if c.passed => ItemRunResult::Passed,
        Some(_) => {
            // Verifier said fail — repair if the sub-agent reported completed,
            // otherwise replan.
            if outcome.status == "completed" {
                ItemRunResult::Repairing
            } else {
                ItemRunResult::Failed
            }
        }
        None => {
            if outcome.status == "completed" {
                ItemRunResult::Passed
            } else {
                ItemRunResult::Failed
            }
        }
    }
}

// Allow the standalone abort hook to be used by external orchestrators.
pub use AbortFlag as GoalLoopAbortFlag;

#[doc(hidden)]
pub fn _force_budget_exceeded(b: &BudgetExceeded) -> String {
    b.to_string()
}
