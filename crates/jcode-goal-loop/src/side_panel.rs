//! Render a [`GoalLoopSnapshot`] into the side panel.
//!
//! The TUI's side panel renders markdown pages, so we produce one ephemeral
//! page per active goal loop. The TUI subscribes to controller events and
//! re-renders this page on each transition.

use jcode_side_panel_types::{
    SidePanelPage, SidePanelPageFormat, SidePanelPageSource,
};
use jcode_task_types::GoalLoopPhase;

use crate::controller::{GoalLoopSnapshot, ItemRunResult};

const PAGE_ID_PREFIX: &str = "goal_loop:";

pub fn page_id_for_goal(goal_id: &str) -> String {
    format!("{PAGE_ID_PREFIX}{goal_id}")
}

pub fn render_snapshot_markdown(snapshot: &GoalLoopSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Goal `{}`\n\n", snapshot.goal_id));
    out.push_str(&format!(
        "**Phase:** `{}`\n\n",
        snapshot.phase.as_str()
    ));

    out.push_str("## Budget\n\n");
    out.push_str(&format!(
        "- tokens used: {}{}\n",
        snapshot.tokens_used,
        cap_suffix("tok", snapshot.budget.tokens.map(|t| t as u64))
    ));
    out.push_str(&format!(
        "- spent: ${:.2}{}\n",
        snapshot.cents_used as f32 / 100.0,
        cap_suffix(
            "USD",
            snapshot.budget.usd_cents.map(|c| c as u64)
        )
        .replace("USD ", "$") // cosmetic only
    ));
    out.push_str(&format!(
        "- elapsed: {}s{}\n",
        snapshot.elapsed_secs,
        cap_suffix("s", snapshot.budget.wall_secs.map(|s| s as u64))
    ));
    out.push_str(&format!(
        "- retries per item: {}\n\n",
        snapshot.budget.retries_per_item
    ));

    out.push_str("## Plan\n\n");
    if snapshot.items.is_empty() {
        out.push_str("_no items yet — planner has not run_\n");
    } else {
        out.push_str("| # | id | status | attempts | check |\n");
        out.push_str("|---|----|--------|----------|-------|\n");
        for (idx, record) in snapshot.items.iter().enumerate() {
            let status = match record.last_outcome {
                Some(ItemRunResult::Passed) => "✓ passed",
                Some(ItemRunResult::Repairing) => "⟳ repairing",
                Some(ItemRunResult::Failed) => "✗ failed",
                None => "· pending",
            };
            let check = record
                .last_check
                .as_ref()
                .map(|c| truncate(&c.detail, 60))
                .unwrap_or_else(|| "—".to_string());
            out.push_str(&format!(
                "| {} | `{}` | {} | {} | {} |\n",
                idx + 1,
                record.item.id,
                status,
                record.attempts,
                check
            ));
        }
    }

    if matches!(snapshot.phase, GoalLoopPhase::Aborted) {
        out.push_str("\n> Goal loop aborted. See ItemFinished events for last status.\n");
    } else if matches!(snapshot.phase, GoalLoopPhase::Done) {
        out.push_str("\n> Goal loop complete. All success criteria passed.\n");
    }

    out
}

pub fn snapshot_as_side_panel_page(snapshot: &GoalLoopSnapshot, updated_at_ms: u64) -> SidePanelPage {
    SidePanelPage {
        id: page_id_for_goal(&snapshot.goal_id),
        title: format!("Goal: {}", snapshot.goal_id),
        file_path: String::new(),
        format: SidePanelPageFormat::Markdown,
        source: SidePanelPageSource::Ephemeral,
        content: render_snapshot_markdown(snapshot),
        updated_at_ms,
    }
}

fn truncate(s: &str, max: usize) -> String {
    let cleaned = s.replace('\n', " ");
    if cleaned.chars().count() <= max {
        cleaned
    } else {
        let cut: String = cleaned.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

fn cap_suffix(unit: &str, cap: Option<u64>) -> String {
    match cap {
        Some(c) => format!(" / {c} {unit}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{ItemRecord, ItemRunResult};
    use jcode_plan::PlanItem;
    use jcode_task_types::{CheckResult, GoalBudget, GoalLoopPhase};

    fn record(id: &str, outcome: Option<ItemRunResult>, detail: Option<&str>) -> ItemRecord {
        ItemRecord {
            item: PlanItem {
                content: format!("do {id}"),
                status: "queued".to_string(),
                priority: "high".to_string(),
                id: id.to_string(),
                subsystem: None,
                file_scope: Vec::new(),
                blocked_by: Vec::new(),
                assigned_to: None,
                success_check: None,
            },
            attempts: 1,
            last_outcome: outcome,
            last_check: detail.map(|d| CheckResult {
                passed: matches!(outcome, Some(ItemRunResult::Passed)),
                detail: d.to_string(),
                duration_ms: 1,
            }),
        }
    }

    fn snapshot() -> GoalLoopSnapshot {
        GoalLoopSnapshot {
            goal_id: "ship-it".into(),
            phase: GoalLoopPhase::Execute,
            items: vec![
                record("a", Some(ItemRunResult::Passed), Some("tests pass")),
                record("b", Some(ItemRunResult::Repairing), Some("missing burst case")),
                record("c", None, None),
            ],
            tokens_used: 12_345,
            cents_used: 42,
            elapsed_secs: 90,
            budget: GoalBudget {
                tokens: None,
                usd_cents: Some(200),
                wall_secs: Some(1_800),
                retries_per_item: 3,
            },
        }
    }

    #[test]
    fn render_includes_phase_and_budget() {
        let md = render_snapshot_markdown(&snapshot());
        assert!(md.contains("# Goal `ship-it`"));
        assert!(md.contains("`execute`"));
        assert!(md.contains("$0.42"));
        assert!(md.contains("90s"));
        assert!(md.contains("retries per item: 3"));
    }

    #[test]
    fn render_shows_per_item_status() {
        let md = render_snapshot_markdown(&snapshot());
        assert!(md.contains("✓ passed"));
        assert!(md.contains("⟳ repairing"));
        assert!(md.contains("· pending"));
        assert!(md.contains("tests pass"));
        assert!(md.contains("missing burst case"));
    }

    #[test]
    fn page_id_is_stable_for_goal() {
        let p1 = page_id_for_goal("foo");
        let p2 = page_id_for_goal("foo");
        assert_eq!(p1, p2);
        assert!(p1.starts_with("goal_loop:"));
    }

    #[test]
    fn empty_plan_renders_placeholder() {
        let mut snap = snapshot();
        snap.items.clear();
        let md = render_snapshot_markdown(&snap);
        assert!(md.contains("no items yet"));
    }

    #[test]
    fn aborted_phase_renders_callout() {
        let mut snap = snapshot();
        snap.phase = GoalLoopPhase::Aborted;
        let md = render_snapshot_markdown(&snap);
        assert!(md.contains("aborted"));
    }

    #[test]
    fn done_phase_renders_callout() {
        let mut snap = snapshot();
        snap.phase = GoalLoopPhase::Done;
        let md = render_snapshot_markdown(&snap);
        assert!(md.contains("complete"));
    }

    #[test]
    fn page_renders_with_ephemeral_source() {
        let page = snapshot_as_side_panel_page(&snapshot(), 1_700_000_000_000);
        assert_eq!(page.source, SidePanelPageSource::Ephemeral);
        assert_eq!(page.format, SidePanelPageFormat::Markdown);
        assert!(page.content.contains("Goal `ship-it`"));
    }
}
