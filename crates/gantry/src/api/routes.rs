use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::response::Json;
use axum::routing::{get, post};
use serde::Deserialize;

use crate::api::AppState;
use crate::error::GantryError;
use crate::ops::OpResponse;

#[derive(Deserialize)]
pub struct TimeoutQuery {
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

#[derive(Deserialize)]
pub struct ConvergeQuery {
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub skip_restart: bool,
}

fn default_timeout() -> u64 {
    60
}

type ApiResult = std::result::Result<Json<OpResponse>, (StatusCode, Json<OpResponse>)>;

fn err_response(e: GantryError) -> (StatusCode, Json<OpResponse>) {
    let (status, error_str) = match &e {
        GantryError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        GantryError::Conflict(_) => (StatusCode::CONFLICT, e.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    (
        status,
        Json(OpResponse {
            result: "failed".to_string(),
            duration_ms: 0,
            error: Some(error_str),
            not_green: vec![],
            actions: Default::default(),
            probes: Default::default(),
            targets: Default::default(),
        }),
    )
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api", get(api_discovery))
        // Queries
        .route("/api/status", get(get_status))
        .route("/api/service/{name}", get(get_service))
        .route("/api/target/{name}", get(get_target))
        .route("/api/graph", get(get_graph))
        // Operations
        .route("/api/stop/service/{name}", post(stop_service))
        .route("/api/start/service/{name}", post(start_service))
        .route("/api/restart/service/{name}", post(restart_service))
        .route("/api/converge/target/{name}", post(converge_target))
        .route("/api/reprobe/service/{name}", post(reprobe_service))
        .route("/api/reprobe/target/{name}", post(reprobe_target))
        .route("/api/reprobe/all", post(reprobe_all))
        .route("/api/message", post(post_message))
        // WebSocket + UI
        .route("/api/ws", get(super::ws::ws_handler))
        .route("/", get(serve_ui))
        .route("/ui/elk.bundled.js", get(serve_elk))
        .with_state(state)
}

async fn api_discovery(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let services: Vec<String> = state.services.read().await.keys().cloned().collect();
    let targets: Vec<String> = state.targets.read().await.keys().cloned().collect();

    Json(serde_json::json!({
        "name": "gantry",
        "endpoints": [
            {"method": "GET",  "path": "/api/status",         "description": "Health summary: target/service states with reasons."},
            {"method": "GET",  "path": "/api/service/:name",  "description": "Service detail: probes, errors, log matches, deps."},
            {"method": "GET",  "path": "/api/target/:name",   "description": "Target detail: root causes, service states."},
            {"method": "GET",  "path": "/api/graph",          "description": "Full topology with probe-level detail (for UI)."},
            {"method": "POST", "path": "/api/converge/target/:name", "params": "?timeout=N&skip_restart=true", "description": "Bring target to green: start services, probe, restart on failure."},
            {"method": "POST", "path": "/api/stop/service/:name",    "description": "Stop a service and propagate state."},
            {"method": "POST", "path": "/api/start/service/:name",   "params": "?timeout=N", "description": "Start a service and wait for probes."},
            {"method": "POST", "path": "/api/restart/service/:name", "params": "?timeout=N", "description": "Stop then start a service."},
            {"method": "POST", "path": "/api/reprobe/service/:name", "params": "?timeout=N", "description": "Re-check probes on a running service."},
            {"method": "POST", "path": "/api/reprobe/target/:name",  "params": "?timeout=N", "description": "Re-check probes for all services in a target."},
            {"method": "POST", "path": "/api/reprobe/all",           "params": "?timeout=N", "description": "Re-check all probes."},
            {"method": "GET",  "path": "/api/ws",     "description": "WebSocket event stream (real-time state changes)."},
        ],
        "services": services,
        "targets": targets,
        "concurrency": "One write operation at a time. Returns 409 Conflict if busy.",
    }))
}

/// Concise health summary for AI callers. No probe detail — just service/target
/// states with human-readable reasons.
async fn get_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let (svcs, tgts) = super::state::compute_display(&state).await;

    let all_green =
        svcs.values().all(|s| s.state == "green") && tgts.values().all(|t| t.state == "green");

    let mut svc_map = serde_json::Map::new();
    for (name, sv) in &svcs {
        let mut entry = serde_json::json!({ "state": sv.state });
        if let Some(r) = &sv.reason {
            entry["reason"] = serde_json::json!(r);
        }
        svc_map.insert(name.clone(), entry);
    }

    let mut tgt_map = serde_json::Map::new();
    for (name, tv) in &tgts {
        let mut entry = serde_json::json!({ "state": tv.state });
        if let Some(r) = &tv.reason {
            entry["reason"] = serde_json::json!(r);
        }
        tgt_map.insert(name.clone(), entry);
    }

    let current_op = state.op_lock.current_op();

    Json(serde_json::json!({
        "healthy": all_green,
        "services": svc_map,
        "targets": tgt_map,
        "current_op": current_op,
    }))
}

/// Per-service detail: state, reason, probes with errors/logs/deps.
async fn get_service(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> std::result::Result<Json<serde_json::Value>, StatusCode> {
    let services = state.services.read().await;
    let targets = state.targets.read().await;
    let active = crate::model::active_services(&services, &targets);
    let Some(svc) = services.get(&name) else {
        return Err(StatusCode::NOT_FOUND);
    };

    let is_active = active.contains(name.as_str());
    let display = crate::model::SvcDisplayState::from_service_active(svc, is_active);

    let reason = crate::ops::compute_svc_reason(display, svc);

    // Build probe details
    let mut probes = serde_json::Map::new();
    for (probe_name, probe_rt) in &svc.probes {
        let probe_display =
            crate::model::ProbeDisplayState::from_probe(probe_rt, svc.state).as_str();
        let mut p = serde_json::json!({
            "state": probe_display,
            "probe_type": probe_rt.probe_config.display_type(),
            "depends_on": probe_rt.depends_on.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
        });
        if let Some(r) = probe_rt.state.short_reason() {
            p["reason"] = serde_json::json!(r);
        }
        if let Some(ref e) = probe_rt.last_error {
            p["error"] = serde_json::json!(e);
        }
        if let Some(ms) = probe_rt.last_probe_ms {
            p["probe_ms"] = serde_json::json!(ms);
        }
        if let Some(ref log) = probe_rt.last_log_match {
            p["log"] = serde_json::json!(log);
        }
        // For DepRed, include which dep is blocking
        if let crate::model::ProbeState::Red(crate::model::RedReason::DepRed { ref dep }) =
            probe_rt.state
        {
            p["blocked_by"] = serde_json::json!(dep.to_string());
        }
        probes.insert(probe_name.clone(), p);
    }

    let mut result = serde_json::json!({
        "name": name,
        "state": display.as_str(),
        "runtime": svc.state.as_str(),
        "container": svc.container,
        "restart_on_fail": svc.restart_on_fail,
        "start_after": svc.start_after.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
        "probes": probes,
    });
    if let Some(r) = reason {
        result["reason"] = serde_json::json!(r);
    }
    Ok(Json(result))
}

/// Per-target detail: state, root causes, service states.
async fn get_target(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> std::result::Result<Json<serde_json::Value>, StatusCode> {
    let services = state.services.read().await;
    let targets = state.targets.read().await;
    let Some(tgt) = targets.get(&name) else {
        return Err(StatusCode::NOT_FOUND);
    };

    let current = tgt.state(&services, &targets);
    let active = crate::model::active_services(&services, &targets);

    // Compute root cause reasons
    let reasons = crate::ops::compute_target_reasons(&current, &services);

    // Build service states for services in this target
    let mut svc_states = serde_json::Map::new();
    let target_services: std::collections::HashSet<String> = tgt
        .transitive_probes
        .iter()
        .map(|p| p.service.clone())
        .collect();
    for svc_name in &target_services {
        if let Some(svc) = services.get(svc_name) {
            let is_active = active.contains(svc_name.as_str());
            let display = crate::model::SvcDisplayState::from_service_active(svc, is_active);
            let mut entry = serde_json::json!({ "state": display.as_str() });
            let svc_reason = crate::ops::compute_svc_reason(display, svc);
            if let Some(r) = svc_reason {
                entry["reason"] = serde_json::json!(r);
            }
            svc_states.insert(svc_name.clone(), entry);
        }
    }

    let mut result = serde_json::json!({
        "name": name,
        "state": current.as_str(),
        "depends_on": tgt.depends_on_targets,
        "services": svc_states,
    });
    if !reasons.is_empty() {
        result["reasons"] = serde_json::json!(reasons);
    }
    Ok(Json(result))
}

async fn stop_service(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire(&format!("stop {name}"))
        .map_err(err_response)?;
    state
        .events
        .emit(crate::events::Event::op_start("stop", &name));
    crate::ops::stop::stop(&state, &name)
        .await
        .map(Json)
        .map_err(err_response)
}

async fn start_service(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<TimeoutQuery>,
) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire(&format!("start {name}"))
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::start::start(&state, &name, timeout, true)
        .await
        .map(Json)
        .map_err(err_response)
}

async fn restart_service(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<TimeoutQuery>,
) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire(&format!("restart {name}"))
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::restart::restart(&state, &name, timeout)
        .await
        .map(Json)
        .map_err(err_response)
}

async fn converge_target(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<ConvergeQuery>,
) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire(&format!("converge {name}"))
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::converge::converge(&state, &name, timeout, !q.skip_restart)
        .await
        .map(Json)
        .map_err(err_response)
}

async fn reprobe_service(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<TimeoutQuery>,
) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire(&format!("reprobe {name}"))
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::reprobe::reprobe_service(&state, &name, timeout)
        .await
        .map(Json)
        .map_err(err_response)
}

async fn reprobe_target(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<TimeoutQuery>,
) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire(&format!("reprobe {name}"))
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::reprobe::reprobe_target(&state, &name, timeout)
        .await
        .map(Json)
        .map_err(err_response)
}

async fn reprobe_all(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TimeoutQuery>,
) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire("reprobe all")
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::reprobe::reprobe_all(&state, timeout)
        .await
        .map(Json)
        .map_err(err_response)
}

async fn post_message(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
    state.events.emit(crate::events::Event::message(text));
    Json(serde_json::json!({"ok": true}))
}

/// Single endpoint for topology + live state. Includes config details (probe_type,
/// container) and runtime state (service/probe/target states) so AI agents need one call.
async fn get_graph(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(super::state::build_graph_json(&state).await)
}

async fn serve_ui() -> Html<&'static str> {
    Html(include_str!("../../ui/index.html"))
}

async fn serve_elk() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        include_str!("../../ui/elk.bundled.js"),
    )
}
