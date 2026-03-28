use std::time::{Duration, Instant};

use crate::api::AppState;
use crate::error::{GantryError, Result};
use crate::model::{ProbeRef, ProbeState, ServiceState};
use crate::ops::{
    OpActions, OpResponse, ProbeStatus, emit_propagated_changes, emit_svc_display_states,
    emit_target_states,
};

pub async fn reprobe_service(
    state: &AppState,
    service_name: &str,
    timeout: Duration,
) -> Result<OpResponse> {
    let start_time = Instant::now();
    state
        .events
        .emit(crate::events::Event::op_start("reprobe", service_name));

    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();
    // Single lock scope: validate state + collect probes atomically (no TOCTOU)
    let probe_refs: Vec<ProbeRef> = {
        let services = state.services.read().await;
        if !services.contains_key(service_name) {
            return Err(GantryError::NotFound(format!(
                "service '{service_name}' not found"
            )));
        }
        let svc = &services[service_name];
        if matches!(
            svc.state,
            crate::model::ServiceState::Stopped | crate::model::ServiceState::Crashed
        ) {
            return Ok(OpResponse {
                result: "failed".to_string(),
                duration_ms: 0,
                error: Some(format!(
                    "service '{service_name}' is {} — start it first",
                    svc.state
                )),
                not_green: vec![service_name.to_string()],
                actions: OpActions::default(),
                probes: indexmap::IndexMap::new(),
                targets: indexmap::IndexMap::new(),
            });
        }
        svc.probes
            .keys()
            .map(|p| ProbeRef::new(service_name, p))
            .collect()
    };

    reprobe_core(state, &probe_refs, &mut probe_statuses, timeout).await;

    // Emit for all services — propagation may have affected others
    emit_svc_display_states(state).await;
    let target_statuses = emit_target_states(state, &[service_name]).await;

    Ok(OpResponse::ok(
        start_time,
        OpActions::default(),
        probe_statuses,
        target_statuses,
    ))
}

pub async fn reprobe_target(
    state: &AppState,
    target_name: &str,
    timeout: Duration,
) -> Result<OpResponse> {
    let start_time = Instant::now();

    let transitive_probes;
    {
        let mut targets = state.targets.write().await;
        if !targets.contains_key(target_name) {
            return Err(GantryError::NotFound(format!(
                "target '{target_name}' not found"
            )));
        }
        // Activate target + transitive dependency targets
        super::activate_target_transitive(&mut targets, target_name);
        transitive_probes = targets[target_name].transitive_probes.clone();
    }

    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();
    reprobe_core(state, &transitive_probes, &mut probe_statuses, timeout).await;

    // Emit for all services — propagation may have affected others
    emit_svc_display_states(state).await;
    let affected_svcs: Vec<String> = transitive_probes
        .iter()
        .map(|c| c.service.clone())
        .collect();
    let affected_refs: Vec<&str> = affected_svcs.iter().map(|s| s.as_str()).collect();
    let target_statuses = emit_target_states(state, &affected_refs).await;

    Ok(OpResponse::ok(
        start_time,
        OpActions::default(),
        probe_statuses,
        target_statuses,
    ))
}

pub async fn reprobe_all(state: &AppState, timeout: Duration) -> Result<OpResponse> {
    let start_time = Instant::now();
    state
        .events
        .emit(crate::events::Event::op_start("reprobe", "all"));
    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();

    // Collect all probes on running services
    let all_probes: Vec<ProbeRef> = {
        let services = state.services.read().await;
        services
            .iter()
            .filter(|(_, svc)| !matches!(svc.state, ServiceState::Stopped | ServiceState::Crashed))
            .flat_map(|(svc_name, svc)| svc.probes.keys().map(move |p| ProbeRef::new(svc_name, p)))
            .collect()
    };

    reprobe_core(state, &all_probes, &mut probe_statuses, timeout).await;

    // Emit SVC display states for all services (probes may have changed any service)
    emit_svc_display_states(state).await;
    let target_statuses = emit_target_states(state, &[]).await;

    Ok(OpResponse::ok(
        start_time,
        OpActions::default(),
        probe_statuses,
        target_statuses,
    ))
}

/// Core reprobe logic shared by reprobe_service, reprobe_target, and reprobe_all.
///
/// 1. Mark specified probes pending + propagate pending downstream
/// 2. Collect all pending non-meta probes (unbounded — catches propagated ones)
/// 3. Fire probes in parallel, resolve in dep order as results stream in
async fn reprobe_core(
    state: &AppState,
    probe_refs: &[ProbeRef],
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
    timeout: Duration,
) {
    mark_pending_and_propagate(state, probe_refs, probe_statuses).await;
    let pending = crate::ops::collect_pending_probes(state, None).await;
    crate::ops::probe_and_resolve(state, &pending, probe_statuses, timeout).await;
}

/// Mark probes pending and propagate pending downstream.
/// Skips probes on stopped/crashed services to avoid leaving them stuck in Pending.
async fn mark_pending_and_propagate(
    state: &AppState,
    probe_refs: &[ProbeRef],
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) {
    let mut services = state.services.write().await;
    let graph = &state.graph;
    let mut changes = Vec::new();
    for probe_ref in probe_refs {
        if let Some(svc) = services.get_mut(&probe_ref.service)
            && matches!(svc.state, crate::model::ServiceState::Running)
            && let Some(probe) = svc.probes.get_mut(&probe_ref.probe)
            && !probe.state.is_pending()
        {
            let prev = probe.state.clone();
            probe.prev_color = Some(prev.color());
            let new_state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
            probe.state = new_state.clone();
            changes.push((probe_ref.clone(), new_state, prev));
        }
        graph.propagate_pending(&probe_ref.to_string(), &mut services, &mut changes);
    }
    emit_propagated_changes(state, &services, &changes, probe_statuses);
}
