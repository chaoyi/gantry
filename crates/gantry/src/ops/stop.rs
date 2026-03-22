use std::time::Instant;

use crate::api::AppState;
use crate::error::{GantryError, Result};
use crate::events::Event;
use crate::model::{ProbeRef, ProbeState, ServiceState};
use crate::ops::{
    OpActions, OpResponse, ProbeStatus, emit_propagated_changes, emit_svc_display_states,
    emit_target_states,
};

pub async fn stop(state: &AppState, service_name: &str) -> Result<OpResponse> {
    let start = Instant::now();
    let mut actions = OpActions::default();
    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();

    {
        let services = state.services.read().await;
        if !services.contains_key(service_name) {
            return Err(GantryError::NotFound(format!(
                "service '{service_name}' not found"
            )));
        }
        if services[service_name].state == ServiceState::Stopped {
            drop(services);
            let target_statuses = emit_target_states(state, &[service_name]).await;
            return Ok(OpResponse {
                result: "ok".into(),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
                actions,
                probes: indexmap::IndexMap::new(),
                targets: target_statuses,
            });
        }
    }

    // Mark stopped and propagate BEFORE docker stop — UI updates instantly via WS
    let container_name;
    {
        let mut services = state.services.write().await;
        let svc = services.get_mut(service_name).unwrap();
        container_name = svc.container.clone();
        svc.state = ServiceState::Stopped;
        svc.last_emitted_display = Some(crate::model::SvcDisplayState::Stopped);
        state.events.emit(Event::service_state(
            service_name,
            ServiceState::Stopped,
            "stopped",
        ));

        // Mark all probes red
        let probe_names: Vec<String> = svc.probes.keys().cloned().collect();
        for probe_name in &probe_names {
            let probe_ref = ProbeRef::new(service_name, probe_name);
            let probe = svc.probes.get_mut(probe_name).unwrap();
            let prev = probe.state;
            probe.prev_state = Some(prev);
            probe.state = ProbeState::Red;
            if prev != ProbeState::Red {
                state.events.emit(Event::probe_state_change(
                    &probe_ref,
                    ProbeState::Red,
                    prev,
                    "stopped",
                ));
            }
            probe_statuses.insert(
                probe_ref.to_string(),
                ProbeStatus {
                    state: "stopped".into(),
                    prev: prev.as_str().into(),
                    probe_ms: None,
                    error: None,
                    logs: None,
                },
            );
        }

        // Propagate staleness downstream — probes already marked Red above
        let graph = state.graph.read().await;
        let mut changes = Vec::new();
        for probe_name in &probe_names {
            let probe_key = format!("{service_name}.{probe_name}");
            graph.propagate_staleness(&probe_key, &mut services, &mut changes);
        }
        emit_propagated_changes(state, &services, &changes, &mut probe_statuses);
    }

    actions.stopped.push(service_name.to_string());
    let all_svcs: Vec<String> = state.services.read().await.keys().cloned().collect();
    let all_refs: Vec<&str> = all_svcs.iter().map(|s| s.as_str()).collect();
    emit_svc_display_states(state, &all_refs).await;
    let target_statuses = emit_target_states(state, &[service_name]).await;

    // Now actually stop the container — state already propagated
    state.docker.stop_container(&container_name).await?;

    Ok(OpResponse {
        result: "ok".to_string(),
        duration_ms: start.elapsed().as_millis() as u64,
        error: None,
        actions,
        probes: probe_statuses,
        targets: target_statuses,
    })
}
