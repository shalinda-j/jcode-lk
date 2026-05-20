use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GoalScope {
    Global,
    #[default]
    Project,
}

impl GoalScope {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "global" => Some(Self::Global),
            "project" => Some(Self::Project),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Draft,
    #[default]
    Active,
    Paused,
    Blocked,
    Completed,
    Archived,
    Abandoned,
}

impl GoalStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "draft" => Some(Self::Draft),
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "blocked" => Some(Self::Blocked),
            "completed" => Some(Self::Completed),
            "archived" => Some(Self::Archived),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Archived => "archived",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn sort_rank(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::Blocked => 1,
            Self::Draft => 2,
            Self::Paused => 3,
            Self::Completed => 4,
            Self::Archived => 5,
            Self::Abandoned => 6,
        }
    }

    pub fn is_resumable(self) -> bool {
        matches!(self, Self::Active | Self::Blocked | Self::Draft)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GoalStep {
    pub id: String,
    pub content: String,
    #[serde(default = "default_pending_status")]
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GoalMilestone {
    pub id: String,
    pub title: String,
    #[serde(default = "default_pending_status")]
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<GoalStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalUpdate {
    pub at: DateTime<Utc>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Goal {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub scope: GoalScope,
    #[serde(default)]
    pub status: GoalStatus,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub why: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub success_criteria: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub milestones: Vec<GoalMilestone>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_milestone_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_percent: Option<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updates: Vec<GoalUpdate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<GoalBudget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_phase: Option<GoalLoopPhase>,
}

impl Goal {
    pub fn new(title: &str, scope: GoalScope) -> Self {
        let now = Utc::now();
        let trimmed = title.trim();
        Self {
            id: sanitize_goal_id(trimmed),
            title: trimmed.to_string(),
            scope,
            status: GoalStatus::Active,
            description: String::new(),
            why: String::new(),
            success_criteria: Vec::new(),
            milestones: Vec::new(),
            next_steps: Vec::new(),
            blockers: Vec::new(),
            current_milestone_id: None,
            progress_percent: None,
            created_at: now,
            updated_at: now,
            updates: Vec::new(),
            budget: None,
            loop_phase: None,
        }
    }

    pub fn current_milestone(&self) -> Option<&GoalMilestone> {
        let current_id = self.current_milestone_id.as_deref()?;
        self.milestones.iter().find(|m| m.id == current_id)
    }
}

pub fn sanitize_goal_id(id: &str) -> String {
    let slug = slugify(id);
    if slug.is_empty() {
        "goal".to_string()
    } else {
        slug
    }
}

fn slugify(input: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in input.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            slug.push(lower);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn default_pending_status() -> String {
    "pending".to_string()
}

// ---------------------------------------------------------------------------
// Goal-driven loop types
//
// These shapes back the `/goal` mode: the controller drives a `Goal` through
// the phases below, the planner emits `PlanItem`s with a `SuccessCheck`, and
// the verifier role runs the check to decide PASS / REPAIR / REPLAN.
// All fields are additive (Option / default) so persisted goals from earlier
// versions still deserialize.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SuccessCheckKind {
    Shell,
    CargoTest,
    Pytest,
    JestTest,
    FileAbsent,
    Regex,
    AgentAssertion,
}

impl SuccessCheckKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "shell" => Some(Self::Shell),
            "cargo_test" => Some(Self::CargoTest),
            "pytest" => Some(Self::Pytest),
            "jest_test" => Some(Self::JestTest),
            "file_absent" => Some(Self::FileAbsent),
            "regex" => Some(Self::Regex),
            "agent_assertion" => Some(Self::AgentAssertion),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::CargoTest => "cargo_test",
            Self::Pytest => "pytest",
            Self::JestTest => "jest_test",
            Self::FileAbsent => "file_absent",
            Self::Regex => "regex",
            Self::AgentAssertion => "agent_assertion",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SuccessCheck {
    pub kind: SuccessCheckKind,
    pub spec: String,
    #[serde(default = "default_check_timeout_ms")]
    pub timeout_ms: u32,
}

fn default_check_timeout_ms() -> u32 {
    120_000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckResult {
    pub passed: bool,
    pub detail: String,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalLoopPhase {
    Define,
    Decompose,
    Dispatch,
    Execute,
    Verify,
    Repair,
    Replan,
    Done,
    Aborted,
}

impl GoalLoopPhase {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "define" => Some(Self::Define),
            "decompose" => Some(Self::Decompose),
            "dispatch" => Some(Self::Dispatch),
            "execute" => Some(Self::Execute),
            "verify" => Some(Self::Verify),
            "repair" => Some(Self::Repair),
            "replan" => Some(Self::Replan),
            "done" => Some(Self::Done),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Define => "define",
            Self::Decompose => "decompose",
            Self::Dispatch => "dispatch",
            Self::Execute => "execute",
            Self::Verify => "verify",
            Self::Repair => "repair",
            Self::Replan => "replan",
            Self::Done => "done",
            Self::Aborted => "aborted",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Aborted)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GoalBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_cents: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_secs: Option<u32>,
    #[serde(default = "default_retries_per_item")]
    pub retries_per_item: u8,
}

fn default_retries_per_item() -> u8 {
    3
}

impl GoalBudget {
    /// Default budget used when the user runs `/goal` without explicit caps.
    /// $2.00, 30 minutes, 3 retries per item, no token cap.
    pub fn default_interactive() -> Self {
        Self {
            tokens: None,
            usd_cents: Some(200),
            wall_secs: Some(1_800),
            retries_per_item: default_retries_per_item(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
    pub priority: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_to: Option<String>,
}

use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedCatchupState {
    #[serde(default)]
    pub seen_at_ms_by_session: HashMap<String, i64>,
}

#[derive(Debug, Clone)]
pub struct CatchupBrief {
    pub reason: String,
    pub tags: Vec<String>,
    pub last_user_prompt: Option<String>,
    pub activity_steps: Vec<String>,
    pub files_touched: Vec<String>,
    pub tool_counts: Vec<(String, usize)>,
    pub validation_notes: Vec<String>,
    pub latest_agent_response: Option<String>,
    pub needs_from_user: String,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod goal_loop_tests {
    use super::*;

    #[test]
    fn goal_round_trips_without_loop_fields() {
        // Persisted goals from earlier versions must still deserialize.
        let legacy = serde_json::json!({
            "id": "old",
            "title": "Old goal",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
        });
        let goal: Goal = serde_json::from_value(legacy).expect("legacy goal deserializes");
        assert!(goal.budget.is_none());
        assert!(goal.loop_phase.is_none());
    }

    #[test]
    fn goal_round_trips_with_loop_fields() {
        let mut goal = Goal::new("Add rate limiting", GoalScope::Project);
        goal.budget = Some(GoalBudget::default_interactive());
        goal.loop_phase = Some(GoalLoopPhase::Execute);

        let encoded = serde_json::to_string(&goal).expect("serialize");
        let decoded: Goal = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded.budget, goal.budget);
        assert_eq!(decoded.loop_phase, goal.loop_phase);
    }

    #[test]
    fn success_check_round_trips() {
        let check = SuccessCheck {
            kind: SuccessCheckKind::CargoTest,
            spec: "rate_limit::*".to_string(),
            timeout_ms: 60_000,
        };
        let encoded = serde_json::to_string(&check).expect("serialize");
        let decoded: SuccessCheck = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, check);
    }

    #[test]
    fn success_check_timeout_defaults_when_missing() {
        let json = r#"{"kind":"shell","spec":"true"}"#;
        let decoded: SuccessCheck = serde_json::from_str(json).expect("deserialize");
        assert_eq!(decoded.timeout_ms, 120_000);
    }

    #[test]
    fn goal_loop_phase_terminal_classification() {
        assert!(GoalLoopPhase::Done.is_terminal());
        assert!(GoalLoopPhase::Aborted.is_terminal());
        assert!(!GoalLoopPhase::Execute.is_terminal());
    }

    #[test]
    fn default_interactive_budget_caps_dollars_and_time() {
        let budget = GoalBudget::default_interactive();
        assert_eq!(budget.usd_cents, Some(200));
        assert_eq!(budget.wall_secs, Some(1_800));
        assert_eq!(budget.retries_per_item, 3);
        assert!(budget.tokens.is_none());
    }
}
