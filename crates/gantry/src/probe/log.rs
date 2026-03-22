use std::time::{Duration, Instant};

use bollard::Docker;
use bollard::container::LogsOptions;
use futures::StreamExt;
use regex::Regex;

use super::ProbeResult;

/// Probe container logs for success/failure patterns.
///
/// Two modes:
/// - `follow=true` (start/converge): scan existing logs first (last match wins),
///   then stream new logs if no definitive result (first match wins).
///   This correctly handles old success before new failure.
/// - `follow=false` (reprobe): scan all existing logs, last match wins.
///
/// `since` controls the log start timestamp (0 = all logs).
pub async fn probe_log(
    docker: &Docker,
    container: &str,
    success_pattern: &str,
    failure_pattern: Option<&str>,
    deadline: Duration,
    since: i64,
    follow: bool,
) -> (ProbeResult, Vec<String>) {
    let start = Instant::now();

    let success_re = match Regex::new(success_pattern) {
        Ok(re) => re,
        Err(e) => {
            return (
                ProbeResult::Failed {
                    error: format!("invalid success pattern: {e}"),
                    duration_ms: 0,
                },
                Vec::new(),
            );
        }
    };

    let failure_re = failure_pattern.and_then(|p| Regex::new(p).ok());

    if follow {
        // Phase 1: Scan existing logs (last match wins)
        let (scan_result, scan_lines) =
            scan_logs(docker, container, &success_re, failure_re.as_ref(), since).await;

        if let Some(ok) = scan_result {
            let elapsed = start.elapsed().as_millis() as u64;
            return if ok {
                (
                    ProbeResult::Ok {
                        duration_ms: elapsed,
                    },
                    scan_lines,
                )
            } else {
                (
                    ProbeResult::Failed {
                        error: "failure pattern matched".into(),
                        duration_ms: elapsed,
                    },
                    scan_lines,
                )
            };
        }

        // Phase 2: No definitive result from scan — stream new logs
        let deadline_at = start + deadline;
        let opts = LogsOptions::<String> {
            follow: true,
            stdout: true,
            stderr: true,
            since: chrono::Utc::now().timestamp(),
            ..Default::default()
        };

        let mut stream = docker.logs(container, Some(opts));
        let mut matched_lines = scan_lines;

        loop {
            let remaining = deadline_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }

            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(output))) => {
                    let line = output.to_string();
                    let trimmed = line.trim().to_string();
                    if let Some(ref fre) = failure_re
                        && fre.is_match(&line)
                    {
                        let elapsed = start.elapsed().as_millis() as u64;
                        matched_lines.push(trimmed);
                        return (
                            ProbeResult::Failed {
                                error: "failure pattern matched".into(),
                                duration_ms: elapsed,
                            },
                            matched_lines,
                        );
                    }
                    if success_re.is_match(&line) {
                        let elapsed = start.elapsed().as_millis() as u64;
                        matched_lines.push(trimmed);
                        return (
                            ProbeResult::Ok {
                                duration_ms: elapsed,
                            },
                            matched_lines,
                        );
                    }
                }
                Ok(Some(Err(e))) => {
                    let elapsed = start.elapsed().as_millis() as u64;
                    return (
                        ProbeResult::Failed {
                            error: format!("log stream error: {e}"),
                            duration_ms: elapsed,
                        },
                        matched_lines,
                    );
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        let elapsed = start.elapsed().as_millis() as u64;
        (
            ProbeResult::Failed {
                error: "no pattern matched before timeout".into(),
                duration_ms: elapsed,
            },
            matched_lines,
        )
    } else {
        // Scan mode: consume all available logs, last match wins.
        let (scan_result, scan_lines) =
            scan_logs(docker, container, &success_re, failure_re.as_ref(), since).await;

        let elapsed = start.elapsed().as_millis() as u64;
        match scan_result {
            Some(true) => (
                ProbeResult::Ok {
                    duration_ms: elapsed,
                },
                scan_lines,
            ),
            Some(false) => (
                ProbeResult::Failed {
                    error: "failure pattern matched".into(),
                    duration_ms: elapsed,
                },
                scan_lines,
            ),
            None => (
                ProbeResult::Failed {
                    error: "success pattern not found in logs".into(),
                    duration_ms: elapsed,
                },
                scan_lines,
            ),
        }
    }
}

/// Scan existing logs without following. Returns (last_match, matched_lines).
/// last_match: Some(true) = success was last, Some(false) = failure was last, None = no match.
async fn scan_logs(
    docker: &Docker,
    container: &str,
    success_re: &Regex,
    failure_re: Option<&Regex>,
    since: i64,
) -> (Option<bool>, Vec<String>) {
    let opts = LogsOptions::<String> {
        follow: false,
        stdout: true,
        stderr: true,
        since,
        ..Default::default()
    };

    let mut stream = docker.logs(container, Some(opts));
    let mut last_result: Option<bool> = None;
    let mut matched_lines: Vec<String> = Vec::new();

    while let Some(Ok(output)) = stream.next().await {
        let line = output.to_string();
        let trimmed = line.trim().to_string();
        if let Some(fre) = failure_re
            && fre.is_match(&line)
        {
            last_result = Some(false);
            matched_lines.push(trimmed.clone());
        }
        if success_re.is_match(&line) {
            last_result = Some(true);
            matched_lines.push(trimmed);
        }
    }

    (last_result, matched_lines)
}
