use chrono::Utc;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::model::{ProbeRef, ProbeState, ServiceState, TargetState};

// ── Raw event (internal) ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Event {
    ServiceState {
        service: String,
        state: ServiceState,
        display_state: String,
        ts: i64,
    },
    ProbeStateChange {
        probe: String,
        state: ProbeState,
        prev: ProbeState,
        display_state: String,
        ts: i64,
    },
    ProbeResult {
        probe: String,
        result: String,
        duration_ms: Option<u64>,
        attempt: u32,
        error: Option<String>,
        matched_lines: Vec<String>,
        /// Probe type detail, e.g. "tcp :8080" or "log \"ready\""
        probe_detail: Option<String>,
        ts: i64,
    },
    TargetState {
        target: String,
        state: TargetState,
        duration_ms: Option<u64>,
        ts: i64,
    },
    OpStart {
        op: String,
        target_or_service: String,
        ts: i64,
    },
    OpComplete {
        op: String,
        target_or_service: String,
        result: String,
        duration_ms: u64,
        ts: i64,
    },
    ServiceRestart {
        service: String,
        reason: String,
        ts: i64,
    },
    Message {
        text: String,
        ts: i64,
    },
}

// ── WebSocket event (serialized to UI) ────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct WsEvent {
    /// Event type: "service", "probe_state", "probe", "target", "op", "restart", "message"
    pub category: &'static str,
    /// Log level: "info" or "debug"
    pub level: &'static str,
    /// Related entity for graph highlighting: service name, probe ref, or target name
    pub entity: String,
    /// One-line summary for the event stream
    pub summary: String,
    /// Optional detail lines (expandable in UI)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub detail: Vec<String>,
    /// Timestamp
    pub ts: i64,
    /// Raw event data
    pub data: serde_json::Value,
}

// ── Constructors ──────────────────────────────────────────────────────

fn now_ts() -> i64 {
    Utc::now().timestamp_millis()
}

impl Event {
    pub fn service_state(service: &str, state: ServiceState, display_state: &str) -> Self {
        Self::ServiceState {
            service: service.into(),
            state,
            display_state: display_state.into(),
            ts: now_ts(),
        }
    }

    pub fn probe_state_change(
        probe: &ProbeRef,
        state: ProbeState,
        prev: ProbeState,
        display_state: &str,
    ) -> Self {
        Self::ProbeStateChange {
            probe: probe.to_string(),
            state,
            prev,
            display_state: display_state.into(),
            ts: now_ts(),
        }
    }

    pub fn probe_result(
        probe: &ProbeRef,
        ok: bool,
        duration_ms: Option<u64>,
        attempt: u32,
        error: Option<String>,
        ts: i64,
    ) -> Self {
        Self::ProbeResult {
            probe: probe.to_string(),
            result: if ok { "ok".into() } else { "fail".into() },
            duration_ms,
            attempt,
            error,
            matched_lines: Vec::new(),
            probe_detail: None,
            ts,
        }
    }

    pub fn probe_result_with_lines(
        probe: &ProbeRef,
        ok: bool,
        duration_ms: Option<u64>,
        attempt: u32,
        error: Option<String>,
        matched_lines: Vec<String>,
        ts: i64,
    ) -> Self {
        Self::ProbeResult {
            probe: probe.to_string(),
            result: if ok { "ok".into() } else { "fail".into() },
            duration_ms,
            attempt,
            error,
            matched_lines,
            probe_detail: None,
            ts,
        }
    }

    pub fn target_state(target: &str, state: TargetState, duration_ms: Option<u64>) -> Self {
        Self::TargetState {
            target: target.into(),
            state,
            duration_ms,
            ts: now_ts(),
        }
    }

    pub fn op_start(op: &str, target_or_service: &str) -> Self {
        Self::OpStart {
            op: op.into(),
            target_or_service: target_or_service.into(),
            ts: now_ts(),
        }
    }

    pub fn op_complete(op: &str, target_or_service: &str, result: &str, duration_ms: u64) -> Self {
        Self::OpComplete {
            op: op.into(),
            target_or_service: target_or_service.into(),
            result: result.into(),
            duration_ms,
            ts: now_ts(),
        }
    }

    pub fn message(text: &str) -> Self {
        Self::Message {
            text: text.into(),
            ts: now_ts(),
        }
    }
}

// ── Shared derivations (log + websocket) ──────────────────────────────

