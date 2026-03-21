pub mod log;
pub mod meta;
pub mod tcp;

use std::time::{Duration, Instant};

use crate::config::{BackoffConfig, ProbeConfig};

#[derive(Debug, Clone)]
pub enum ProbeResult {
    Ok { duration_ms: u64 },
    Failed { error: String, duration_ms: u64 },
}

impl ProbeResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }
}

#[derive(Debug, Clone)]
pub struct ProbeAttempt {
    pub attempt: u32,
    pub ok: bool,
    /// Milliseconds elapsed since probe started (wall clock position of this attempt)
    pub elapsed_ms: u64,
    pub error: Option<String>,
    /// Human-readable probe detail, e.g. "tcp :5432" or "log \"ready to accept\""
    pub detail: String,
    /// Wall-clock timestamp when this attempt occurred
    pub ts: i64,
}

#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub result: ProbeResult,
    pub attempts: Vec<ProbeAttempt>,
}

impl ProbeAttempt {
    pub fn new(
        attempt: u32,
        ok: bool,
        elapsed_ms: u64,
        error: Option<String>,
        detail: String,
    ) -> Self {
        Self {
            ts: chrono::Utc::now().timestamp_millis(),
            attempt,
            ok,
            elapsed_ms,
            error,
            detail,
        }
    }
}

impl ProbeOutcome {
    pub fn immediate(result: ProbeResult) -> Self {
        Self {
            result,
            attempts: Vec::new(),
        }
    }

    /// Create a single-attempt outcome from a ProbeResult.
    pub fn single(result: ProbeResult, detail: &str) -> Self {
        let (ok, ms, err) = match &result {
            ProbeResult::Ok { duration_ms } => (true, *duration_ms, None),
            ProbeResult::Failed { error, duration_ms } => {
                (false, *duration_ms, Some(error.clone()))
            }
        };
        Self {
            attempts: vec![ProbeAttempt::new(1, ok, ms, err, detail.to_string())],
            result,
        }
    }
}

/// Run a probe with retry + backoff. Used by start/restart/replace.
pub async fn run_with_retry(
    docker: &bollard::Docker,
    service_name: &str,
    container_name: &str,
    probe_config: &ProbeConfig,
    timeout: Duration,
    backoff: &BackoffConfig,
) -> ProbeOutcome {
    match probe_config {
        ProbeConfig::Tcp {
            host,
            port,
            timeout: probe_timeout,
        } => {
            let probe_host = host.as_deref().unwrap_or(service_name);
            let deadline = (*probe_timeout).min(timeout);
            tcp::probe_tcp(probe_host, *port, deadline, backoff).await
        }
        ProbeConfig::Log {
            success,
            failure,
            timeout: probe_timeout,
        } => {
            let deadline = (*probe_timeout).min(timeout);
            let result = log::probe_log(
                docker,
                container_name,
                success,
                failure.as_deref(),
                deadline,
                0,
                true,
            )
            .await;
            ProbeOutcome::single(result, &format!("log \"{}\"", truncate(success, 40)))
        }
        ProbeConfig::Meta => ProbeOutcome::immediate(ProbeResult::Ok { duration_ms: 0 }),
    }
}

/// Run a single-attempt probe. Used by reprobe and converge phase 2.
/// For log probes, checks the tail of existing logs (doesn't wait for new output).
pub async fn run_single_attempt(
    docker: &bollard::Docker,
    service_name: &str,
    container_name: &str,
    probe_config: &ProbeConfig,
    timeout: Duration,
) -> ProbeOutcome {
    match probe_config {
        ProbeConfig::Tcp { host, port, .. } => {
            let probe_host = host.as_deref().unwrap_or(service_name);
            let connect_timeout = timeout.min(Duration::from_secs(5));
            let detail = format!("tcp :{port}");
            let start = Instant::now();
            let result = match tokio::time::timeout(
                connect_timeout,
                tokio::net::TcpStream::connect(format!("{probe_host}:{port}")),
            )
            .await
            {
                Ok(Ok(_)) => ProbeResult::Ok {
                    duration_ms: start.elapsed().as_millis() as u64,
                },
                Ok(Err(e)) => ProbeResult::Failed {
                    error: e.to_string(),
                    duration_ms: start.elapsed().as_millis() as u64,
                },
                Err(_) => ProbeResult::Failed {
                    error: "timeout".into(),
                    duration_ms: start.elapsed().as_millis() as u64,
                },
            };
            ProbeOutcome::single(result, &detail)
        }
        ProbeConfig::Log {
            success, failure, ..
        } => {
            // Reprobe: scan all logs without following — checks current state
            let result = log::probe_log(
                docker,
                container_name,
                success,
                failure.as_deref(),
                Duration::ZERO,
                0,
                false,
            )
            .await;
            ProbeOutcome::single(result, &format!("log \"{}\"", truncate(success, 40)))
        }
        ProbeConfig::Meta => ProbeOutcome::immediate(ProbeResult::Ok { duration_ms: 0 }),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}

pub struct BackoffIter {
    current: Duration,
    max: Duration,
    multiplier: f64,
}

impl BackoffIter {
    pub fn new(config: &BackoffConfig) -> Self {
        Self {
            current: config.initial,
            max: config.max,
            multiplier: config.multiplier,
        }
    }
}

impl Iterator for BackoffIter {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        let val = self.current;
        self.current = self.current.mul_f64(self.multiplier).min(self.max);
        Some(val)
    }
}
