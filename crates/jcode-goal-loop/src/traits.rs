use async_trait::async_trait;
use jcode_plan::PlanItem;
use jcode_task_types::{CheckResult, Goal};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::budget::UsageDelta;

#[derive(Debug, Error)]
pub enum PlannerError {
    #[error("planner produced invalid JSON: {0}")]
    InvalidJson(String),
    #[error("planner produced zero items")]
    Empty,
    #[error("planner failed: {0}")]
    Other(String),
}

#[derive(Debug, Error)]
pub enum DispatcherError {
    #[error("sub-agent failed to start: {0}")]
    SpawnFailed(String),
    #[error("sub-agent crashed: {0}")]
    Crashed(String),
}

/// Outcome of running one [`PlanItem`] through a sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemOutcome {
    pub item_id: String,
    /// Status reported by the sub-agent — `completed`, `failed`, etc. The
    /// controller treats anything other than `completed` as a soft failure
    /// that the verifier may still PASS or REPAIR.
    pub status: String,
    /// Free-form structured completion report from the sub-agent.
    pub report: Option<String>,
    /// Token / dollar use this dispatch consumed.
    pub usage: UsageDelta,
    /// Result of running `item.success_check` if one was attached. `None`
    /// means there was no check (item assumed trusting the sub-agent).
    pub check: Option<CheckResult>,
}

/// Decomposes a goal into a plan, or replans given the failure history.
#[async_trait]
pub trait Planner: Send + Sync {
    async fn decompose(&self, goal: &Goal) -> Result<Vec<PlanItem>, PlannerError>;

    async fn replan(
        &self,
        goal: &Goal,
        completed: &[PlanItem],
        failed: &[PlanItem],
    ) -> Result<Vec<PlanItem>, PlannerError>;
}

/// Spawns and supervises one sub-agent for a [`PlanItem`].
#[async_trait]
pub trait SwarmDispatcher: Send + Sync {
    async fn dispatch(&self, item: &PlanItem) -> Result<ItemOutcome, DispatcherError>;
}
