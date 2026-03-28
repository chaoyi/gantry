//! Shared graph state computation used by both GET /graph and WS snapshot.

use crate::api::AppState;
use crate::model::{ProbeDisplayState, SvcDisplayState, active_services};

/// Compute display states for all services, probes, and targets.
/// Returns (services_by_name, targets_by_name) with display states resolved.
pub async fn compute_display(
    state: &AppState,
) -> (
    indexmap::IndexMap<String, SvcView>,
    indexmap::IndexMap<String, TgtView>,
) {
    let services = state.services.read().await;
    let targets = state.targets.read().await;
    let active = active_services(&services, &targets);

    let mut svc_views = indexmap::IndexMap::new();
    for (name, svc) in services.iter() {
        let is_active = active.contains(name.as_str());
        let display = SvcDisplayState::from_service_active(svc, is_active);

        let mut probe_views = indexmap::IndexMap::new();
        for (probe_name, probe_rt) in &svc.probes {
            let probe_display = ProbeDisplayState::from_probe(probe_rt, svc.state);
            let probe_type = probe_rt.probe_config.display_type();
            probe_views.insert(
                probe_name.clone(),
                ProbeView {
                    state: probe_display.as_str(),
                    probe_type,
                    reason: probe_rt.state.reason(),
                    depends_on: probe_rt.depends_on.iter().map(|c| c.to_string()).collect(),
                },
            );
        }

        let svc_reason = crate::ops::compute_svc_reason(display, svc);

        svc_views.insert(
            name.clone(),
            SvcView {
                state: display.as_str(),
                runtime: svc.state.as_str(),
                reason: svc_reason,
                container: svc.container.clone(),
                restart_on_fail: svc.restart_on_fail,
                start_after: svc.start_after.iter().map(|c| c.to_string()).collect(),
                probes: probe_views,
            },
        );
    }

    let mut tgt_views = indexmap::IndexMap::new();
    for (name, tgt) in targets.iter() {
        let state_val = tgt.state(&services, &targets);
        // Compute human-readable reason by walking DepRed chains
        let reasons = crate::ops::compute_target_reasons(&state_val, &services);
        let reason = if reasons.is_empty() {
            None
        } else {
            Some(reasons.join(", "))
        };
        tgt_views.insert(
            name.clone(),
            TgtView {
                state: state_val.as_str(),
                reason,
                probes: tgt
                    .transitive_probes
                    .iter()
                    .map(|c| c.to_string())
                    .collect(),
                direct_probes: tgt.direct_probes.iter().map(|c| c.to_string()).collect(),
                depends_on: tgt.depends_on_targets.clone(),
            },
        );
    }

    (svc_views, tgt_views)
}

pub struct SvcView {
    pub state: &'static str,
    pub runtime: &'static str,
    pub reason: Option<String>,
    pub container: String,
    pub restart_on_fail: bool,
    pub start_after: Vec<String>,
    pub probes: indexmap::IndexMap<String, ProbeView>,
}

pub struct ProbeView {
    pub state: &'static str,
    pub probe_type: String,
    pub reason: Option<String>,
    pub depends_on: Vec<String>,
}

pub struct TgtView {
    pub state: &'static str,
    pub reason: Option<String>,
    pub probes: Vec<String>,
    pub direct_probes: Vec<String>,
    pub depends_on: Vec<String>,
}

/// Build the GET /graph JSON response.
pub async fn build_graph_json(state: &AppState) -> serde_json::Value {
    let (svcs, tgts) = compute_display(state).await;

    let svc_list: Vec<serde_json::Value> = svcs
        .iter()
        .map(|(name, sv)| {
            let probes: Vec<serde_json::Value> = sv
                .probes
                .iter()
                .map(|(pn, pv)| {
                    let mut j = serde_json::json!({
                        "name": pn,
                        "state": pv.state,
                        "probe_type": pv.probe_type,
                        "depends_on": pv.depends_on,
                    });
                    if let Some(r) = &pv.reason {
                        j["reason"] = serde_json::json!(r);
                    }
                    j
                })
                .collect();
            let mut j = serde_json::json!({
                "name": name,
                "state": sv.state,
                "container": sv.container,
                "runtime": sv.runtime,
                "restart_on_fail": sv.restart_on_fail,
                "start_after": sv.start_after,
                "probes": probes,
            });
            if let Some(r) = &sv.reason {
                j["reason"] = serde_json::json!(r);
            }
            j
        })
        .collect();

    let tgt_list: Vec<serde_json::Value> = tgts
        .iter()
        .map(|(name, tv)| {
            let mut j = serde_json::json!({
                "name": name,
                "state": tv.state,
                "probes": tv.probes,
                "direct_probes": tv.direct_probes,
                "depends_on": tv.depends_on,
            });
            if let Some(r) = &tv.reason {
                j["reason"] = serde_json::json!(r);
            }
            j
        })
        .collect();

    let running = svcs.values().filter(|s| s.runtime == "running").count();
    let all_green =
        svcs.values().all(|s| s.state == "green") && tgts.values().all(|t| t.state == "green");
    let current_op = state.op_lock.current_op();

    serde_json::json!({
        "status": if all_green { "healthy" } else { "degraded" },
        "services": svc_list,
        "targets": tgt_list,
        "summary": {
            "services_running": running,
            "services_total": svcs.len(),
            "targets_total": tgts.len(),
        },
        "current_op": current_op,
    })
}

/// Build WS snapshot — same data as graph, map format for efficient UI updates.
pub async fn build_ws_snapshot(state: &AppState) -> serde_json::Value {
    let (svcs, tgts) = compute_display(state).await;

    let mut svc_map = serde_json::Map::new();
    for (name, sv) in &svcs {
        let mut probes = serde_json::Map::new();
        for (pn, pv) in &sv.probes {
            let mut j = serde_json::json!({ "state": pv.state });
            if let Some(r) = &pv.reason {
                j["reason"] = serde_json::json!(r);
            }
            probes.insert(pn.clone(), j);
        }
        svc_map.insert(
            name.clone(),
            serde_json::json!({
                "state": sv.state,
                "runtime": sv.runtime,
                "probes": probes,
            }),
        );
    }

    let mut tgt_map = serde_json::Map::new();
    for (name, tv) in &tgts {
        let mut j = serde_json::json!({ "state": tv.state });
        if let Some(r) = &tv.reason {
            j["reason"] = serde_json::json!(r);
        }
        tgt_map.insert(name.clone(), j);
    }

    serde_json::json!({
        "type": "snapshot",
        "services": svc_map,
        "targets": tgt_map,
    })
}
