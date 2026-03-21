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

fn default_timeout() -> u64 {
    60
}

type ApiResult = std::result::Result<Json<OpResponse>, (StatusCode, Json<OpResponse>)>;
type StatusResult = std::result::Result<Json<serde_json::Value>, (StatusCode, String)>;

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
        .route("/api/graph", get(get_graph))
        .route("/api/status/service/{name}", get(service_status))
        // Operations
        .route("/api/stop/service/{name}", post(stop_service))
        .route("/api/start/service/{name}", post(start_service))
        .route("/api/restart/service/{name}", post(restart_service))
        .route("/api/replace/service/{name}", post(replace_service))
        .route("/api/converge/target/{name}", post(converge_target))
        .route("/api/reprobe/service/{name}", post(reprobe_service))
        .route("/api/reprobe/target/{name}", post(reprobe_target))
        .route("/api/reprobe/all", post(reprobe_all))
        .route("/api/reload", post(reload_config))
        // WebSocket + UI
        .route("/api/ws", get(super::ws::ws_handler))
        .route("/", get(serve_ui))
        .route("/ui/elk.bundled.js", get(serve_elk))
        .with_state(state)
}

async fn api_discovery() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "gantry",
        "description": "Docker service orchestrator with dependency-aware health probing.",
        "endpoints": [
            {"method": "GET",  "path": "/api",                          "description": "This discovery document."},
            {"method": "GET",  "path": "/api/graph",                    "description": "Full topology + live state. Returns {status, current_op, summary, services[], targets[]}."},
            {"method": "GET",  "path": "/api/status/service/:name",     "description": "Detailed service status with probe_ms and error per probe."},
            {"method": "POST", "path": "/api/converge/target/:name",    "description": "Bring a target to green: start services, probe, restart broken."},
            {"method": "POST", "path": "/api/start/service/:name",      "description": "Start a service and run its probes."},
            {"method": "POST", "path": "/api/stop/service/:name",       "description": "Stop a service. State propagates immediately."},
            {"method": "POST", "path": "/api/restart/service/:name",    "description": "Stop then start."},
            {"method": "POST", "path": "/api/replace/service/:name",    "description": "Rebuild, recreate, start (picks up code changes)."},
            {"method": "POST", "path": "/api/reprobe/service/:name",    "description": "Mark service probes stale and reprobe."},
            {"method": "POST", "path": "/api/reprobe/target/:name",     "description": "Mark target probes stale and reprobe."},
            {"method": "POST", "path": "/api/reprobe/all",              "description": "Reprobe all probes."},
            {"method": "POST", "path": "/api/reload",                   "description": "Reload gantry.yaml configuration."},
            {"method": "WS",   "path": "/api/ws",                       "description": "WebSocket: snapshot on connect, then event stream."},
        ],
        "states": {
            "service": "green | red | stale | stopped",
            "probe": "green (probe ok + deps ok) | red (probe fail or dep red) | stale (needs reprobe) | stopped",
            "target": "green | red | stale",
            "runtime": "running | stopped | starting | crashed",
        },
        "response_format": {
            "POST_operations": "{result: 'ok'|'failed', duration_ms, error?, actions: {started[], stopped[], restarted[], rebuilt[]}, probes: {probe: {state, prev, probe_ms?, error?}}, targets: {target: {state, prev}}}",
            "timeout": "All POST operations accept ?timeout=N in seconds (default: 60).",
            "concurrency": "One operation at a time. Returns 409 if busy. GET /api/graph includes current_op.",
        },
        "workflow": "GET /api/graph -> check status -> if degraded: POST /api/converge/target/{name}?timeout=120 -> check result",
    }))
}

