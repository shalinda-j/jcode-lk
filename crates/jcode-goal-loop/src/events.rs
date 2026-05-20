use jcode_task_types::{CheckResult, GoalLoopPhase};
use serde::{Deserialize, Serialize};

use crate::budget::BudgetExceeded;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum GoalLoopEvent {
    PhaseChanged {
        from: GoalLoopPhase,
        to: GoalLoopPhase,
    },
    PlanReady {
        item_count: usize,
        attempt: u32,
    },
    ItemStarted {
        item_id: String,
    },
    ItemFinished {
        item_id: String,
        outcome: ItemOutcomeKind,
        check: Option<CheckResult>,
    },
    BudgetUpdate {
        tokens_used: u64,
        cents_used: u32,
        elapsed_secs: u32,
    },
    Aborted {
        reason: String,
    },
    Done,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemOutcomeKind {
    Passed,
    Repairing,
    Failed,
}

impl GoalLoopEvent {
    pub fn budget_exhausted(reason: &BudgetExceeded) -> Self {
        Self::Aborted {
            reason: reason.to_string(),
        }
    }
}
