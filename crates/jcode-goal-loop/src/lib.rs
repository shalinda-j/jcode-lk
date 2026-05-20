//! Goal-driven loop controller.
//!
//! The controller drives a [`Goal`] through the phases described in
//! `docs/GOAL_LOOPS.md`. It does not know how to spawn LLM sub-agents — it
//! talks to its environment through trait objects. The root `jcode` crate
//! wires in real implementations; this crate ships stubs sufficient for unit
//! tests.

mod budget;
mod controller;
mod events;
pub mod prompts;
pub mod side_panel;
mod traits;

pub use budget::{BudgetExceeded, BudgetTracker, UsageDelta};
pub use controller::{
    GoalLoopController, GoalLoopOutcome, GoalLoopSnapshot, ItemRecord, ItemRunResult,
};
pub use events::{GoalLoopEvent, ItemOutcomeKind};
pub use prompts::{
    DECOMPOSE_PROMPT, LlmCaller, LlmPlanner, REPLAN_PROMPT, VERIFIER_PROMPT, extract_json,
    parse_plan_items,
};
pub use side_panel::{page_id_for_goal, render_snapshot_markdown, snapshot_as_side_panel_page};
pub use traits::{DispatcherError, ItemOutcome, Planner, PlannerError, SwarmDispatcher};

#[cfg(test)]
mod tests;