impl Event {
    fn category(&self) -> &'static str {
        match self {
            Self::ServiceState { .. } => "service",
            Self::ProbeStateChange { .. } => "probe_state",
            Self::ProbeResult { .. } => "probe",
            Self::TargetState { .. } => "target",
            Self::OpStart { .. } | Self::OpComplete { .. } => "op",
            Self::ServiceRestart { .. } => "restart",
            Self::Message { .. } => "message",
        }
    }

    fn level(&self) -> &'static str {
        match self {
            Self::ProbeResult { .. }
            | Self::ProbeStateChange {
                state: ProbeState::Stale(_),
                ..
            } => "debug",
            _ => "info",
        }
    }

    fn entity(&self) -> String {
        match self {
            Self::ServiceState { service, .. } => service.clone(),
            Self::ProbeStateChange { probe, .. } => probe.clone(),
            Self::ProbeResult { probe, .. } => probe.clone(),
            Self::TargetState { target, .. } => target.clone(),
            Self::OpStart {
                target_or_service, ..
            } => target_or_service.clone(),
            Self::OpComplete {
                target_or_service, ..
            } => target_or_service.clone(),
            Self::ServiceRestart { service, .. } => service.clone(),
            Self::Message { .. } => String::new(),
        }
    }

    fn ts(&self) -> i64 {
        match self {
            Self::ServiceState { ts, .. }
            | Self::ProbeStateChange { ts, .. }
            | Self::ProbeResult { ts, .. }
            | Self::TargetState { ts, .. }
            | Self::OpStart { ts, .. }
            | Self::OpComplete { ts, .. }
            | Self::ServiceRestart { ts, .. }
            | Self::Message { ts, .. } => *ts,
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::ServiceState {
                service,
                display_state,
                ..
            } => {
                format!("[{service}] {display_state}")
            }
            Self::ProbeStateChange {
                probe,
                display_state,
                ..
            } => {
                format!("[{probe}] {display_state}")
            }
            Self::ProbeResult {
                probe,
                result,
                duration_ms,
                attempt,
                ..
            } => {
                let dur = match duration_ms {
                    None => String::new(),
                    Some(d) => format!(" {d}ms"),
                };
                format!("[{probe}] probe #{attempt}{dur} {result}")
            }
            Self::TargetState {
                target,
                state,
                duration_ms,
                ..
            } => {
                let s = match state {
                    TargetState::Green => "ok",
                    TargetState::Red(_) => "failed",
                    TargetState::Stale { .. } => "stale",
                };
                let dur = duration_ms
                    .filter(|d| *d > 0)
                    .map(|d| format!(" {d}ms"))
                    .unwrap_or_default();
                format!("[{target}] {s}{dur}")
            }
            Self::OpStart {
                op,
                target_or_service,
                ..
            } => {
                format!("{op} [{target_or_service}]")
            }
            Self::OpComplete {
                op,
                target_or_service,
                result,
                duration_ms,
                ..
            } => {
                format!("{op} [{target_or_service}] {result} {duration_ms}ms")
            }
            Self::ServiceRestart {
                service, reason, ..
            } => {
                format!("[{service}] restart ({reason})")
            }
            Self::Message { text, .. } => text.clone(),
        }
    }

    fn detail(&self) -> Vec<String> {
        match self {
            Self::ProbeResult {
                error,
                matched_lines,
                probe_detail,
                ..
            } => {
                let mut lines = Vec::new();
                lines.extend(probe_detail.iter().cloned());
                lines.extend(error.iter().cloned());
                lines.extend(matched_lines.iter().cloned());
                lines
            }
            Self::ProbeStateChange { state, .. } => state.reason().into_iter().collect(),
            Self::ServiceRestart { reason, .. } => vec![reason.clone()],
            _ => Vec::new(),
        }
    }

    fn data(&self) -> serde_json::Value {
        match self {
            Self::ServiceState {
                service,
                state,
                display_state,
                ..
            } => serde_json::json!({
                "service": service, "state": state.as_str(),
                "display_state": display_state,
            }),
            Self::ProbeStateChange {
                probe,
                state,
                prev,
                display_state,
                ..
            } => serde_json::json!({
                "probe": probe,
                "state": state.as_str(),
                "prev": prev.as_str(),
                "display_state": display_state,
                "reason": state.reason(),
            }),
            Self::ProbeResult {
                probe,
                result,
                duration_ms,
                attempt,
                error,
                ..
            } => serde_json::json!({
                "probe": probe, "result": result, "duration_ms": duration_ms,
                "attempt": attempt, "error": error,
            }),
            Self::TargetState {
                target,
                state,
                duration_ms,
                ..
            } => serde_json::json!({
                "target": target,
                "state": state.as_str(),
                "duration_ms": duration_ms,
                "reason": state.reason(),
            }),
            Self::OpStart {
                op,
                target_or_service,
                ..
            } => serde_json::json!({
                "op": op, "scope": target_or_service,
            }),
            Self::OpComplete {
                op,
                target_or_service,
                result,
                duration_ms,
                ..
            } => serde_json::json!({
                "op": op, "scope": target_or_service,
                "result": result, "duration_ms": duration_ms,
            }),
            Self::ServiceRestart {
                service, reason, ..
            } => serde_json::json!({
                "service": service, "reason": reason,
            }),
            Self::Message { text, .. } => serde_json::json!({"text": text}),
        }
    }

    /// Convert to WebSocket event (for UI)
    pub fn to_ws_event(&self) -> WsEvent {
        WsEvent {
            category: self.category(),
            level: self.level(),
            entity: self.entity(),
            summary: self.summary(),
            detail: self.detail(),
            ts: self.ts(),
            data: self.data(),
        }
    }
}

