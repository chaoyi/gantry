use std::collections::HashMap;
use std::sync::Arc;

use bollard::system::EventsOptions;
use futures::StreamExt;

use crate::api::AppState;
use crate::model::ServiceState;
use crate::ops::{
    ProbeStatus, emit_propagated_changes, emit_svc_display_states, emit_target_states,
    mark_all_probes_pending, mark_all_probes_red, propagate_all_pending,
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
        let container_name = event
            .actor
            .as_ref()
            .and_then(|a| a.attributes.as_ref())
            .and_then(|attrs| attrs.get("name"))
            .cloned();
        let Some(container_name) = container_name else {
            continue;
        };

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

        match action {
            "die" => handle_die(&state, &svc_name).await,
            "start" => handle_start(&state, &svc_name).await,
            _ => {}
        }
    }
}

/// Container died: mark service crashed + probes red + propagate.
async fn handle_die(state: &AppState, svc_name: &str) {
    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();

    {
        let mut services = state.services.write().await;
        let Some(svc) = services.get_mut(svc_name) else {
            return;
        };
        if svc.state == ServiceState::Stopped {
            return; // gantry stopped it — not an external death
        }
        tracing::debug!("watcher: [{svc_name}] container died");
        svc.state = ServiceState::Crashed;
        svc.generation += 1;

        // Mark all probes red — collect changes for unified emission
        let changes = mark_all_probes_red(svc_name, svc, || crate::model::RedReason::ContainerDied);
        emit_propagated_changes(state, &services, &changes, &mut probe_statuses);

        // Propagate pending downstream
        let prop_changes = propagate_all_pending(&state.graph, svc_name, &mut services);
        emit_propagated_changes(state, &services, &prop_changes, &mut probe_statuses);
    }

    emit_svc_display_states(state).await;
    emit_target_states(state, &[svc_name]).await;
}

/// Container started: mark service running + probes pending + propagate recovery.
async fn handle_start(state: &AppState, svc_name: &str) {
    tracing::debug!("watcher: [{svc_name}] container started");
    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();

    {
        let mut services = state.services.write().await;
        let Some(svc) = services.get_mut(svc_name) else {
            return;
        };
        if svc.state == ServiceState::Running {
            return;
        }
        svc.state = ServiceState::Running;
        svc.generation += 1;

        let changes = mark_all_probes_pending(svc_name, svc, || {
            crate::model::PendingReason::ContainerStarted
        });
        let was_red: Vec<String> = changes
            .iter()
            .filter(|(_, _, prev)| prev.is_red())
            .map(|(pr, _, _)| pr.probe.clone())
            .collect();
        emit_propagated_changes(state, &services, &changes, &mut probe_statuses);

        let graph = &state.graph;
        let mut prop_changes = Vec::new();
        for probe_name in &was_red {
            let probe_key = format!("{svc_name}.{probe_name}");
            graph.propagate_recovery(&probe_key, &mut services, &mut prop_changes);
        }
        emit_propagated_changes(state, &services, &prop_changes, &mut probe_statuses);
    }

    emit_svc_display_states(state).await;
    emit_target_states(state, &[svc_name]).await;
}
