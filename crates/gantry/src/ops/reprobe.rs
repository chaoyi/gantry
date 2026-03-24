use futures::StreamExt;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::api::AppState;
use crate::error::{GantryError, Result};
use crate::model::{ProbeRef, ProbeState};
use crate::ops::{
    OpActions, OpResponse, ProbeStatus, apply_probe_result, emit_propagated_changes,
    emit_svc_display_states, emit_target_states, resolve_probe_batch, update_meta_probes,
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

    {
        let services = state.services.read().await;
        if !services.contains_key(service_name) {
            return Err(GantryError::NotFound(format!(
                "service '{service_name}' not found"
            )));
        }
    }

    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();
    let probe_refs: Vec<ProbeRef> = {
        let services = state.services.read().await;
        services[service_name]
            .probes
            .keys()
            .map(|p| ProbeRef::new(service_name, p))
            .collect()
    };
    mark_stale_and_propagate(state, &probe_refs, &mut probe_statuses).await;

    let stale = crate::ops::collect_stale_probes(state, None).await;
    crate::ops::probe_and_resolve(state, &stale, &mut probe_statuses, timeout).await;

    emit_svc_display_states(state, &[service_name]).await;
    let target_statuses = emit_target_states(state, &[service_name]).await;

    Ok(OpResponse {
        result: "ok".to_string(),
        duration_ms: start_time.elapsed().as_millis() as u64,
        error: None,
        actions: OpActions::default(),
        probes: probe_statuses,
        targets: target_statuses,
    })
}

pub async fn reprobe_target(
    state: &AppState,
    target_name: &str,
    timeout: Duration,
) -> Result<OpResponse> {
    let start_time = Instant::now();

    let transitive_probes;
    {
        let targets = state.targets.read().await;
        let tgt = targets
            .get(target_name)
            .ok_or_else(|| GantryError::NotFound(format!("target '{target_name}' not found")))?;
        transitive_probes = tgt.transitive_probes.clone();
    }

    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();
    mark_stale_and_propagate(state, &transitive_probes, &mut probe_statuses).await;

    let affected_svcs: Vec<String> = transitive_probes
        .iter()
        .map(|c| c.service.clone())
        .collect();
    let affected_refs: Vec<&str> = affected_svcs.iter().map(|s| s.as_str()).collect();
    let stale = crate::ops::collect_stale_probes(state, None).await;
    crate::ops::probe_and_resolve(state, &stale, &mut probe_statuses, timeout).await;

    emit_svc_display_states(state, &affected_refs).await;
    let target_statuses = emit_target_states(state, &affected_refs).await;

    Ok(OpResponse {
        result: "ok".to_string(),
        duration_ms: start_time.elapsed().as_millis() as u64,
        error: None,
        actions: OpActions::default(),
        probes: probe_statuses,
        targets: target_statuses,
    })
}

