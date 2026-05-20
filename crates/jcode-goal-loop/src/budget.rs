use std::time::{Duration, Instant};

use jcode_task_types::GoalBudget;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One sub-agent's resource use, reported back after a turn.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageDelta {
    pub tokens: u64,
    pub usd_cents: u32,
}

/// Why the loop stopped on budget.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum BudgetExceeded {
    #[error("token budget exhausted ({used} >= {cap})")]
    Tokens { used: u64, cap: u64 },
    #[error("dollar budget exhausted ({used_cents}¢ >= {cap_cents}¢)")]
    Dollars { used_cents: u32, cap_cents: u32 },
    #[error("wall-clock budget exhausted ({elapsed_secs}s >= {cap_secs}s)")]
    Wall { elapsed_secs: u32, cap_secs: u32 },
    #[error("retry cap exhausted for item `{item_id}` ({tries}/{cap})")]
    Retries { item_id: String, tries: u8, cap: u8 },
}

#[derive(Debug)]
pub struct BudgetTracker {
    caps: GoalBudget,
    tokens_used: u64,
    cents_used: u32,
    started_at: Instant,
    retries_by_item: std::collections::HashMap<String, u8>,
}

impl BudgetTracker {
    pub fn new(caps: GoalBudget) -> Self {
        Self {
            caps,
            tokens_used: 0,
            cents_used: 0,
            started_at: Instant::now(),
            retries_by_item: std::collections::HashMap::new(),
        }
    }

    pub fn record(&mut self, delta: UsageDelta) {
        self.tokens_used = self.tokens_used.saturating_add(delta.tokens);
        self.cents_used = self.cents_used.saturating_add(delta.usd_cents);
    }

    /// Returns the new retry count, or `BudgetExceeded::Retries` if the cap is
    /// already at-or-past the configured `retries_per_item`.
    pub fn note_retry(&mut self, item_id: &str) -> Result<u8, BudgetExceeded> {
        let cap = self.caps.retries_per_item;
        let counter = self.retries_by_item.entry(item_id.to_string()).or_insert(0);
        *counter = counter.saturating_add(1);
        if *counter > cap {
            Err(BudgetExceeded::Retries {
                item_id: item_id.to_string(),
                tries: *counter,
                cap,
            })
        } else {
            Ok(*counter)
        }
    }

    pub fn retries(&self, item_id: &str) -> u8 {
        self.retries_by_item.get(item_id).copied().unwrap_or(0)
    }

    pub fn tokens_used(&self) -> u64 {
        self.tokens_used
    }

    pub fn cents_used(&self) -> u32 {
        self.cents_used
    }

    pub fn elapsed_secs(&self) -> u32 {
        self.started_at.elapsed().as_secs().min(u32::MAX as u64) as u32
    }

    pub fn caps(&self) -> GoalBudget {
        self.caps
    }

    /// Returns the first cap that has been exceeded, if any. Checked in the
    /// priority order documented in `docs/GOAL_LOOPS.md` (excluding per-item
    /// retries, which are surfaced from [`note_retry`]).
    pub fn check(&self) -> Option<BudgetExceeded> {
        if let Some(cap) = self.caps.tokens
            && self.tokens_used >= cap
        {
            return Some(BudgetExceeded::Tokens {
                used: self.tokens_used,
                cap,
            });
        }
        if let Some(cap) = self.caps.usd_cents
            && self.cents_used >= cap
        {
            return Some(BudgetExceeded::Dollars {
                used_cents: self.cents_used,
                cap_cents: cap,
            });
        }
        if let Some(cap) = self.caps.wall_secs
            && self.elapsed_secs() >= cap
        {
            return Some(BudgetExceeded::Wall {
                elapsed_secs: self.elapsed_secs(),
                cap_secs: cap,
            });
        }
        None
    }

    /// Test hook: pretend more wall time has passed.
    #[cfg(test)]
    pub fn rewind_started_at(&mut self, by: Duration) {
        self.started_at = self
            .started_at
            .checked_sub(by)
            .unwrap_or(self.started_at);
    }
}
