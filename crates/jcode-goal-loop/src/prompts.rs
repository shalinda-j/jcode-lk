//! Planner / verifier prompt templates and JSON parsers.

use async_trait::async_trait;
use jcode_plan::PlanItem;
use jcode_task_types::{Goal, SuccessCheck, SuccessCheckKind};
use serde::Deserialize;

use crate::traits::{Planner, PlannerError};

pub const DECOMPOSE_PROMPT: &str = r#"You are the planner for a coding agent's goal-driven loop.

Decompose the user's goal into a minimal list of plan items. Each item must be:
- self-contained (a single sub-agent can complete it in one turn),
- ordered or labelled with `blocked_by` if there are dependencies,
- attached to a `success_check` that the verifier can run *automatically*.

Return ONLY a JSON object of the form:

```json
{
  "items": [
    {
      "id": "<slug, e.g. add-middleware>",
      "content": "<short imperative description>",
      "priority": "<high|medium|low>",
      "blocked_by": ["<id>", ...],
      "success_check": {
        "kind": "shell|cargo_test|pytest|jest_test|file_absent|regex|agent_assertion",
        "spec": "<command-or-pattern>",
        "timeout_ms": <number>
      }
    }
  ]
}
```

Constraints:
- Prefer `cargo_test`, `pytest`, or `jest_test` when the goal includes test coverage.
- Use `regex` to enforce "absent from repo" (e.g. ban a deprecated symbol).
- Use `agent_assertion` only when no mechanical check is possible.
- Every spec must be runnable in the project root with no further setup.
- Do not include success_criteria the user did not state.
"#;

pub const REPLAN_PROMPT: &str = r#"You are replanning a goal-driven loop because the verifier rejected progress.

You receive: the original goal, the items that PASSED, and the items that FAILED with their
last verifier detail. Produce a new minimal item list that finishes the goal without
re-doing already-passing work.

Return ONLY a JSON object using the same shape as the decompose prompt:

```json
{ "items": [ ... ] }
```

Rules:
- Do NOT include items that have already passed.
- If a previous item failed because the success_check itself was wrong, propose a corrected
  `success_check` in the new item.
- Keep the new list shorter than the original.
"#;

pub const VERIFIER_PROMPT: &str = r#"You are the verifier for a goal-driven loop.

You receive an assertion about a repository and the most recent sub-agent's completion report.
Decide whether the assertion is TRUE based on file contents you may inspect through tools (if
any are provided). Respond ONLY with this JSON:

```json
{ "passed": <true|false>, "detail": "<one sentence quoting evidence>" }
```

Never say "passed" unless you can quote concrete evidence.
"#;

/// LLM call abstraction so this crate stays provider-agnostic.
#[async_trait]
pub trait LlmCaller: Send + Sync {
    async fn complete(&self, prompt: &str, user_msg: &str) -> Result<String, String>;
}

#[derive(Debug, Deserialize)]
struct PlanWire {
    items: Vec<PlanItemWire>,
}

#[derive(Debug, Deserialize)]
struct PlanItemWire {
    id: String,
    content: String,
    #[serde(default = "default_priority")]
    priority: String,
    #[serde(default)]
    blocked_by: Vec<String>,
    #[serde(default)]
    success_check: Option<SuccessCheckWire>,
    #[serde(default)]
    subsystem: Option<String>,
    #[serde(default)]
    file_scope: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SuccessCheckWire {
    kind: String,
    spec: String,
    #[serde(default)]
    timeout_ms: Option<u32>,
}

fn default_priority() -> String {
    "medium".to_string()
}

/// Extract a JSON object from possibly fenced or chatty LLM output.
pub fn extract_json(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        return Some(trimmed);
    }
    // Try ```json fences first.
    if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + 7..];
        if let Some(end) = after.find("```") {
            return Some(after[..end].trim());
        }
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        if let Some(end) = after.find("```") {
            return Some(after[..end].trim());
        }
    }
    // Fall back to first '{' to last '}' .
    let first = trimmed.find('{')?;
    let last = trimmed.rfind('}')?;
    if last > first {
        Some(&trimmed[first..=last])
    } else {
        None
    }
}

/// Parse planner output into [`PlanItem`]s. Returns an error with the raw
/// snippet on failure so the caller can retry with the parser error appended.
pub fn parse_plan_items(raw: &str) -> Result<Vec<PlanItem>, PlannerError> {
    let json = extract_json(raw).ok_or_else(|| PlannerError::InvalidJson(raw.to_string()))?;
    let wire: PlanWire =
        serde_json::from_str(json).map_err(|e| PlannerError::InvalidJson(e.to_string()))?;
    if wire.items.is_empty() {
        return Err(PlannerError::Empty);
    }
    let mut out = Vec::with_capacity(wire.items.len());
    for w in wire.items {
        let success_check = match w.success_check {
            Some(c) => {
                let kind = SuccessCheckKind::parse(&c.kind).ok_or_else(|| {
                    PlannerError::InvalidJson(format!("unknown success_check kind `{}`", c.kind))
                })?;
                Some(SuccessCheck {
                    kind,
                    spec: c.spec,
                    timeout_ms: c.timeout_ms.unwrap_or(120_000),
                })
            }
            None => None,
        };
        out.push(PlanItem {
            content: w.content,
            status: "queued".to_string(),
            priority: w.priority,
            id: w.id,
            subsystem: w.subsystem,
            file_scope: w.file_scope,
            blocked_by: w.blocked_by,
            assigned_to: None,
            success_check,
        });
    }
    Ok(out)
}

/// Planner backed by a generic `LlmCaller`. The caller handles model
/// selection, retries on transport errors, etc.
pub struct LlmPlanner {
    caller: std::sync::Arc<dyn LlmCaller>,
}