/// Reprobe all probes: mark stale, probe in parallel, resolve as results arrive.
/// Uses the dependency graph to resolve probes as their deps become green.
pub async fn reprobe_all(state: &AppState, timeout: Duration) -> Result<OpResponse> {
    use crate::model::ServiceState;
    use std::collections::HashMap;

    let start_time = Instant::now();
    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();

    // 1. Mark all running services' non-red probes as stale
    {
        let mut services = state.services.write().await;
        let mut changes = Vec::new();
        for (svc_name, svc) in services.iter_mut() {
            if svc.state == ServiceState::Stopped {
                continue;
            }
            for (probe_name, probe) in svc.probes.iter_mut() {
                if probe.state.is_green() || probe.state.is_stale() {
                    let prev = probe.state.clone();
                    probe.prev_color = Some(prev.color());
                    let new_state = ProbeState::Stale(crate::model::StaleReason::Reprobing);
                    probe.state = new_state.clone();
                    if !prev.is_stale() {
                        changes.push((ProbeRef::new(svc_name, probe_name), new_state, prev));
                    }
                }
            }
        }
        emit_propagated_changes(state, &services, &changes, &mut probe_statuses);
    }

    // 2. Collect all stale non-meta probes and fire probes in parallel
    let stale_probes = crate::ops::collect_stale_probes(state, None).await;

    let docker = state.docker.inner();
    let tx = state.events.tx.clone();
    let mut futs = futures::stream::FuturesUnordered::new();
    for (probe_ref, svc_name, container, probe_config) in &stale_probes {
        let docker = docker.clone();
        let svc = svc_name.clone();
        let ctr = container.clone();
        let pc = probe_config.clone();
        let cr = probe_ref.clone();
        let tx = tx.clone();
        futs.push(async move {
            let _ = tx.send(crate::events::Event::probe_result(
                &cr,
                false,
                None,
                0,
                None,
                chrono::Utc::now().timestamp_millis(),
            ));
            let result = crate::probe::run_single_attempt(&docker, &svc, &ctr, &pc, timeout).await;
            (cr, result)
        });
    }

    // 3. As each probe completes, try to resolve it and cascade through the graph.
    // pending: probes whose probe passed but deps aren't all green yet.
    let mut pending: HashMap<String, crate::probe::ProbeOutcome> = HashMap::new();
    let mut resolved = HashSet::new();

    while let Some((probe_ref, outcome)) = futs.next().await {
        let key = probe_ref.to_string();
        let probe_ok = matches!(outcome.result, crate::probe::ProbeResult::Ok { .. });

        if !probe_ok {
            apply_probe_result(state, &probe_ref, &outcome, &mut probe_statuses).await;
            resolved.insert(key.clone());
        } else {
            pending.insert(key, outcome);
        }

        // Try to resolve pending probes whose deps are now green (cascading)
        let mut batch_svcs = HashSet::new();
        let mut progress = true;
        while progress {
            progress = false;
            let pending_keys: Vec<String> = pending.keys().cloned().collect();
            for pkey in pending_keys {
                let pcr = ProbeRef::parse(&pkey).unwrap();
                let deps_ok = {
                    let services = state.services.read().await;
                    let probe = &services[&pcr.service].probes[&pcr.probe];
                    probe.depends_on.iter().all(|dep| {
                        services
                            .get(&dep.service)
                            .and_then(|s| s.probes.get(&dep.probe))
                            .is_some_and(|c| c.state.is_green())
                    })
                };
                if deps_ok {
                    let outcome = pending.remove(&pkey).unwrap();
                    apply_probe_result(state, &pcr, &outcome, &mut probe_statuses).await;
                    resolved.insert(pkey);
                    batch_svcs.insert(pcr.service.clone());
                    progress = true;
                }
            }
        }

        // Debounced: meta + display for all affected in this round
        batch_svcs.insert(probe_ref.service.clone());
        for svc in &batch_svcs {
            update_meta_probes(state, svc, &mut probe_statuses).await;
        }
        let refs: Vec<&str> = batch_svcs.iter().map(|s| s.as_str()).collect();
        emit_svc_display_states(state, &refs).await;
    }

    // 4. Remaining pending: apply as batch (deps never resolved → stays stale)
    if !pending.is_empty() {
        let remaining: Vec<(ProbeRef, crate::probe::ProbeOutcome)> = pending
            .into_iter()
            .map(|(k, v)| (ProbeRef::parse(&k).unwrap(), v))
            .collect();
        resolve_probe_batch(state, &remaining, &mut probe_statuses).await;
    }

    // 5. Final target state emit
    let target_statuses = emit_target_states(state, &[]).await;

    Ok(OpResponse {
        result: "ok".to_string(),
        duration_ms: start_time.elapsed().as_millis() as u64,
        error: None,
        actions: OpActions::default(),
        probes: probe_statuses,
        targets: target_statuses,
    })
}

/// Mark green probes stale and propagate staleness downstream.
async fn mark_stale_and_propagate(
    state: &AppState,
    probe_refs: &[ProbeRef],
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
) {
    let mut services = state.services.write().await;
    let graph = state.graph.read().await;
    let mut changes = Vec::new();
    for probe_ref in probe_refs {
        if let Some(svc) = services.get_mut(&probe_ref.service)
            && let Some(probe) = svc.probes.get_mut(&probe_ref.probe)
            && probe.state.is_green()
        {
            let prev = probe.state.clone();
            probe.prev_color = Some(prev.color());
            let new_state = ProbeState::Stale(crate::model::StaleReason::Reprobing);
            probe.state = new_state.clone();
            changes.push((probe_ref.clone(), new_state, prev));
        }
        graph.propagate_staleness(&probe_ref.to_string(), &mut services, &mut changes);
    }
    emit_propagated_changes(state, &services, &changes, probe_statuses);
}
