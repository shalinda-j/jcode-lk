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
mod traits;

pub use budget::{BudgetExceeded, BudgetTracker, UsageDelta};
pub use controller::{
    GoalLoopController, GoalLoopOutcome, GoalLoopSnapshot, ItemRecord, ItemRunResult,
};
pub use events::{GoalLoopEvent, ItemOutcomeKind};
pub use traits::{DispatcherError, ItemOutcome, Planner, PlannerError, SwarmDispatcher};

#[cfg(test)]
mod tests;
