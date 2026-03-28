use std::time::Instant;

use crate::api::AppState;
use crate::error::{GantryError, Result};
use crate::model::ServiceState;
use crate::ops::{
    OpActions, OpResponse, ProbeStatus, emit_propagated_changes, emit_svc_display_states,
    emit_target_states, mark_all_probes_red, propagate_all_pending,
};

pub async fn stop(state: &AppState, service_name: &str) -> Result<OpResponse> {
    let start = Instant::now();
    let mut actions = OpActions::default();
    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();

    // Single write lock: check state + mark stopped + propagate.
    // No gap between check and update → watcher can't intervene with "died external".
    let (container_name, skip_docker_stop) = {
        let mut services = state.services.write().await;
        let Some(svc) = services.get_mut(service_name) else {
            return Err(GantryError::NotFound(format!(
                "service '{service_name}' not found"
            )));
        };

        if svc.state == ServiceState::Stopped {
            drop(services);
            let target_statuses = emit_target_states(state, &[service_name]).await;
            return Ok(OpResponse::ok(
                start,
                actions,
                indexmap::IndexMap::new(),
                target_statuses,
            ));
        }

        let skip = svc.state == ServiceState::Crashed;
        let container = svc.container.clone();
        svc.state = ServiceState::Stopped;
        svc.generation += 1;

        // Mark all probes red — collect changes for unified emission
        let changes = mark_all_probes_red(service_name, svc, || crate::model::RedReason::Stopped);
        emit_propagated_changes(state, &services, &changes, &mut probe_statuses);

        // Propagate pending downstream
        let changes = propagate_all_pending(&state.graph, service_name, &mut services);
        emit_propagated_changes(state, &services, &changes, &mut probe_statuses);

        (container, skip)
    };

    // Emit display states for all services (some may change due to propagation)
    actions.stopped.push(service_name.to_string());
    emit_svc_display_states(state).await;
    let target_statuses = emit_target_states(state, &[service_name]).await;

    // Stop the container (blocking — AI callers need to know when it's done).
    // Errors are ignored since state is already correct.
    if !skip_docker_stop && let Err(e) = state.docker.stop_container(&container_name).await {
        tracing::warn!("svc [{service_name}] docker stop error (ignored): {e}");
    }

    Ok(OpResponse::ok(
        start,
        actions,
        probe_statuses,
        target_statuses,
    ))
}
