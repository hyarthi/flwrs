//! The pre-flight retry loop, shared by the director (a load held back) and
//! the scene service (a draft's submit held back).

use super::handle::{PreflightIssue, PreflightRetryStatus};
use crate::modules::configurator::{self, Limits, PreflightTarget};
use crate::modules::node_config::PreflightRetry;
use crate::modules::scene::service::{SceneSnapshot, Service as SceneService};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// What a wait has seen so far.
#[derive(Clone, Default)]
pub(crate) struct WaitProgress {
    pub issues: Vec<PreflightIssue>,
    pub retry: PreflightRetryStatus,
}

/// How a wait ended.
pub(crate) enum WaitEnd {
    Passed,
    /// A permanent problem, or the retry limit (`gave_up`: after that many checks).
    Problems {
        issues: Vec<PreflightIssue>,
        gave_up: Option<u32>,
    },
    Cancelled,
}

/// The line a wait that gave up adds to its problems.
pub(crate) fn gave_up_line(attempts: u32) -> String {
    format!("pre-flight: gave up after {attempts} check(s) (preflight_retry)")
}

/// Checks `targets` until they pass, one has a permanent problem, or the
/// limits are reached. Only the nodes still failing are checked again.
pub(crate) async fn preflight_loop(
    mut targets: Vec<PreflightTarget>,
    limits: Limits,
    retry: PreflightRetry,
    cancel: CancellationToken,
    progress: watch::Sender<WaitProgress>,
    scenes: &'static SceneService,
    snapshot: SceneSnapshot,
) -> WaitEnd {
    let started = tokio::time::Instant::now();
    let mut delay = retry.initial_delay;
    let mut attempt = 0;
    loop {
        attempt += 1;
        progress.send_modify(|p| {
            p.retry = PreflightRetryStatus {
                attempt,
                max_attempts: retry.max_attempts,
                next_attempt_time: None,
            };
        });
        // What's running can change between attempts.
        scenes.fill_releasing(&snapshot, &mut targets);
        let reports = configurator::run_preflight(&targets, &limits, &cancel).await;
        if cancel.is_cancelled() {
            return WaitEnd::Cancelled;
        }
        let mut issues = vec![];
        for (node_id, report) in &reports {
            for warning in &report.warnings {
                log::debug!("Pre-flight: node [{node_id}]: {warning}");
            }
            issues.extend(report.problems.iter().map(|p| PreflightIssue {
                node_id: node_id.clone(),
                message: p.message.clone(),
                retryable: p.retryable,
            }));
        }
        if issues.is_empty() {
            return WaitEnd::Passed;
        }
        if issues.iter().any(|i| !i.retryable) {
            return WaitEnd::Problems {
                issues,
                gave_up: None,
            };
        }
        let left = retry.max_wait.saturating_sub(started.elapsed());
        if attempt >= retry.max_attempts || left.is_zero() {
            return WaitEnd::Problems {
                issues,
                gave_up: Some(attempt),
            };
        }
        let pause = delay.min(left);
        targets.retain(|t| issues.iter().any(|i| i.node_id == t.node_id));
        let next = chrono::Utc::now() + pause;
        progress.send_modify(|p| {
            p.issues = issues;
            p.retry.next_attempt_time = Some(next.timestamp_millis());
        });
        tokio::select! {
            _ = cancel.cancelled() => return WaitEnd::Cancelled,
            _ = tokio::time::sleep(pause) => {}
        }
        delay = delay.saturating_mul(2).min(retry.max_delay);
    }
}
