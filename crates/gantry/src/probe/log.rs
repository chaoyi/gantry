use std::time::{Duration, Instant};

use bollard::Docker;
use bollard::container::LogsOptions;
use futures::StreamExt;
use regex::Regex;

use super::ProbeResult;

/// Stream or scan container logs looking for a success/failure pattern.
///
/// When `follow` is true (used by start/restart): streams new log output with a
/// deadline timeout, returning immediately on the first success match.
/// `since` controls the log start timestamp.
///
/// When `follow` is false (used by reprobe): fetches all existing logs without
/// following, scans every line, and lets the last match win (failure after
/// success → probe is red).
pub async fn probe_log(
    docker: &Docker,
    container: &str,
    success_pattern: &str,
    failure_pattern: Option<&str>,
    deadline: Duration,
    since: i64,
    follow: bool,
) -> ProbeResult {
    let start = Instant::now();

    let success_re = match Regex::new(success_pattern) {
        Ok(re) => re,
        Err(e) => {
            return ProbeResult::Failed {
                error: format!("invalid success pattern: {e}"),
                duration_ms: 0,
            };
        }
    };

    let failure_re = failure_pattern.and_then(|p| Regex::new(p).ok());

    let opts = LogsOptions::<String> {
        follow,
        stdout: true,
        stderr: true,
        since,
        ..Default::default()
    };

    let mut stream = docker.logs(container, Some(opts));
    let mut last_result: Option<bool> = None; // true = success, false = failure

    if follow {
        // Streaming mode: race against deadline, return on first success.
        let deadline_at = start + deadline;

        loop {
            let remaining = deadline_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }

            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(output))) => {
                    let line = output.to_string();
                    if let Some(ref fre) = failure_re
                        && fre.is_match(&line)
                    {
                        last_result = Some(false);
                    }
                    if success_re.is_match(&line) {
                        last_result = Some(true);
                    }
                    // Return immediately on success — during start, success means ready.
                    if last_result == Some(true) {
                        let elapsed = start.elapsed().as_millis() as u64;
                        return ProbeResult::Ok {
                            duration_ms: elapsed,
                        };
                    }
                }
                Ok(Some(Err(e))) => {
                    let elapsed = start.elapsed().as_millis() as u64;
                    return ProbeResult::Failed {
                        error: format!("log stream error: {e}"),
                        duration_ms: elapsed,
                    };
                }
                Ok(None) => break, // stream ended
                Err(_) => break,   // deadline exceeded
            }
        }
    } else {
        // Scan mode: consume all available logs, last match wins.
        while let Some(Ok(output)) = stream.next().await {
            let line = output.to_string();
            if let Some(ref fre) = failure_re
                && fre.is_match(&line)
            {
                last_result = Some(false);
            }
            if success_re.is_match(&line) {
                last_result = Some(true);
            }
        }
    }

    let elapsed = start.elapsed().as_millis() as u64;
    match last_result {
        Some(true) => ProbeResult::Ok {
            duration_ms: elapsed,
        },
        Some(false) => ProbeResult::Failed {
            error: "failure pattern matched".to_string(),
            duration_ms: elapsed,
        },
        None => ProbeResult::Failed {
            error: if follow {
                "no pattern matched before timeout".to_string()
            } else {
                "success pattern not found in logs".to_string()
            },
            duration_ms: elapsed,
        },
    }
}
