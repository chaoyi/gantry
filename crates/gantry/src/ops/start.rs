use std::time::{Duration, Instant};

use crate::api::AppState;
use crate::config::ProbeConfig;
use crate::error::{GantryError, Result};
use crate::events::Event;
use crate::model::{ProbeRef, ServiceState};
use crate::ops::{OpActions, OpResponse, ProbeStatus, emit_target_states, resolve_probe_batch};

pub async fn start(state: &AppState, service_name: &str, timeout: Duration) -> Result<OpResponse> {
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
            return Ok(OpResponse {
                result: "ok".into(),
                duration_ms: start_time.elapsed().as_millis() as u64,
                error: None,
                actions: OpActions::default(),
                probes: indexmap::IndexMap::new(),
                targets: target_statuses,
            });
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
        svc.log_since = log_since;
        state.events.emit(Event::service_state(
            service_name,
            ServiceState::Running,
            "red",
        ));
    }

    // Probe probes using centralized probe dispatch
    let probe_configs: Vec<(String, ProbeConfig)>;
    {
        let services = state.services.read().await;
        let svc = &services[service_name];
        probe_configs = svc
            .probes
            .iter()
            .map(|(name, probe_rt)| (name.clone(), probe_rt.probe_config.clone()))
            .collect();
    }
    let backoff = state.config.read().await.defaults.probe_backoff.clone();
    let remaining = timeout.saturating_sub(start_time.elapsed());

    state.events.emit(Event::op_start("start", service_name));

    // Probe all probes in parallel — apply each result as it arrives
    let docker = state.docker.inner();
    let mut futs = futures::stream::FuturesUnordered::new();
    for (probe_name, probe_config) in &probe_configs {
        if matches!(probe_config, crate::config::ProbeConfig::Meta) {
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
            let result = crate::probe::run_with_retry(
                &docker, &svc, &container, &pc, remaining, &backoff, log_since,
            )
            .await;
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

    Ok(OpResponse {
        result: "ok".to_string(),
        duration_ms: start_time.elapsed().as_millis() as u64,
        error: None,
        actions,
        probes: probe_statuses,
        targets: target_statuses,
    })
}
