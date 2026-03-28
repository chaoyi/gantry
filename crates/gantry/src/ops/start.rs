use std::time::{Duration, Instant};

use crate::api::AppState;
use crate::config::ProbeConfig;
use crate::error::{GantryError, Result};
use crate::events::Event;
use crate::model::{ProbeRef, ProbeState, ServiceState};
use crate::ops::{OpActions, OpResponse, ProbeStatus, emit_target_states, resolve_probe_batch};

/// Start a service. If `check_start_after` is true (API calls), validates that
/// start_after deps are Green before starting. Converge passes false (it manages
/// ordering itself).
pub async fn start(
    state: &AppState,
    service_name: &str,
    timeout: Duration,
    check_start_after: bool,
) -> Result<OpResponse> {
    let start_time = Instant::now();
    let mut actions = OpActions::default();
    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();

    let container_name;
    {
        let services = state.services.read().await;
        let svc = services
            .get(service_name)
            .ok_or_else(|| GantryError::NotFound(format!("service '{service_name}' not found")))?;
        if svc.state == ServiceState::Running {
            drop(services);
            let target_statuses = emit_target_states(state, &[service_name]).await;
            return Ok(OpResponse::ok(
                start_time,
                OpActions::default(),
                indexmap::IndexMap::new(),
                target_statuses,
            ));
        }
        // Check start_after deps (API calls only; converge skips this)
        if check_start_after && !svc.start_after.is_empty() {
            let mut unmet: Vec<String> = Vec::new();
            for dep in &svc.start_after {
                let dep_green = services
                    .get(&dep.service)
                    .and_then(|s| s.probes.get(&dep.probe))
                    .is_some_and(|p| p.state.is_green());
                if !dep_green {
                    let dep_state_str = services
                        .get(&dep.service)
                        .and_then(|s| s.probes.get(&dep.probe))
                        .map(|p| p.state.as_str())
                        .unwrap_or("missing");
                    let suggestion = match dep_state_str {
                        "red" => format!("check {}", dep.service),
                        "pending" => format!("reprobe {}", dep.service),
                        _ => format!("start {}", dep.service),
                    };
                    unmet.push(format!("{dep}: {dep_state_str} ({suggestion})"));
                }
            }
            if !unmet.is_empty() {
                return Err(GantryError::Operation(format!(
                    "start_after not satisfied: {}. Use converge to resolve all dependencies",
                    unmet.join(", ")
                )));
            }
        }

        container_name = svc.container.clone();
    }

    // Start the container
    state.docker.start_container(&container_name).await?;

    // Get container start timestamp for log probes (avoid matching old logs)
    let log_since = state
        .docker
        .inspect_container(&container_name)
        .await
        .ok()
        .flatten()
        .map(|info| info.started_at)
        .unwrap_or(0);

    {
        let mut services = state.services.write().await;
        let svc = services.get_mut(service_name).unwrap();
        svc.state = ServiceState::Running;
        svc.generation += 1;
        svc.log_since = log_since;
        // Mark all probes as Reprobing — even if already pending (ContainerStarted from watcher)
        let mut changes = Vec::new();
        for (probe_name, probe) in svc.probes.iter_mut() {
            if !matches!(
                probe.state,
                ProbeState::Pending(crate::model::PendingReason::Reprobing)
            ) {
                let prev = probe.state.clone();
                probe.prev_color = Some(prev.color());
                let new_state = ProbeState::Pending(crate::model::PendingReason::Reprobing);
                probe.state = new_state.clone();
                changes.push((ProbeRef::new(service_name, probe_name), new_state, prev));
            }
        }
        let mut probe_statuses_tmp = indexmap::IndexMap::new();
        crate::ops::emit_propagated_changes(state, &services, &changes, &mut probe_statuses_tmp);
    }
    crate::ops::emit_svc_display_states(state).await;

    // Probe probes using centralized probe dispatch
    let probe_configs: Vec<(String, ProbeConfig)>;
    let generation: u64;
    {
        let services = state.services.read().await;
        let svc = &services[service_name];
        probe_configs = svc
            .probes
            .iter()
            .map(|(name, probe_rt)| (name.clone(), probe_rt.probe_config.clone()))
            .collect();
        generation = svc.generation;
    }
    let backoff = state.config.defaults.probe_backoff.clone();
    let remaining = timeout.saturating_sub(start_time.elapsed());

    state.events.emit(Event::op_start("start", service_name));

    // Probe all probes in parallel — apply each result as it arrives
    let docker = state.docker.inner();
    let mut futs = futures::stream::FuturesUnordered::new();
    for (probe_name, probe_config) in &probe_configs {
        if probe_config.is_meta() {
            continue;
        }
        let docker = docker.clone();
        let svc = service_name.to_string();
        let container = container_name.clone();
        let pc = probe_config.clone();
        let backoff = backoff.clone();
        let probe_name = probe_name.clone();
        futs.push(async move {
            let cr = ProbeRef::new(&svc, &probe_name);
            let mut result = crate::probe::run_with_retry(
                &docker, &svc, &container, &pc, remaining, &backoff, log_since,
            )
            .await;
            result.generation = generation;
            (cr, result)
        });
    }

    // Collect all results, then resolve in topo order.
    // This ensures intra-service deps (e.g. port before ready) are satisfied
    // before dependents are evaluated.
    use futures::StreamExt;
    let mut all_results = Vec::new();
    while let Some(item) = futs.next().await {
        all_results.push(item);
    }
    resolve_probe_batch(state, &all_results, &mut probe_statuses).await;

    actions.started.push(service_name.to_string());
    let target_statuses = emit_target_states(state, &[service_name]).await;

    Ok(OpResponse::ok(
        start_time,
        actions,
        probe_statuses,
        target_statuses,
    ))
}