async fn service_status(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> StatusResult {
    let services = state.services.read().await;
    let svc = services
        .get(&name)
        .ok_or((StatusCode::NOT_FOUND, format!("service '{name}' not found")))?;

    let mut probes_map = serde_json::Map::new();
    for (probe_name, probe_rt) in &svc.probes {
        let mut entry = serde_json::Map::new();
        entry.insert(
            "state".into(),
            serde_json::Value::String(probe_rt.state.as_str().into()),
        );
        if let Some(ms) = probe_rt.last_probe_ms {
            entry.insert("probe_ms".into(), serde_json::Value::Number(ms.into()));
        }
        if let Some(ref err) = probe_rt.last_error {
            entry.insert("error".into(), serde_json::Value::String(err.clone()));
        }
        probes_map.insert(probe_name.clone(), serde_json::Value::Object(entry));
    }

    Ok(Json(serde_json::json!({
        "name": name,
        "state": svc.state.as_str(),
        "probes": probes_map,
    })))
}

async fn stop_service(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire(&format!("stop {name}"))
        .await
        .map_err(err_response)?;
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
        .await
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::start::start(&state, &name, timeout)
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
        .await
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::restart::restart(&state, &name, timeout)
        .await
        .map(Json)
        .map_err(err_response)
}

async fn replace_service(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<TimeoutQuery>,
) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire(&format!("replace {name}"))
        .await
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::replace::replace(&state, &name, timeout)
        .await
        .map(Json)
        .map_err(err_response)
}

async fn converge_target(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<TimeoutQuery>,
) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire(&format!("converge {name}"))
        .await
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::converge::converge(&state, &name, timeout)
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
        .await
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
        .await
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
        .await
        .map_err(err_response)?;
    let timeout = Duration::from_secs(q.timeout);
    crate::ops::reprobe::reprobe_all(&state, timeout)
        .await
        .map(Json)
        .map_err(err_response)
}

async fn reload_config(State(state): State<Arc<AppState>>) -> ApiResult {
    let _guard = state
        .op_lock
        .try_acquire("reload")
        .await
        .map_err(err_response)?;
    let path = state.config_path.clone();
    crate::ops::reload::reload(&state, &path)
        .await
        .map(Json)
        .map_err(err_response)
}

/// Single endpoint for topology + live state. Includes config details (probe_type,
/// container) and runtime state (service/probe/target states) so AI agents need one call.
async fn get_graph(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use crate::config::ProbeConfig;
    use crate::model::{ProbeDisplayState, SvcDisplayState};

    let services = state.services.read().await;
    let targets = state.targets.read().await;

    let mut svc_list = Vec::new();
    for (name, svc) in services.iter() {
        let mut probes = Vec::new();
        for (probe_name, probe_rt) in &svc.probes {
            let display = ProbeDisplayState::from_probe(probe_rt, svc.state);
            let probe_type = match &probe_rt.probe_config {
                ProbeConfig::Tcp { port, .. } => format!("tcp:{port}"),
                ProbeConfig::Log { .. } => "log".into(),
                ProbeConfig::Meta => "meta".into(),
            };
            probes.push(serde_json::json!({
                "name": probe_name,
                "state": display.as_str(),
                "probe_type": probe_type,
                "depends_on": probe_rt.depends_on.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            }));
        }
        let svc_display = SvcDisplayState::from_service(svc);
        svc_list.push(serde_json::json!({
            "name": name,
            "state": svc_display.as_str(),
            "container": svc.container,
            "runtime": svc.state.as_str(),
            "start_after": svc.start_after.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            "probes": probes,
        }));
    }

    let mut tgt_list = Vec::new();
    for (name, tgt) in targets.iter() {
        let state_val = tgt.state(&services);
        tgt_list.push(serde_json::json!({
            "name": name,
            "state": state_val.as_str(),
            // "probes" is transitive (for UI highlighting); "direct_probes" is own
            "probes": tgt.transitive_probes.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            "direct_probes": tgt.direct_probes.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            "depends_on": tgt.depends_on_targets,
        }));
    }

    let running = services
        .values()
        .filter(|s| s.state == crate::model::ServiceState::Running)
        .count();
    let all_green = svc_list.iter().all(|s| s["state"] == "green")
        && tgt_list.iter().all(|t| t["state"] == "green");
    let current_op = state.op_lock.current_op().await;

    Json(serde_json::json!({
        "status": if all_green { "healthy" } else { "degraded" },
        "services": svc_list,
        "targets": tgt_list,
        "summary": {
            "services_running": running,
            "services_total": services.len(),
            "targets_total": targets.len(),
        },
        "current_op": current_op,
    }))
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
