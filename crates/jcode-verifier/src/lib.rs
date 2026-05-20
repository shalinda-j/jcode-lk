//! Runs a single [`SuccessCheck`] and returns a [`CheckResult`].
//!
//! Each [`SuccessCheckKind`] maps to one of the handlers below. The
//! [`AgentAssertionRunner`] trait lets the controller inject an LLM-backed
//! evaluator without giving this crate a hard dependency on a provider.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jcode_task_types::{CheckResult, SuccessCheck, SuccessCheckKind};
use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

const MAX_CAPTURED_OUTPUT_BYTES: usize = 4 * 1024;

#[derive(Debug, Error)]
pub enum VerifierError {
    #[error("spec was empty for kind {0}")]
    EmptySpec(&'static str),
    #[error("invalid glob: {0}")]
    InvalidGlob(String),
    #[error("invalid regex: {0}")]
    InvalidRegex(String),
}

/// Pluggable evaluator for [`SuccessCheckKind::AgentAssertion`] checks.
///
/// The controller in `src/goal_loop.rs` wires up a real implementation that
/// runs a one-shot LLM call with a fixed verifier prompt. Tests use
/// [`StubAgentAssertion`] or a custom impl.
#[async_trait]
pub trait AgentAssertionRunner: Send + Sync {
    async fn evaluate(&self, assertion: &str, cwd: &Path) -> CheckResult;
}

/// Always-fail runner used when no real agent runner is configured.
pub struct NullAgentAssertion;

#[async_trait]
impl AgentAssertionRunner for NullAgentAssertion {
    async fn evaluate(&self, _assertion: &str, _cwd: &Path) -> CheckResult {
        CheckResult {
            passed: false,
            detail: "agent assertion runner not configured".to_string(),
            duration_ms: 0,
        }
    }
}

/// Fixed-verdict runner for tests.
pub struct StubAgentAssertion {
    pub passed: bool,
    pub detail: String,
}

#[async_trait]
impl AgentAssertionRunner for StubAgentAssertion {
    async fn evaluate(&self, _assertion: &str, _cwd: &Path) -> CheckResult {
        CheckResult {
            passed: self.passed,
            detail: self.detail.clone(),
            duration_ms: 0,
        }
    }
}

/// Run a single success check against `cwd`.
///
/// Network access and process spawning are intentional: the loop controller
/// must reject checks whose specs use non-allowlisted binaries before calling
/// this function. See `docs/GOAL_LOOPS.md` for the policy.
pub async fn run_check(
    check: &SuccessCheck,
    cwd: &Path,
    agent_runner: Arc<dyn AgentAssertionRunner>,
) -> CheckResult {
    let started = Instant::now();
    let result = match check.kind {
        SuccessCheckKind::Shell => run_shell(&check.spec, cwd, check.timeout_ms).await,
        SuccessCheckKind::CargoTest => {
            run_test_command("cargo", &["test", "--"], &check.spec, cwd, check.timeout_ms).await
        }
        SuccessCheckKind::Pytest => {
            run_test_command("pytest", &[], &check.spec, cwd, check.timeout_ms).await
        }
        SuccessCheckKind::JestTest => {
            run_test_command("npx", &["jest"], &check.spec, cwd, check.timeout_ms).await
        }
        SuccessCheckKind::FileAbsent => run_file_absent(&check.spec, cwd),
        SuccessCheckKind::Regex => run_regex_must_be_absent(&check.spec, cwd),
        SuccessCheckKind::AgentAssertion => {
            return finalize(agent_runner.evaluate(&check.spec, cwd).await, started);
        }
    };
    finalize(result, started)
}

fn finalize(mut result: CheckResult, started: Instant) -> CheckResult {
    let elapsed_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
    if result.duration_ms == 0 {
        result.duration_ms = elapsed_ms;
    }
    result
}

async fn run_shell(spec: &str, cwd: &Path, timeout_ms: u32) -> CheckResult {
    if spec.trim().is_empty() {
        return CheckResult {
            passed: false,
            detail: "empty shell spec".to_string(),
            duration_ms: 0,
        };
    }
    let mut cmd = Command::new("sh");
    cmd.arg("-lc").arg(spec).current_dir(cwd);
    run_command(cmd, timeout_ms).await
}

async fn run_test_command(
    program: &str,
    base_args: &[&str],
    spec: &str,
    cwd: &Path,
    timeout_ms: u32,
) -> CheckResult {
    let mut cmd = Command::new(program);
    for a in base_args {
        cmd.arg(a);
    }
    for token in spec.split_whitespace() {
        cmd.arg(token);
    }
    cmd.current_dir(cwd);
    run_command(cmd, timeout_ms).await
}

async fn run_command(mut cmd: Command, timeout_ms: u32) -> CheckResult {
    cmd.kill_on_drop(true);
    let fut = cmd.output();
    let dur = Duration::from_millis(u64::from(timeout_ms.max(1)));
    match timeout(dur, fut).await {
        Ok(Ok(output)) => {
            let stdout = trim_tail(&output.stdout);
            let stderr = trim_tail(&output.stderr);
            let detail = if stderr.is_empty() {
                stdout
            } else if stdout.is_empty() {
                stderr
            } else {
                format!("{stdout}\n---\n{stderr}")
            };
            CheckResult {
                passed: output.status.success(),
                detail,
                duration_ms: 0,
            }
        }
        Ok(Err(e)) => CheckResult {
            passed: false,
            detail: format!("spawn failed: {e}"),
            duration_ms: 0,
        },
        Err(_) => CheckResult {
            passed: false,
            detail: format!("timeout after {timeout_ms}ms"),
            duration_ms: timeout_ms,
        },
    }
}

fn trim_tail(buf: &[u8]) -> String {
    let start = buf.len().saturating_sub(MAX_CAPTURED_OUTPUT_BYTES);
    String::from_utf8_lossy(&buf[start..]).trim().to_string()
}

fn run_file_absent(spec: &str, cwd: &Path) -> CheckResult {
    let pattern = if Path::new(spec).is_absolute() {
        spec.to_string()
    } else {
        cwd.join(spec).to_string_lossy().to_string()
    };
    let matches: Vec<PathBuf> = match glob::glob(&pattern) {
        Ok(iter) => iter.flatten().collect(),
        Err(e) => {
            return CheckResult {
                passed: false,
                detail: format!("invalid glob `{spec}`: {e}"),
                duration_ms: 0,
            };
        }
    };
    if matches.is_empty() {
        CheckResult {
            passed: true,
            detail: format!("no files match `{spec}`"),
            duration_ms: 0,
        }
    } else {
        let listing: Vec<String> = matches
            .iter()
            .take(10)
            .map(|p| p.display().to_string())
            .collect();
        CheckResult {
            passed: false,
            detail: format!(
                "{} file(s) still match `{spec}`: {}",
                matches.len(),
                listing.join(", ")
            ),
            duration_ms: 0,
        }
    }
}

fn run_regex_must_be_absent(spec: &str, cwd: &Path) -> CheckResult {
    let re = match regex::Regex::new(spec) {
        Ok(r) => r,
        Err(e) => {
            return CheckResult {
                passed: false,
                detail: format!("invalid regex `{spec}`: {e}"),
                duration_ms: 0,
            };
        }
    };
    let walker = ignore::WalkBuilder::new(cwd).build();
    let mut hits: Vec<(PathBuf, usize, String)> = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path().to_path_buf();
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                hits.push((path.clone(), i + 1, line.to_string()));
                if hits.len() >= 20 {
                    break;
                }
            }
        }
        if hits.len() >= 20 {
            break;
        }
    }
    if hits.is_empty() {
        CheckResult {
            passed: true,
            detail: format!("regex `{spec}` not found in repo"),
            duration_ms: 0,
        }
    } else {
        let preview: Vec<String> = hits
            .iter()
            .take(5)
            .map(|(p, ln, line)| {
                let rel = p.strip_prefix(cwd).unwrap_or(p).display();
                let snippet = if line.len() > 80 {
                    format!("{}...", &line[..80])
                } else {
                    line.clone()
                };
                format!("{rel}:{ln}: {snippet}")
            })
            .collect();
        CheckResult {
            passed: false,
            detail: format!(
                "regex `{spec}` matched {} line(s) (showing up to 5):\n{}",
                hits.len(),
                preview.join("\n")
            ),
            duration_ms: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_check(spec: &str) -> SuccessCheck {
        SuccessCheck {
            kind: SuccessCheckKind::Shell,
            spec: spec.to_string(),
            timeout_ms: 5_000,
        }
    }

    fn null_runner() -> Arc<dyn AgentAssertionRunner> {
        Arc::new(NullAgentAssertion)
    }

    #[tokio::test]
    async fn shell_pass_on_zero_exit() {
        let r = run_check(&shell_check("exit 0"), Path::new("."), null_runner()).await;
        assert!(r.passed, "detail = {}", r.detail);
    }

    #[tokio::test]
    async fn shell_fail_on_nonzero_exit() {
        let r = run_check(&shell_check("exit 7"), Path::new("."), null_runner()).await;
        assert!(!r.passed);
    }

    #[tokio::test]
    async fn shell_captures_output_tail() {
        let r = run_check(
            &shell_check("echo hello-from-shell-check; exit 0"),
            Path::new("."),
            null_runner(),
        )
        .await;
        assert!(r.passed);
        assert!(r.detail.contains("hello-from-shell-check"));
    }

    #[tokio::test]
    async fn shell_times_out() {
        let check = SuccessCheck {
            kind: SuccessCheckKind::Shell,
            spec: "sleep 5".to_string(),
            timeout_ms: 150,
        };
        let r = run_check(&check, Path::new("."), null_runner()).await;
        assert!(!r.passed);
        assert!(r.detail.contains("timeout"));
    }

    #[tokio::test]
    async fn file_absent_passes_on_zero_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let check = SuccessCheck {
            kind: SuccessCheckKind::FileAbsent,
            spec: "*.never-matches-this".to_string(),
            timeout_ms: 1_000,
        };
        let r = run_check(&check, tmp.path(), null_runner()).await;
        assert!(r.passed, "{}", r.detail);
    }

    #[tokio::test]
    async fn file_absent_fails_when_match_exists() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("forbidden.tmp"), "x").unwrap();
        let check = SuccessCheck {
            kind: SuccessCheckKind::FileAbsent,
            spec: "*.tmp".to_string(),
            timeout_ms: 1_000,
        };
        let r = run_check(&check, tmp.path(), null_runner()).await;
        assert!(!r.passed);
        assert!(r.detail.contains("forbidden.tmp"));
    }

    #[tokio::test]
    async fn regex_passes_when_unmatched() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "ok content\n").unwrap();
        let check = SuccessCheck {
            kind: SuccessCheckKind::Regex,
            spec: "FORBIDDEN_PATTERN".to_string(),
            timeout_ms: 1_000,
        };
        let r = run_check(&check, tmp.path(), null_runner()).await;
        assert!(r.passed, "{}", r.detail);
    }

    #[tokio::test]
    async fn regex_fails_when_matched_and_reports_location() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("b.txt"), "before\nFORBIDDEN_PATTERN here\nafter\n")
            .unwrap();
        let check = SuccessCheck {
            kind: SuccessCheckKind::Regex,
            spec: "FORBIDDEN_PATTERN".to_string(),
            timeout_ms: 1_000,
        };
        let r = run_check(&check, tmp.path(), null_runner()).await;
        assert!(!r.passed);
        assert!(r.detail.contains("b.txt"), "detail = {}", r.detail);
    }

    #[tokio::test]
    async fn agent_assertion_routes_to_runner() {
        let check = SuccessCheck {
            kind: SuccessCheckKind::AgentAssertion,
            spec: "tests cover the burst case".to_string(),
            timeout_ms: 1_000,
        };
        let runner: Arc<dyn AgentAssertionRunner> = Arc::new(StubAgentAssertion {
            passed: true,
            detail: "verifier said yes".to_string(),
        });
        let r = run_check(&check, Path::new("."), runner).await;
        assert!(r.passed);
        assert_eq!(r.detail, "verifier said yes");
    }

    #[tokio::test]
    async fn agent_assertion_with_null_runner_fails_cleanly() {
        let check = SuccessCheck {
            kind: SuccessCheckKind::AgentAssertion,
            spec: "anything".to_string(),
            timeout_ms: 1_000,
        };
        let r = run_check(&check, Path::new("."), null_runner()).await;
        assert!(!r.passed);
        assert!(r.detail.contains("not configured"));
    }
}