// ── EventBus ──────────────────────────────────────────────────────────

pub struct EventBus {
    pub tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn emit(&self, event: Event) {
        // Log with appropriate level
        match &event {
            Event::OpStart {
                op,
                target_or_service,
                ..
            } => {
                tracing::info!("▶ {op} {target_or_service}");
            }
            Event::OpComplete {
                op,
                target_or_service,
                result,
                duration_ms,
                ..
            } => {
                tracing::info!("✓ {op} {target_or_service} → {result} ({duration_ms}ms)");
            }
            Event::ServiceState {
                service,
                display_state,
                ..
            } => {
                tracing::info!("[{service}] {display_state}");
            }
            Event::ServiceRestart {
                service, reason, ..
            } => {
                tracing::info!("[{service}] restart: {reason}");
            }
            Event::ProbeStateChange { probe, state, .. } => match state {
                ProbeState::Stale(_) => tracing::debug!("[{probe}] stale"),
                _ => tracing::info!("[{probe}] {}", state.as_str()),
            },
            Event::TargetState { target, state, .. } => {
                let s = match state {
                    TargetState::Green => "green",
                    TargetState::Red(_) => "red",
                    TargetState::Stale { .. } => "stale",
                };
                tracing::info!("[{target}] {s}");
            }
            Event::ProbeResult {
                probe,
                result,
                duration_ms,
                attempt,
                ..
            } => {
                let dur = duration_ms.map(|d| format!(" {d}ms")).unwrap_or_default();
                tracing::debug!("[{probe}] probe #{attempt}{dur} {result}");
            }
            Event::Message { text, .. } => {
                tracing::info!("{text}");
            }
        }
        // Broadcast to WebSocket subscribers
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProbeRef;

    #[test]
    fn event_categories() {
        let svc = Event::service_state("db", ServiceState::Running, "green");
        assert_eq!(svc.to_ws_event().category, "service");

        let probe = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Green,
            ProbeState::Red(crate::model::RedReason::Stopped),
            "green",
        );
        assert_eq!(probe.to_ws_event().category, "probe_state");

        let result = Event::probe_result(&ProbeRef::new("db", "port"), true, Some(10), 1, None, 0);
        assert_eq!(result.to_ws_event().category, "probe");

        let target = Event::target_state("full", TargetState::Green, None);
        assert_eq!(target.to_ws_event().category, "target");

        let op = Event::op_start("converge", "full");
        assert_eq!(op.to_ws_event().category, "op");
    }

    #[test]
    fn event_levels() {
        // Probe results are debug
        let result = Event::probe_result(&ProbeRef::new("db", "port"), true, Some(10), 1, None, 0);
        assert_eq!(result.to_ws_event().level, "debug");

        // Stale probe state is debug
        let stale = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Stale(crate::model::StaleReason::Reprobing),
            ProbeState::Green,
            "stale",
        );
        assert_eq!(stale.to_ws_event().level, "debug");

        // Green/red probe state is info
        let green = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Green,
            ProbeState::Stale(crate::model::StaleReason::Reprobing),
            "green",
        );
        assert_eq!(green.to_ws_event().level, "info");

        // Service state is info
        let svc = Event::service_state("db", ServiceState::Running, "green");
        assert_eq!(svc.to_ws_event().level, "info");
    }

    #[test]
    fn ws_event_data_fields() {
        let probe = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Green,
            ProbeState::Red(crate::model::RedReason::Stopped),
            "green",
        );
        let ws = probe.to_ws_event();
        assert_eq!(ws.entity, "db.port");
        assert_eq!(ws.data["probe"], "db.port");
        assert_eq!(ws.data["state"], "green");
        assert_eq!(ws.data["prev"], "red");
    }
}
