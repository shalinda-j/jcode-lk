# Goal-Driven Loops

Status: **Phase 0 — types landed, runtime not yet wired.**

`/goal` mode lets a user state a high-level outcome in one sentence and have
jcode drive the swarm against it until the goal's **success criteria** all
pass — or a budget cap is hit, or the user aborts. This document describes the
contract: the types that back the loop, the state machine the controller will
run, and the boundaries every later phase must respect.

## Why this exists

The swarm primitives in `jcode-swarm-core` (`SwarmRole`, `SwarmLifecycleStatus`,
completion reports) and the goal primitives in `jcode-task-types` (`Goal`,
`GoalMilestone`) already let the system *describe* multi-step work. What's been
missing is the **loop that ties them together**: take a goal, decompose it,
dispatch sub-agents, verify the result, and stop only when verifiable success
criteria are satisfied. Without that loop, agents stop on intuition (or token
limit) — not on the actual definition of done.

## State machine

The controller transitions a goal through these phases. No other transitions
are valid; anything else is a bug.

```
Define     → Decompose
Decompose  → Dispatch
Dispatch   → Execute
Execute    → Verify
Verify     → Done | Repair | Replan | Aborted
Repair     → Dispatch
Replan     → Decompose
<any>      → Aborted    (budget exhausted, user /abort, unrecoverable verifier failure)
```

Terminal states: `Done`, `Aborted`. They are reached **by criteria, not by
turn count**.

### Stop conditions, in priority order

1. User issued `/abort` for this goal.
2. `BudgetTracker` exceeded any of: `tokens`, `usd_cents`, `wall_secs`, or
   `retries_per_item` on the same item.
3. All `Goal.success_criteria` pass according to the verifier.
4. Plan has zero runnable items **and** verifier reports unrecoverable.

## Types (Phase 0 — landed)

All new types live in `jcode-task-types`. Existing persisted state still
deserializes — every new field on `Goal` / `PlanItem` is `Option` or
`#[serde(default)]`.

### `SuccessCheckKind`

How a single criterion is verified:

| Variant            | Spec interpreted as                                            |
|--------------------|----------------------------------------------------------------|
| `Shell`            | A shell command. Exit 0 = pass. Stdout/stderr captured.        |
| `CargoTest`        | A `cargo test <filter>` argument.                              |
| `Pytest`           | A pytest selector (`-k`, node id, or path).                    |
| `JestTest`         | A jest selector.                                               |
| `FileAbsent`       | A glob; passes if zero matches.                                |
| `Regex`            | A regex; passes if **no** match found anywhere in the repo.    |
| `AgentAssertion`   | Natural-language assertion, evaluated by the verifier sub-agent with a structured JSON verdict. |

### `SuccessCheck`

```rust
pub struct SuccessCheck {
    pub kind: SuccessCheckKind,
    pub spec: String,
    pub timeout_ms: u32,  // default: 120_000 (2 min)
}
```

### `CheckResult`

```rust
pub struct CheckResult {
    pub passed: bool,
    pub detail: String,
    pub duration_ms: u32,
}
```

### `GoalLoopPhase`

The state-machine state, mirrored to disk via `Goal.loop_phase` so the side
panel and replay can show progress and a crash can resume mid-loop.

### `GoalBudget`

```rust
pub struct GoalBudget {
    pub tokens: Option<u64>,
    pub usd_cents: Option<u32>,
    pub wall_secs: Option<u32>,
    pub retries_per_item: u8,  // default: 3
}
```

`GoalBudget::default_interactive()` returns the budget used by `/goal` when no
caps are given: `tokens: None, usd_cents: Some(200), wall_secs: Some(1_800),
retries_per_item: 3`.

### Goal additions

`Goal` gains two optional fields:

- `budget: Option<GoalBudget>` — caps for the loop. `None` means "no caps
  beyond per-item retries".
- `loop_phase: Option<GoalLoopPhase>` — current phase, surfaced in the side
  panel. `None` outside an active loop.

### PlanItem addition

`PlanItem` and `SwarmPlanItemSpec` gain `success_check: Option<SuccessCheck>`.
The planner attaches one of these per item; the verifier reads it.

## Decisions baked in for v0.13

These are the defaults the controller will use unless overridden. They are
deliberately conservative; we can revisit once we have telemetry.

1. **Verifier model** — the cheapest configured provider model (e.g. Haiku).
   The verifier should not pay Opus rates to grade tests.
2. **Default interactive budget** — `$2.00`, `30 min`, no token cap,
   `3` retries per item.
3. **Unknown-binary safety check** — if a `Shell` check uses a binary not on
   the safety allowlist, the controller asks the user **once** at goal start.
   In non-interactive mode (`--no-tui`), the goal aborts instead of asking.

## Out of scope for Phase 0

- The `GoalLoopController` state machine.
- The `jcode-verifier` crate and check executors.
- Decompose / replan prompts.
- Side-panel DAG view.
- `/goal` slash command and CLI subcommand.
- Telemetry counters.

These ship in PR-2 through PR-5. See the implementation plan in the session
transcript for sequencing.

## Compatibility

Persisted goals from earlier versions deserialize unchanged: `budget` and
`loop_phase` default to `None`, and `PlanItem.success_check` defaults to
`None`. Code that constructs these structs by literal had to add
`success_check: None` (mechanical change, no behavior shift).
