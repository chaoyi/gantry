use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::AppState;
use crate::error::{GantryError, Result};
use crate::events::Event;
use crate::model::{ProbeRef, ProbeState, ServiceState};
use crate::ops::{
    OpActions, OpResponse, ProbeStatus, emit_propagated_changes, emit_svc_display_states,
    emit_target_states,
};

pub async fn converge(
    state: &Arc<AppState>,
    target_name: &str,
    timeout: Duration,
) -> Result<OpResponse> {
    let start_time = Instant::now();
    let mut actions = OpActions::default();
    let mut probe_statuses: indexmap::IndexMap<String, ProbeStatus> = indexmap::IndexMap::new();
    let mut attempted: HashSet<String> = HashSet::new();

    let transitive_probes;
    {
        let targets = state.targets.read().await;
        let tgt = targets
            .get(target_name)
            .ok_or_else(|| GantryError::NotFound(format!("target '{target_name}' not found")))?;
        transitive_probes = tgt.transitive_probes.clone();
    }

    state.events.emit(Event::op_start("converge", target_name));

    // Collect services needed for this target
    let needed_services: Vec<String> = {
        let mut svcs: Vec<String> = transitive_probes
            .iter()
            .map(|cr| cr.service.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let graph = state.graph.read().await;
        let topo = &graph.topo_order;
        svcs.sort_by_key(|s| topo.iter().position(|t| t == s).unwrap_or(usize::MAX));
        svcs
    };

    // Phase 1: Start services that aren't running.
    // Event-driven: each service waits only for its own start_after probes.
    {
        let to_start: Vec<(String, Vec<ProbeRef>)> = {
            let services = state.services.read().await;
            needed_services
                .iter()
                .filter(|svc_name| services[*svc_name].state != ServiceState::Running)
                .map(|svc_name| {
                    let deps = services[svc_name].start_after.clone();
                    (svc_name.clone(), deps)
                })
                .collect()
        };

        let remaining = timeout.saturating_sub(start_time.elapsed());
        let mut handles = Vec::new();
        for (svc_name, start_after_deps) in to_start {
            let state = state.clone();
            handles.push(tokio::spawn(async move {
                let deadline = Instant::now() + remaining;
                // Wait for start_after probes to be green
                for dep in &start_after_deps {
                    tracing::trace!("WAIT {svc_name} for {dep}");
                    while Instant::now() < deadline {
                        let services = state.services.read().await;
                        if let Some(svc) = services.get(&dep.service)
                            && let Some(probe) = svc.probes.get(&dep.probe)
                            && probe.state == ProbeState::Green
                        {
                            break;
                        }
                        drop(services);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    tracing::trace!("WAIT {svc_name} for {dep} -> ready");
                }
                if start_after_deps.is_empty() {
                    tracing::info!("[{svc_name}] starting");
                } else {
                    let deps: Vec<String> =
                        start_after_deps.iter().map(|d| d.to_string()).collect();
                    tracing::info!("[{svc_name}] starting (after {})", deps.join(", "));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let result = super::start::start(&state, &svc_name, remaining).await;
                (svc_name, result)
            }));
        }

        for handle in handles {
            match handle.await {
                Ok((svc_name, result)) => {
                    match result {
                        Ok(resp) => {
                            merge_probe_statuses(&mut probe_statuses, &resp.probes);
                            actions.started.push(svc_name.clone());
                        }
                        Err(e) => {
                            tracing::warn!("converge: failed to start {svc_name}: {e}");
                        }
                    }
                    attempted.insert(svc_name);
                }
                Err(e) => {
                    tracing::error!("converge: start task panicked: {e}");
                }
            }
        }
    }

    // Phase 2: Reprobe stale probes (single pass — resolve_probe_batch handles topo order)
    reprobe_target_stale(
        state,
        &transitive_probes,
        &mut probe_statuses,
        timeout.saturating_sub(start_time.elapsed()),
    )
    .await;

    // Phase 3: Restart services with red probes (at most once each)
    let broken_services: Vec<(String, Vec<String>)> = {
        let services = state.services.read().await;
        needed_services
            .iter()
            .filter_map(|svc_name| {
                if attempted.contains(svc_name) {
                    return None;
                }
                let svc = &services[svc_name];
                let red_probes: Vec<String> = svc
                    .probes
                    .iter()
                    .filter(|(_, probe)| probe.state == ProbeState::Red)
                    .map(|(name, _)| name.clone())
                    .collect();
                if red_probes.is_empty() {
                    None
                } else {
                    Some((svc_name.clone(), red_probes))
                }
            })
            .collect()
    };

    for (svc_name, red_probes) in &broken_services {
        if start_time.elapsed() >= timeout {
            break;
        }

        let reason = format!("{} red", red_probes.join(", "));
        tracing::info!("restart {svc_name} ({reason})");
        state.events.emit(Event::ServiceRestart {
            service: svc_name.clone(),
            reason: reason.clone(),
            ts: chrono::Utc::now().timestamp_millis(),
        });

        let remaining = timeout.saturating_sub(start_time.elapsed());
        match super::restart::restart(state, svc_name, remaining).await {
            Ok(resp) => {
                merge_probe_statuses(&mut probe_statuses, &resp.probes);
                actions.restarted.push(svc_name.clone());
            }
            Err(e) => {
                tracing::warn!("converge: failed to restart {svc_name}: {e}");
            }
        }
        attempted.insert(svc_name.clone());

        // Propagate staleness only from probes that are NOT green after restart
        {
            let graph = state.graph.read().await;
            let mut services = state.services.write().await;
            let stale_probe_keys: Vec<String> = services[svc_name]
                .probes
                .iter()
                .filter(|(_, probe)| probe.state != ProbeState::Green)
                .map(|(name, _)| format!("{svc_name}.{name}"))
                .collect();
            let mut changes = Vec::new();
            for probe_key in &stale_probe_keys {
                graph.propagate_staleness(probe_key, &mut services, &mut changes);
            }
            emit_propagated_changes(state, &services, &changes, &mut probe_statuses);
        }

        reprobe_target_stale(
            state,
            &transitive_probes,
            &mut probe_statuses,
            timeout.saturating_sub(start_time.elapsed()),
        )
        .await;
    }

    // Phase 4: Compute definitive result
    let (result_str, target_statuses) = {
        let affected_svcs: Vec<String> = transitive_probes
            .iter()
            .map(|c| c.service.clone())
            .collect();
        let affected_refs: Vec<&str> = affected_svcs.iter().map(|s| s.as_str()).collect();
        emit_svc_display_states(state, &affected_refs).await;
        let target_statuses = emit_target_states(state, &affected_refs).await;

        let result_str = match target_statuses.get(target_name).map(|s| s.state.as_str()) {
            Some("green") => "ok",
            _ => "failed",
        };
        (result_str, target_statuses)
    };

    let duration_ms = start_time.elapsed().as_millis() as u64;
    state.events.emit(Event::op_complete(
        "converge",
        target_name,
        result_str,
        duration_ms,
    ));

    Ok(OpResponse {
        result: result_str.to_string(),
        duration_ms,
        error: if result_str == "failed" {
            Some("target not testable".into())
        } else {
            None
        },
        actions,
        probes: probe_statuses,
        targets: target_statuses,
    })
}

/// Reprobe stale probes within a target's transitive set.
async fn reprobe_target_stale(
    state: &AppState,
    transitive_probes: &[ProbeRef],
    probe_statuses: &mut indexmap::IndexMap<String, ProbeStatus>,
    timeout: Duration,
) {
    use crate::ops::{collect_stale_probes, probe_and_resolve};
    let stale = collect_stale_probes(state, Some(transitive_probes)).await;
    probe_and_resolve(state, &stale, probe_statuses, timeout).await;
}

fn merge_probe_statuses(
    target: &mut indexmap::IndexMap<String, ProbeStatus>,
    source: &indexmap::IndexMap<String, ProbeStatus>,
) {
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
}
