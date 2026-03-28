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
        reason: Option<String>,
        /// Extra detail for UI expand (e.g. which probe/dep caused the red state)
        svc_detail: Option<String>,
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
        /// Summary reason computed at emission time
        reason_override: Option<String>,
        /// Additional reasons for UI detail expand (when multiple root causes)
        extra_reasons: Vec<String>,
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
    Message {
        text: String,
        ts: i64,
    },
}

// ── WebSocket event (serialized to UI) ────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct WsEvent {
    /// Event type: "service", "probe_state", "probe", "target", "op", "message"
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
    pub fn service_state(
        service: &str,
        state: ServiceState,
        display_state: &str,
        reason: Option<String>,
        svc_detail: Option<String>,
    ) -> Self {
        Self::ServiceState {
            service: service.into(),
            state,
            display_state: display_state.into(),
            reason,
            svc_detail,
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

    pub fn target_state(
        target: &str,
        state: TargetState,
        duration_ms: Option<u64>,
        reason_override: Option<String>,
        extra_reasons: Vec<String>,
    ) -> Self {
        Self::TargetState {
            target: target.into(),
            state,
            duration_ms,
            reason_override,
            extra_reasons,
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
            Self::Message { .. } => "message",
        }
    }

    fn level(&self) -> &'static str {
        match self {
            // Probe attempts are debug; pending states are debug except Reprobing
            Self::ProbeResult { .. }
            | Self::ProbeStateChange {
                state: ProbeState::Pending(crate::model::PendingReason::DepRecovered { .. }),
                ..
            }
            | Self::ProbeStateChange {
                state: ProbeState::Pending(crate::model::PendingReason::DepNotReady { .. }),
                ..
            }
            | Self::ProbeStateChange {
                state: ProbeState::Pending(crate::model::PendingReason::ContainerStarted),
                ..
            }
            | Self::ProbeStateChange {
                state: ProbeState::Pending(crate::model::PendingReason::Unchecked),
                ..
            } => "debug",
            // DepRed and Stopped propagation is debug — SVC event tells the story.
            // Still emitted (UI needs probe state for edge colors) but hidden in terminal log.
            Self::ProbeStateChange {
                state: ProbeState::Red(crate::model::RedReason::DepRed { .. }),
                ..
            }
            | Self::ProbeStateChange {
                state: ProbeState::Red(crate::model::RedReason::Stopped),
                ..
            }
            | Self::ProbeStateChange {
                state: ProbeState::Red(crate::model::RedReason::ContainerDied),
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
            | Self::Message { ts, .. } => *ts,
        }
    }

    fn summary(&self) -> String {
        match self {
            // SVC: [entity] state: reason
            Self::ServiceState {
                service,
                display_state,
                reason,
                ..
            } => match reason {
                Some(r) => format!("[{service}] {display_state}: {r}"),
                None => format!("[{service}] {display_state}"),
            },
            // PRB: [entity] green | [entity] display_state: short_reason
            Self::ProbeStateChange {
                probe,
                state,
                display_state,
                ..
            } => {
                if state.is_green() {
                    format!("[{probe}] green")
                } else {
                    match state.short_reason() {
                        Some(r) if r != *display_state => {
                            format!("[{probe}] {display_state}: {r}")
                        }
                        _ => format!("[{probe}] {display_state}"),
                    }
                }
            }
            // ATT: [entity] result: #attempt durms
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
                format!("[{probe}] {result}: #{attempt}{dur}")
            }
            // TGT: [entity] state: reason (reason computed at emission time)
            Self::TargetState {
                target,
                state,
                reason_override,
                ..
            } => match reason_override {
                Some(r) => format!("[{target}] {}: {r}", state.as_str()),
                None => format!("[{target}] {}", state.as_str()),
            },
            // CMD
            Self::OpStart {
                op,
                target_or_service,
                ..
            } => format!("{op} [{target_or_service}]"),
            Self::OpComplete {
                op,
                target_or_service,
                result,
                duration_ms,
                ..
            } => format!("{op} [{target_or_service}] {result} {duration_ms}ms"),
            // MSG
            Self::Message { text, .. } => text.clone(),
        }
    }

    fn detail(&self) -> Vec<String> {
        match self {
            // ATT: probe type, error, matched lines
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
            // SVC: detail from probe/dep info
            Self::ServiceState { svc_detail, .. } => svc_detail.iter().cloned().collect(),
            // PRB: entity/error detail from state. Green: no detail (for now).
            Self::ProbeStateChange { state, .. } => state.state_detail().into_iter().collect(),
            // TGT: extra reasons when multiple root causes
            Self::TargetState { extra_reasons, .. } => extra_reasons.clone(),
            _ => Vec::new(),
        }
    }

    fn data(&self) -> serde_json::Value {
        match self {
            Self::ServiceState {
                service,
                state,
                display_state,
                reason,
                ..
            } => serde_json::json!({
                "service": service, "state": state.as_str(),
                "display_state": display_state,
                "reason": reason,
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
                "reason": state.first_red_probe().map(|p| p.to_string()),
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
        // Log: summary + inline detail for target events (full entity info)
        let ws = event.to_ws_event();
        let prefix = match ws.category {
            "service" => "svc",
            "probe_state" | "probe" => "prb",
            "target" => "tgt",
            "op" => "cmd",
            _ => "   ",
        };
        let log_line = if !ws.detail.is_empty() {
            format!("{prefix} {} — {}", ws.summary, ws.detail.join(", "))
        } else {
            format!("{prefix} {}", ws.summary)
        };
        match ws.level {
            "debug" => tracing::debug!("{log_line}"),
            _ => tracing::info!("{log_line}"),
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
        let svc = Event::service_state("db", ServiceState::Running, "green", None, None);
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

        let target = Event::target_state("full", TargetState::Green, None, None, vec![]);
        assert_eq!(target.to_ws_event().category, "target");

        let op = Event::op_start("converge", "full");
        assert_eq!(op.to_ws_event().category, "op");
    }

    #[test]
    fn event_levels() {
        // Probe results are debug
        let result = Event::probe_result(&ProbeRef::new("db", "port"), true, Some(10), 1, None, 0);
        assert_eq!(result.to_ws_event().level, "debug");

        // Reprobing is info (visible to UI for pulsing state)
        let reprobing = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Pending(crate::model::PendingReason::Reprobing),
            ProbeState::Green,
            "probing",
        );
        assert_eq!(reprobing.to_ws_event().level, "info");

        // Other pending states are debug
        let dep_recovered = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Pending(crate::model::PendingReason::DepRecovered {
                dep: ProbeRef::new("redis", "port"),
            }),
            ProbeState::Red(crate::model::RedReason::Stopped),
            "pending",
        );
        assert_eq!(dep_recovered.to_ws_event().level, "debug");

        // Green/red probe state is info
        let green = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Green,
            ProbeState::Pending(crate::model::PendingReason::Reprobing),
            "green",
        );
        assert_eq!(green.to_ws_event().level, "info");

        // Service state is info
        let svc = Event::service_state("db", ServiceState::Running, "green", None, None);
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

    // ── Summary format tests ──

    #[test]
    fn summary_svc_green() {
        let e = Event::service_state("db", ServiceState::Running, "green", None, None);
        assert_eq!(e.to_ws_event().summary, "[db] green");
    }

    #[test]
    fn summary_svc_red_with_reason() {
        let e = Event::service_state(
            "db",
            ServiceState::Stopped,
            "red",
            Some("stopped".into()),
            None,
        );
        assert_eq!(e.to_ws_event().summary, "[db] red: stopped");
    }

    #[test]
    fn summary_svc_stopped() {
        let e = Event::service_state("db", ServiceState::Stopped, "stopped", None, None);
        assert_eq!(e.to_ws_event().summary, "[db] stopped");
    }

    #[test]
    fn summary_probe_green() {
        let e = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Green,
            ProbeState::Pending(crate::model::PendingReason::Reprobing),
            "green",
        );
        assert_eq!(e.to_ws_event().summary, "[db.port] green");
    }

    #[test]
    fn summary_probe_red_failed() {
        let e = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Red(crate::model::RedReason::ProbeFailed(
                crate::model::ProbeFailure {
                    error: "conn refused".into(),
                    duration_ms: 120,
                },
            )),
            ProbeState::Green,
            "red",
        );
        let ws = e.to_ws_event();
        assert_eq!(ws.summary, "[db.port] red: probe failed");
        assert_eq!(ws.detail, vec!["conn refused (120ms)"]);
    }

    #[test]
    fn summary_probe_pending() {
        let e = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Pending(crate::model::PendingReason::Reprobing),
            ProbeState::Green,
            "probing",
        );
        assert_eq!(e.to_ws_event().summary, "[db.port] probing: reprobing");
    }

    #[test]
    fn summary_target_green() {
        let e = Event::target_state("app", TargetState::Green, None, None, vec![]);
        assert_eq!(e.to_ws_event().summary, "[app] green");
    }

    #[test]
    fn summary_target_red_probe_failed() {
        let e = Event::target_state(
            "app",
            TargetState::Red {
                probes: vec![ProbeRef::new("db", "port")],
                dep_targets: vec![],
            },
            None,
            Some("probe db.port failed".into()),
            vec![],
        );
        let ws = e.to_ws_event();
        assert_eq!(ws.summary, "[app] red: probe db.port failed");
        assert!(ws.detail.is_empty());
    }

    #[test]
    fn summary_target_red_service_stopped() {
        let e = Event::target_state(
            "app",
            TargetState::Red {
                probes: vec![ProbeRef::new("db", "port")],
                dep_targets: vec![],
            },
            None,
            Some("service db stopped".into()),
            vec![],
        );
        let ws = e.to_ws_event();
        assert_eq!(ws.summary, "[app] red: service db stopped");
        assert!(ws.detail.is_empty());
    }

    #[test]
    fn summary_target_multiple_reasons() {
        let e = Event::target_state(
            "app",
            TargetState::Red {
                probes: vec![ProbeRef::new("db", "port"), ProbeRef::new("cache", "port")],
                dep_targets: vec![],
            },
            None,
            Some("service db stopped (+1 more)".into()),
            vec!["service db stopped".into(), "service cache stopped".into()],
        );
        let ws = e.to_ws_event();
        assert_eq!(ws.summary, "[app] red: service db stopped (+1 more)");
        assert_eq!(
            ws.detail,
            vec!["service db stopped", "service cache stopped"]
        );
    }

    #[test]
    fn summary_target_inactive() {
        let e = Event::target_state("app", TargetState::Inactive, None, None, vec![]);
        assert_eq!(e.to_ws_event().summary, "[app] inactive");
    }

    #[test]
    fn summary_op_start() {
        let e = Event::op_start("converge", "app");
        assert_eq!(e.to_ws_event().summary, "converge [app]");
    }

    #[test]
    fn summary_op_restart() {
        let e = Event::op_start("restart", "db");
        assert_eq!(e.to_ws_event().summary, "restart [db]");
        assert_eq!(e.to_ws_event().category, "op");
    }

    #[test]
    fn summary_probe_result() {
        let e = Event::probe_result(&ProbeRef::new("db", "port"), true, Some(5), 1, None, 0);
        assert_eq!(e.to_ws_event().summary, "[db.port] ok: #1 5ms");
    }

    // ── Detail tests ──

    #[test]
    fn detail_probe_green_empty() {
        // Green probe detail is empty (for now)
        let e = Event::probe_state_change(
            &ProbeRef::new("db", "port"),
            ProbeState::Green,
            ProbeState::Red(crate::model::RedReason::Stopped),
            "green",
        );
        assert!(e.to_ws_event().detail.is_empty());
    }

    #[test]
    fn detail_probe_red_dep() {
        // Red(DepRed) detail has the dep entity name
        let e = Event::probe_state_change(
            &ProbeRef::new("db", "ready"),
            ProbeState::Red(crate::model::RedReason::DepRed {
                dep: ProbeRef::new("db", "port"),
            }),
            ProbeState::Green,
            "red",
        );
        assert_eq!(e.to_ws_event().detail, vec!["db.port"]);
    }

    #[test]
    fn detail_svc_empty() {
        // Service events have no detail (reason is in summary)
        let e = Event::service_state(
            "db",
            ServiceState::Stopped,
            "red",
            Some("stopped".into()),
            None,
        );
        assert!(e.to_ws_event().detail.is_empty());
    }
}