impl LlmPlanner {
    pub fn new(caller: std::sync::Arc<dyn LlmCaller>) -> Self {
        Self { caller }
    }
}

#[async_trait]
impl Planner for LlmPlanner {
    async fn decompose(&self, goal: &Goal) -> Result<Vec<PlanItem>, PlannerError> {
        let user_msg = render_goal_for_decompose(goal);
        let mut last_err: Option<PlannerError> = None;
        for _attempt in 0..2 {
            match self.caller.complete(DECOMPOSE_PROMPT, &user_msg).await {
                Ok(raw) => match parse_plan_items(&raw) {
                    Ok(items) => return Ok(items),
                    Err(e) => last_err = Some(e),
                },
                Err(e) => return Err(PlannerError::Other(e)),
            }
        }
        Err(last_err.unwrap_or(PlannerError::Empty))
    }

    async fn replan(
        &self,
        goal: &Goal,
        completed: &[PlanItem],
        failed: &[PlanItem],
    ) -> Result<Vec<PlanItem>, PlannerError> {
        let user_msg = render_goal_for_replan(goal, completed, failed);
        match self.caller.complete(REPLAN_PROMPT, &user_msg).await {
            Ok(raw) => parse_plan_items(&raw),
            Err(e) => Err(PlannerError::Other(e)),
        }
    }
}

fn render_goal_for_decompose(goal: &Goal) -> String {
    let mut out = String::new();
    out.push_str(&format!("GOAL: {}\n", goal.title));
    if !goal.why.is_empty() {
        out.push_str(&format!("WHY: {}\n", goal.why));
    }
    if !goal.success_criteria.is_empty() {
        out.push_str("SUCCESS CRITERIA:\n");
        for c in &goal.success_criteria {
            out.push_str(&format!("- {c}\n"));
        }
    }
    out
}

fn render_goal_for_replan(
    goal: &Goal,
    completed: &[PlanItem],
    failed: &[PlanItem],
) -> String {
    let mut out = render_goal_for_decompose(goal);
    out.push_str("\nCOMPLETED ITEMS:\n");
    if completed.is_empty() {
        out.push_str("(none)\n");
    }
    for item in completed {
        out.push_str(&format!("- [{}] {}\n", item.id, item.content));
    }
    out.push_str("\nFAILED ITEMS:\n");
    if failed.is_empty() {
        out.push_str("(none)\n");
    }
    for item in failed {
        out.push_str(&format!("- [{}] {}\n", item.id, item.content));
    }
    out
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[test]
    fn extract_json_unwraps_fenced_block() {
        let raw = "Sure thing:\n```json\n{\"items\":[]}\n```";
        assert_eq!(extract_json(raw), Some("{\"items\":[]}"));
    }

    #[test]
    fn extract_json_unwraps_bare_object() {
        let raw = "  {\"items\": []}  ";
        assert_eq!(extract_json(raw), Some("{\"items\": []}"));
    }

    #[test]
    fn extract_json_falls_back_to_brace_range() {
        let raw = "Sure: {\"items\":[{\"id\":\"a\",\"content\":\"x\"}]} done";
        let got = extract_json(raw).unwrap();
        assert!(got.starts_with('{') && got.ends_with('}'));
    }

    #[test]
    fn parse_plan_items_minimal() {
        let raw = r#"{"items":[{"id":"a","content":"do a"}]}"#;
        let items = parse_plan_items(raw).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "a");
        assert_eq!(items[0].priority, "medium");
        assert!(items[0].success_check.is_none());
    }

    #[test]
    fn parse_plan_items_with_success_check() {
        let raw = r#"{"items":[
            {"id":"a","content":"do a","priority":"high",
             "success_check":{"kind":"cargo_test","spec":"rate_limit::*","timeout_ms":30000}}
        ]}"#;
        let items = parse_plan_items(raw).unwrap();
        let check = items[0].success_check.as_ref().unwrap();
        assert_eq!(check.kind, SuccessCheckKind::CargoTest);
        assert_eq!(check.spec, "rate_limit::*");
        assert_eq!(check.timeout_ms, 30_000);
    }

    #[test]
    fn parse_plan_items_rejects_unknown_kind() {
        let raw = r#"{"items":[
            {"id":"a","content":"do a",
             "success_check":{"kind":"nonsense","spec":"x"}}
        ]}"#;
        let err = parse_plan_items(raw).unwrap_err();
        matches!(err, PlannerError::InvalidJson(_));
    }

    #[test]
    fn parse_plan_items_rejects_empty() {
        let raw = r#"{"items":[]}"#;
        let err = parse_plan_items(raw).unwrap_err();
        matches!(err, PlannerError::Empty);
    }

    // Stub LlmCaller used to exercise LlmPlanner.
    struct StubCaller {
        responses: Mutex<Vec<Result<String, String>>>,
    }

    #[async_trait]
    impl LlmCaller for StubCaller {
        async fn complete(&self, _prompt: &str, _user: &str) -> Result<String, String> {
            let mut g = self.responses.lock().unwrap();
            g.remove(0)
        }
    }

    #[tokio::test]
    async fn llm_planner_retries_on_first_invalid_parse() {
        let caller = Arc::new(StubCaller {
            responses: Mutex::new(vec![
                Ok("not json at all".to_string()),
                Ok(r#"{"items":[{"id":"a","content":"x"}]}"#.to_string()),
            ]),
        });
        let planner = LlmPlanner::new(caller);
        let goal = Goal::new("ship it", jcode_task_types::GoalScope::Project);
        let items = planner.decompose(&goal).await.unwrap();
        assert_eq!(items.len(), 1);
    }
}
