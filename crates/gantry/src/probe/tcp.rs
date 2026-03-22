use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

use crate::config::BackoffConfig;

use super::{BackoffIter, ProbeAttempt, ProbeOutcome, ProbeResult};

pub async fn probe_tcp(
    host: &str,
    port: u16,
    deadline: Duration,
    backoff: &BackoffConfig,
) -> ProbeOutcome {
    let start = Instant::now();
    let deadline_at = start + deadline;
    let addr = format!("{host}:{port}");
    let detail = format!("tcp :{port}");
    let mut attempt = 0u32;
    let mut attempts = Vec::new();

    for wait in BackoffIter::new(backoff) {
        attempt += 1;
        let remaining = deadline_at.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        let elapsed_at_attempt = start.elapsed().as_millis() as u64;
        let connect_timeout = remaining.min(Duration::from_secs(2));
        match timeout(connect_timeout, TcpStream::connect(&addr)).await {
            Ok(Ok(_stream)) => {
                let total_ms = start.elapsed().as_millis() as u64;
                attempts.push(ProbeAttempt::new(
                    attempt,
                    true,
                    total_ms,
                    None,
                    detail.clone(),
                ));
                return ProbeOutcome {
                    result: ProbeResult::Ok {
                        duration_ms: total_ms,
                    },
                    attempts,
                    matched_lines: Vec::new(),
                };
            }
            Ok(Err(e)) => {
                attempts.push(ProbeAttempt::new(
                    attempt,
                    false,
                    elapsed_at_attempt,
                    Some(e.to_string()),
                    detail.clone(),
                ));
            }
            Err(_) => {
                attempts.push(ProbeAttempt::new(
                    attempt,
                    false,
                    elapsed_at_attempt,
                    Some("timeout".into()),
                    detail.clone(),
                ));
            }
        }

        let remaining = deadline_at.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let actual_wait = wait.min(remaining);
        sleep(actual_wait).await;
    }

    let elapsed = start.elapsed().as_millis() as u64;
    ProbeOutcome {
        result: ProbeResult::Failed {
            error: format!("tcp :{port} timed out after {attempt} attempts"),
            duration_ms: elapsed,
        },
        attempts,
        matched_lines: Vec::new(),
    }
}
