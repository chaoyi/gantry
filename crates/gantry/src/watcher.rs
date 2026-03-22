use std::collections::HashMap;
use std::sync::Arc;

use bollard::system::EventsOptions;
use futures::StreamExt;

use crate::api::AppState;
use crate::events::Event;
use crate::model::{ProbeRef, ProbeState, ServiceState};
use crate::ops::{
    ProbeStatus, emit_propagated_changes, emit_svc_display_states, emit_target_states,
};

/// Watch Docker events and update gantry state when containers start/stop externally.
pub async fn watch_docker_events(state: Arc<AppState>) {
    let filters: HashMap<String, Vec<String>> = HashMap::from([(
        "event".to_string(),
        vec!["die".to_string(), "start".to_string()],
    )]);
    let opts = EventsOptions {
        filters,
        ..Default::default()
    };

    let mut stream = state.docker.inner().events(Some(opts));

    while let Some(Ok(event)) = stream.next().await {
        let Some(action) = event.action.as_deref() else {
            continue;
        };
        // Container name from event actor attributes
        let container_name = event
            .actor
            .as_ref()
            .and_then(|a| a.attributes.as_ref())
            .and_then(|attrs| attrs.get("name"))
            .cloned();
        let Some(container_name) = container_name else {
            continue;
        };

        // Find matching gantry service by container name
        let svc_name = {
            let services = state.services.read().await;
            services
                .iter()
                .find(|(_, svc)| svc.container == container_name)
                .map(|(name, _)| name.clone())
        };
        let Some(svc_name) = svc_name else {
            continue;
        };

        // Skip if gantry is currently running any operation —
        // the operation will handle state updates itself
        if state.op_lock.current_op().is_some() {
            continue;
        }

        match action {
            "die" => handle_die(&state, &svc_name).await,
            "start" => handle_start(&state, &svc_name).await,
            _ => {}
        }
    }
}

/// Container died: mark service stopped + probes red + propagate.
async fn handle_die(state: &AppState, svc_name: &str) {
    tracing::info!("[{svc_name}] container died (external)");
    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();

    {
        let mut services = state.services.write().await;
        let Some(svc) = services.get_mut(svc_name) else {
            return;
        };
        if svc.state == ServiceState::Stopped {
            return; // already stopped (e.g., gantry stopped it)
        }
        svc.state = ServiceState::Stopped;
        state.events.emit(Event::service_state(
            svc_name,
            ServiceState::Stopped,
            "stopped",
        ));

        let probe_names: Vec<String> = svc.probes.keys().cloned().collect();
        for probe_name in &probe_names {
            let probe_ref = ProbeRef::new(svc_name, probe_name);
            let probe = svc.probes.get_mut(probe_name).unwrap();
            let prev = probe.state;
            probe.prev_state = Some(prev);
            probe.state = ProbeState::Red;
            state.events.emit(Event::probe_state_change(
                &probe_ref,
                ProbeState::Red,
                prev,
                "stopped",
            ));
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

        let graph = state.graph.read().await;
        let mut changes = Vec::new();
        for probe_name in &probe_names {
            let probe_key = format!("{svc_name}.{probe_name}");
            graph.propagate_staleness(&probe_key, &mut services, &mut changes);
        }
        emit_propagated_changes(state, &services, &changes, &mut probe_statuses);
    }

    let all_svcs: Vec<String> = state.services.read().await.keys().cloned().collect();
    let all_refs: Vec<&str> = all_svcs.iter().map(|s| s.as_str()).collect();
    emit_svc_display_states(state, &all_refs).await;
    emit_target_states(state, &[svc_name]).await;
}

/// Container started: mark service running + reprobe to get green/red.
async fn handle_start(state: &AppState, svc_name: &str) {
    tracing::info!("[{svc_name}] container started (external)");

    {
        let mut services = state.services.write().await;
        let Some(svc) = services.get_mut(svc_name) else {
            return;
        };
        if svc.state == ServiceState::Running {
            return; // already running (e.g., gantry started it)
        }
        svc.state = ServiceState::Running;
        state.events.emit(Event::service_state(
            svc_name,
            ServiceState::Running,
            "running",
        ));

        // Mark all probes stale for reprobing
        for (probe_name, probe) in svc.probes.iter_mut() {
            if probe.state != ProbeState::Stale {
                let prev = probe.state;
                probe.prev_state = Some(prev);
                probe.state = ProbeState::Stale;
                let probe_ref = ProbeRef::new(svc_name, probe_name);
                state.events.emit(Event::probe_state_change(
                    &probe_ref,
                    ProbeState::Stale,
                    prev,
                    "stale",
                ));
            }
        }
    }

    emit_svc_display_states(state, &[svc_name]).await;
    emit_target_states(state, &[svc_name]).await;
}
